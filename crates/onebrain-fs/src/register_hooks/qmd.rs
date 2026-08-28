//! qmd PostToolUse hook registration + legacy `qmd update ...` migration + dedupe.
//!
//! Mirrors Bun `register-hooks.ts::applyQmdHook` and `migrateLegacyQmdEntries`.

use super::hooks::{
    append_json_if_needed, matches_spec, matches_spec_pre_json, rewrite_if_shell_form, HookSpec,
    HookStatus,
};
use serde_json::{json, Map, Value};

const QMD_MATCHER: &str = "Write|Edit";

fn is_canonical_qmd_entry(entry: &Value) -> bool {
    matches_spec(entry, &HookSpec::REINDEX)
}

fn is_runner_entry(entry: &Value) -> bool {
    matches_spec(entry, &HookSpec::RUNNER)
}

fn is_canonical_runner(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("command")
        && entry.get("command").and_then(Value::as_str) == Some(HookSpec::RUNNER.command)
        && entry
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| args.as_slice() == [json!("hook")])
}

/// True when the entry is the pre-v3.1 (no `--json`) shape of the qmd hook.
/// Used by strip_qmd_hook to clean both shapes.
fn is_pre_json_qmd_entry(entry: &Value) -> bool {
    matches_spec_pre_json(entry, &HookSpec::REINDEX)
}

/// Match legacy `qmd update <args>` patterns. Word-bounded so wrapped commands
/// still match (e.g. `powershell.exe -Command qmd update -c 'x'`,
/// `bash -lc 'qmd update -c x'`). Equivalent to Bun's regex
/// `/\bqmd\s+update\b/`.
fn is_legacy_qmd_cmd(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i + 3 <= len {
        if &bytes[i..i + 3] == b"qmd"
            && (i == 0 || !is_word(bytes[i - 1]))
            && i + 3 < len
            && bytes[i + 3].is_ascii_whitespace()
        {
            let mut j = i + 3;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j + 6 <= len
                && &bytes[j..j + 6] == b"update"
                && (j + 6 == len || !is_word(bytes[j + 6]))
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True when `entry` is the v3.0/v3.1 hidden alias form of the qmd hook —
/// `onebrain qmd-reindex` (hyphen) rather than the canonical `qmd reindex`
/// (space). Covers exec form (`args: ["qmd-reindex", ...]`) and shell form
/// (`command: "onebrain qmd-reindex ..."`). These must migrate to the new
/// canonical form on `apply_qmd_hook`, or be stripped on `strip_qmd_hook`.
fn is_legacy_alias_qmd_entry(entry: &Value) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    match entry.get("args").and_then(|v| v.as_array()) {
        // Exec form: command == "onebrain", first arg == "qmd-reindex".
        Some(args) => {
            cmd == "onebrain" && args.first().and_then(|v| v.as_str()) == Some("qmd-reindex")
        }
        // Shell form: full command string starts with "onebrain qmd-reindex".
        None => cmd == "onebrain qmd-reindex" || cmd == "onebrain qmd-reindex --json",
    }
}

/// True when `entry` is the v3.2–v3.4 `qmd reindex` form (exec: command
/// "onebrain", args starting ["qmd","reindex"]; or shell: command string
/// "onebrain qmd reindex[ --json]"). This WAS canonical; v3.4.5 makes it a
/// legacy form that migrates to `search reindex`.
fn is_legacy_qmd_reindex_entry(entry: &Value) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    match entry.get("args").and_then(|v| v.as_array()) {
        Some(args) => {
            cmd == "onebrain"
                && args.first().and_then(|v| v.as_str()) == Some("qmd")
                && args.get(1).and_then(|v| v.as_str()) == Some("reindex")
        }
        None => cmd == "onebrain qmd reindex" || cmd == "onebrain qmd reindex --json",
    }
}

/// True when `entry` is the v3.4.5 Track-2 canonical form (exec: command
/// "onebrain", args EXACTLY `["search","reindex","--json"]`; or shell form
/// `"onebrain search reindex --json"` / `"onebrain search reindex"`, and the
/// pre-json exec form `["search","reindex"]`). This WAS canonical before
/// Track 4 introduced `--lex-only`; it is now legacy and migrates to the new
/// canonical `HookSpec::REINDEX` (`--lex-only --json`).
///
/// Matches the EXACT arg list only (not a prefix) so this never matches the
/// Stop `EMBED` entry (`--pending-only`), which also starts with
/// `search reindex`.
fn is_legacy_track2_reindex_entry(entry: &Value) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    match entry.get("args").and_then(|v| v.as_array()) {
        Some(args) => {
            cmd == "onebrain"
                && (args.as_slice() == [json!("search"), json!("reindex"), json!("--json")]
                    || args.as_slice() == [json!("search"), json!("reindex")])
        }
        None => cmd == "onebrain search reindex --json" || cmd == "onebrain search reindex",
    }
}

