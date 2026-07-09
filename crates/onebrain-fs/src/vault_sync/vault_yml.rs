//! Step 7 · ensure `update_channel` in the vault config file carries the
//! resolved channel. Port of Bun's `updateVaultYml`, rewritten for the
//! v3.4.8 self-documenting template (ADR 0026).
//!
//! - Reads the existing config file (canonical `onebrain.yml` preferred ·
//!   legacy `vault.yml` fallback · empty doc when neither exists).
//! - **Change-detects first:** when `update_channel` already equals the
//!   resolved channel the file is NOT touched at all — no rewrite, no
//!   backup, no mtime bump. This is the common case: a freshly scaffolded
//!   template (default `onebrain init`), and every re-sync of an
//!   already-synced vault.
//! - When a change IS needed, it lands as a **comment-preserving line edit**
//!   (`crate::yaml_edit`): replace the existing key line's value portion, or
//!   append the key when absent. The pre-v3.4.8 whole-file
//!   `serde_yaml::to_string` rewrite destroyed every comment in the new
//!   template on the very first sync.
//! - Atomic-writes back to the SAME path that was read · so vault-sync stays
//!   filename-agnostic and never resurrects a legacy `vault.yml` after
//!   `doctor --fix` migrated it.
//! - For a fresh vault (no config yet) writes the canonical `onebrain.yml`.
//! - Degenerate shapes keep the legacy behavior: a non-mapping root is
//!   replaced with a fresh single-key mapping; a mapping the line editor
//!   can't address (e.g. flow-style `{…}` root) falls back to the serde
//!   rewrite — correctness over comments in that corner.

use onebrain_core::{find_config_file, CONFIG_FILENAME};
use serde_yaml::Value;
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

    let fresh_single_key = format!("update_channel: {update_channel}\n");

    // Missing / empty config → seed a minimal one.
    if text.trim().is_empty() {
        crate::backup::backup_config_file(&config_path).map_err(io_err)?;
        return atomic_write(&config_path, fresh_single_key.as_bytes());
    }

    // Invalid YAML stays a hard error (unchanged contract).
    let parsed: Value = serde_yaml::from_str(&text).map_err(io_err)?;
    let Some(mapping) = parsed.as_mapping() else {
        // Degenerate non-mapping root (scalar/sequence): legacy behavior —
        // replace with a fresh mapping carrying only the channel.
        crate::backup::backup_config_file(&config_path).map_err(io_err)?;
        return atomic_write(&config_path, fresh_single_key.as_bytes());
    };

    let channel_key = Value::String("update_channel".to_string());
    let current = mapping.get(&channel_key).and_then(|v| v.as_str());
    if current == Some(update_channel) {
        // Already correct — do not touch the file (the template scaffolds
        // `update_channel: stable`, so the default init path lands here).
        return Ok(());
    }

    let updated = if mapping.contains_key(&channel_key) {
        // Key present with a different (or non-string) value → surgical
        // replace. The line editor only fails on shapes it can't address
        // (flow-style root) — fall back to the legacy serde rewrite there,
        // since a correct file beats a comment-preserving broken one.
        match crate::yaml_edit::set_value(&text, &["update_channel"], update_channel) {
            Some(t) => t,
            None => serde_rewrite_with_channel(&text, update_channel)?,
        }
    } else {
        // Key genuinely absent at the top level → append (duplicate-safe).
        crate::yaml_edit::append_top_level(&text, "update_channel", update_channel)
    };

    // Defense-in-depth: back up the existing config before overwriting it.
    // Hard precondition — if the backup can't be made, refuse the write.
    crate::backup::backup_config_file(&config_path).map_err(io_err)?;
    atomic_write(&config_path, updated.as_bytes())
}

/// Legacy whole-file rewrite (comments NOT preserved) — kept only as the
/// fallback for mapping shapes the line editor refuses, e.g. a flow-style
/// `{…}` root. Never reached for block-form configs.
fn serde_rewrite_with_channel(text: &str, update_channel: &str) -> std::io::Result<String> {
    let mut raw: Value = serde_yaml::from_str(text).map_err(io_err)?;
    if let Value::Mapping(m) = &mut raw {
        m.insert(
            Value::String("update_channel".to_string()),
            Value::String(update_channel.to_string()),
        );
    }
    let mut serialized = serde_yaml::to_string(&raw).map_err(io_err)?;
    if let Some(stripped) = serialized.strip_prefix("---\n") {
        serialized = stripped.to_string();
    }
    Ok(serialized)
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
    fn no_write_when_channel_already_correct() {
        // The default init path: the freshly scaffolded template already
        // carries `update_channel: stable` — sync must not touch the file at
        // all (no rewrite, no backup).
        let dir = tempdir().unwrap();
        let template =
            crate::init::render_onebrain_yml(crate::init::SchedulePreset::Essentials).unwrap();
        fs::write(dir.path().join("onebrain.yml"), &template).unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let after = fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, template, "no-op sync must not rewrite the file");
        assert!(
            !dir.path().join(crate::backup::BACKUP_DIR).exists(),
            "no write ⇒ no backup"
        );
    }

    #[test]
    fn channel_change_preserves_comments() {
        let dir = tempdir().unwrap();
        let text = "# header comment
                    update_channel: next
                    folders:
                        # inbox comment
                        inbox: 00-inbox
";
        fs::write(dir.path().join("onebrain.yml"), text).unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let after = fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("update_channel: stable"), "{after}");
        assert!(after.contains("# header comment"), "{after}");
        assert!(after.contains("# inbox comment"), "{after}");
        assert!(after.contains("inbox: 00-inbox"), "{after}");
    }

    #[test]
    fn missing_channel_key_appends_without_touching_comments() {
        let dir = tempdir().unwrap();
        let text = "# header comment
folders:
  # inbox comment
  inbox: 00-inbox
";
        fs::write(dir.path().join("onebrain.yml"), text).unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let after = fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(after.contains("update_channel: stable"), "{after}");
        assert!(after.contains("# header comment"), "{after}");
        assert!(after.contains("# inbox comment"), "{after}");
        // Still valid YAML with both keys.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
        assert_eq!(parsed["update_channel"].as_str(), Some("stable"), "{after}");
        assert_eq!(parsed["folders"]["inbox"].as_str(), Some("00-inbox"));
    }

    #[test]
    fn scalar_root_is_replaced_with_fresh_mapping() {
        // Legacy behavior for a degenerate config (non-mapping root): replace
        // with a fresh mapping carrying the channel.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("onebrain.yml"),
            "just-a-scalar
",
        )
        .unwrap();
        update_vault_yml(dir.path(), "stable").unwrap();
        let after = fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert_eq!(after, "update_channel: stable\n");
    }

    #[test]
    fn invalid_yaml_returns_err() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("vault.yml"), "key: [unterminated\n").unwrap();
        let err = update_vault_yml(dir.path(), "stable").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
