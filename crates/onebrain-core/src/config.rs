use crate::{find_config_file, CoreError, Result, VaultRoot, CONFIG_FILENAME};
use onebrain_token::OptLevel;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Token-optimization ladder + cache + read-hook config (v3.4.10).
    /// Defaults supplied by `TokenOptimizationConfig::default`.
    #[serde(default)]
    pub token_optimization: TokenOptimizationConfig,

    /// Doctor-managed housekeeping flags parsed from the config's `stats:`
    /// block. `last_doctor_run` / `last_doctor_fix` are NOT modeled here —
    /// they're pure timestamps written by a specialized comment-preserving
    /// line edit (`doctor.rs`'s `stamp_doctor_run`) and never read back.
    /// `qmd_cleanup_declined` IS read back (it gates whether `doctor --fix`
    /// re-offers the qmd leftover cleanup prompt), so it gets its own typed
    /// field. Defaults supplied by `VaultStats::default`.
    #[serde(default)]
    pub stats: VaultStats,
}

/// Token-optimization config parsed from the config's `token_optimization:`
/// block (v3.4.10 design §5). `level` reuses `onebrain_token::OptLevel`
/// directly — its `Serialize`/`Deserialize` route through `Display`/`FromStr`
/// (`off|conservative|balanced|aggressive`), so the wire format here and the
/// `--opt-level` CLI flag can never drift from each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenOptimizationConfig {
    /// Optimization ladder rung. Default `conservative` — the ladder's own
    /// documented default rung (level 1); distinct from `OptLevel::default()`
    /// (`off`), which is the crate-internal "no level resolved" safety value.
    #[serde(default = "default_token_level")]
    pub level: OptLevel,
    /// `search get` / MCP `get` continuation cap, in estimated tokens.
    /// `0` = unlimited. Default 6000.
    #[serde(default = "default_token_get_max_tokens")]
    pub get_max_tokens: u32,
    /// Per-hit snippet length cap, in characters. Level-specific defaults
    /// (150 at balanced, 120 at aggressive) override this when unset.
    /// Default 200.
    #[serde(default = "default_token_snippet_max_chars")]
    pub snippet_max_chars: u32,
    /// When to strip YAML frontmatter from `get`/`multi_get` doc bodies.
    /// Default `auto` (strips at balanced+, per the ladder).
    #[serde(default)]
    pub strip_frontmatter: StripFrontmatter,
    /// Model family hint for token estimation (`onebrain_token::ModelFamily`
    /// resolution) and per-model pricing. `auto` sniffs `settings.json` as a
    /// hint only. Default `auto`.
    #[serde(default = "default_token_model")]
    pub model: String,
    /// Vault-read ledger-gate hook mode (design §5b). `off` = the hook
    /// (if registered) always allows; `ledger` = repeat reads of an
    /// already-sent doc are denied with a reference. Default `off` — the
    /// product-wide default; ob-1 flips this to `ledger` as the field test.
    #[serde(default)]
    pub read_hook: ReadHookMode,
}

fn default_token_level() -> OptLevel {
    OptLevel::Conservative
}

fn default_token_get_max_tokens() -> u32 {
    6000
}

fn default_token_snippet_max_chars() -> u32 {
    200
}

fn default_token_model() -> String {
    "auto".to_string()
}

impl Default for TokenOptimizationConfig {
    fn default() -> Self {
        Self {
            level: default_token_level(),
            get_max_tokens: default_token_get_max_tokens(),
            snippet_max_chars: default_token_snippet_max_chars(),
            strip_frontmatter: StripFrontmatter::default(),
            model: default_token_model(),
            read_hook: ReadHookMode::default(),
        }
    }
}

/// `token_optimization.strip_frontmatter` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StripFrontmatter {
    /// Strip at balanced+ (the ladder's own default behavior for that rung).
    #[default]
    Auto,
    Always,
    Never,
}

impl std::fmt::Display for StripFrontmatter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StripFrontmatter::Auto => "auto",
            StripFrontmatter::Always => "always",
            StripFrontmatter::Never => "never",
        };
        f.write_str(s)
    }
}

