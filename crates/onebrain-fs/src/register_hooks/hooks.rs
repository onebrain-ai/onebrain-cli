//! Hook-entry primitives + Stop event registration.
//!
//! Mirrors Bun `register-hooks.ts::matchesSpec`, `rewriteIfShellForm`,
//! `checkHookPresence`, `applyHooks`.

use serde_json::{json, Map, Value};

/// Canonical hook spec (command + args[]).
#[derive(Debug, Clone)]
pub(crate) struct HookSpec {
    pub command: &'static str,
    pub args: &'static [&'static str],
}

impl HookSpec {
    /// Shared lifecycle runner. The harness event is read from hook stdin.
    pub(crate) const RUNNER: HookSpec = HookSpec {
        command: "onebrain",
        args: &["hook"],
    };

    // v3.1: hook-protocol commands default to text output for interactive
    // use; machine consumers (Claude Code Stop / PostToolUse hooks) need
    // `--json` to keep getting the structured envelope they parse. Fresh
    // installs scaffold this directly; existing installs are auto-migrated
    // by `crate::v31::hook_rewriter`.
    pub(crate) const STOP: HookSpec = HookSpec {
        command: "onebrain",
        args: &["checkpoint", "stop", "--json"],
    };

    // Historical qmd registration helper. The production PostToolUse
    // lifecycle path now uses the shared runner; legacy qmd/reindex forms
    // are still recognized and migrated by `migrate_legacy_qmd_entries`.
    //
    // v3.4.5 Track 4: `--lex-only` scopes the PostToolUse hook to a
    // lexical-only reindex (fast, no embedding) since it runs synchronously
    // after every Write/Edit. Full embedding is deferred to the Stop hook
    // (see `EMBED` below).
    //
    // v3.4.5 Track 3b: renamed from `QMD` — the hook has run native `search
    // reindex` since Track 2, so the old `qmd` name was a misnomer. (The
    // legacy-entry detection/migration helpers keep the `qmd` name because
    // they match real `qmd …` command strings in users' settings.json.)
    pub(crate) const REINDEX: HookSpec = HookSpec {
        command: "onebrain",
        args: &["search", "reindex", "--lex-only", "--json"],
    };

    // v3.4.5 Track 4: Stop-event companion to `QMD` above. Runs a
    // pending-only embed pass at session end; the CLI detaches itself in
    // structured (--json) mode so this is non-blocking. Registered as a
    // SEPARATE Stop entry alongside `STOP` (the checkpoint hook) — it must
    // never replace, merge with, or dedupe against the checkpoint entry.
    pub(crate) const EMBED: HookSpec = HookSpec {
        command: "onebrain",
        args: &["search", "reindex", "--pending-only", "--json"],
    };

    /// Shell-form representation: e.g. `"onebrain checkpoint stop"`.
    pub(crate) fn full_cmd(&self) -> String {
        let mut s = String::with_capacity(self.command.len() + 16);
        s.push_str(self.command);
        for a in self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }

    /// Canonical JSON entry object: `{ type, command, args }`.
    pub(crate) fn to_canonical_entry(&self) -> Value {
        let args: Vec<Value> = self
            .args
            .iter()
            .map(|s| Value::String((*s).to_string()))
            .collect();
        json!({
            "type": "command",
            "command": self.command,
            "args": args,
        })
    }
}

/// `entry` matches `spec` in canonical or legacy shell form.
///
/// v3.1 contract: matches the canonical args[] (with trailing `--json`),
/// the v3.0 canonical (same args[] minus `--json`), and either of the
/// shell-form spellings ("onebrain checkpoint stop" or
/// "onebrain checkpoint stop --json"). Used everywhere we ask "is this
/// already the OneBrain Stop / qmd hook?". The in-place migration helper
/// `append_json_if_needed` upgrades the v3.0 canonical to the v3.1 shape.
pub(crate) fn matches_spec(entry: &Value, spec: &HookSpec) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let args = entry.get("args").and_then(|v| v.as_array());

    if cmd == spec.command {
        if let Some(args) = args {
            let got: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
            // v3.1 canonical OR v3.0 canonical (== spec.args minus trailing
            // --json). Strict equality only.
            if got == spec.args {
                return true;
            }
            if let Some(head) = spec_args_without_json(spec) {
                if got == head {
                    return true;
                }
            }
        }
    }
    // Shell-form: "onebrain checkpoint stop --json" or the v3.0 form
    // "onebrain checkpoint stop". Both count as a match; the migration
    // pass in `apply_hooks` converts shell→exec AND ensures --json.
    if args.is_none() && (cmd == spec.full_cmd() || matches_pre_json_full_cmd(cmd, spec)) {
        return true;
    }
    false
}

