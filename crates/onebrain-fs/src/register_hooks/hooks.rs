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
    pub(crate) const STOP: HookSpec = HookSpec {
        command: "onebrain",
        args: &["checkpoint", "stop"],
    };

    pub(crate) const QMD: HookSpec = HookSpec {
        command: "onebrain",
        args: &["qmd-reindex"],
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
pub(crate) fn matches_spec(entry: &Value, spec: &HookSpec) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let args = entry.get("args").and_then(|v| v.as_array());

    if cmd == spec.command {
        if let Some(args) = args {
            let got: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
            if got == spec.args {
                return true;
            }
        }
    }
    if cmd == spec.full_cmd() && args.is_none() {
        return true;
    }
    false
}

/// If `entry` is the legacy shell form of `spec`, rewrite it to canonical.
/// Returns true on rewrite.
pub(crate) fn rewrite_if_shell_form(entry: &mut Value, spec: &HookSpec) -> bool {
    let cmd = entry
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let has_args = entry.get("args").and_then(|v| v.as_array()).is_some();
    if cmd != spec.full_cmd() || has_args {
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
    obj.entry("type".to_string())
        .or_insert_with(|| Value::String("command".into()));
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
                    h.retain(|entry| {
                        let cmd = entry.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        !cmd.contains("onebrain")
                    });
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

        let presence = check_hook_presence(groups, spec);
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
                                obj.entry("type".to_string())
                                    .or_insert_with(|| Value::String("command".into()));
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
                    arr.retain(|h| {
                        let cmd = h.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        !cmd.contains("onebrain")
                    });
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
    fn matches_spec_canonical_form() {
        let entry = json!({"command": "onebrain", "args": ["checkpoint", "stop"]});
        assert!(matches_spec(&entry, &HookSpec::STOP));
    }

    #[test]
    fn matches_spec_legacy_shell_form() {
        let entry = json!({"command": "onebrain checkpoint stop"});
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
    fn rewrite_if_shell_form_rewrites_legacy_in_place() {
        let mut entry = json!({"command": "onebrain checkpoint stop"});
        assert!(rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["command"], "onebrain");
        assert_eq!(entry["args"], json!(["checkpoint", "stop"]));
        assert_eq!(entry["type"], "command");
    }

    #[test]
    fn rewrite_if_shell_form_leaves_canonical_alone() {
        let mut entry = json!({"command": "onebrain", "args": ["checkpoint", "stop"]});
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
    fn rewrite_if_shell_form_preserves_existing_type() {
        let mut entry = json!({
            "command": "onebrain checkpoint stop",
            "type": "custom-type",
        });
        assert!(rewrite_if_shell_form(&mut entry, &HookSpec::STOP));
        assert_eq!(entry["type"], "custom-type");
    }

    #[test]
    fn check_hook_presence_found() {
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
                {"command": "onebrain", "args": ["checkpoint", "stop"]},
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
        assert_eq!(entries[0]["args"], json!(["checkpoint", "stop"]));
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
        assert_eq!(entries[0]["args"], json!(["checkpoint", "stop"]));
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
        assert_eq!(entry["args"], json!(["checkpoint", "stop"]));
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
}
