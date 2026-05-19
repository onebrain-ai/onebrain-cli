//! Step 7 · write `update_channel` into vault.yml. Port of Bun's `updateVaultYml`.
//!
//! - Reads existing vault.yml (or starts from empty doc on missing).
//! - Overwrites the `update_channel` key with the resolved channel.
//! - Atomic-writes the result.
//!
//! Divergence from Bun: serde_yaml emits keys in alphabetical order regardless
//! of insertion order. The vault-sync tests don't pin key order beyond a
//! `.toContain` check, so this is acceptable.

use serde_yaml::{Mapping, Value};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Step 7 entry point. Critical step — propagates errors to the orchestrator.
pub fn update_vault_yml(vault_root: &Path, update_channel: &str) -> std::io::Result<()> {
    let vault_yml_path = vault_root.join("vault.yml");
    let text = match fs::read_to_string(&vault_yml_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    let mut raw: Value = if text.trim().is_empty() {
        Value::Mapping(Mapping::new())
    } else {
        serde_yaml::from_str(&text).map_err(io_err)?
    };

    // Normalize to mapping if the file was e.g. a top-level scalar.
    if !raw.is_mapping() {
        raw = Value::Mapping(Mapping::new());
    }
    if let Value::Mapping(m) = &mut raw {
        m.insert(
            Value::String("update_channel".to_string()),
            Value::String(update_channel.to_string()),
        );
    }

    let mut serialized = serde_yaml::to_string(&raw).map_err(io_err)?;
    // serde_yaml may emit a leading `---\n` for empty maps; strip it to keep
    // the file looking like Bun's output (`yaml.stringify` doesn't emit `---`
    // by default).
    if let Some(stripped) = serialized.strip_prefix("---\n") {
        serialized = stripped.to_string();
    }

    atomic_write(&vault_yml_path, serialized.as_bytes())
}

fn io_err<E: std::error::Error>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

/// Best-effort atomic write — `{path}.tmp` + rename. If rename fails on a
/// platform that doesn't allow cross-mount renames (rare for vault.yml since
/// the tmp lives next to it), fall back to a direct write.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all().ok(); // best-effort
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Fall back to direct write so we don't strand the .tmp file.
            let _ = fs::remove_file(&tmp);
            fs::write(path, bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn writes_update_channel_when_file_missing() {
        let dir = tempdir().unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let yaml = fs::read_to_string(dir.path().join("vault.yml")).unwrap();
        assert!(yaml.contains("update_channel: stable"));
    }

    #[test]
    fn preserves_other_keys() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("vault.yml"),
            "update_channel: stable\nfolders:\n  inbox: 00-inbox\n  logs: 07-logs\n",
        )
        .unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let yaml = fs::read_to_string(dir.path().join("vault.yml")).unwrap();
        assert!(yaml.contains("update_channel: stable"));
        assert!(yaml.contains("inbox: 00-inbox"));
        assert!(yaml.contains("logs: 07-logs"));
        // Crucially: no onebrain_version key — per test 6.
        assert!(!yaml.contains("onebrain_version"));
    }

    #[test]
    fn invalid_yaml_returns_err() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "key: [unterminated\n").unwrap();
        let err = update_vault_yml(dir.path(), "stable").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