/// `token_optimization.read_hook` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadHookMode {
    #[default]
    Off,
    Ledger,
}

impl std::fmt::Display for ReadHookMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ReadHookMode::Off => "off",
            ReadHookMode::Ledger => "ledger",
        };
        f.write_str(s)
    }
}

/// Doctor-managed housekeeping flags parsed read-only from the config's
/// `stats:` block. See [`VaultConfig::stats`] for why this only carries
/// `qmd_cleanup_declined` and not the timestamp fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultStats {
    /// Set to `true` once the user declines the `doctor --fix` qmd leftover
    /// cleanup prompt — suppresses re-prompting on later `--fix` runs while
    /// still surfacing the advisory finding. `None`/absent means "never
    /// declined" (prompt normally).
    #[serde(default)]
    pub qmd_cleanup_declined: Option<bool>,
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

/// Reranker config parsed from the config's `search.reranker:` block.
///
/// The reranker improves search result relevance by re-scoring candidate results.
/// Model-name validation happens at the engine/CLI layer, not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankerConfig {
    /// Whether reranking is enabled. Default `true`.
    #[serde(default = "default_reranker_enabled")]
    pub enabled: bool,
    /// Reranker model name. Default `"onebrain-rerank-v1"`.
    /// Validation happens at engine/CLI layer where the supported-names list lives.
    #[serde(default = "default_reranker_model")]
    pub model: String,
    /// Minimum fused-candidate pool to rerank — a FLOOR, not a ceiling: the
    /// reranked pool is actually `max(min_candidates, top_k)`, auto-raised so
    /// every returned result is always reranked. Default 10 (calibrated).
    #[serde(default = "default_reranker_min_candidates")]
    pub min_candidates: usize,
    /// Minimum reranker score threshold. `None` uses the engine's calibrated
    /// default (`DEFAULT_RERANK_MIN_SCORE` = 0.30, measured on real ob-1).
    #[serde(default)]
    pub min_score: Option<f32>,
}

fn default_reranker_enabled() -> bool {
    true
}

fn default_reranker_model() -> String {
    "onebrain-rerank-v1".to_string()
}

fn default_reranker_min_candidates() -> usize {
    // Calibrated on real ob-1 (2026-07-06): 10 keeps golden-set quality (matches
    // land in top ~5 after rerank) at ~1/3 the CPU cost of 30. Mirror of
    // `RerankSettings::default().min_candidates` in onebrain-search.
    10
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: default_reranker_enabled(),
            model: default_reranker_model(),
            min_candidates: default_reranker_min_candidates(),
            min_score: None,
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
    /// Extra index-exclusion patterns on top of the built-ins (hidden dirs
    /// and `node_modules` are ALWAYS skipped). Each entry is either a
    /// vault-relative path prefix (`attachments/demo`) or a bare directory
    /// name matched at any depth (`drafts`). Defaults to `["attachments"]`
    /// — the vault's copied-file staging area, not knowledge notes; set
    /// `exclude: []` explicitly to index everything.
    #[serde(default = "default_search_exclude")]
    pub exclude: Vec<String>,
    /// Reranker config for improving search result relevance.
    #[serde(default)]
    pub reranker: RerankerConfig,
    /// Vault-level default result count (`top_k`) for a search surface that
    /// doesn't specify one explicitly (e.g. the webui's `/api/vault/search`
    /// when its `top_k` query param is omitted). Per-query callers — the CLI's
    /// `--top-k` flag, or an explicit API param — always override this.
    /// Default 10.
    #[serde(default = "default_search_default_top_k")]
    pub default_top_k: usize,
}

fn default_embed_model() -> String {
    "multilingual-e5-small".to_string()
}

fn default_search_exclude() -> Vec<String> {
    vec!["attachments".to_string()]
}