/// Helper: `spec.args` minus the trailing `--json` flag, when present.
fn spec_args_without_json(spec: &HookSpec) -> Option<&[&'static str]> {
    if spec.args.last().copied() == Some("--json") {
        Some(&spec.args[..spec.args.len() - 1])
    } else {
        None
    }
}

/// Helper: does `cmd` look like the v3.0 shell form (spec without --json)?
fn matches_pre_json_full_cmd(cmd: &str, spec: &HookSpec) -> bool {
    let Some(head) = spec_args_without_json(spec) else {
        return false;
    };
    let mut s = String::with_capacity(spec.command.len() + 16);
    s.push_str(spec.command);
    for a in head {
        s.push(' ');
        s.push_str(a);
    }
    cmd == s
}

/// `entry` matches the v3.0 / pre-v3.1 version of `spec` — same positional
/// args but missing the trailing `--json` flag. Returned true entries are
/// candidates for in-place flag-append (handled by `apply_hooks`).
pub(crate) fn matches_spec_pre_json(entry: &Value, spec: &HookSpec) -> bool {
    let Some(head) = spec_args_without_json(spec) else {
        return false;
    };
    let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if cmd != spec.command {
        return false;
    }
    let Some(args) = entry.get("args").and_then(|v| v.as_array()) else {
        return false;
    };
    let got: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
    got == head
}

/// Append `--json` to `entry.args` if `spec.args` ends with `--json` AND
/// the entry currently doesn't have it. Returns true on append.
pub(crate) fn append_json_if_needed(entry: &mut Value, spec: &HookSpec) -> bool {
    if !matches_spec_pre_json(entry, spec) {
        return false;
    }
    let Some(args) = entry.get_mut("args").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    args.push(Value::String("--json".to_string()));
    true
}

/// If `entry` is the legacy shell form of `spec`, rewrite it to canonical.
/// Returns true on rewrite.
///
/// v3.1 contract: accepts either the v3.1 shell form
/// (`"onebrain checkpoint stop --json"`) OR the v3.0 shell form
/// (`"onebrain checkpoint stop"`). Either way the rewritten entry lands on
/// the v3.1 canonical args[] (with `--json`) so machine consumers keep
/// getting structured output now that text is the new default.
pub(crate) fn rewrite_if_shell_form(entry: &mut Value, spec: &HookSpec) -> bool {
    let cmd = entry
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let has_args = entry.get("args").and_then(|v| v.as_array()).is_some();
    if has_args {
        return false;
    }
    if cmd != spec.full_cmd() && !matches_pre_json_full_cmd(&cmd, spec) {
        return false;
    }
    let Some(obj) = entry.as_object_mut() else {
        return false;
    };
    obj.insert("command".into(), Value::String(spec.command.into()));
    let args: Vec<Value> = spec
        .args
        .iter()
        .map(|s| Value::String((*s).to_string()))
        .collect();
    obj.insert("args".into(), Value::Array(args));
    obj.insert("type".into(), Value::String("command".into()));
    true
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Presence {
    Found,
    Migrate,
    Missing,
}

/// Scan groups for any entry matching `spec` (Found), failing that any
/// `checkpoint-hook.sh` reference (Migrate), failing that Missing.
pub(crate) fn check_hook_presence(groups: &[Value], spec: &HookSpec) -> Presence {
    let mut saw_migrate = false;
    for g in groups {
        let Some(hooks) = g.get("hooks").and_then(|v| v.as_array()) else {
            continue;
        };
        for entry in hooks {
            if matches_spec(entry, spec) {
                return Presence::Found;
            }
            let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.contains("checkpoint-hook.sh") {
                saw_migrate = true;
            }
        }
    }
    if saw_migrate {
        Presence::Migrate
    } else {
        Presence::Missing
    }
}

const HOOK_EVENTS: &[(&str, HookSpec)] = &[("Stop", HookSpec::STOP)];
const ALLOWED_HOOK_EVENTS: &[&str] = &["Stop", "PostToolUse"];

/// True when the hook actually executes OneBrain, or invokes one of its
/// retired wrapper scripts. A foreign shell command that merely mentions the
/// word (for example `echo onebrain checkpoint stop`) is not managed.
pub(crate) fn is_managed_hook_entry(entry: &Value) -> bool {
    let Some(command) = entry.get("command").and_then(Value::as_str) else {
        return false;
    };
    let executable_is_onebrain = if command == "onebrain" {
        true
    } else {
        command.split_ascii_whitespace().next() == Some("onebrain")
    };
    executable_is_onebrain
        || command.contains("checkpoint-hook.sh")
        || command.contains("session-init.sh")
}

/// Outcome per hook event after `apply_hooks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStatus {
    Added,
    Migrated,
    Ok,
}

