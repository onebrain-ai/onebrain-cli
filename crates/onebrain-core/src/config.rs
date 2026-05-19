use crate::{CoreError, Result, VaultRoot};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Optional qmd MCP collection name · None disables semantic search wiring.
    #[serde(default)]
    pub qmd_collection: Option<String>,

    /// Checkpoint policy (Stop hook thresholds). Defaults supplied by `CheckpointPolicy::default`.
    #[serde(default)]
    pub checkpoint: CheckpointPolicy,
}

/// Checkpoint policy fields parsed from `vault.yml`'s `checkpoint:` block.
///
/// Defaults match Bun v2.3.3 (`DEFAULT_CHECKPOINT` in `src/lib/parser.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointPolicy {
    /// Minutes between Stop hook checkpoint emissions. Default 30.
    #[serde(default = "default_checkpoint_minutes")]
    pub minutes: u32,
}

fn default_checkpoint_minutes() -> u32 {
    30
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self {
            minutes: default_checkpoint_minutes(),
        }
    }
}

/// Read and parse `<root>/vault.yml`. Returns [`CoreError::VaultYamlMissing`]
/// when the file does not exist and [`CoreError::InvalidYaml`] on parse errors.
pub fn load_vault_config(root: &VaultRoot) -> Result<VaultConfig> {
    let path = root.join("vault.yml");
    let content = std::fs::read_to_string(&path).map_err(|source| CoreError::VaultYamlMissing {
        path: path.clone(),
        source,
    })?;
    let config: VaultConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

/// Load `vault.yml` from an arbitrary directory path (no `VaultRoot` invariant).
///
/// Use this when the caller has a raw path that *may or may not* be a vault root
/// (e.g., the Active-Session Guard threshold derivation reads vault.yml from a
/// best-effort location and falls back on any error).
pub fn load_vault_config_at(path: &Path) -> Result<VaultConfig> {
    let yml = path.join("vault.yml");
    let content = std::fs::read_to_string(&yml).map_err(|source| CoreError::VaultYamlMissing {
        path: yml.clone(),
        source,
    })?;
    let config: VaultConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_vault(content: &str) -> (tempfile::TempDir, VaultRoot) {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), content).unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn loads_minimal_vault_yml() {
        let (_dir, root) = write_vault("qmd_collection: ob-1\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.qmd_collection.as_deref(), Some("ob-1"));
    }

    #[test]
    fn missing_qmd_collection_defaults_to_none() {
        let (_dir, root) = write_vault("# empty config\n");
        let cfg = load_vault_config(&root).unwrap();
        assert!(cfg.qmd_collection.is_none());
    }

    #[test]
    fn malformed_yaml_returns_invalid_yaml_variant() {
        let (_dir, root) = write_vault("not: : valid");
        let err = load_vault_config(&root).unwrap_err();
        assert!(matches!(err, CoreError::InvalidYaml(_)));
    }

    #[test]
    fn vault_yml_missing_returns_vault_yaml_missing_variant() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "").unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        std::fs::remove_file(root.join("vault.yml")).unwrap();
        let err = load_vault_config(&root).unwrap_err();
        assert!(matches!(err, CoreError::VaultYamlMissing { .. }));
    }

    #[test]
    fn loads_checkpoint_minutes_from_vault_yml() {
        let (_dir, root) = write_vault("checkpoint:\n  minutes: 45\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.checkpoint.minutes, 45);
    }

    #[test]
    fn missing_checkpoint_defaults_to_30() {
        let (_dir, root) = write_vault("# no checkpoint config\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.checkpoint.minutes, 30);
    }

    #[test]
    fn partial_checkpoint_uses_default_for_missing_minutes() {
        let (_dir, root) = write_vault("checkpoint:\n  messages: 10\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.checkpoint.minutes, 30);
    }
}
