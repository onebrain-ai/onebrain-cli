//! `.claude/settings.json` hooks + permission validation.
//!
//! Bun parity: `src/lib/validator.ts::checkSettingsHooks` (lines 574-760).
//!
//! Validates that:
//! - The Stop hook is registered in canonical exec form
//!   (`command: "onebrain", args: ["checkpoint", "stop"]`).
//! - If `qmd_collection` is set in vault.yml, the PostToolUse qmd hook is
//!   also registered.
//! - No onebrain-* commands are registered under hook events other than
//!   `Stop` / `PostToolUse` (stale events from previous CLI versions).
//! - No stale bash-wrapper scripts (`checkpoint-hook.sh`, `session-init.sh`)
//!   are referenced from any hook entry.
//! - The `Bash(onebrain *)` permission is granted in `permissions.allow`.

use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use serde_json::Value;
use std::path::Path;

pub struct SettingsHooksCheck;

/// Hooks OneBrain must register. Each entry pairs the event name with the
/// substring expected to appear in the joined `command + args` string.
const REQUIRED_HOOKS: &[(&str, &str)] = &[("Stop", "onebrain checkpoint stop")];

/// Hook events OneBrain is allowed to register. Any onebrain-* command found
/// under any other hook event (PreCompact, PostCompact, UserPromptSubmit,
/// SessionStart, etc.) is stale and must be removed.
const ALLOWED_HOOK_EVENTS: &[&str] = &["Stop", "PostToolUse"];

const QMD_HOOK_SUBSTRING: &str = "onebrain qmd-reindex";
const ONEBRAIN_COMMAND_SUBSTRING: &str = "onebrain";
const REQUIRED_PERMISSION: &str = "Bash(onebrain *)";
const STALE_HOOK_SUBSTRINGS: &[&str] = &["checkpoint-hook.sh", "session-init.sh"];
const CANONICAL_HOOK_COMMAND: &str = "onebrain";

/// Form of the matching hook entry, if any:
/// - `Exec`   — canonical exec form: `{ command: "onebrain", args: [...] }`
/// - `Legacy` — any matching entry not in canonical exec form (shell-form,
///   wrapper like `bash -c …`, missing args[], etc.).
/// - `Absent` — no entry matches the substring.
#[derive(Debug, PartialEq, Eq)]
enum HookForm {
    Exec,
    Legacy,
    Absent,
}

/// Effective command string for a hook entry.
///
/// Tolerates both schemas Claude Code accepts:
/// - legacy shell form: `{ command: "onebrain checkpoint stop" }`
/// - new exec form:     `{ command: "onebrain", args: ["checkpoint", "stop"] }`
///
/// Both reduce to the same space-joined string, so a single substring check
/// works for either. settings.json is user-edited JSON, so `args` may carry
/// non-string entries despite the typed interface — filter defensively before
/// joining so a stray null/number can't produce ghost substring matches.
fn effective_command(hook: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(c) = hook.get("command").and_then(|v| v.as_str()) {
        if !c.is_empty() {
            parts.push(c);
        }
    }
    if let Some(args) = hook.get("args").and_then(|v| v.as_array()) {
        for a in args {
            if let Some(s) = a.as_str() {
                if !s.is_empty() {
                    parts.push(s);
                }
            }
        }
    }
    parts.join(" ")
}

/// Returns true iff the hook entry is in canonical exec form:
/// `command == "onebrain"` AND `args` is a non-empty array.
fn is_canonical(hook: &Value) -> bool {
    let cmd_ok = hook.get("command").and_then(|v| v.as_str()) == Some(CANONICAL_HOOK_COMMAND);
    let args_ok = hook
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    cmd_ok && args_ok
}

/// Classify all matching hook entries under `event`. Scan ALL matches first —
/// if any one is in canonical exec form, return `Exec` even when a legacy
/// duplicate also matches. This handles partial migrations where a new
/// canonical entry was added before the legacy one was removed: the canonical
/// entry is what actually fires, so it should win the form classification.
fn detect_hook_form(settings: &Value, event: &str, substring: &str) -> HookForm {
    let mut saw_legacy = false;
    let Some(groups) = settings
        .pointer(&format!("/hooks/{}", event))
        .and_then(|v| v.as_array())
    else {
        return HookForm::Absent;
    };
    for g in groups {
        let Some(hooks) = g.get("hooks").and_then(|v| v.as_array()) else {
            continue;
        };
        for h in hooks {
            if !effective_command(h).contains(substring) {
                continue;
            }
            if is_canonical(h) {
                return HookForm::Exec;
            }
            saw_legacy = true;
        }
    }
    if saw_legacy {
        HookForm::Legacy
    } else {
        HookForm::Absent
    }
}