impl HookStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookStatus::Added => "added",
            HookStatus::Migrated => "migrated",
            HookStatus::Ok => "ok",
        }
    }
}

/// Register the Stop hook and remove stale onebrain entries from
/// non-allowed events.
pub(crate) fn apply_hooks(settings: &mut Value) -> Vec<(&'static str, HookStatus)> {
    let root = settings.as_object_mut().expect("settings is a JSON object");
    let hooks_val = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks_val.is_object() {
        *hooks_val = Value::Object(Map::new());
    }

    // 1) Stale-event cleanup.
    let event_names: Vec<String> = hooks_val
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    for event in event_names {
        if ALLOWED_HOOK_EVENTS.iter().any(|e| *e == event) {
            continue;
        }
        let drop_event = {
            let hooks_obj = hooks_val.as_object_mut().unwrap();
            let Some(groups) = hooks_obj.get_mut(&event).and_then(|v| v.as_array_mut()) else {
                continue;
            };
            for g in groups.iter_mut() {
                if let Some(h) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                    h.retain(|entry| !is_managed_hook_entry(entry));
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            });
            groups.is_empty()
        };
        if drop_event {
            hooks_val.as_object_mut().unwrap().remove(&event);
        }
    }

    // 2) Register each HOOK_EVENTS entry (currently just Stop).
    let mut results: Vec<(&'static str, HookStatus)> = Vec::new();
    for (event, spec) in HOOK_EVENTS {
        let entry_val = hooks_val
            .as_object_mut()
            .unwrap()
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !entry_val.is_array() {
            *entry_val = Value::Array(Vec::new());
        }
        let groups = entry_val.as_array_mut().unwrap();

        // Pass 1: rewrite legacy shell-form in place.
        let mut rewrote_shell = false;
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                for h in hs.iter_mut() {
                    if rewrite_if_shell_form(h, spec) {
                        rewrote_shell = true;
                    }
                }
            }
        }

        // Pass 1b: v3.1 — append `--json` to entries that match the spec
        // minus the trailing flag (pre-v3.1 installs). Counts as a migrate
        // because the on-disk shape changed.
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                for h in hs.iter_mut() {
                    if append_json_if_needed(h, spec) {
                        rewrote_shell = true;
                    }
                }
            }
        }

        let runner_present = groups.iter().any(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|entry| matches_spec(entry, &HookSpec::RUNNER))
                })
        });
        let presence = if runner_present {
            Presence::Found
        } else {
            check_hook_presence(groups, spec)
        };
        let status = match presence {
            Presence::Found => {
                if rewrote_shell {
                    HookStatus::Migrated
                } else {
                    HookStatus::Ok
                }
            }
            Presence::Migrate => {
                for g in groups.iter_mut() {
                    if g.get("matcher").is_none() {
                        g.as_object_mut()
                            .unwrap()
                            .insert("matcher".into(), Value::String("".into()));
                    }
                    if let Some(hs) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                        for h in hs.iter_mut() {
                            let cmd = h
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if cmd.contains("checkpoint-hook.sh") {
                                let obj = h.as_object_mut().unwrap();
                                obj.insert("command".into(), Value::String(spec.command.into()));
                                let args: Vec<Value> = spec
                                    .args
                                    .iter()
                                    .map(|s| Value::String((*s).to_string()))
                                    .collect();
                                obj.insert("args".into(), Value::Array(args));
                                obj.insert("type".into(), Value::String("command".into()));
                            }
                        }
                    }
                }
                HookStatus::Migrated
            }
            Presence::Missing => {
                groups.push(json!({
                    "matcher": "",
                    "hooks": [spec.to_canonical_entry()],
                }));
                HookStatus::Added
            }
        };
        results.push((*event, status));
    }
    results
}

