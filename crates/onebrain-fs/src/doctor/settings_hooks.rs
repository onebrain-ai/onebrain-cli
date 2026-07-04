//! `.claude/settings.json` hooks + permission validation.
//!
//! Bun parity: `src/lib/validator.ts::checkSettingsHooks` (lines 574-760).
//!
//! Validates that:
//! - The Stop hook is registered in canonical exec form
//!   (`command: "onebrain", args: ["checkpoint", "stop"]`).
//! - If `qmd_collection` is set in vault.yml, the PostToolUse reindex hook
//!   (canonical `search reindex`) is also registered.
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

/// Canonical NEW reindex hook form (v3.4.5+): the native `search reindex`
/// subcommand.
const QMD_HOOK_SUBSTRING_NEW: &str = "onebrain search reindex";
/// Legacy v3.2–v3.4 form: `qmd reindex` (space). doctor must still recognize
/// this so it can advise migrating to the new form via `--fix`.
const QMD_HOOK_SUBSTRING_LEGACY_QMD_REINDEX: &str = "onebrain qmd reindex";
/// Legacy v3.0/v3.1 hidden alias `qmd-reindex` (hyphen). doctor must still
/// recognize this so it can advise migrating to the new form via `--fix`.
const QMD_HOOK_SUBSTRING_LEGACY: &str = "onebrain qmd-reindex";
const ONEBRAIN_COMMAND_SUBSTRING: &str = "onebrain";
const REQUIRED_PERMISSION: &str = "Bash(onebrain *)";
const STALE_HOOK_SUBSTRINGS: &[&str] = &["checkpoint-hook.sh", "session-init.sh"];
const CANONICAL_HOOK_COMMAND: &str = "onebrain";

/// Form of the matching hook entry, if any:
/// - `Exec`        — canonical exec form: `{ command: "onebrain", args: [...] }`
/// - `LegacyShell` — exactly one matching entry, not in canonical exec form
///   (shell-form, wrapper like `bash -c …`, missing args[], etc.).
/// - `LegacyAlias` — exactly one matching entry in exec form but using a
///   legacy reindex subcommand (`qmd reindex` space or `qmd-reindex` hyphen)
///   instead of the canonical `search reindex`. qmd-specific.
/// - `Duplicate(n)` — `n >= 2` matching entries (any mix of forms).
/// - `Absent`      — no entry matches.
#[derive(Debug, PartialEq, Eq)]
enum HookForm {
    Exec,
    LegacyShell,
    LegacyAlias,
    Duplicate(usize),
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
        HookForm::LegacyShell
    } else {
        HookForm::Absent
    }
}

