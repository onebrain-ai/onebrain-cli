//! `enabledPlugins.onebrain@onebrain` merge into `.claude/settings.json`.
//!
//! Claude Code reads this top-level key to know which marketplace plugins to
//! load for the current project. Without it, the marketplace manifest at
//! `<vault>/.claude-plugin/marketplace.json` is visible but the plugin
//! stays dormant — `/onboarding` and every other OneBrain skill silently
//! fail to load.
//!
//! Atomic merge semantics:
//!   - Read existing settings.json (missing file is fine — treated as empty)
//!   - Insert `enabledPlugins.onebrain@onebrain = true` (preserving all other
//!     top-level keys: hooks, permissions, model, theme, …)
//!   - Write back via the same atomic tmp+rename used by register-hooks
//!   - Skip write entirely when the key is already `true` (idempotent)
//!
//! Malformed pre-existing settings.json (invalid JSON) surfaces as a hard
//! error rather than silently overwriting the user's data.

use crate::register_hooks::settings::{read_settings, settings_path, write_settings};
use crate::{FsError, Result};
use onebrain_core::CoreError;
use serde_json::Value;
use std::path::Path;

/// Plugin name as registered in `marketplace.json`. Plugin id format is
/// `<plugin-name>@<marketplace-name>` — both are `onebrain` for us.
pub(crate) const PLUGIN_KEY: &str = "onebrain@onebrain";

/// Outcome of a plugin-enable attempt. Carries the warning text (if any) so
/// the caller can route it to stderr without re-deriving the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnableOutcome {
    /// True if settings.json was newly written (or rewritten). False if the
    /// key was already `true` and we no-oped.
    pub wrote: bool,
    /// Optional warning to surface on stderr. Set when `--force` clobbered a
    /// non-bool existing entry.
    pub warning: Option<String>,
}

/// Merge `enabledPlugins.onebrain@onebrain = true` into the vault's
/// `.claude/settings.json`.
///
/// - When the key is already `Some(true)`: no-op.
/// - When the key is missing or `Some(false)`: insert/update without
///   complaint.
/// - When the key exists with a NON-bool value (string, object, …):
///   - `force = false` → refuse with [`CoreError::InitTargetNotEmpty`]
///     (re-using the existing exit-75 envelope — the user opted into a
///     destructive clobber path that they need to consciously confirm).
///   - `force = true` → overwrite with `true` AND return a warning string
///     describing what was replaced.
pub(crate) fn enable_onebrain_plugin_with_force(
    vault_dir: &Path,
    force: bool,
) -> Result<EnableOutcome> {
    let path = settings_path(vault_dir);
    let mut settings = read_settings(&path)?;

    if already_enabled(&settings) {
        return Ok(EnableOutcome {
            wrote: false,
            warning: None,
        });
    }

    // B1 (SF-H3): detect non-bool existing entry before overwriting.
    let existing = settings
        .get("enabledPlugins")
        .and_then(|v| v.get(PLUGIN_KEY))
        .cloned();
    let warning = match existing {
        Some(ref v) if !matches!(v, Value::Bool(_)) => {
            let kind = value_type_name(v);
            if !force {
                return Err(FsError::Core(CoreError::InitTargetNotEmpty(format!(
                    "enabledPlugins.{PLUGIN_KEY} is set to a non-bool value ({kind}) in .claude/settings.json · re-run with --force to replace it"
                ))));
            }
            Some(format!(
                "init: replacing non-bool enabledPlugins.{PLUGIN_KEY} (was: {kind}) with true"
            ))
        }
        _ => None,
    };

    set_enabled(&mut settings);
    write_settings(&path, &settings)?;
    Ok(EnableOutcome {
        wrote: true,
        warning,
    })
}