impl Check for SettingsHooksCheck {
    fn name(&self) -> &'static str {
        "settings-hooks"
    }

    fn run(&self, vault_root: &Path, config: &VaultConfig) -> DoctorResult {
        let path = vault_root.join(".claude").join("settings.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                return DoctorResult::warn("settings-hooks", "settings.json not found")
                    .with_hint("Run onebrain doctor --fix to register hooks");
            }
        };
        let settings: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                return DoctorResult::error("settings-hooks", "settings.json contains invalid JSON");
            }
        };

        let mut warnings: Vec<String> = Vec::new();
        let mut confirmed_hooks: Vec<String> = Vec::new();
        let mut permission_ok = false;

        // Required hooks
        for (event, cmd_substring) in REQUIRED_HOOKS {
            match detect_hook_form(&settings, event, cmd_substring) {
                HookForm::Exec => confirmed_hooks.push(format!("{} ✓", event)),
                HookForm::Legacy => warnings.push(format!(
                    "{} hook in legacy shell form — --fix will migrate to exec form",
                    event
                )),
                HookForm::Absent => warnings.push(format!("{} hook missing", event)),
            }
        }

        // PostToolUse (qmd) — conditional on qmd_collection
        if config.qmd_collection.is_some() {
            match detect_hook_form(&settings, "PostToolUse", QMD_HOOK_SUBSTRING) {
                HookForm::Exec => confirmed_hooks.push("PostToolUse ✓".to_string()),
                HookForm::Legacy => warnings.push(
                    "PostToolUse (qmd) hook in legacy shell form — --fix will migrate to exec form"
                        .to_string(),
                ),
                HookForm::Absent => warnings.push("PostToolUse (qmd) hook missing".to_string()),
            }
        }

        // Stale hooks: any onebrain-* command registered under an event NOT in
        // the allowed set, plus any reference to a stale bash wrapper script.
        if let Some(hooks_map) = settings.get("hooks").and_then(|v| v.as_object()) {
            for (event, groups_val) in hooks_map {
                let Some(groups) = groups_val.as_array() else {
                    continue;
                };
                for g in groups {
                    let Some(hs) = g.get("hooks").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    for h in hs {
                        let cmd = effective_command(h);
                        if !ALLOWED_HOOK_EVENTS.contains(&event.as_str())
                            && cmd.contains(ONEBRAIN_COMMAND_SUBSTRING)
                        {
                            warnings.push(format!(
                                "stale {} hook found (onebrain CLI only registers Stop + PostToolUse)",
                                event
                            ));
                        }
                        for sub in STALE_HOOK_SUBSTRINGS {
                            if cmd.contains(sub) {
                                warnings.push(format!("stale bash hook reference: {}", sub));
                            }
                        }
                    }
                }
            }
        }

        // Permission check
        let allow_has_required = settings
            .pointer("/permissions/allow")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|v| v.as_str() == Some(REQUIRED_PERMISSION)))
            .unwrap_or(false);
        if !allow_has_required {
            warnings.push(format!("missing permission: {}", REQUIRED_PERMISSION));
        } else {
            permission_ok = true;
        }

        if !warnings.is_empty() {
            let n = warnings.len();
            let hint = "Run onebrain doctor --fix to repair hooks";
            let mut details = warnings;
            details.push(hint.to_string());
            return DoctorResult::warn("settings-hooks", format!("{} issue(s)", n))
                .with_hint(hint)
                .with_details(details);
        }

        let mut ok_details: Vec<String> = Vec::new();
        if !confirmed_hooks.is_empty() {
            ok_details.push(format!("hooks: {}", confirmed_hooks.join("  ")));
        }
        if permission_ok {
            ok_details.push("permissions: Bash(onebrain *) ✓".to_string());
        }
        let mut r = DoctorResult::ok("settings-hooks", "hooks ok");
        if !ok_details.is_empty() {
            r = r.with_details(ok_details);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorStatus;
    use serde_json::json;
    use std::path::Path;
    use tempfile::tempdir;

    fn cfg(qmd: Option<&str>) -> VaultConfig {
        VaultConfig {
            qmd_collection: qmd.map(|s| s.to_string()),
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    }

    fn write_settings(root: &Path, value: &Value) {
        let dir = root.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), serde_json::to_string(value).unwrap()).unwrap();
    }

    fn write_settings_raw(root: &Path, text: &str) {
        let dir = root.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), text).unwrap();
    }

    /// Canonical clean settings.json (exec-form Stop hook + permission).
    fn clean_settings() -> Value {
        json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        })
    }

    #[test]
    fn missing_settings_warns_with_fix_hint() {
        let d = tempdir().unwrap();
        // No .claude/settings.json written
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert_eq!(r.message, "settings.json not found");
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain doctor --fix to register hooks")
        );
    }

    #[test]
    fn invalid_json_errors() {
        let d = tempdir().unwrap();
        write_settings_raw(d.path(), "{not valid json");
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Error);
        assert_eq!(r.message, "settings.json contains invalid JSON");
    }

    #[test]
    fn stop_hook_missing_is_warning() {
        let d = tempdir().unwrap();
        // No hooks at all, but permission present so only Stop is flagged
        let s = json!({
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.details.iter().any(|s| s == "Stop hook missing"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain doctor --fix to repair hooks")
        );
    }

    #[test]
    fn stop_hook_in_legacy_shell_form_is_warning() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            // Legacy shell form — single command string, no args
                            { "command": "onebrain checkpoint stop" }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("Stop hook in legacy shell form")));
    }

    #[test]
    fn stop_hook_canonical_exec_form_is_ok() {
        let d = tempdir().unwrap();
        write_settings(d.path(), &clean_settings());
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Ok);
        assert_eq!(r.message, "hooks ok");
        assert!(r.details.iter().any(|s| s.contains("Stop ✓")));
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("permissions: Bash(onebrain *) ✓")));
    }

    #[test]
    fn canonical_wins_over_legacy_duplicate() {
        // Partial-migration: legacy duplicate still present alongside canonical.
        // The canonical entry is what actually fires, so it should win classification.
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain checkpoint stop" },
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Ok, "details: {:?}", r.details);
    }

    #[test]
    fn stale_precompact_event_is_warning() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ],
                // Stale: PreCompact isn't allowed any more
                "PreCompact": [
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("stale PreCompact hook found")));
    }

    #[test]
    fn stale_bash_script_reference_is_warning() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            // Reference to retired bash wrapper script
                            { "command": "bash", "args": [".claude/plugins/onebrain/checkpoint-hook.sh"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("stale bash hook reference: checkpoint-hook.sh")));
    }

    #[test]
    fn missing_permission_is_warning() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": [] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("missing permission: Bash(onebrain *)")));
    }

    #[test]
    fn qmd_collection_set_requires_posttooluse() {
        let d = tempdir().unwrap();
        // Stop hook + permission present, but no PostToolUse — qmd_collection is set
        write_settings(d.path(), &clean_settings());
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("PostToolUse (qmd) hook missing")));
    }

    #[test]
    fn all_clean_with_qmd_reports_ok() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ],
                "PostToolUse": [
                    {
                        "matcher": "Write|Edit",
                        "hooks": [
                            { "command": "onebrain", "args": ["qmd-reindex"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Ok, "details: {:?}", r.details);
        assert_eq!(r.message, "hooks ok");
        let hooks_detail = r
            .details
            .iter()
            .find(|s| s.starts_with("hooks:"))
            .expect("hooks detail present");
        assert!(hooks_detail.contains("Stop ✓"));
        assert!(hooks_detail.contains("PostToolUse ✓"));
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("permissions: Bash(onebrain *) ✓")));
    }

    #[test]
    fn defensive_non_string_args_dont_create_ghost_matches() {
        // args containing null/number should not contribute to effective command
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            // command is "foo" with junk args → effective command "foo"
                            // (no "onebrain" substring), so Stop hook is absent.
                            { "command": "foo", "args": [null, 42, "bar"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r.details.iter().any(|s| s == "Stop hook missing"));
    }
}