/// Rewrite a legacy-alias qmd entry in place to the historical qmd helper
/// form. The production lifecycle path subsequently converges it to the
/// shared runner. Returns true if the entry was a legacy alias and rewritten.
fn rewrite_legacy_alias_to_canonical(entry: &mut Value) -> bool {
    if !is_legacy_alias_qmd_entry(entry) {
        return false;
    }
    let qmd = HookSpec::REINDEX;
    let Some(obj) = entry.as_object_mut() else {
        return false;
    };
    obj.insert("command".into(), Value::String(qmd.command.into()));
    let args: Vec<Value> = qmd
        .args
        .iter()
        .map(|s| Value::String((*s).to_string()))
        .collect();
    obj.insert("args".into(), Value::Array(args));
    obj.insert("type".into(), Value::String("command".into()));
    true
}

/// Mutate `groups` (the array at hooks.PostToolUse) to migrate, dedupe, or
/// strip qmd entries depending on `keep_canonical`. Returns true if anything
/// changed.
pub(crate) fn migrate_legacy_qmd_entries(groups: &mut Vec<Value>, keep_canonical: bool) -> bool {
    let mut touched = false;
    let qmd = HookSpec::REINDEX;

    // Pass 1: rewrite or strip legacy `qmd update ...` entries.
    for g in groups.iter_mut() {
        let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        if keep_canonical {
            let mut group_touched = false;
            for entry in hooks_arr.iter_mut() {
                let cmd = entry
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // Two legacy shapes rewrite to the new canonical form here:
                //   1. ancient `qmd update ...` (pre-CLI shell command)
                //   2. v3.0/v3.1 `onebrain qmd-reindex` hidden alias (hyphen)
                if is_legacy_qmd_cmd(&cmd) {
                    let obj = entry.as_object_mut().unwrap();
                    obj.insert("command".into(), Value::String(qmd.command.into()));
                    let args: Vec<Value> = qmd
                        .args
                        .iter()
                        .map(|s| Value::String((*s).to_string()))
                        .collect();
                    obj.insert("args".into(), Value::Array(args));
                    obj.insert("type".into(), Value::String("command".into()));
                    group_touched = true;
                } else if rewrite_legacy_alias_to_canonical(entry) {
                    group_touched = true;
                } else if is_legacy_qmd_reindex_entry(entry)
                    || is_legacy_track2_reindex_entry(entry)
                {
                    let obj = entry.as_object_mut().unwrap();
                    obj.insert("command".into(), Value::String(qmd.command.into()));
                    let args: Vec<Value> = qmd
                        .args
                        .iter()
                        .map(|s| Value::String((*s).to_string()))
                        .collect();
                    obj.insert("args".into(), Value::Array(args));
                    obj.insert("type".into(), Value::String("command".into()));
                    group_touched = true;
                }
            }
            if group_touched {
                g.as_object_mut()
                    .unwrap()
                    .insert("matcher".into(), Value::String(QMD_MATCHER.into()));
                touched = true;
            }
        } else {
            let before = hooks_arr.len();
            hooks_arr.retain(|h| {
                let cmd = h.get("command").and_then(|v| v.as_str()).unwrap_or("");
                !is_legacy_qmd_cmd(cmd)
                    && !is_legacy_alias_qmd_entry(h)
                    && !is_legacy_qmd_reindex_entry(h)
                    && !is_legacy_track2_reindex_entry(h)
                    && !is_canonical_qmd_entry(h)
                    && !is_pre_json_qmd_entry(h)
            });
            if hooks_arr.len() != before {
                touched = true;
            }
        }
    }

    if keep_canonical {
        // Pass 2: rewrite shell-form canonical entries to exec form in place.
        for g in groups.iter_mut() {
            if let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                for entry in hooks_arr.iter_mut() {
                    if rewrite_if_shell_form(entry, &qmd) {
                        touched = true;
                    }
                }
            }
        }
        // Pass 2b: v3.1 — append `--json` flag to pre-v3.1 canonical entries
        // (correct shape, but missing the JSON output flag now that text is
        // the default).
        for g in groups.iter_mut() {
            if let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                for entry in hooks_arr.iter_mut() {
                    if append_json_if_needed(entry, &qmd) {
                        touched = true;
                    }
                    if is_canonical_qmd_entry(entry)
                        && entry.get("type").and_then(Value::as_str) != Some("command")
                    {
                        entry
                            .as_object_mut()
                            .expect("canonical qmd hook is an object")
                            .insert("type".into(), Value::String("command".into()));
                        touched = true;
                    }
                }
            }
        }
        // Pass 3: dedupe canonical entries; keep first, drop rest.
        let mut seen = false;
        for g in groups.iter_mut() {
            if let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                let before = hooks_arr.len();
                hooks_arr.retain(|h| {
                    if !is_canonical_qmd_entry(h) {
                        return true;
                    }
                    if seen {
                        return false;
                    }
                    seen = true;
                    true
                });
                if hooks_arr.len() != before {
                    touched = true;
                }
            }
        }
    }

    // Pass 4: splice out empty groups (reverse iteration).
    let mut i = groups.len();
    while i > 0 {
        i -= 1;
        let empty = groups[i]
            .get("hooks")
            .and_then(|v| v.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        if empty {
            groups.remove(i);
        }
    }

    touched
}

