//! qmd PostToolUse hook registration + legacy `qmd update ...` migration + dedupe.
//!
//! Mirrors Bun `register-hooks.ts::applyQmdHook` and `migrateLegacyQmdEntries`.

use super::hooks::{matches_spec, rewrite_if_shell_form, HookSpec, HookStatus};
use serde_json::{json, Map, Value};

const QMD_MATCHER: &str = "Write|Edit";

fn is_canonical_qmd_entry(entry: &Value) -> bool {
    matches_spec(entry, &HookSpec::QMD)
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

/// Mutate `groups` (the array at hooks.PostToolUse) to migrate, dedupe, or
/// strip qmd entries depending on `keep_canonical`. Returns true if anything
/// changed.
pub(crate) fn migrate_legacy_qmd_entries(groups: &mut Vec<Value>, keep_canonical: bool) -> bool {
    let mut touched = false;
    let qmd = HookSpec::QMD;

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
                if is_legacy_qmd_cmd(&cmd) {
                    let obj = entry.as_object_mut().unwrap();
                    obj.insert("command".into(), Value::String(qmd.command.into()));
                    let args: Vec<Value> = qmd
                        .args
                        .iter()
                        .map(|s| Value::String((*s).to_string()))
                        .collect();
                    obj.insert("args".into(), Value::Array(args));
                    obj.entry("type".to_string())
                        .or_insert_with(|| Value::String("command".into()));
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
                !is_legacy_qmd_cmd(cmd) && !is_canonical_qmd_entry(h)
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
        "hooks": [HookSpec::QMD.to_canonical_entry()],
    }));
    HookStatus::Added
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
        assert_eq!(entry["args"], json!(["qmd-reindex"]));
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
        assert_eq!(entries[0]["args"], json!(["qmd-reindex"]));
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

    #[test]
    fn apply_qmd_hook_canonical_already_present_no_op() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "onebrain", "args": ["qmd-reindex"]}],
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
    fn apply_qmd_hook_shell_form_canonical_migrates_to_exec() {
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
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(entry["command"], "onebrain");
        assert_eq!(entry["args"], json!(["qmd-reindex"]));
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
        assert_eq!(entries[0]["args"], json!(["qmd-reindex"]));
    }

    #[test]
    fn apply_qmd_hook_legacy_plus_canonical_dedupes() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "qmd update -c x"}]},
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}]},
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
        let want_args = vec![Value::String("qmd-reindex".into())];
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
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}]},
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}]},
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
        assert!(entries
            .iter()
            .any(|e| e["command"] == "onebrain" && e["args"] == json!(["qmd-reindex"])));
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
                    "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}],
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
}
