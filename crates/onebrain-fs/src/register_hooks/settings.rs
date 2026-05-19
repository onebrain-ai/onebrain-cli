//! `.claude/settings.json` read / atomic write helpers.
//!
//! The write side matches Bun's `JSON.stringify(value, null, 4)` byte-for-byte
//! (4-space indent, no trailing newline). Atomic via `*.tmp` → rename, same
//! pattern as `onebrain-cache/src/state.rs`.

use crate::{FsError, Result};
use serde_json::{ser::PrettyFormatter, Serializer, Value};
use std::path::{Path, PathBuf};

/// Conventional location of the Claude Code harness settings file.
pub(crate) fn settings_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".claude").join("settings.json")
}

/// Read settings.json as a `serde_json::Value`. A missing file is treated as
/// an empty object (matches Bun's `readSettings` ENOENT branch); any other
/// I/O or JSON-parse failure surfaces as `FsError::Io`.
pub(crate) fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| FsError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Default::default())),
        Err(e) => Err(FsError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

/// Pretty-print `value` with a 4-space indent (matching Bun's
/// `JSON.stringify(value, null, 4)`).
fn pretty_4_space(value: &Value) -> Result<String> {
    let indent = b"    ";
    let formatter = PrettyFormatter::with_indent(indent);
    let mut buf = Vec::with_capacity(256);
    let mut ser = Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut ser).map_err(|e| FsError::Io {
        path: PathBuf::new(),
        source: std::io::Error::other(e),
    })?;
    String::from_utf8(buf).map_err(|e| FsError::Io {
        path: PathBuf::new(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })
}

use serde::Serialize;

/// Atomic write: pretty-print + write to `.tmp` + rename. Creates parent dirs
/// as needed.
pub(crate) fn write_settings(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .expect("settings.json path always has a parent");
    std::fs::create_dir_all(parent).map_err(|e| FsError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    let text = pretty_4_space(value)?;
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, &text).map_err(|e| FsError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    std::fs::rename(&tmp, path).map_err(|e| FsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn read_settings_missing_returns_empty_object() {
        let d = tempdir().unwrap();
        let path = d.path().join("missing.json");
        let v = read_settings(&path).unwrap();
        assert!(v.is_object());
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn read_settings_invalid_json_errors() {
        let d = tempdir().unwrap();
        let path = d.path().join("settings.json");
        std::fs::write(&path, "{not valid").unwrap();
        let err = read_settings(&path).unwrap_err();
        match err {
            FsError::Io { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn read_settings_round_trips_unknown_keys() {
        let d = tempdir().unwrap();
        let path = d.path().join("settings.json");
        let original = json!({
            "theme": "dark",
            "customField": {"nested": [1, 2, 3]},
            "permissions": {"allow": ["Read"]},
        });
        std::fs::write(&path, serde_json::to_string(&original).unwrap()).unwrap();
        let loaded = read_settings(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn write_settings_creates_parent_dir() {
        let d = tempdir().unwrap();
        let path = d.path().join(".claude").join("settings.json");
        let v = json!({"k": "v"});
        write_settings(&path, &v).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn write_settings_atomic_via_tmp_rename() {
        let d = tempdir().unwrap();
        let path = d.path().join("settings.json");
        write_settings(&path, &json!({})).unwrap();
        // Tmp file must not be left behind
        let tmp = d.path().join("settings.json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn write_settings_uses_4_space_indent() {
        let d = tempdir().unwrap();
        let path = d.path().join("settings.json");
        write_settings(&path, &json!({"a": {"b": "c"}})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Inner field "b" should be indented by 8 spaces (2 levels × 4).
        assert!(
            text.contains("        \"b\""),
            "expected 8-space indent in: {text:?}"
        );
    }

    #[test]
    fn write_settings_matches_bun_json_stringify_format() {
        // Spot-check that the produced output looks like JSON.stringify(value, null, 4):
        // 4-space indent, no trailing newline.
        let d = tempdir().unwrap();
        let path = d.path().join("settings.json");
        write_settings(&path, &json!({"hooks": {"Stop": []}})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n    \"hooks\": "), "got: {text:?}");
        assert!(!text.ends_with('\n'), "no trailing newline; got: {text:?}");
    }
}
