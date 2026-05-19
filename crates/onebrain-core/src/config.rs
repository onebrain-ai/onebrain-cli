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

    /// Folder layout · defaults supplied by `VaultFolders::default`.
    #[serde(default)]
    pub folders: VaultFolders,
}

/// Checkpoint policy fields parsed from `vault.yml`'s `checkpoint:` block.
///
/// Defaults match Bun v2.3.3 (`DEFAULT_CHECKPOINT` in `src/lib/parser.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointPolicy {
    /// Minutes between Stop hook checkpoint emissions. Default 30.
    #[serde(default = "default_checkpoint_minutes")]
    pub minutes: u32,
    /// Message-count threshold for Stop hook checkpoint emission. Default 15.
    #[serde(default = "default_checkpoint_messages")]
    pub messages: u32,
}

fn default_checkpoint_minutes() -> u32 {
    30
}

fn default_checkpoint_messages() -> u32 {
    15
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self {
            minutes: default_checkpoint_minutes(),
            messages: default_checkpoint_messages(),
        }
    }
}

/// Vault folder layout parsed from `vault.yml`'s `folders:` block.
/// Defaults match Bun v2.3.3 (`DEFAULT_FOLDERS` in `src/lib/parser.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultFolders {
    #[serde(default = "default_folders_inbox")]
    pub inbox: String,
    #[serde(default = "default_folders_projects")]
    pub projects: String,
    #[serde(default = "default_folders_areas")]
    pub areas: String,
    #[serde(default = "default_folders_knowledge")]
    pub knowledge: String,
    #[serde(default = "default_folders_resources")]
    pub resources: String,
    #[serde(default = "default_folders_agent")]
    pub agent: String,
    #[serde(default = "default_folders_archive")]
    pub archive: String,
    #[serde(default = "default_folders_logs")]
    pub logs: String,
}

fn default_folders_inbox() -> String {
    "00-inbox".to_string()
}

fn default_folders_projects() -> String {
    "01-projects".to_string()
}

fn default_folders_areas() -> String {
    "02-areas".to_string()
}

fn default_folders_knowledge() -> String {
    "03-knowledge".to_string()
}

fn default_folders_resources() -> String {
    "04-resources".to_string()
}

fn default_folders_agent() -> String {
    "05-agent".to_string()
}

fn default_folders_archive() -> String {
    "06-archive".to_string()
}

fn default_folders_logs() -> String {
    "07-logs".to_string()
}

impl Default for VaultFolders {
    fn default() -> Self {
        Self {
            inbox: default_folders_inbox(),
            projects: default_folders_projects(),
            areas: default_folders_areas(),
            knowledge: default_folders_knowledge(),
            resources: default_folders_resources(),
            agent: default_folders_agent(),
            archive: default_folders_archive(),
            logs: default_folders_logs(),
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

    #[test]
    fn loads_checkpoint_messages_from_vault_yml() {
        let (_dir, root) = write_vault("checkpoint:\n  messages: 20\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.checkpoint.messages, 20);
    }

    #[test]
    fn missing_checkpoint_messages_defaults_to_15() {
        let (_dir, root) = write_vault("# empty\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.checkpoint.messages, 15);
    }

    #[test]
    fn loads_folders_logs_from_vault_yml() {
        let (_dir, root) = write_vault("folders:\n  logs: custom-logs\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.folders.logs, "custom-logs");
    }

    #[test]
    fn missing_folders_logs_defaults_to_07_logs() {
        let (_dir, root) = write_vault("# empty\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.folders.logs, "07-logs");
    }

    #[test]
    fn folders_default_is_8_standard_keys() {
        let f = VaultFolders::default();
        assert_eq!(f.inbox, "00-inbox");
        assert_eq!(f.projects, "01-projects");
        assert_eq!(f.areas, "02-areas");
        assert_eq!(f.knowledge, "03-knowledge");
        assert_eq!(f.resources, "04-resources");
        assert_eq!(f.agent, "05-agent");
        assert_eq!(f.archive, "06-archive");
        assert_eq!(f.logs, "07-logs");
    }

    #[test]
    fn folders_partial_override_keeps_defaults() {
        let (_dir, root) = write_vault("folders:\n  inbox: my-inbox\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.folders.inbox, "my-inbox");
        assert_eq!(cfg.folders.projects, "01-projects"); // default preserved
    }
}