/// Apply the qmd PostToolUse hook for vaults where `qmd_collection` is set.
#[cfg(test)]
pub(crate) fn apply_qmd_hook(settings: &mut Value) -> HookStatus {
    let root = settings.as_object_mut().expect("settings is JSON object");
    let hooks_val = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks_val.is_object() {
        *hooks_val = Value::Object(Map::new());
    }
    let entry_val = hooks_val
        .as_object_mut()
        .unwrap()
        .entry("PostToolUse".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry_val.is_array() {
        *entry_val = Value::Array(Vec::new());
    }
    let groups = entry_val.as_array_mut().unwrap();

    let migrated = migrate_legacy_qmd_entries(groups, true);

    let already = groups.iter().any(|g| {
        g.get("hooks")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(is_canonical_qmd_entry))
            .unwrap_or(false)
    });
    if already {
        return if migrated {
            HookStatus::Migrated
        } else {
            HookStatus::Ok
        };
    }
    groups.push(json!({
        "matcher": QMD_MATCHER,
        "hooks": [HookSpec::REINDEX.to_canonical_entry()],
    }));
    HookStatus::Added
}

/// Converge PostToolUse on the shared lifecycle runner while retaining the
/// legacy migration logic above for every historical qmd/reindex spelling.
pub(crate) fn apply_lifecycle_hook(settings: &mut Value) -> HookStatus {
    let root = settings.as_object_mut().expect("settings is JSON object");
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let event = hooks
        .as_object_mut()
        .unwrap()
        .entry("PostToolUse".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !event.is_array() {
        *event = Value::Array(Vec::new());
    }
    let groups = event.as_array_mut().unwrap();

    let initial_runner_count = groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|entry| is_runner_entry(entry))
        .count();
    let initially_clean = initial_runner_count == 1
        && groups.iter().any(|group| {
            group.get("matcher").and_then(Value::as_str) == Some(QMD_MATCHER)
                && group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|entries| entries.iter().any(is_canonical_runner))
        });
    let had_managed = initial_runner_count > 0
        || groups
            .iter()
            .filter_map(|group| group.get("hooks").and_then(Value::as_array))
            .flatten()
            .any(|entry| {
                is_canonical_qmd_entry(entry)
                    || is_pre_json_qmd_entry(entry)
                    || is_legacy_alias_qmd_entry(entry)
                    || is_legacy_qmd_reindex_entry(entry)
                    || is_legacy_track2_reindex_entry(entry)
                    || entry
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_legacy_qmd_cmd)
            });

    let mut changed = migrate_legacy_qmd_entries(groups, true);
    let mut seen = false;
    for group in groups.iter_mut() {
        let mut kept_runner = false;
        if let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            entries.retain_mut(|entry| {
                if !is_runner_entry(entry) && !is_canonical_qmd_entry(entry) {
                    return true;
                }
                if seen {
                    changed = true;
                    return false;
                }
                seen = true;
                kept_runner = true;
                if !is_canonical_runner(entry) {
                    let object = entry.as_object_mut().expect("managed hook is an object");
                    object.insert("command".to_string(), Value::String("onebrain".to_string()));
                    object.insert("args".to_string(), json!(["hook"]));
                    object.insert("type".to_string(), Value::String("command".to_string()));
                    changed = true;
                }
                true
            });
        }
        if kept_runner && group.get("matcher").and_then(Value::as_str) != Some(QMD_MATCHER) {
            group
                .as_object_mut()
                .expect("hook group is an object")
                .insert(
                    "matcher".to_string(),
                    Value::String(QMD_MATCHER.to_string()),
                );
            changed = true;
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    });
    if !seen {
        groups.push(json!({
            "matcher": QMD_MATCHER,
            "hooks": [HookSpec::RUNNER.to_canonical_entry()],
        }));
    }

    if !had_managed {
        HookStatus::Added
    } else if initially_clean && !changed {
        HookStatus::Ok
    } else {
        HookStatus::Migrated
    }
}

/// Remove every OneBrain-managed PostToolUse lifecycle entry when search is
/// not configured, including the generic runner from a previous setup.
pub(crate) fn strip_lifecycle_hook(settings: &mut Value) -> bool {
    let mut changed = strip_qmd_hook(settings);
    let Some(groups) = settings
        .pointer_mut("/hooks/PostToolUse")
        .and_then(Value::as_array_mut)
    else {
        return changed;
    };
    for group in groups.iter_mut() {
        if let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = entries.len();
            entries.retain(|entry| !is_runner_entry(entry));
            changed |= entries.len() != before;
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    });
    if groups.is_empty() {
        if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
            hooks.remove("PostToolUse");
        }
    }
    changed
}

