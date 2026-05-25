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
use crate::Result;
use serde_json::Value;
use std::path::Path;

/// Plugin name as registered in `marketplace.json`. Plugin id format is
/// `<plugin-name>@<marketplace-name>` — both are `onebrain` for us.
pub(crate) const PLUGIN_KEY: &str = "onebrain@onebrain";

/// Merge `enabledPlugins.onebrain@onebrain = true` into the vault's
/// `.claude/settings.json`. Returns `true` when the file was written
/// (newly-created or modified), `false` when the key was already set so
/// no write happened.
pub(crate) fn enable_onebrain_plugin(vault_dir: &Path) -> Result<bool> {
    let path = settings_path(vault_dir);
    let mut settings = read_settings(&path)?;

    if already_enabled(&settings) {
        return Ok(false);
    }

    set_enabled(&mut settings);
    write_settings(&path, &settings)?;
    Ok(true)
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