/// Back-compat wrapper for tests that don't care about the force surface.
/// Behaves like the pre-fix function (force-implied so non-bool values are
/// silently overwritten — keeps the pre-existing test expectations green).
#[cfg(test)]
pub(crate) fn enable_onebrain_plugin(vault_dir: &Path) -> Result<bool> {
    Ok(enable_onebrain_plugin_with_force(vault_dir, true)?.wrote)
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn already_enabled(settings: &Value) -> bool {
    settings
        .get("enabledPlugins")
        .and_then(|v| v.get(PLUGIN_KEY))
        .and_then(|v| v.as_bool())
        == Some(true)
}

fn set_enabled(settings: &mut Value) {
    // Ensure top-level is an object — read_settings always returns one for a
    // missing file, but a malformed-but-non-object existing file could pass
    // parse and hit this path; defensive promotion keeps the merge total.
    if !settings.is_object() {
        *settings = Value::Object(Default::default());
    }
    let obj = settings.as_object_mut().expect("promoted to object above");
    let enabled = obj
        .entry("enabledPlugins")
        .or_insert_with(|| Value::Object(Default::default()));
    if !enabled.is_object() {
        *enabled = Value::Object(Default::default());
    }
    enabled
        .as_object_mut()
        .expect("promoted to object above")
        .insert(PLUGIN_KEY.to_string(), Value::Bool(true));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn read_back(vault: &Path) -> Value {
        let text = std::fs::read_to_string(vault.join(".claude").join("settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn fresh_vault_creates_settings_json_with_enabled_plugins() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let wrote = enable_onebrain_plugin(d.path()).unwrap();
        assert!(wrote);
        let v = read_back(d.path());
        assert_eq!(v["enabledPlugins"][PLUGIN_KEY], json!(true));
    }

    #[test]
    fn merges_into_existing_settings_preserving_keys() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "hooks": {"Stop": []},
                "permissions": {"allow": ["Bash(custom *)"]},
                "model": "claude-sonnet"
            }))
            .unwrap(),
        )
        .unwrap();
        let wrote = enable_onebrain_plugin(d.path()).unwrap();
        assert!(wrote);
        let v = read_back(d.path());
        assert_eq!(v["enabledPlugins"][PLUGIN_KEY], json!(true));
        assert_eq!(v["hooks"]["Stop"], json!([]));
        assert_eq!(v["permissions"]["allow"][0], "Bash(custom *)");
        assert_eq!(v["model"], "claude-sonnet");
    }

    #[test]
    fn idempotent_when_already_enabled() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "enabledPlugins": { PLUGIN_KEY: true },
                "hooks": {}
            }))
            .unwrap(),
        )
        .unwrap();
        // Capture mtime before
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let wrote = enable_onebrain_plugin(d.path()).unwrap();
        assert!(!wrote);
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "file should not have been rewritten");
    }

    #[test]
    fn merges_into_existing_enabled_plugins_block() {
        // An existing enabledPlugins with a different plugin must keep that
        // entry and pick up the new onebrain key alongside it.
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "enabledPlugins": { "other@marketplace": true }
            }))
            .unwrap(),
        )
        .unwrap();
        let wrote = enable_onebrain_plugin(d.path()).unwrap();
        assert!(wrote);
        let v = read_back(d.path());
        assert_eq!(v["enabledPlugins"]["other@marketplace"], json!(true));
        assert_eq!(v["enabledPlugins"][PLUGIN_KEY], json!(true));
    }

    /// B1 (SF-H3): a STRING value for `enabledPlugins.onebrain@onebrain` is
    /// refused without `--force` (exit 75 / InitTargetNotEmpty re-use). The
    /// file is left untouched on refusal — defensive against silent clobber
    /// of a user's manual customization.
    #[test]
    fn set_enabled_refuses_string_clobber_without_force() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "enabledPlugins": { PLUGIN_KEY: "true" }
            }))
            .unwrap(),
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let err = enable_onebrain_plugin_with_force(d.path(), false).unwrap_err();
        match err {
            crate::FsError::Core(onebrain_core::CoreError::InitTargetNotEmpty(msg)) => {
                assert!(msg.contains("non-bool"));
                assert!(msg.contains("string"));
                assert!(msg.contains("--force"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        // File untouched
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// B1 follow-up: with `--force`, the same scenario overwrites the
    /// non-bool entry with `true` AND returns a warning string describing
    /// what was replaced so the orchestrator can route it to stderr.
    #[test]
    fn set_enabled_force_clobbers_string_with_warning() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "enabledPlugins": { PLUGIN_KEY: "true" }
            }))
            .unwrap(),
        )
        .unwrap();

        let outcome = enable_onebrain_plugin_with_force(d.path(), true).unwrap();
        assert!(outcome.wrote);
        let warning = outcome.warning.expect("expected warning on force-clobber");
        assert!(warning.contains("replacing non-bool"));
        assert!(warning.contains("string"));

        let v = read_back(d.path());
        assert_eq!(v["enabledPlugins"][PLUGIN_KEY], json!(true));
    }

    /// B1 follow-up: object value (e.g. {"version": "..."}) is also refused
    /// without --force, and the warning names the type correctly under
    /// --force.
    #[test]
    fn set_enabled_object_value_refused_without_force() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "enabledPlugins": { PLUGIN_KEY: {"version": "1.0"} }
            }))
            .unwrap(),
        )
        .unwrap();

        let err = enable_onebrain_plugin_with_force(d.path(), false).unwrap_err();
        match err {
            crate::FsError::Core(onebrain_core::CoreError::InitTargetNotEmpty(msg)) => {
                assert!(msg.contains("object"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }

        let outcome = enable_onebrain_plugin_with_force(d.path(), true).unwrap();
        assert!(outcome.wrote);
        assert!(outcome.warning.unwrap().contains("object"));
    }

    #[test]
    fn malformed_settings_json_surfaces_error() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        let path = settings_path(d.path());
        std::fs::write(&path, "{not valid json").unwrap();
        let err = enable_onebrain_plugin(d.path()).unwrap_err();
        // FsError::Io with InvalidData kind — same pattern register-hooks uses
        match err {
            crate::FsError::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        // Existing file untouched
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{not valid json");
    }
}