fn is_embed_entry(entry: &Value) -> bool {
    matches_spec(entry, &HookSpec::EMBED)
}

/// Apply the Stop `search reindex --pending-only --json` embed hook for
/// vaults where `qmd_collection` is set — same condition as
/// `apply_qmd_hook`. Adds it as a SEPARATE group under the Stop event.
///
/// CRITICAL: only ever inspects/mutates entries matching `HookSpec::EMBED`.
/// It never touches, dedupes, or reorders the `checkpoint stop` (`STOP`)
/// entry registered by `hooks::apply_hooks` — those live in a different
/// group (or, if the caller merged them, are skipped by the `is_embed_entry`
/// filter below) and are left byte-for-byte untouched.
#[cfg(test)]
pub(crate) fn apply_embed_hook(settings: &mut Value) -> HookStatus {
    let root = settings.as_object_mut().expect("settings is JSON object");
    let hooks_val = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks_val.is_object() {
        *hooks_val = Value::Object(Map::new());
    }
    let entry_val = hooks_val
        .as_object_mut()
        .unwrap()
        .entry("Stop".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry_val.is_array() {
        *entry_val = Value::Array(Vec::new());
    }
    let groups = entry_val.as_array_mut().unwrap();

    // Pass 1: rewrite shell-form / pre-json embed entries in place (parity
    // with the qmd hook's own migration passes). These helpers key off
    // `matches_spec`/`matches_spec_pre_json` against `HookSpec::EMBED`
    // specifically, so they cannot touch a `STOP` (checkpoint) entry.
    let mut migrated = false;
    for g in groups.iter_mut() {
        if let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
            for entry in hooks_arr.iter_mut() {
                if rewrite_if_shell_form(entry, &HookSpec::EMBED) {
                    migrated = true;
                }
                if append_json_if_needed(entry, &HookSpec::EMBED) {
                    migrated = true;
                }
            }
        }
    }

    // Pass 2: dedupe — keep first embed entry, drop the rest. Non-embed
    // entries (including checkpoint stop) are never inspected by
    // `is_embed_entry` and pass through `retain` untouched.
    let mut seen = false;
    for g in groups.iter_mut() {
        if let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
            let before = hooks_arr.len();
            hooks_arr.retain(|h| {
                if !is_embed_entry(h) {
                    return true;
                }
                if seen {
                    return false;
                }
                seen = true;
                true
            });
            if hooks_arr.len() != before {
                migrated = true;
            }
        }
    }
    // Splice out groups left empty by the dedupe pass.
    groups.retain(|g| {
        g.get("hooks")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    });

    let already = groups.iter().any(|g| {
        g.get("hooks")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(is_embed_entry))
            .unwrap_or(false)
    });
    if already {
        return if migrated {
            HookStatus::Migrated
        } else {
            HookStatus::Ok
        };
    }

    // Not present — add as a new, separate group. Never appended into the
    // checkpoint's existing group, so the checkpoint entry's group/array is
    // untouched.
    groups.push(json!({
        "matcher": "",
        "hooks": [HookSpec::EMBED.to_canonical_entry()],
    }));
    HookStatus::Added
}

/// `qmd_collection` is absent — strip the Stop embed entry (and any legacy
/// shell/pre-json shape of it). Never touches the checkpoint `STOP` entry.
/// Returns true if anything was removed.
pub(crate) fn strip_embed_hook(settings: &mut Value) -> bool {
    let Some(root) = settings.as_object_mut() else {
        return false;
    };
    let Some(hooks_val) = root.get_mut("hooks") else {
        return false;
    };
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return false;
    };
    let Some(groups) = hooks_obj.get_mut("Stop").and_then(|v| v.as_array_mut()) else {
        return false;
    };

    let mut changed = false;
    for g in groups.iter_mut() {
        if let Some(hooks_arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
            let before = hooks_arr.len();
            hooks_arr.retain(|h| !is_embed_entry(h) && !matches_spec_pre_json(h, &HookSpec::EMBED));
            if hooks_arr.len() != before {
                changed = true;
            }
        }
    }
    groups.retain(|g| {
        g.get("hooks")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    });
    if groups.is_empty() {
        hooks_obj.remove("Stop");
    }
    changed
}