fn is_managed_stop_entry(entry: &Value) -> bool {
    matches_spec(entry, &HookSpec::RUNNER)
        || matches_spec(entry, &HookSpec::STOP)
        || matches_spec(entry, &HookSpec::EMBED)
        || entry
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains("checkpoint-hook.sh"))
}

fn is_canonical_runner(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("command")
        && entry.get("command").and_then(Value::as_str) == Some(HookSpec::RUNNER.command)
        && entry
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| args.as_slice() == [json!("hook")])
}

fn rewrite_to_runner(entry: &mut Value) {
    let Some(object) = entry.as_object_mut() else {
        return;
    };
    object.insert(
        "command".to_string(),
        Value::String(HookSpec::RUNNER.command.to_string()),
    );
    object.insert("args".to_string(), json!(["hook"]));
    object.insert("type".to_string(), Value::String("command".to_string()));
}

fn converge_stop_entries(settings: &mut Value) -> bool {
    let hooks = settings
        .as_object_mut()
        .expect("settings is a JSON object")
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let stop = hooks
        .as_object_mut()
        .unwrap()
        .entry("Stop".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !stop.is_array() {
        *stop = Value::Array(Vec::new());
    }
    let groups = stop.as_array_mut().unwrap();

    let mut seen = false;
    let mut changed = false;
    for group in groups.iter_mut() {
        let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain_mut(|entry| {
            if !is_managed_stop_entry(entry) {
                return true;
            }
            if seen {
                changed = true;
                return false;
            }
            seen = true;
            if !is_canonical_runner(entry) {
                rewrite_to_runner(entry);
                changed = true;
            }
            true
        });
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    });

    if !seen {
        groups.push(json!({
            "matcher": "",
            "hooks": [HookSpec::RUNNER.to_canonical_entry()],
        }));
        changed = true;
    }
    changed
}

/// Register one shared Stop runner and collapse every legacy OneBrain Stop
/// action into that single entry.
pub(crate) fn apply_lifecycle_hook(settings: &mut Value) -> HookStatus {
    let initial: Vec<&Value> = settings
        .pointer("/hooks/Stop")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|entry| is_managed_stop_entry(entry))
        .collect();
    let initially_clean = initial.len() == 1 && is_canonical_runner(initial[0]);
    let initially_missing = initial.is_empty();

    let _ = apply_hooks(settings);
    let changed = converge_stop_entries(settings);

    if initially_missing {
        HookStatus::Added
    } else if initially_clean && !changed {
        HookStatus::Ok
    } else {
        HookStatus::Migrated
    }
}

