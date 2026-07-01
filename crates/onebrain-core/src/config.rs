use crate::{find_config_file, CoreError, Result, VaultRoot, CONFIG_FILENAME};
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

    /// Native search config (embedding model, collection, auto-embed gate).
    /// Defaults supplied by `SearchConfig::default`.
    #[serde(default)]
    pub search: SearchConfig,
}

/// Checkpoint policy fields parsed from the config's `checkpoint:` block.
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

/// Vault folder layout parsed from the config's `folders:` block.
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

/// Native search config parsed from the config's `search:` block.
///
/// `collection` falls back to the legacy top-level `qmd_collection` when
/// absent — see [`load_vault_config`] / [`load_vault_config_at`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Collection name for the native search index. Falls back to
    /// `qmd_collection` when unset (legacy vaults).
    #[serde(default)]
    pub collection: Option<String>,
    /// Embedding model name. Default `"multilingual-e5-small"` (small + fast;
    /// fastembed has no quantized bge-m3, so bge-m3 fp32 is opt-in via set-model).
    #[serde(default = "default_embed_model")]
    pub embed_model: String,
    /// Auto-embed gate (thresholds transferred from the folded auto-embed
    /// design). Parsed here only — enforcement happens in a later task.
    #[serde(default)]
    pub embed: EmbedGate,
}

fn default_embed_model() -> String {
    "multilingual-e5-small".to_string()
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            collection: None,
            embed_model: default_embed_model(),
            embed: EmbedGate::default(),
        }
    }
}

/// Auto-embed gate: controls when/how the native search index re-embeds
/// changed documents. Parsed only in this task — enforcement is added later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedGate {
    /// Whether auto-embed runs at all. Default `true`.
    #[serde(default = "default_embed_auto")]
    pub auto: bool,
    /// Minimum number of changed docs before an auto-embed run triggers. Default 10.
    #[serde(default = "default_embed_threshold")]
    pub threshold: u32,
    /// Debounce window (seconds) before an auto-embed run fires. Default 45.
    #[serde(default = "default_embed_debounce_seconds")]
    pub debounce_seconds: u64,
    /// Max docs embedded per batch. Default 200.
    #[serde(default = "default_embed_max_batch")]
    pub max_batch: u32,
    /// Optional cron-style schedule for a periodic full re-embed. Default `None`.
    #[serde(default)]
    pub schedule: Option<String>,
}

fn default_embed_auto() -> bool {
    true
}

fn default_embed_threshold() -> u32 {
    10
}

fn default_embed_debounce_seconds() -> u64 {
    45
}

fn default_embed_max_batch() -> u32 {
    200
}

impl Default for EmbedGate {
    fn default() -> Self {
        Self {
            auto: default_embed_auto(),
            threshold: default_embed_threshold(),
            debounce_seconds: default_embed_debounce_seconds(),
            max_batch: default_embed_max_batch(),
            schedule: None,
        }
    }
}

/// Read and parse the active vault config (`onebrain.yml` preferred,
/// legacy `vault.yml` as fallback with one-time deprecation warning).
/// Returns [`CoreError::VaultYamlMissing`] when neither file exists and
/// [`CoreError::InvalidYaml`] on parse errors.
pub fn load_vault_config(root: &VaultRoot) -> Result<VaultConfig> {
    let path = find_config_file(root.as_path()).unwrap_or_else(|| root.join(CONFIG_FILENAME));
    let content = std::fs::read_to_string(&path).map_err(|source| CoreError::VaultYamlMissing {
        path: path.clone(),
        source,
    })?;
    let mut config: VaultConfig = serde_yaml::from_str(&content)?;
    if config.search.collection.is_none() {
        config.search.collection = config.qmd_collection.clone();
    }
    Ok(config)
}

/// Load the active vault config from an arbitrary directory path (no
/// `VaultRoot` invariant). Same dual-read semantics as
/// [`load_vault_config`].
///
/// Use this when the caller has a raw path that *may or may not* be a vault root
/// (e.g., the Active-Session Guard threshold derivation reads the config from a
/// best-effort location and falls back on any error).
pub fn load_vault_config_at(path: &Path) -> Result<VaultConfig> {
    let yml = find_config_file(path).unwrap_or_else(|| path.join(CONFIG_FILENAME));
    let content = std::fs::read_to_string(&yml).map_err(|source| CoreError::VaultYamlMissing {
        path: yml.clone(),
        source,
    })?;
    let mut config: VaultConfig = serde_yaml::from_str(&content)?;
    if config.search.collection.is_none() {
        config.search.collection = config.qmd_collection.clone();
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_vault(content: &str) -> (tempfile::TempDir, VaultRoot) {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILENAME), content).unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        (dir, root)
    }

    fn write_legacy_vault(content: &str) -> (tempfile::TempDir, VaultRoot) {
        // Quiet the deprecation warning when the test fixture is the legacy
        // filename. Tests that want to observe the warning toggle the env
        // var off themselves via `path::reset_legacy_warning_for_test`.
        std::env::set_var("ONEBRAIN_QUIET_VAULT_YML_DEPRECATION", "1");
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(crate::LEGACY_CONFIG_FILENAME), content).unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        std::env::remove_var("ONEBRAIN_QUIET_VAULT_YML_DEPRECATION");
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
        std::fs::write(dir.path().join(CONFIG_FILENAME), "").unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        std::fs::remove_file(root.join(CONFIG_FILENAME)).unwrap();
        let err = load_vault_config(&root).unwrap_err();
        assert!(matches!(err, CoreError::VaultYamlMissing { .. }));
    }

    #[test]
    fn load_vault_config_reads_legacy_vault_yml() {
        // Back-compat read: vault.yml-only vault still loads cleanly.
        let (_dir, root) = write_legacy_vault("qmd_collection: legacy-ob\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.qmd_collection.as_deref(), Some("legacy-ob"));
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

    #[test]
    fn search_config_defaults() {
        let (_dir, root) = write_vault("qmd_collection: ob-1-441565\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.search.embed_model, "multilingual-e5-small");
        assert!(cfg.search.embed.auto);
        assert_eq!(cfg.search.embed.threshold, 10);
        assert_eq!(cfg.search.collection.as_deref(), Some("ob-1-441565"));
    }

    #[test]
    fn search_config_overrides() {
        let (_dir, root) =
            write_vault("search:\n  embed_model: multilingual-e5-large\n  collection: c1\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.search.embed_model, "multilingual-e5-large");
        assert_eq!(cfg.search.collection.as_deref(), Some("c1"));
    }
}
