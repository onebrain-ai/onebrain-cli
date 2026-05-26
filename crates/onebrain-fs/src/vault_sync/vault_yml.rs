//! Step 7 · write `update_channel` into the vault config file. Port of Bun's
//! `updateVaultYml`.
//!
//! - Reads the existing config file (canonical `onebrain.yml` preferred ·
//!   legacy `vault.yml` fallback · empty doc when neither exists).
//! - Overwrites the `update_channel` key with the resolved channel.
//! - Atomic-writes the result back to the SAME path that was read · so
//!   vault-sync stays filename-agnostic and never resurrects a legacy
//!   `vault.yml` after `doctor --fix` migrated it.
//! - For a fresh vault (no config yet) writes the canonical `onebrain.yml`.
//!
//! Divergence from Bun: serde_yaml emits keys in alphabetical order regardless
//! of insertion order. The vault-sync tests don't pin key order beyond a
//! `.toContain` check, so this is acceptable.

use onebrain_core::{find_config_file, CONFIG_FILENAME};
use serde_yaml::{Mapping, Value};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Step 7 entry point. Critical step — propagates errors to the orchestrator.
pub fn update_vault_yml(vault_root: &Path, update_channel: &str) -> std::io::Result<()> {
    // Honour whichever config file is already present (canonical preferred ·
    // legacy fallback). When neither exists, write the canonical filename so a
    // fresh vault-sync seeds `onebrain.yml`, not the deprecated `vault.yml`.
    let config_path =
        find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let text = match fs::read_to_string(&config_path) {
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

    // Defense-in-depth: back up the existing config before overwriting it.
    // Hard precondition — if the backup can't be made, refuse the write.
    crate::backup::backup_config_file(&config_path).map_err(io_err)?;

    atomic_write(&config_path, serialized.as_bytes())
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
        // Fresh vault · no config file yet · should seed the canonical
        // `onebrain.yml` (NOT the deprecated `vault.yml`).
        let dir = tempdir().unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let yaml = fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("update_channel: stable"));
        assert!(
            !dir.path().join("vault.yml").exists(),
            "fresh vaults must not get a legacy vault.yml created"
        );
    }

    #[test]
    fn preserves_other_keys() {
        // Legacy vault.yml exists · update_vault_yml MUST write back to
        // the same legacy file (round-trip preservation). The
        // vault-config-migration doctor recipe handles the rename to
        // onebrain.yml separately — vault-sync stays filename-agnostic.
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

    /// Canonical-only vault: update_vault_yml must round-trip onebrain.yml
    /// and NEVER materialize a stale vault.yml beside it (regression guard
    /// for the doctor --fix bug where vault-sync wrote vault.yml right
    /// after the migration recipe renamed it away).
    #[test]
    fn does_not_resurrect_legacy_when_canonical_present() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("onebrain.yml"),
            "update_channel: stable\nfolders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let yaml = fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("update_channel: stable"));
        assert!(yaml.contains("inbox: 00-inbox"));
        assert!(
            !dir.path().join("vault.yml").exists(),
            "vault-sync must not recreate vault.yml when onebrain.yml is canonical"
        );
    }

    #[test]
    fn invalid_yaml_returns_err() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "key: [unterminated\n").unwrap();
        let err = update_vault_yml(dir.path(), "stable").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