/// Strip OneBrain-managed hook entries from every event. Used by `--remove`.
pub(crate) fn strip_onebrain_hooks(settings: &mut Value) {
    let Some(root) = settings.as_object_mut() else {
        return;
    };
    let Some(hooks_val) = root.get_mut("hooks") else {
        return;
    };
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return;
    };
    let events: Vec<String> = hooks_obj.keys().cloned().collect();
    for event in events {
        let drop_event = {
            let Some(groups) = hooks_obj.get_mut(&event).and_then(|v| v.as_array_mut()) else {
                continue;
            };
            for g in groups.iter_mut() {
                if let Some(arr) = g.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                    arr.retain(|entry| !is_managed_hook_entry(entry));
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            });
            groups.is_empty()
        };
        if drop_event {
            hooks_obj.remove(&event);
        }
    }
    if hooks_obj.is_empty() {
        root.remove("hooks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_spec_canonical_form_v31() {
        let entry = json!({"command": "onebrain", "args": ["checkpoint", "stop", "--json"]});
        assert!(matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_canonical_form_v30_pre_json() {
        // v3.0 canonical (no --json) still recognised so apply_hooks can
        // migrate it in place by appending the flag.
        let entry = json!({"command": "onebrain", "args": ["checkpoint", "stop"]});
        assert!(matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_legacy_shell_form_v30() {
        let entry = json!({"command": "onebrain checkpoint stop"});
        assert!(matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_legacy_shell_form_v31() {
        let entry = json!({"command": "onebrain checkpoint stop --json"});
        assert!(matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_non_match() {
        let entry = json!({"command": "echo hi"});
        assert!(!matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_canonical_wrong_args_does_not_match() {
        let entry = json!({"command": "onebrain", "args": ["checkpoint"]});
        assert!(!matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_legacy_shell_form_with_args_does_not_match() {
        let entry = json!({"command": "onebrain checkpoint stop", "args": []});
        assert!(!matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn rewrite_if_shell_form_rewrites_v30_legacy_in_place() {
        let mut entry = json!({"command": "onebrain checkpoint stop"});
        assert!(rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["command"], "onebrain");
        assert_eq!(entry["args"], json!(["checkpoint", "stop", "--json"]));
        assert_eq!(entry["type"], "command");
    }

    #[test]
    fn rewrite_if_shell_form_rewrites_v31_shell_in_place() {
        let mut entry = json!({"command": "onebrain checkpoint stop --json"});
        assert!(rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["command"], "onebrain");
        assert_eq!(entry["args"], json!(["checkpoint", "stop", "--json"]));
    }

    #[test]
    fn rewrite_if_shell_form_leaves_canonical_alone() {
        let mut entry = json!({"command": "onebrain", "args": ["checkpoint", "stop", "--json"]});
        assert!(!rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
    }

    #[test]
    fn rewrite_if_shell_form_preserves_unknown_fields() {
        let mut entry = json!({
            "command": "onebrain checkpoint stop",
            "comment": "user note",
        });
        assert!(rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["comment"], "user note");
    }

    #[test]
    fn rewrite_if_shell_form_repairs_existing_type() {
        let mut entry = json!({
            "command": "onebrain checkpoint stop",
            "type": "custom-type",
        });
        assert!(rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["type"], "command");
    }

    #[test]
    fn append_json_if_needed_v30_canonical() {
        let mut entry = json!({"command": "onebrain", "args": ["checkpoint", "stop"]});
        assert!(append_json_if_needed(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["args"], json!(["checkpoint", "stop", "--json"]));
    }

    #[test]
    fn append_json_if_needed_already_v31_noop() {
        let mut entry = json!({"command": "onebrain", "args": ["checkpoint", "stop", "--json"]});
        assert!(!append_json_if_needed(&mut entry, &HookSpec::STOP));
    }

    #[test]
    fn check_hook_presence_found_v31() {
        let groups = vec![json!({
            "hooks": [{"command": "onebrain", "args": ["checkpoint", "stop", "--json"]}]
        })];
        assert_eq!(
            check_hook_presence(&groups, &HookSpec::STOP),
            Presence::Found
        );
    }

    #[test]
    fn check_hook_presence_found_v30_canonical() {
        // Pre-v3.1 canonical also counts as "found" — apply_hooks migrates
        // it in place by appending --json.
        let groups = vec![json!({
            "hooks": [{"command": "onebrain", "args": ["checkpoint", "stop"]}]
        })];
        assert_eq!(
            check_hook_presence(&groups, &HookSpec::STOP),
            Presence::Found
        );
    }

    #[test]
    fn check_hook_presence_migrate() {
        let groups = vec![json!({
            "hooks": [{"command": "/x/checkpoint-hook.sh stop"}]
        })];
        assert_eq!(
            check_hook_presence(&groups, &HookSpec::STOP),
            Presence::Migrate
        );
    }

    #[test]
    fn check_hook_presence_missing() {
        let groups = vec![json!({"hooks": [{"command": "echo hi"}]})];
        assert_eq!(
            check_hook_presence(&groups, &HookSpec::STOP),
            Presence::Missing
        );
    }

    #[test]
    fn check_hook_presence_canonical_beats_migrate() {
        let groups = vec![json!({
            "hooks": [
                {"command": "/x/checkpoint-hook.sh stop"},
                {"command": "onebrain", "args": ["checkpoint", "stop", "--json"]},
            ]
        })];
        assert_eq!(
            check_hook_presence(&groups, &HookSpec::STOP),
            Presence::Found
        );
    }

    #[test]
    fn apply_hooks_fresh_adds_stop() {
        let mut s = json!({});
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Added)]);
        let entries = s["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "onebrain");
        // v3.1: fresh installs scaffold the --json flag so machine
        // consumers (Claude Code Stop hook) get JSON now that text is the
        // new default.
        assert_eq!(entries[0]["args"], json!(["checkpoint", "stop", "--json"]));
        assert_eq!(entries[0]["type"], "command");
        assert_eq!(s["hooks"]["Stop"][0]["matcher"], "");
    }

    #[test]
    fn apply_hooks_idempotent_second_run_is_ok() {
        let mut s = json!({});
        apply_hooks(&mut s);
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Ok)]);
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn apply_lifecycle_hook_repairs_missing_runner_type() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"command": "onebrain", "args": ["hook"], "note": "keep"}
                ]}]
            }
        });

        let status = apply_lifecycle_hook(&mut s);

        assert_eq!(status, HookStatus::Migrated);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["type"], "command");
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["note"], "keep");
    }

    #[test]
    fn apply_lifecycle_hook_repairs_wrong_runner_type() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"type": "shell", "command": "onebrain", "args": ["hook"]}
                ]}]
            }
        });

        let status = apply_lifecycle_hook(&mut s);

        assert_eq!(status, HookStatus::Migrated);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["type"], "command");
    }

    #[test]
    fn apply_hooks_legacy_shell_form_migrates_in_place_no_duplicate() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{"command": "onebrain checkpoint stop"}]}],
            }
        });
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Migrated)]);
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
    fn apply_hooks_v30_exec_form_migrates_with_json_flag() {
        // v3.0 canonical args (no --json) get the flag appended in-place.
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{
                    "command": "onebrain", "args": ["checkpoint", "stop"]
                }]}],
            }
        });
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Migrated)]);
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
    fn apply_hooks_legacy_shell_form_idempotent_second_run() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{"command": "onebrain checkpoint stop"}]}],
            }
        });
        let r1 = apply_hooks(&mut s);
        assert_eq!(r1, vec![("Stop", HookStatus::Migrated)]);
        let r2 = apply_hooks(&mut s);
        assert_eq!(r2, vec![("Stop", HookStatus::Ok)]);
    }

    #[test]
    fn apply_hooks_checkpoint_hook_sh_migrated_to_exec() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"hooks": [{"command": "/path/to/checkpoint-hook.sh stop"}]}],
            }
        });
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Migrated)]);
        let entry = &s["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(entry["command"], "onebrain");
        assert_eq!(entry["args"], json!(["checkpoint", "stop", "--json"]));
        assert_eq!(entry["type"], "command");
        assert_eq!(s["hooks"]["Stop"][0]["matcher"], "");
    }

    #[test]
    fn apply_hooks_strips_user_prompt_submit_with_onebrain_cmd() {
        let mut s = json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [{"command": "onebrain checkpoint user-prompt-submit"}]
                }],
                "SessionStart": [{
                    "matcher": "",
                    "hooks": [{"command": "onebrain something"}]
                }],
            }
        });
        apply_hooks(&mut s);
        assert!(s["hooks"].get("UserPromptSubmit").is_none());
        assert!(s["hooks"].get("SessionStart").is_none());
        assert!(s["hooks"]["Stop"].is_array());
    }

    #[test]
    fn apply_hooks_preserves_user_script_under_user_prompt_submit() {
        let mut s = json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "matcher": "",
                    "hooks": [
                        {"command": "onebrain checkpoint user-prompt-submit"},
                        {"command": "my-custom-script.sh"},
                    ]
                }],
            }
        });
        apply_hooks(&mut s);
        let ups = &s["hooks"]["UserPromptSubmit"];
        assert!(ups.is_array());
        let entries = ups[0]["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "my-custom-script.sh");
    }

    #[test]
    fn apply_hooks_strips_precompact_postcompact() {
        let mut s = json!({
            "hooks": {
                "PreCompact": [{"matcher": "", "hooks": [{"command": "onebrain checkpoint precompact"}]}],
                "PostCompact": [{"matcher": "", "hooks": [{"command": "onebrain checkpoint postcompact"}]}],
            }
        });
        apply_hooks(&mut s);
        assert!(s["hooks"].get("PreCompact").is_none());
        assert!(s["hooks"].get("PostCompact").is_none());
    }

    #[test]
    fn apply_hooks_preserves_post_tool_use_event_during_sweep() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"command": "onebrain qmd-reindex"}]
                }],
            }
        });
        apply_hooks(&mut s);
        assert!(s["hooks"]["PostToolUse"].is_array());
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(entry["command"], "onebrain qmd-reindex");
    }

    #[test]
    fn strip_onebrain_hooks_removes_stop_entries() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{"command": "onebrain", "args": ["checkpoint", "stop"]}]}],
            }
        });
        strip_onebrain_hooks(&mut s);
        assert!(
            s.get("hooks").is_none(),
            "hooks object should be dropped when empty"
        );
    }

    #[test]
    fn strip_onebrain_hooks_preserves_user_hooks() {
        let mut s = json!({
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"command": "onebrain", "args": ["checkpoint", "stop"]},
                    {"command": "my-custom.sh"},
                ]}],
            }
        });
        strip_onebrain_hooks(&mut s);
        let entries = s["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "my-custom.sh");
    }

    // ── HookStatus::as_str ──────────────────────────────────────────────────

    #[test]
    fn hookstatus_as_str_added() {
        assert_eq!(HookStatus::Added.as_str(), "added");
    }

    #[test]
    fn hookstatus_as_str_migrated() {
        assert_eq!(HookStatus::Migrated.as_str(), "migrated");
    }

    #[test]
    fn hookstatus_as_str_ok() {
        assert_eq!(HookStatus::Ok.as_str(), "ok");
    }

    // ── strip_onebrain_hooks early-return guards ─────────────────────────────

    #[test]
    fn strip_onebrain_hooks_noop_on_non_object() {
        // settings is not an object → first guard fires, returns immediately.
        let mut s = json!([1, 2, 3]);
        strip_onebrain_hooks(&mut s);
        assert_eq!(s, json!([1, 2, 3]));
    }

    #[test]
    fn strip_onebrain_hooks_noop_when_no_hooks_key() {
        // No "hooks" key → second guard fires, returns immediately.
        let mut s = json!({"theme": "dark"});
        strip_onebrain_hooks(&mut s);
        assert!(s.get("hooks").is_none());
        assert_eq!(s["theme"], "dark");
    }

    #[test]
    fn strip_onebrain_hooks_noop_when_hooks_is_not_object() {
        // "hooks" is an array rather than an object → third guard fires.
        let mut s = json!({"hooks": ["not", "an", "object"]});
        strip_onebrain_hooks(&mut s);
        assert_eq!(s["hooks"], json!(["not", "an", "object"]));
    }

    // ── apply_hooks reset branches ───────────────────────────────────────────

    #[test]
    fn apply_hooks_resets_non_object_hooks_field() {
        // "hooks" is a scalar → must be reset to {} before Stop is registered.
        let mut s = json!({"hooks": "corrupted"});
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Added)]);
        assert!(s["hooks"].is_object());
        let groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "onebrain");
    }

    #[test]
    fn apply_hooks_resets_non_array_stop_event() {
        // "hooks.Stop" is a scalar → must be reset to [] then populated.
        let mut s = json!({"hooks": {"Stop": "bad"}});
        let r = apply_hooks(&mut s);
        assert_eq!(r, vec![("Stop", HookStatus::Added)]);
        let groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "onebrain");
    }

    #[test]
    fn apply_hooks_stale_event_non_array_value_is_skipped() {
        // A stale event whose value is a scalar (not an array) → the cleanup
        // loop hits the `else { continue }` guard without panicking.
        let mut s = json!({
            "hooks": {
                "UserPromptSubmit": "not-an-array",
            }
        });
        let r = apply_hooks(&mut s);
        // Stop is still added; the malformed stale entry is left untouched.
        assert_eq!(r, vec![("Stop", HookStatus::Added)]);
        assert!(s["hooks"]["Stop"].is_array());
    }

    // ── spec_args_without_json / matches_pre_json_full_cmd None paths ────────

    #[test]
    fn matches_spec_pre_json_false_when_spec_has_no_json_suffix() {
        // spec_args_without_json returns None → matches_spec_pre_json returns
        // false immediately (the early-return else branch).
        const SPEC: HookSpec = HookSpec {
            command: "onebrain",
            args: &["checkpoint", "stop"], // no trailing --json
        };
        let entry = json!({"command": "onebrain", "args": ["checkpoint", "stop"]});
        assert!(!matches_spec_pre_json(&entry, &SPEC));
    }

    #[test]
    fn rewrite_if_shell_form_false_for_spec_without_json_suffix() {
        // With no trailing --json on the spec, matches_pre_json_full_cmd hits
        // the None → return-false branch inside its own else guard.
        const SPEC: HookSpec = HookSpec {
            command: "onebrain",
            args: &["checkpoint", "stop"], // no trailing --json
        };
        // cmd doesn't match spec.full_cmd() either, so both branches return false.
        let mut entry = json!({"command": "something-entirely-different"});
        assert!(!rewrite_if_shell_form(&mut entry, &SPEC));
    }

    // ── check_hook_presence with empty input ─────────────────────────────────

    #[test]
    fn check_hook_presence_empty_groups_is_missing() {
        assert_eq!(check_hook_presence(&[], &HookSpec::STOP), Presence::Missing);
    }

    // ── HookSpec::EMBED (v3.4.5 Track 4) ─────────────────────────────────────

    #[test]
    fn embed_spec_has_pending_only_json_args() {
        assert_eq!(HookSpec::EMBED.command, "onebrain");
        assert_eq!(
            HookSpec::EMBED.args,
            &["search", "reindex", "--pending-only", "--json"]
        );
    }

    #[test]
    fn qmd_spec_has_lex_only_json_args() {
        assert_eq!(HookSpec::REINDEX.command, "onebrain");
        assert_eq!(
            HookSpec::REINDEX.args,
            &["search", "reindex", "--lex-only", "--json"]
        );
    }

    #[test]
    fn matches_spec_embed_does_not_match_qmd_entry() {
        // `--lex-only` and `--pending-only` entries must never cross-match —
        // they share the `search reindex` prefix but are distinct hooks.
        let qmd_entry = json!({
            "command": "onebrain",
            "args": ["search", "reindex", "--lex-only", "--json"]
        });
        assert!(!matches_spec(&qmd_entry, &HookSpec::EMBED));
        let embed_entry = json!({
            "command": "onebrain",
            "args": ["search", "reindex", "--pending-only", "--json"]
        });
        assert!(!matches_spec(&embed_entry, &HookSpec::REINDEX));
    }

    #[test]
    fn matches_spec_embed_does_not_match_checkpoint_stop_entry() {
        let checkpoint = json!({"command": "onebrain", "args": ["checkpoint", "stop", "--json"]});
        assert!(!matches_spec(&checkpoint, &HookSpec::EMBED));
    }
}