/// `qmd_collection` is absent — strip every legacy + canonical qmd entry. If
/// PostToolUse ends up empty, delete the key. Returns true if any entry was
/// removed.
pub(crate) fn strip_qmd_hook(settings: &mut Value) -> bool {
    let Some(root) = settings.as_object_mut() else {
        return false;
    };
    let Some(hooks_val) = root.get_mut("hooks") else {
        return false;
    };
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return false;
    };
    let Some(groups) = hooks_obj
        .get_mut("PostToolUse")
        .and_then(|v| v.as_array_mut())
    else {
        return false;
    };
    let changed = migrate_legacy_qmd_entries(groups, false);
    let empty = groups.is_empty();
    if empty {
        hooks_obj.remove("PostToolUse");
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_qmd_cmd_matches_plain() {
        assert!(is_legacy_qmd_cmd("qmd update -c x"));
        assert!(is_legacy_qmd_cmd("qmd update"));
        assert!(is_legacy_qmd_cmd("qmd  update -c x")); // multi-space
    }

    #[test]
    fn legacy_qmd_cmd_matches_powershell_wrap() {
        assert!(is_legacy_qmd_cmd(
            "powershell.exe -NoProfile -Command qmd update -c 'foo'"
        ));
        assert!(is_legacy_qmd_cmd("bash -lc 'qmd update -c x'"));
        assert!(is_legacy_qmd_cmd("cmd.exe /c qmd update -c x"));
    }

    #[test]
    fn legacy_qmd_cmd_no_match_for_unrelated() {
        assert!(!is_legacy_qmd_cmd("my-qmd-update.sh"));
        assert!(!is_legacy_qmd_cmd("qmd status"));
        assert!(!is_legacy_qmd_cmd("update qmd"));
        assert!(!is_legacy_qmd_cmd("xqmd update"));
        assert!(!is_legacy_qmd_cmd("qmdupdate"));
        assert!(!is_legacy_qmd_cmd("qmd updated"));
        assert!(!is_legacy_qmd_cmd(""));
    }

    #[test]
    fn apply_qmd_hook_fresh_adds_canonical() {
        let mut s = json!({});
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Added);
        assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(entry["command"], "onebrain");
        // The legacy qmd helper retains its historical reindex form; the
        // production lifecycle path below now emits the shared runner.
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
        assert_eq!(entry["type"], "command");
    }

    #[test]
    fn apply_qmd_hook_legacy_migrates_directly_to_exec_form() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "qmd update -c ob-1-test"}],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// v3.1 legacy ALIAS exec form (`qmd-reindex` hyphen) migrates to the
    /// new canonical (`qmd reindex` space) in place.
    #[test]
    fn apply_qmd_hook_legacy_alias_exec_migrates_to_new_form() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain",
                        "args": ["qmd-reindex", "--json"]
                    }],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// v3.0 legacy ALIAS exec form (`qmd-reindex`, no `--json`) migrates to
    /// the new canonical form.
    #[test]
    fn apply_qmd_hook_legacy_alias_v30_exec_migrates_to_new_form() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["qmd-reindex"]
                    }],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// Legacy alias shell form (`onebrain qmd-reindex`) migrates to new exec.
    #[test]
    fn apply_qmd_hook_legacy_alias_shell_migrates_to_new_form() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// Mixed legacy alias + new-form duplicate collapse to ONE canonical
    /// new-form entry.
    #[test]
    fn apply_qmd_hook_legacy_alias_plus_new_dedupes_to_single() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write|Edit", "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["qmd-reindex", "--json"]
                    }]},
                    {"matcher": "Write|Edit", "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["qmd", "reindex", "--json"]
                    }]},
                ]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// Two Track-2 (`search reindex --json`, pre-Track-4) duplicates both
    /// migrate to the new `--lex-only` canonical and collapse to ONE.
    #[test]
    fn apply_qmd_hook_two_track2_legacy_dedupes_to_single() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write|Edit", "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["search", "reindex", "--json"]
                    }]},
                    {"matcher": "Write|Edit", "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["search", "reindex", "--json"]
                    }]},
                ]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// Two NEW (`--lex-only`) canonical duplicates collapse to ONE.
    #[test]
    fn apply_qmd_hook_two_new_form_dedupes_to_single() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write|Edit", "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["search", "reindex", "--lex-only", "--json"]
                    }]},
                    {"matcher": "Write|Edit", "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["search", "reindex", "--lex-only", "--json"]
                    }]},
                ]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    #[test]
    fn apply_qmd_hook_legacy_idempotent_on_repeated_runs() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "qmd update -c x"}],
                }]
            }
        });
        apply_qmd_hook(&mut s);
        let st2 = apply_qmd_hook(&mut s);
        assert_eq!(st2, HookStatus::Ok);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    /// Track-2 qmd helper form (`search reindex --json`, no `--lex-only`) is
    /// now legacy — the helper migrates it to the latest historical qmd form.
    #[test]
    fn apply_qmd_hook_track2_canonical_migrates_to_lex_only() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain",
                        "args": ["search", "reindex", "--json"]
                    }],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// The TRUE new canonical (`--lex-only --json`) already present is a
    /// genuine no-op.
    #[test]
    fn apply_qmd_hook_canonical_already_present_no_op() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain",
                        "args": ["search", "reindex", "--lex-only", "--json"]
                    }],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Ok);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn apply_lifecycle_hook_repairs_missing_runner_type() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{"matcher": "Write|Edit", "hooks": [
                    {"command": "onebrain", "args": ["hook"], "note": "keep"}
                ]}]
            }
        });

        let status = apply_lifecycle_hook(&mut s);

        assert_eq!(status, HookStatus::Migrated);
        assert_eq!(s["hooks"]["PostToolUse"][0]["hooks"][0]["type"], "command");
        assert_eq!(s["hooks"]["PostToolUse"][0]["hooks"][0]["note"], "keep");
    }

    #[test]
    fn apply_lifecycle_hook_repairs_wrong_runner_type() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{"matcher": "Write|Edit", "hooks": [
                    {"type": "shell", "command": "onebrain", "args": ["hook"]}
                ]}]
            }
        });

        let status = apply_lifecycle_hook(&mut s);

        assert_eq!(status, HookStatus::Migrated);
        assert_eq!(s["hooks"]["PostToolUse"][0]["hooks"][0]["type"], "command");
    }

    #[test]
    fn apply_qmd_hook_v31_canonical_args_migrate_with_json_flag() {
        // Pre-json Track-2 canonical (no --json, no --lex-only) is a legacy
        // shape (`is_legacy_track2_reindex_entry`) that migrates straight to
        // the new `--lex-only --json` canonical via Pass 1.
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain",
                        "args": ["search", "reindex"]
                    }],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        // Migrated because the args[] changed.
        assert_eq!(st, HookStatus::Migrated);
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// v3.2–v3.4 `qmd reindex` exec form (no `--json`) migrates directly to
    /// `search reindex --json` via Pass 1, not the Pass 2b flag-append.
    #[test]
    fn apply_qmd_hook_legacy_qmd_reindex_no_json_migrates_to_search() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain",
                        "args": ["qmd", "reindex"]
                    }],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    #[test]
    fn apply_qmd_hook_shell_form_canonical_migrates_to_exec() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "onebrain qmd reindex"}],
                }]
            }
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(entry["command"], "onebrain");
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    #[test]
    fn apply_qmd_hook_powershell_wrapped_legacy_migrates() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command",
                        "command": "powershell.exe -NoProfile -Command qmd update -c 'ob-1-test'",
                    }],
                }]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    #[test]
    fn apply_qmd_hook_legacy_plus_canonical_dedupes() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "qmd update -c x"}]},
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain qmd reindex"}]},
                ]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        let want_args = vec![
            Value::String("search".into()),
            Value::String("reindex".into()),
            Value::String("--lex-only".into()),
            Value::String("--json".into()),
        ];
        let canonical_count = entries
            .iter()
            .filter(|e| {
                e.get("command").and_then(|v| v.as_str()) == Some("onebrain")
                    && e.get("args").and_then(|v| v.as_array()) == Some(&want_args)
            })
            .count();
        assert_eq!(canonical_count, 1);
    }

    #[test]
    fn apply_qmd_hook_two_canonical_dedupes() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain qmd reindex"}]},
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain qmd reindex"}]},
                ]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn apply_qmd_hook_narrow_matcher_normalized_to_write_edit() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write",
                    "hooks": [{"type": "command", "command": "qmd update -c x"}],
                }]
            }
        });
        apply_qmd_hook(&mut s);
        assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
    }

    #[test]
    fn apply_qmd_hook_leaves_unrelated_post_tool_use_intact() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [
                        {"type": "command", "command": "qmd update -c x"},
                        {"type": "command", "command": "echo user-custom-hook"},
                    ]
                }]
            }
        });
        apply_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert!(entries
            .iter()
            .any(|e| e["command"] == "echo user-custom-hook"));
        assert!(entries.iter().any(|e| e["command"] == "onebrain"
            && e["args"] == json!(["search", "reindex", "--lex-only", "--json"])));
    }

    #[test]
    fn strip_qmd_hook_legacy_only_deletes_event_key() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "qmd update -c x"}],
                }]
            }
        });
        strip_qmd_hook(&mut s);
        assert!(s["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn strip_qmd_hook_canonical_only_deletes_event_key() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "onebrain qmd reindex"}],
                }]
            }
        });
        strip_qmd_hook(&mut s);
        assert!(s["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn strip_qmd_hook_legacy_alias_exec_deletes_event_key() {
        // Old hyphen-alias exec form must also be stripped when qmd disabled.
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{
                        "type": "command", "command": "onebrain", "args": ["qmd-reindex", "--json"]
                    }],
                }]
            }
        });
        strip_qmd_hook(&mut s);
        assert!(s["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn strip_qmd_hook_preserves_user_hook() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [
                        {"type": "command", "command": "qmd update -c x"},
                        {"type": "command", "command": "echo user"},
                    ]
                }]
            }
        });
        strip_qmd_hook(&mut s);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "echo user");
    }

    #[test]
    fn strip_qmd_hook_mixed_legacy_and_canonical_both_dropped() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [
                        {"type": "command", "command": "qmd update -c x"},
                        {"type": "command", "command": "onebrain qmd-reindex"},
                        {"type": "command", "command": "onebrain qmd reindex"},
                    ]
                }]
            }
        });
        strip_qmd_hook(&mut s);
        assert!(s["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn strip_qmd_hook_returns_false_when_no_post_tool_use() {
        let mut s = json!({"hooks": {"Stop": []}});
        assert!(!strip_qmd_hook(&mut s));
    }

    /// `strip_qmd_hook` on a non-object JSON value (e.g. an array) must return
    /// false immediately. Covers the first `settings.as_object_mut() else { return false }`.
    #[test]
    fn strip_qmd_hook_returns_false_for_non_object_settings() {
        let mut s = json!([]);
        assert!(!strip_qmd_hook(&mut s));
    }

    /// `strip_qmd_hook` when `"hooks"` is present but is not a JSON object
    /// (e.g. a string) must return false. Covers the
    /// `hooks_val.as_object_mut() else { return false }` branch.
    #[test]
    fn strip_qmd_hook_returns_false_when_hooks_value_is_not_object() {
        let mut s = json!({"hooks": "not-an-object"});
        assert!(!strip_qmd_hook(&mut s));
    }

    /// When `"hooks"` key exists but holds a non-object value, `apply_qmd_hook`
    /// must reset it to an empty object and then add the canonical entry.
    /// Covers the `if !hooks_val.is_object() { *hooks_val = … }` branch.
    #[test]
    fn apply_qmd_hook_resets_non_object_hooks_value() {
        let mut s = json!({"hooks": "invalid"});
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Added);
        assert!(s["hooks"]["PostToolUse"].is_array());
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// When `"PostToolUse"` exists but holds a non-array value, `apply_qmd_hook`
    /// must reset it to an empty array and add the canonical entry.
    /// Covers the `if !entry_val.is_array() { *entry_val = … }` branch.
    #[test]
    fn apply_qmd_hook_resets_non_array_post_tool_use() {
        let mut s = json!({"hooks": {"PostToolUse": "not-an-array"}});
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Added);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// A group that has no `"hooks"` key is silently skipped in
    /// `migrate_legacy_qmd_entries` (both keep and strip paths).
    /// Covers the `else { continue; }` inside the group iteration.
    #[test]
    fn migrate_groups_keep_canonical_skips_group_without_hooks_key() {
        let mut groups = vec![
            json!({"matcher": "Write|Edit"}), // no "hooks" key → skipped in Pass 1
            json!({"matcher": "Write|Edit", "hooks": [
                {"type": "command", "command": "qmd update -c x"}
            ]}),
        ];
        let changed = migrate_legacy_qmd_entries(&mut groups, true);
        assert!(changed);
        // The hookless group is spliced out in Pass 4; the migrated group remains.
        let all_entries: Vec<_> = groups
            .iter()
            .filter_map(|g| g.get("hooks").and_then(|v| v.as_array()))
            .flat_map(|arr| arr.iter())
            .collect();
        assert_eq!(all_entries.len(), 1);
        assert_eq!(all_entries[0]["command"], "onebrain");
        assert_eq!(
            all_entries[0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    #[test]
    fn migrate_groups_strip_skips_group_without_hooks_key() {
        let mut groups = vec![
            json!({"matcher": "Write|Edit"}), // no "hooks" key → continue in strip pass
            json!({"matcher": "Write|Edit", "hooks": [
                {"type": "command", "command": "qmd update -c x"}
            ]}),
        ];
        let changed = migrate_legacy_qmd_entries(&mut groups, false);
        assert!(changed);
        // Both groups end up empty (hookless group removed by Pass 4; the other's
        // sole entry was stripped → empty → also removed).
        assert!(groups.is_empty(), "groups after strip: {groups:?}");
    }

    /// An exec-form `onebrain` hook in PostToolUse whose first arg is NOT
    /// `"qmd-reindex"` must be preserved by `strip_qmd_hook`.
    /// Exercises the `is_legacy_alias_qmd_entry` `Some(args)` branch returning
    /// false when `cmd == "onebrain"` but `args[0] != "qmd-reindex"`.
    #[test]
    fn strip_qmd_hook_does_not_remove_non_qmd_onebrain_entry() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [
                        {"type": "command", "command": "onebrain", "args": ["checkpoint", "stop"]},
                        {"type": "command", "command": "qmd update -c x"},
                    ]
                }]
            }
        });
        let changed = strip_qmd_hook(&mut s);
        assert!(changed);
        let entries: Vec<_> = s["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        // legacy qmd entry removed; non-qmd onebrain entry preserved
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(entries[0]["args"], json!(["checkpoint", "stop"]));
    }

    /// v3.2–v3.4 `qmd reindex` exec form migrates to v3.4.5 `search reindex`.
    #[test]
    fn apply_qmd_hook_legacy_qmd_reindex_exec_migrates_to_search() {
        let mut s = json!({
            "hooks": { "PostToolUse": [{
                "matcher": "Write|Edit",
                "hooks": [{"type":"command","command":"onebrain","args":["qmd","reindex","--json"]}],
            }]}
        });
        let st = apply_qmd_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let e = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(
            e["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    /// Disabled qmd strips the old `qmd reindex` form too.
    #[test]
    fn strip_qmd_hook_removes_legacy_qmd_reindex_exec() {
        let mut s = json!({
            "hooks": { "PostToolUse": [{
                "matcher": "Write|Edit",
                "hooks": [{"type":"command","command":"onebrain","args":["qmd","reindex","--json"]}],
            }]}
        });
        strip_qmd_hook(&mut s);
        assert!(s["hooks"].get("PostToolUse").is_none());
    }

    // ── v3.4.5 Track 4: apply_embed_hook / strip_embed_hook ──────────────────

    #[test]
    fn apply_embed_hook_fresh_adds_pending_only_entry() {
        let mut s = json!({});
        let st = apply_embed_hook(&mut s);
        assert_eq!(st, HookStatus::Added);
        let entries: Vec<_> = s["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(
            entries[0]["args"],
            json!(["search", "reindex", "--pending-only", "--json"])
        );
    }

    #[test]
    fn apply_embed_hook_never_touches_existing_checkpoint_entry() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"command": "onebrain", "args": ["checkpoint", "stop", "--json"]}
                ]}],
            }
        });
        apply_embed_hook(&mut s);
        let stop = s["hooks"]["Stop"].as_array().unwrap();
        // Checkpoint's original group is untouched; embed lands in a new group.
        assert_eq!(
            stop[0]["hooks"][0],
            json!({"command": "onebrain", "args": ["checkpoint", "stop", "--json"]})
        );
        let entries: Vec<_> = stop
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 2, "entries: {entries:?}");
        assert!(entries
            .iter()
            .any(|e| e["args"] == json!(["search", "reindex", "--pending-only", "--json"])));
    }

    #[test]
    fn apply_embed_hook_idempotent_second_run_is_ok() {
        let mut s = json!({});
        apply_embed_hook(&mut s);
        let st = apply_embed_hook(&mut s);
        assert_eq!(st, HookStatus::Ok);
        let entries: Vec<_> = s["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn apply_embed_hook_dedupes_existing_duplicates() {
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {"matcher": "", "hooks": [
                        {"command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]}
                    ]},
                    {"matcher": "", "hooks": [
                        {"command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]}
                    ]},
                ],
            }
        });
        let st = apply_embed_hook(&mut s);
        assert_eq!(st, HookStatus::Migrated);
        let entries: Vec<_> = s["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn apply_embed_hook_does_not_match_qmd_lex_only_entry() {
        // Sanity: the PostToolUse `--lex-only` entry living under Stop
        // (shouldn't happen, but prove the matcher is exact-args) must not
        // be treated as "already present" for the embed hook.
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"command": "onebrain", "args": ["search", "reindex", "--lex-only", "--json"]}
                ]}],
            }
        });
        let st = apply_embed_hook(&mut s);
        assert_eq!(st, HookStatus::Added);
        let entries: Vec<_> = s["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 2, "entries: {entries:?}");
    }

    #[test]
    fn strip_embed_hook_removes_entry_keeps_checkpoint() {
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {"matcher": "", "hooks": [
                        {"command": "onebrain", "args": ["checkpoint", "stop", "--json"]}
                    ]},
                    {"matcher": "", "hooks": [
                        {"command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]}
                    ]},
                ],
            }
        });
        let changed = strip_embed_hook(&mut s);
        assert!(changed);
        let entries: Vec<_> = s["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["args"], json!(["checkpoint", "stop", "--json"]));
    }

    #[test]
    fn strip_embed_hook_removes_stop_key_when_only_embed_present() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]}
                ]}],
            }
        });
        strip_embed_hook(&mut s);
        assert!(s["hooks"].get("Stop").is_none());
    }

    #[test]
    fn strip_embed_hook_returns_false_when_no_stop_event() {
        let mut s = json!({"hooks": {"PostToolUse": []}});
        assert!(!strip_embed_hook(&mut s));
    }

    #[test]
    fn strip_embed_hook_returns_false_for_non_object_settings() {
        let mut s = json!([]);
        assert!(!strip_embed_hook(&mut s));
    }

    #[test]
    fn strip_embed_hook_returns_false_when_hooks_value_is_not_object() {
        let mut s = json!({"hooks": "not-an-object"});
        assert!(!strip_embed_hook(&mut s));
    }

    #[test]
    fn strip_embed_hook_returns_false_when_no_hooks_key() {
        let mut s = json!({"theme": "dark"});
        assert!(!strip_embed_hook(&mut s));
    }
}