/// Classify the PostToolUse reindex hook by counting EVERY entry whose
/// effective command matches the new canonical (`onebrain search reindex`)
/// OR either legacy form (`onebrain qmd reindex` space, `onebrain
/// qmd-reindex` hyphen). Unlike `detect_hook_form` (which short-circuits on
/// the first canonical match), this counts all matches so a duplicated hook
/// is reported as such rather than silently passing.
///
/// Returns:
/// - `Absent`        — 0 matches
/// - `Duplicate(n)`  — n >= 2 matches (any mix of forms)
/// - `Exec`          — exactly 1, new canonical exec form (`search reindex`)
/// - `LegacyAlias`   — exactly 1, exec form using a legacy subcommand
///   (`qmd reindex` or `qmd-reindex`)
/// - `LegacyShell`   — exactly 1, shell form (command string, no args[])
fn detect_qmd_hook_form(settings: &Value) -> HookForm {
    // (is_exec, is_new_form) for each matching entry.
    let mut matches: Vec<(bool, bool)> = Vec::new();
    if let Some(groups) = settings
        .pointer("/hooks/PostToolUse")
        .and_then(|v| v.as_array())
    {
        for g in groups {
            let Some(hooks) = g.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for h in hooks {
                let cmd = effective_command(h);
                let is_new = cmd.contains(QMD_HOOK_SUBSTRING_NEW);
                let is_legacy = cmd.contains(QMD_HOOK_SUBSTRING_LEGACY_QMD_REINDEX)
                    || cmd.contains(QMD_HOOK_SUBSTRING_LEGACY);
                if !is_new && !is_legacy {
                    continue;
                }
                // `is_canonical` only checks command=="onebrain" + non-empty
                // args[]; pair it with the form flag so we can tell the new
                // exec form apart from a legacy-subcommand exec form.
                matches.push((is_canonical(h), is_new));
            }
        }
    }

    match matches.len() {
        0 => HookForm::Absent,
        1 => {
            let (is_exec, is_new) = matches[0];
            if !is_exec {
                HookForm::LegacyShell
            } else if is_new {
                HookForm::Exec
            } else {
                HookForm::LegacyAlias
            }
        }
        n => HookForm::Duplicate(n),
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
                return DoctorResult::error(
                    "settings-hooks",
                    "settings.json contains invalid JSON",
                );
            }
        };

        let mut warnings: Vec<String> = Vec::new();
        let mut confirmed_hooks: Vec<String> = Vec::new();
        let mut permission_ok = false;

        // Required hooks
        for (event, cmd_substring) in REQUIRED_HOOKS {
            match detect_hook_form(&settings, event, cmd_substring) {
                HookForm::Exec => confirmed_hooks.push(format!("{} ✓", event)),
                HookForm::LegacyShell => warnings.push(format!(
                    "{} hook in legacy shell form — --fix will migrate to exec form",
                    event
                )),
                HookForm::Absent => warnings.push(format!("{} hook missing", event)),
                // detect_hook_form (Stop) never yields these; covered for
                // exhaustiveness only.
                HookForm::LegacyAlias | HookForm::Duplicate(_) => {
                    warnings.push(format!("{} hook needs repair", event))
                }
            }
        }

        // PostToolUse (qmd) — conditional on qmd_collection. Recognizes the
        // new canonical `search reindex` form as well as either legacy form
        // (`qmd reindex` space, `qmd-reindex` hyphen).
        if config.qmd_collection.is_some() {
            match detect_qmd_hook_form(&settings) {
                HookForm::Exec => confirmed_hooks.push("PostToolUse ✓".to_string()),
                HookForm::LegacyAlias => warnings.push(
                    "PostToolUse (qmd) hook uses legacy form (qmd reindex) — run onebrain doctor --fix to migrate"
                        .to_string(),
                ),
                HookForm::LegacyShell => warnings.push(
                    "PostToolUse (qmd) hook in legacy shell form — --fix will migrate to exec form"
                        .to_string(),
                ),
                HookForm::Duplicate(n) => warnings.push(format!(
                    "PostToolUse (qmd) hook duplicated (×{n}) — run onebrain doctor --fix"
                )),
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
            search: Default::default(),
        }
    }

    fn write_settings(root: &Path, value: &Value) {
        let dir = root.join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            serde_json::to_string(value).unwrap(),
        )
        .unwrap();
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
                            // Canonical NEW form: `search reindex`, not a
                            // legacy `qmd reindex` / `qmd-reindex` form.
                            { "command": "onebrain", "args": ["search", "reindex", "--json"] }
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

    /// New canonical exec form `search reindex` → ✓ ok. This is exactly the
    /// shape `apply_qmd_hook` (register_hooks/hooks.rs) now emits — this
    /// test is the cross-check that prevents the two modules from drifting.
    #[test]
    fn qmd_new_exec_form_reports_ok() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [
                        { "command": "onebrain", "args": ["checkpoint", "stop"] }
                    ] }
                ],
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain", "args": ["search", "reindex", "--json"] }
                    ] }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Ok, "details: {:?}", r.details);
        let hooks_detail = r
            .details
            .iter()
            .find(|s| s.starts_with("hooks:"))
            .expect("hooks detail present");
        assert!(hooks_detail.contains("PostToolUse ✓"));
    }

    /// `detect_qmd_hook_form` directly on the exact entry shape emitted by
    /// `apply_qmd_hook`'s `HookSpec::QMD` (`{"type":"command","command":
    /// "onebrain","args":["search","reindex","--json"]}`) — classifies as
    /// the present canonical form.
    #[test]
    fn apply_qmd_hook_emitted_entry_is_recognized_as_canonical() {
        let settings = json!({
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        {
                            "type": "command",
                            "command": "onebrain",
                            "args": ["search", "reindex", "--json"]
                        }
                    ] }
                ]
            }
        });
        assert_eq!(detect_qmd_hook_form(&settings), HookForm::Exec);
    }

    /// Single legacy exec form `qmd reindex` (space) → advisory to migrate.
    #[test]
    fn qmd_legacy_space_form_is_advisory() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [
                        { "command": "onebrain", "args": ["checkpoint", "stop"] }
                    ] }
                ],
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain", "args": ["qmd", "reindex", "--json"] }
                    ] }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Warn, "details: {:?}", r.details);
        assert!(r.details.iter().any(|s| s.contains(
            "PostToolUse (qmd) hook uses legacy form (qmd reindex) — run onebrain doctor --fix to migrate"
        )), "details: {:?}", r.details);
    }

    /// Single legacy-alias exec form `qmd-reindex` (hyphen) → advisory to migrate.
    #[test]
    fn qmd_legacy_alias_exec_form_is_advisory() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [
                        { "command": "onebrain", "args": ["checkpoint", "stop"] }
                    ] }
                ],
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain", "args": ["qmd-reindex", "--json"] }
                    ] }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Warn, "details: {:?}", r.details);
        assert!(r.details.iter().any(|s| s.contains(
            "PostToolUse (qmd) hook uses legacy form (qmd reindex) — run onebrain doctor --fix to migrate"
        )), "details: {:?}", r.details);
    }

    /// Duplicated qmd hook (×2) → duplicated warning.
    #[test]
    fn qmd_duplicate_reports_duplicated() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [
                        { "command": "onebrain", "args": ["checkpoint", "stop"] }
                    ] }
                ],
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain", "args": ["search", "reindex", "--json"] }
                    ] },
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain", "args": ["search", "reindex", "--json"] }
                    ] }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Warn, "details: {:?}", r.details);
        assert!(
            r.details.iter().any(|s| s
                .contains("PostToolUse (qmd) hook duplicated (×2) — run onebrain doctor --fix")),
            "details: {:?}",
            r.details
        );
    }

    /// Mixed legacy + new duplicate (×2 across forms) → duplicated warning
    /// (duplicate detection counts BOTH forms).
    #[test]
    fn qmd_mixed_legacy_and_new_reports_duplicated() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [
                        { "command": "onebrain", "args": ["checkpoint", "stop"] }
                    ] }
                ],
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain", "args": ["qmd-reindex", "--json"] },
                        { "command": "onebrain", "args": ["search", "reindex", "--json"] }
                    ] }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Warn, "details: {:?}", r.details);
        assert!(
            r.details.iter().any(|s| s
                .contains("PostToolUse (qmd) hook duplicated (×2) — run onebrain doctor --fix")),
            "details: {:?}",
            r.details
        );
    }

    /// Single shell-form qmd hook → legacy shell warning (existing behavior).
    #[test]
    fn qmd_shell_form_is_legacy_shell_warning() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [
                        { "command": "onebrain", "args": ["checkpoint", "stop"] }
                    ] }
                ],
                "PostToolUse": [
                    { "matcher": "Write|Edit", "hooks": [
                        { "command": "onebrain search reindex" }
                    ] }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(Some("ob-1")));
        assert_eq!(r.status, DoctorStatus::Warn, "details: {:?}", r.details);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("PostToolUse (qmd) hook in legacy shell form")));
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

    /// Empty `command: ""` must NOT be pushed to the effective-command string.
    /// Only non-empty command strings contribute (covers the `if !c.is_empty()`
    /// false branch in `effective_command`).
    #[test]
    fn effective_command_skips_empty_command_string() {
        let hook = json!({ "command": "", "args": ["checkpoint", "stop"] });
        assert_eq!(effective_command(&hook), "checkpoint stop");
    }

    /// A group entry that has no `"hooks"` array key is silently skipped inside
    /// `detect_hook_form`; the subsequent group with the canonical entry wins.
    /// Covers the `else { continue; }` branch.
    #[test]
    fn detect_hook_form_skips_group_missing_hooks_key() {
        let settings = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "" },  // no "hooks" key → skipped
                    {
                        "matcher": "",
                        "hooks": [
                            { "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ]
            }
        });
        assert_eq!(
            detect_hook_form(&settings, "Stop", "onebrain checkpoint stop"),
            HookForm::Exec
        );
    }

    /// When the top-level `"permissions"` key is absent entirely, the
    /// `unwrap_or(false)` fallback fires and the missing-permission warning
    /// is raised. Distinct from `"allow": []` (empty array) — here the whole
    /// pointer `/permissions/allow` resolves to `None`.
    #[test]
    fn permissions_key_entirely_absent_warns() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [{ "command": "onebrain", "args": ["checkpoint", "stop"] }]
                }]
            }
            // no "permissions" key at all
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Warn);
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("missing permission: Bash(onebrain *)")));
    }

    /// A hook event whose value is a non-array JSON value (e.g. a string) must
    /// be silently skipped by the stale-hooks scan without panicking.
    /// Covers the `let Some(groups) = groups_val.as_array() else { continue }` branch.
    #[test]
    fn stale_check_skips_non_array_hook_event_value() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [{ "command": "onebrain", "args": ["checkpoint", "stop"] }]
                }],
                "UserPromptSubmit": "not-an-array"
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Ok, "details: {:?}", r.details);
    }

    /// A group inside an allowed hook event that has no `"hooks"` array key
    /// must be silently skipped by the stale-hooks scan.
    /// Covers the inner `let Some(hs) = g.get("hooks") … else { continue }` branch.
    #[test]
    fn stale_check_skips_group_without_hooks_key() {
        let d = tempdir().unwrap();
        let s = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "" },  // no "hooks" key → skipped
                    {
                        "matcher": "",
                        "hooks": [{ "command": "onebrain", "args": ["checkpoint", "stop"] }]
                    }
                ]
            },
            "permissions": { "allow": ["Bash(onebrain *)"] }
        });
        write_settings(d.path(), &s);
        let r = SettingsHooksCheck.run(d.path(), &cfg(None));
        assert_eq!(r.status, DoctorStatus::Ok, "details: {:?}", r.details);
    }
}
