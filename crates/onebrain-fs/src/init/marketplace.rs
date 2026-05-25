//! `<vault>/.claude-plugin/marketplace.json` writer.
//!
//! Claude Code requires a marketplace manifest at the vault root to discover
//! the plugin shipped at `.claude/plugins/onebrain/`. Without it, the plugin's
//! skills (including `/onboarding`) never load even when `enabledPlugins` is
//! set in `.claude/settings.json`.
//!
//! Idempotent on re-init: if the file already exists we leave it alone so
//! users who hand-tweaked the description / source path don't lose their
//! changes.

use crate::{FsError, Result};
use serde::Serialize;
use serde_json::{
    json,
    ser::{PrettyFormatter, Serializer},
    Value,
};
use std::path::Path;

/// Write `<vault>/.claude-plugin/marketplace.json` if it doesn't already
/// exist. Returns `true` when the file was newly written, `false` when it
/// was left untouched (already present).
pub(crate) fn write_marketplace_json(vault_dir: &Path) -> Result<bool> {
    let plugin_dir = vault_dir.join(".claude-plugin");
    let path = plugin_dir.join("marketplace.json");
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(&plugin_dir).map_err(|e| FsError::Io {
        path: plugin_dir.clone(),
        source: e,
    })?;
    let value = json!({
        "name": "onebrain",
        "owner": {
            "name": "OneBrain Contributors"
        },
        "plugins": [
            {
                "name": "onebrain",
                "source": "./.claude/plugins/onebrain",
                "description": "OneBrain — Your AI Thinking Partner (vault-bundled plugin)"
            }
        ]
    });
    let text = pretty_4_space(&value)?;
    std::fs::write(&path, text).map_err(|e| FsError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(true)
}

/// 4-space pretty-print matching settings.rs / Bun's `JSON.stringify(v, null, 4)`.
fn pretty_4_space(value: &Value) -> Result<String> {
    let indent = b"    ";
    let formatter = PrettyFormatter::with_indent(indent);
    let mut buf = Vec::with_capacity(256);
    let mut ser = Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut ser).map_err(|e| FsError::Io {
        path: std::path::PathBuf::new(),
        source: std::io::Error::other(e),
    })?;
    String::from_utf8(buf).map_err(|e| FsError::Io {
        path: std::path::PathBuf::new(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_marketplace_json_to_correct_path() {
        let d = tempdir().unwrap();
        let wrote = write_marketplace_json(d.path()).unwrap();
        assert!(wrote);
        let path = d.path().join(".claude-plugin").join("marketplace.json");
        assert!(path.is_file());
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["name"], "onebrain");
        assert_eq!(v["plugins"][0]["name"], "onebrain");
        assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
        assert!(v["plugins"][0]["description"]
            .as_str()
            .unwrap()
            .contains("Your AI Thinking Partner"));
    }

    #[test]
    fn does_not_overwrite_existing_marketplace_json() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        let custom = r#"{"name":"customized"}"#;
        std::fs::write(&path, custom).unwrap();
        let wrote = write_marketplace_json(d.path()).unwrap();
        assert!(!wrote, "expected no overwrite on second call");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, custom);
    }

    #[test]
    fn uses_4_space_indent() {
        let d = tempdir().unwrap();
        write_marketplace_json(d.path()).unwrap();
        let text =
            std::fs::read_to_string(d.path().join(".claude-plugin").join("marketplace.json"))
                .unwrap();
        // Inner field "name" should be indented by 4 spaces (1 level × 4).
        assert!(
            text.contains("\n    \"name\""),
            "expected 4-space indent: {text:?}"
        );
    }

    #[test]
    fn second_call_returns_false() {
        let d = tempdir().unwrap();
        assert!(write_marketplace_json(d.path()).unwrap());
        assert!(!write_marketplace_json(d.path()).unwrap());
    }
}