fn default_search_default_top_k() -> usize {
    10
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            collection: None,
            embed_model: default_embed_model(),
            embed: EmbedGate::default(),
            exclude: default_search_exclude(),
            reranker: RerankerConfig::default(),
            default_top_k: default_search_default_top_k(),
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

    #[test]
    fn search_exclude_defaults_to_attachments() {
        let cfg = SearchConfig::default();
        assert_eq!(cfg.exclude, vec!["attachments".to_string()]);
        // YAML without the key gets the same default via serde.
        let parsed: SearchConfig = serde_yaml::from_str("embed_model: bge-m3").unwrap();
        assert_eq!(parsed.exclude, vec!["attachments".to_string()]);
        // Explicit empty list opts out.
        let parsed: SearchConfig = serde_yaml::from_str("exclude: []").unwrap();
        assert!(parsed.exclude.is_empty());
    }
    use crate::test_support::EnvVarGuard;
    use tempfile::tempdir;

    fn write_vault(content: &str) -> (tempfile::TempDir, VaultRoot) {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILENAME), content).unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        (dir, root)
    }

    fn write_legacy_vault(content: &str) -> (tempfile::TempDir, VaultRoot) {
        // Quiet the deprecation warning when the test fixture is the legacy
        // filename (restored when `_quiet` drops at end of scope). Tests that
        // want to observe the warning toggle the env var off themselves via
        // `path::reset_legacy_warning_for_test`.
        let _quiet = EnvVarGuard::set("ONEBRAIN_QUIET_VAULT_YML_DEPRECATION", "1");
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(crate::LEGACY_CONFIG_FILENAME), content).unwrap();
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

    #[test]
    fn reranker_absent_uses_all_defaults() {
        let (_dir, root) = write_vault("search:\n  embed_model: multilingual-e5-small\n");
        let cfg = load_vault_config(&root).unwrap();
        assert!(cfg.search.reranker.enabled);
        assert_eq!(cfg.search.reranker.model, "onebrain-rerank-v1");
        assert_eq!(cfg.search.reranker.min_candidates, 10);
        assert_eq!(cfg.search.reranker.min_score, None);
    }

    #[test]
    fn reranker_partial_config_uses_defaults_for_missing_fields() {
        let (_dir, root) = write_vault("search:\n  reranker:\n    enabled: false\n");
        let cfg = load_vault_config(&root).unwrap();
        assert!(!cfg.search.reranker.enabled);
        assert_eq!(cfg.search.reranker.model, "onebrain-rerank-v1");
        assert_eq!(cfg.search.reranker.min_candidates, 10);
        assert_eq!(cfg.search.reranker.min_score, None);
    }

    #[test]
    fn reranker_full_config_round_trips() {
        let yaml = "search:\n  reranker:\n    enabled: true\n    model: custom-reranker\n    min_candidates: 50\n    min_score: 0.5\n";
        let (_dir, root) = write_vault(yaml);
        let cfg = load_vault_config(&root).unwrap();
        assert!(cfg.search.reranker.enabled);
        assert_eq!(cfg.search.reranker.model, "custom-reranker");
        assert_eq!(cfg.search.reranker.min_candidates, 50);
        assert_eq!(cfg.search.reranker.min_score, Some(0.5));
    }

    #[test]
    fn default_top_k_absent_defaults_to_ten() {
        let (_dir, root) = write_vault("search:\n  embed_model: multilingual-e5-small\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.search.default_top_k, 10);
        assert_eq!(SearchConfig::default().default_top_k, 10);
    }

    #[test]
    fn default_top_k_overrides_from_yaml() {
        let (_dir, root) = write_vault("search:\n  default_top_k: 25\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.search.default_top_k, 25);
        // Other search defaults preserved alongside the override.
        assert_eq!(cfg.search.embed_model, "multilingual-e5-small");
    }

    // ── token_optimization ──────────────────────────────────────────────

    #[test]
    fn token_optimization_absent_uses_all_defaults() {
        let (_dir, root) = write_vault("# no token_optimization block\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.token_optimization.level, OptLevel::Conservative);
        assert_eq!(cfg.token_optimization.get_max_tokens, 6000);
        assert_eq!(cfg.token_optimization.snippet_max_chars, 200);
        assert_eq!(
            cfg.token_optimization.strip_frontmatter,
            StripFrontmatter::Auto
        );
        assert_eq!(cfg.token_optimization.model, "auto");
        assert_eq!(cfg.token_optimization.read_hook, ReadHookMode::Off);
    }

    #[test]
    fn token_optimization_default_matches_struct_default() {
        assert_eq!(
            TokenOptimizationConfig::default().level,
            OptLevel::Conservative
        );
        let d = TokenOptimizationConfig::default();
        assert_eq!(d.get_max_tokens, 6000);
        assert_eq!(d.snippet_max_chars, 200);
        assert_eq!(d.model, "auto");
    }

    #[test]
    fn token_optimization_level_round_trips_through_yaml() {
        let (_dir, root) = write_vault("token_optimization:\n  level: aggressive\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.token_optimization.level, OptLevel::Aggressive);
        // Untouched siblings keep their defaults.
        assert_eq!(cfg.token_optimization.get_max_tokens, 6000);
    }

    #[test]
    fn token_optimization_bad_level_string_errors() {
        let (_dir, root) = write_vault("token_optimization:\n  level: yolo\n");
        let err = load_vault_config(&root).unwrap_err();
        assert!(matches!(err, CoreError::InvalidYaml(_)));
    }

    #[test]
    fn token_optimization_partial_override_keeps_other_defaults() {
        let (_dir, root) =
            write_vault("token_optimization:\n  get_max_tokens: 3000\n  read_hook: ledger\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.token_optimization.get_max_tokens, 3000);
        assert_eq!(cfg.token_optimization.read_hook, ReadHookMode::Ledger);
        // Defaults preserved for keys not overridden.
        assert_eq!(cfg.token_optimization.level, OptLevel::Conservative);
        assert_eq!(cfg.token_optimization.snippet_max_chars, 200);
        assert_eq!(
            cfg.token_optimization.strip_frontmatter,
            StripFrontmatter::Auto
        );
    }

    #[test]
    fn token_optimization_strip_frontmatter_all_variants_parse() {
        for (yaml_value, expect) in [
            ("auto", StripFrontmatter::Auto),
            ("always", StripFrontmatter::Always),
            ("never", StripFrontmatter::Never),
        ] {
            let (_dir, root) = write_vault(&format!(
                "token_optimization:\n  strip_frontmatter: {yaml_value}\n"
            ));
            let cfg = load_vault_config(&root).unwrap();
            assert_eq!(cfg.token_optimization.strip_frontmatter, expect);
        }
    }

    #[test]
    fn token_optimization_full_config_round_trips() {
        let yaml = "token_optimization:\n  \
                     level: balanced\n  \
                     get_max_tokens: 4000\n  \
                     snippet_max_chars: 150\n  \
                     strip_frontmatter: always\n  \
                     model: gpt4\n  \
                     read_hook: ledger\n";
        let (_dir, root) = write_vault(yaml);
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.token_optimization.level, OptLevel::Balanced);
        assert_eq!(cfg.token_optimization.get_max_tokens, 4000);
        assert_eq!(cfg.token_optimization.snippet_max_chars, 150);
        assert_eq!(
            cfg.token_optimization.strip_frontmatter,
            StripFrontmatter::Always
        );
        assert_eq!(cfg.token_optimization.model, "gpt4");
        assert_eq!(cfg.token_optimization.read_hook, ReadHookMode::Ledger);
    }

    #[test]
    fn token_optimization_serializes_level_as_lowercase_string() {
        // Guard against the level field ever drifting from OptLevel's
        // Display/FromStr string format (`off|conservative|balanced|aggressive`).
        let cfg = TokenOptimizationConfig {
            level: OptLevel::Balanced,
            ..TokenOptimizationConfig::default()
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        assert!(yaml.contains("level: balanced"), "{yaml}");
    }
}
