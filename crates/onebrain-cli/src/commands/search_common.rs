//! Shared helpers for the `search` verb group (`query` / `search` / `vsearch`
//! / `get` / `status` / `reindex`) — all backed by `onebrain_search::Engine`.
//!
//! ## Collection resolution
//!
//! The engine's on-disk state (tantivy index, vector store, redb metadata)
//! lives under `<data_dir>/onebrain/search/<collection>/`, keyed by the
//! `search.collection` config value (falling back to the legacy
//! `qmd_collection` — see `onebrain_core::load_vault_config`).
//!
//! When neither is set, the collection name is **auto-generated** as
//! `<vault-dir-name>-<short-hash>` — where `<short-hash>` is the first 6 hex
//! chars of sha256 of the vault's absolute path (see
//! [`onebrain_search::engine::short_path_hash`]) — and **persisted** to
//! `onebrain.yml` under `search.collection`. This is:
//! - **deterministic** per vault path (same vault → same name across runs),
//! - **stable** once written (the persisted value is read on the next run, so
//!   moving/renaming the vault won't silently re-derive a different name and
//!   split the index),
//! - **headless-safe** (no prompt) so agents and cron "just work" on a fresh
//!   machine, and
//! - **visible + editable** in `onebrain.yml` afterwards.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use onebrain_core::{
    load_vault_config, load_vault_config_at, RerankerConfig, ResolvedVault, VaultRoot,
};
use onebrain_search::engine::{short_path_hash, Engine, RerankSettings, DEFAULT_RERANK_MIN_SCORE};

use crate::vault_ctx;

/// Base directory for all native-search state:
/// `<data_dir>/onebrain/search/`. Shares the same root as
/// `crate::migration::default_state_dir` (`~/Library/Application Support/onebrain/`
/// on macOS · `$XDG_DATA_HOME/onebrain/` or `~/.local/share/onebrain/` on Linux ·
/// `%APPDATA%\onebrain\` on Windows — relocated out of the OS-purgeable cache
/// dir in v3.4.5, issue #114), honouring the same `ONEBRAIN_CACHE_DIR` test
/// override so search-engine tests can redirect index state into a tempdir
/// exactly like the migration-notice tests do.
pub fn search_cache_root() -> PathBuf {
    crate::migration::default_state_dir().join("search")
}

/// Resolve the vault and its search collection name, without opening the
/// engine. When no collection is configured (neither `search.collection` nor
/// the legacy `qmd_collection`), auto-generate `<dir>-<hash>` and persist it
/// to `onebrain.yml` so it is stable and editable — see the module doc.
///
/// Returns `Some(name)` in the normal case. It only returns `None` if the
/// config had no collection AND persisting the generated name failed (the
/// generated name is still returned to the caller in that case — the `None`
/// path is unreachable in practice; kept `Option` for signature stability).
pub fn resolve_collection(vault_flag: Option<PathBuf>) -> Result<(ResolvedVault, Option<String>)> {
    let resolved = vault_ctx::require(vault_flag)?;
    let collection = collection_for(&resolved)?;
    Ok((resolved, Some(collection)))
}

/// The effective collection name for a resolved vault: the configured value
/// if present, otherwise a freshly generated `<dir>-<hash>` name persisted to
/// `onebrain.yml`. Deterministic and headless-safe (never prompts).
///
/// The configured value comes from `search.collection`, falling back to the
/// deprecated top-level `qmd_collection` (v3.3 and earlier) via
/// [`load_vault_config`]. That legacy read-fallback stays so vaults that
/// haven't migrated keep working, but it is transitional: `onebrain doctor
/// --fix` migrates `qmd_collection` → `search.collection` and removes the old
/// key (see `LegacyQmdCollectionCheck`).
pub fn collection_for(resolved: &ResolvedVault) -> Result<String> {
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    if let Some(existing) = config.search.collection {
        return Ok(existing);
    }

    let name = generate_collection_name(resolved.root.name(), resolved.root.as_path());
    persist_collection(resolved.root.as_path(), &name)
        .with_context(|| format!("persisting search.collection = {name}"))?;
    Ok(name)
}

/// Read-only collection resolver for surfaces that must NOT mutate config
/// (the webui `GET /api/vault/search`). Returns the configured value
/// (`search.collection`, else the legacy `qmd_collection`), or the
/// deterministic auto-generated `<dir>-<hash>` name **without persisting**
/// it. The generated name matches what [`collection_for`] would later write,
/// so a webui search before the first `search reindex` uses the same
/// collection reindex will adopt.
pub fn collection_name_readonly(vault_root: &Path) -> Result<String> {
    let config = load_vault_config_at(vault_root).context("load vault config")?;
    if let Some(existing) = config.search.collection {
        return Ok(existing);
    }
    let root = VaultRoot::from_path(vault_root)
        .with_context(|| format!("resolving vault root at {}", vault_root.display()))?;
    Ok(generate_collection_name(root.name(), vault_root))
}

/// Build the auto-generated collection name `<dir>-<short-hash>` for a vault
/// whose directory name is `dir_name` and absolute path is `abs_path`.
fn generate_collection_name(dir_name: String, abs_path: &Path) -> String {
    format!("{dir_name}-{}", short_path_hash(abs_path))
}

/// Collection's on-disk cache dir: `<search_cache_root>/<collection>/`.
pub fn collection_cache_dir(collection: &str) -> PathBuf {
    search_cache_root().join(collection)
}

/// Resolve the vault and open the engine rooted at its collection's cache
/// dir. Returns the resolved vault (for building the envelope's `vault`
/// field) alongside the opened engine.
///
/// The collection name is resolved via [`collection_for`]: configured value
/// if present, else an auto-generated `<dir>-<hash>` persisted to
/// `onebrain.yml`. This makes a fresh `onebrain search reindex` / `status`
/// "just work" with no manual config and no prompt.
pub fn open_engine(vault_flag: Option<PathBuf>) -> Result<(Engine, ResolvedVault)> {
    let resolved = vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let collection = collection_for(&resolved)?;

    let cache_dir = collection_cache_dir(&collection);
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model)
        .map_err(|e| map_engine_open_error(e, &cache_dir))?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    engine.set_rerank_settings(rerank_settings_from_config(&config.search.reranker));
    Ok((engine, resolved))
}

/// Map the config's `search.reranker` block to the engine-facing
/// [`RerankSettings`] — the CLI is the only layer that reads config files, so
/// this is the single seam where `onebrain.yml` values become the engine's
/// rerank knobs. `min_score` falls back to the engine's calibrated
/// [`DEFAULT_RERANK_MIN_SCORE`] when the config leaves it unset.
///
/// Drift guard: an absent `search.reranker` block must map to EXACTLY
/// `RerankSettings::default()` — see the test below. If the two defaults
/// ever diverge (one crate's default changes without the other), this
/// mapping silently stops being a no-op for the common case.
pub(crate) fn rerank_settings_from_config(cfg: &RerankerConfig) -> RerankSettings {
    RerankSettings {
        enabled: cfg.enabled,
        model: cfg.model.clone(),
        candidates: cfg.candidates,
        min_score: cfg.min_score.unwrap_or(DEFAULT_RERANK_MIN_SCORE),
    }
}

/// Turn an `Engine::open` failure into the right typed CLI error. The redb
/// single-process lock case (already classified as `onebrain_search`'s
/// `EngineBusy`) becomes `CoreError::EngineBusy`, which the envelope + exit
/// mapping render as `E_ENGINE_BUSY` / exit 77. Every other failure keeps the
/// prior behaviour: a plain `.context(...)`-wrapped opaque error. This is the
/// single choke point so `query`, `vsearch`, `get`, and a full `reindex` all
/// report lock contention uniformly (v3.4.6).
pub(crate) fn map_engine_open_error(err: anyhow::Error, cache_dir: &Path) -> anyhow::Error {
    if onebrain_search::error::is_engine_busy(&err) {
        return anyhow::Error::new(onebrain_core::CoreError::EngineBusy(format!(
            "index at {} is locked by another process (e.g. the `onebrain mcp` server) — \
             retry once it releases the lock",
            cache_dir.display()
        )));
    }
    err.context(format!("opening search engine at {}", cache_dir.display()))
}

/// Turn a warm-daemon request failure into the right typed CLI error — the
/// daemon-path analogue of [`map_engine_open_error`]. When the daemon 503s
/// because it holds no engine (another process owns the redb lock), the client
/// classifies the error as `onebrain_search`'s `EngineBusy`; convert it to the
/// same `CoreError::EngineBusy` (E_ENGINE_BUSY / exit 77) the direct path uses,
/// so `query` / `get` / `reindex` report contention identically whether they
/// went direct or through the daemon. Any other daemon error keeps its verb
/// context (an opaque `E_INTERNAL`).
pub(crate) fn map_daemon_error(err: anyhow::Error, ctx: &'static str) -> anyhow::Error {
    if onebrain_search::error::is_engine_busy(&err) {
        return anyhow::Error::new(onebrain_core::CoreError::EngineBusy(
            "the search index is held by another process (e.g. an `onebrain mcp` \
             server, possibly from before an upgrade) — retry once it releases the engine"
                .to_string(),
        ));
    }
    err.context(ctx)
}

/// Persist `search.collection = collection` into the vault's config file
/// (`onebrain.yml`, or legacy `vault.yml` if that's what's present),
/// preserving every other key. Thin wrapper over the shared
/// `onebrain_fs::persist_search_key` (read → mutate `search.*` → backup →
/// atomic-write; `backup_config_file` is a hard precondition), which
/// `search_model::persist_embed_model` also uses.
fn persist_collection(vault_root: &Path, collection: &str) -> Result<()> {
    onebrain_fs::persist_search_key(vault_root, "collection", collection)
}

/// `true` when the collection's cache dir already exists on disk. Used as a
/// read-only pre-check by `session_init::native_pending` and `doctor` to skip
/// opening the engine (which would create the dir as a side effect) on a
/// never-indexed vault. Note: `search status` does NOT use this — it derives
/// its `indexed` field from `doc_count > 0`, since a cache dir can exist while
/// holding zero docs. Pure path check — never touches the engine, so it never
/// triggers a model download.
pub fn is_indexed(cache_dir: &Path) -> bool {
    cache_dir.is_dir()
}

/// Total on-disk bytes of the index itself under `cache_dir`: the `tantivy/`
/// and `vectors/` directories plus the `engine.redb` file. The downloaded
/// `models--*` dirs are deliberately excluded — those are the model's size,
/// reported separately. Returns `None` when none of the three exist yet (no
/// index). Pure fs; reuses the shared `onebrain_search::embed::dir_size_bytes`.
pub fn index_size_bytes(cache_dir: &Path) -> Option<u64> {
    use onebrain_search::embed::dir_size_bytes;
    let mut total = 0u64;
    let mut any = false;
    for sub in ["tantivy", "vectors"] {
        let dir = cache_dir.join(sub);
        if dir.is_dir() {
            any = true;
            total += dir_size_bytes(&dir);
        }
    }
    if let Ok(meta) = std::fs::metadata(cache_dir.join("engine.redb")) {
        if meta.is_file() {
            any = true;
            total += meta.len();
        }
    }
    any.then_some(total)
}

/// `true` when the vault's config file physically contains a
/// `search.embed_model` key. Distinct from `config.search.embed_model`, which
/// serde fills with the `multilingual-e5-small` default even when the key is
/// absent — so this raw-YAML check is the only way to tell "user has chosen a
/// model" from "user hasn't touched it yet".
///
/// Missing config / unreadable file / non-mapping YAML all count as "not
/// present" (a fresh vault hasn't chosen a model). Pure `std::fs` + parse.
pub fn embed_model_key_present(vault_root: &Path) -> bool {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};

    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(yaml) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return false;
    };
    yaml.get("search")
        .and_then(|s| s.get("embed_model"))
        .is_some()
}

/// `true` when at least one registry model's `models--*` dir exists under the
/// collection cache dir. Pure `std::fs` scan — never downloads.
pub fn any_model_downloaded(cache_dir: &Path) -> bool {
    use onebrain_search::embed::{model_download_status, model_registry};
    model_registry()
        .iter()
        .any(|m| model_download_status(m, cache_dir).downloaded)
}

/// Whether the vault has *no* embedding model chosen yet: neither a
/// physically-present `search.embed_model` key NOR any downloaded model. This
/// is the gate for the first-run "pick a model before we download" prompt in
/// `search reindex` — a `false` here means the user has already committed to a
/// model (explicitly configured or already on disk), so no prompt is shown.
pub fn model_not_chosen(vault_root: &Path, cache_dir: &Path) -> bool {
    !embed_model_key_present(vault_root) && !any_model_downloaded(cache_dir)
}

/// Reconcile a stale model choice after a cache purge (or a manual model
/// deletion): when `search.embed_model` names a model that is no longer on
/// disk, drop the key so the vault reverts to "no model chosen". `search
/// reindex` then re-prompts for a selection and the UI stops marking a
/// not-downloaded model as active.
///
/// Race-safe + best-effort — does nothing when a reindex is in progress (it
/// downloads the model, so the dir is legitimately absent mid-run), when no
/// model key is committed, or when the configured model IS downloaded. A write
/// failure is swallowed: reconciliation is advisory, never fatal. Call only
/// from mutating surfaces (`search model` TUI, `search reindex`) — NOT the
/// read-only `search status` / `search model list` paths.
pub(crate) fn reconcile_missing_model(vault_root: &Path, cache_dir: &Path, embed_model: &str) {
    use onebrain_search::embed::{model_download_status, model_registry};

    if read_reindex_progress(cache_dir).is_some() || !embed_model_key_present(vault_root) {
        return;
    }
    let downloaded = model_registry()
        .iter()
        .find(|m| m.name == embed_model)
        .is_some_and(|m| model_download_status(m, cache_dir).downloaded);
    if !downloaded {
        let _ = onebrain_fs::remove_search_key(vault_root, "embed_model");
    }
}

/// Human-readable byte size (`471 MB`, `2.2 GB`, `12 B`) — whole-number MB/KB,
/// one decimal for GB. Shared by `search status` and `search model`
/// (`search reindex` keeps a local one-decimal-MB variant: its size DELTAS
/// like `+1.2 MB` need the precision).
pub(crate) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Warm-daemon routing (v3.4.6 Track 2c)
// ─────────────────────────────────────────────────────────────────────────
//
// redb is single-process: exactly ONE process (the warm daemon) may open a
// collection's engine at a time (ADR 0023). So when a daemon is already
// running — typically because an `onebrain mcp` session holds the engine — the
// CLI search verbs route their request through it (over the existing localhost
// HTTP surface) instead of opening a second engine and hitting the redb lock
// (`E_ENGINE_BUSY`). When no daemon is running there's no contention, so the
// verbs open the engine directly (today's path), and T1's honest `EngineBusy`
// still applies if the lock is genuinely held by something else.

use crate::commands::daemon_client::{self, DaemonHandle};

/// `true` when daemon routing is turned OFF via `ONEBRAIN_NO_DAEMON` (any
/// non-empty value). The escape hatch: with it set, every search verb / hook
/// opens the engine directly and never discovers or starts a daemon — the
/// pre-daemon behaviour. Used by tests that must exercise the direct-open /
/// honest-`EngineBusy` path deterministically, and as an operator kill-switch.
pub(crate) fn daemon_routing_disabled() -> bool {
    std::env::var_os("ONEBRAIN_NO_DAEMON").is_some_and(|v| !v.is_empty())
}

/// Discover an already-running daemon and return a handle ONLY when it is
/// **live, at our version, AND bound to the vault the CLI is targeting**;
/// otherwise `None` (the caller opens the engine directly).
///
/// This is the CLI's **PASSIVE** routing check — it uses
/// [`daemon_client::discover_matching`], NOT #170's active
/// [`daemon_client::discover`]/[`ensure_running`]. The distinction is
/// load-bearing:
/// - `discover`/`ensure_running` are the MCP path's ACTIVE lifecycle owner:
///   on a version- or vault-mismatch they STOP + restart the daemon for the
///   caller's vault (one daemon per machine; switching vaults restarts it).
/// - `onebrain search …` must NEVER kill a daemon serving another vault — that
///   would disrupt a live MCP session. So the CLI reads `daemon.json`'s stored
///   canonical `vault` and, on ANY mismatch (wrong vault, wrong version, dead,
///   or a pre-vault-field record), simply routes to DIRECT open — it never
///   restarts and never spawns.
///
/// The result: a plain `onebrain search …` routes to the warm daemon only when
/// it already serves this exact vault (the contention case an MCP session
/// creates), and otherwise opens the engine directly so T1's honest
/// `E_ENGINE_BUSY` still applies if the lock is genuinely held.
pub(crate) fn route_to_daemon(resolved: &ResolvedVault) -> Option<DaemonHandle> {
    if daemon_routing_disabled() {
        return None;
    }
    match daemon_client::discover_matching(Some(resolved.root.as_path())) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::debug!(error = %e, "daemon discover_matching failed; opening engine directly");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_daemon_error_engine_busy_becomes_core_engine_busy() {
        // A daemon 503 arrives as `onebrain_search::EngineBusy` in the chain
        // (via `daemon_client::daemon_engine_busy`); `map_daemon_error` must turn
        // it into `CoreError::EngineBusy` so the verb exits 77 / E_ENGINE_BUSY —
        // exactly like the direct path's `map_engine_open_error`.
        let busy = anyhow::Error::new(onebrain_search::error::EngineBusy).context("via daemon");
        let mapped = map_daemon_error(busy, "route search through warm daemon");
        assert!(
            matches!(
                mapped.downcast_ref::<onebrain_core::CoreError>(),
                Some(onebrain_core::CoreError::EngineBusy(_))
            ),
            "an engine-busy daemon error must become CoreError::EngineBusy, got: {mapped:#}"
        );
    }

    #[test]
    fn map_daemon_error_other_keeps_verb_context() {
        // A non-busy daemon error keeps its opaque verb context (an E_INTERNAL),
        // and must NOT be classified as engine-busy.
        let other = anyhow::anyhow!("some genuine daemon failure");
        let mapped = map_daemon_error(other, "route `search get` through warm daemon");
        assert!(
            !onebrain_search::error::is_engine_busy(&mapped),
            "a non-busy error must not be classified as EngineBusy"
        );
        assert!(
            format!("{mapped:#}").contains("route `search get` through warm daemon"),
            "the verb context must be preserved, got: {mapped:#}"
        );
    }

    #[test]
    fn rerank_settings_from_config_default_matches_engine_default() {
        // Drift guard: an absent `search.reranker` block (config default)
        // must map to EXACTLY `RerankSettings::default()` — if the two
        // defaults ever diverge, this test catches it.
        let mapped = rerank_settings_from_config(&onebrain_core::RerankerConfig::default());
        assert_eq!(mapped, onebrain_search::engine::RerankSettings::default());
    }

    #[test]
    fn rerank_settings_from_config_maps_every_field() {
        let cfg = onebrain_core::RerankerConfig {
            enabled: false,
            model: "onebrain-reranker-v1".to_string(),
            candidates: 42,
            min_score: Some(0.55),
        };
        let mapped = rerank_settings_from_config(&cfg);
        assert!(!mapped.enabled);
        assert_eq!(mapped.model, "onebrain-reranker-v1");
        assert_eq!(mapped.candidates, 42);
        assert_eq!(mapped.min_score, 0.55);
    }

    #[test]
    fn rerank_settings_from_config_falls_back_to_engine_default_min_score() {
        let cfg = onebrain_core::RerankerConfig {
            min_score: None,
            ..Default::default()
        };
        let mapped = rerank_settings_from_config(&cfg);
        assert_eq!(
            mapped.min_score,
            onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE
        );
    }

    #[test]
    fn format_size_renders_units() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(471 * 1024 * 1024), "471 MB");
        assert_eq!(
            format_size(2 * 1024 * 1024 * 1024 + 200 * 1024 * 1024),
            "2.2 GB"
        );
    }
    use onebrain_core::{ResolvedVault, VaultRoot, VaultSource};
    use tempfile::tempdir;

    #[test]
    fn collection_name_readonly_does_not_persist_generated_name() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let name = collection_name_readonly(dir.path()).unwrap();
        // Deterministic <dir>-<hash>, same as the persisting resolver would generate.
        assert_eq!(
            name,
            generate_collection_name(VaultRoot::from_path(dir.path()).unwrap().name(), dir.path())
        );
        // Config file must be UNCHANGED (no `collection:` written).
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(
            !yaml.contains("collection:"),
            "readonly resolver must not persist: {yaml}"
        );
    }

    #[test]
    fn collection_name_readonly_prefers_configured_value() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: explicit-name\n",
        )
        .unwrap();
        assert_eq!(
            collection_name_readonly(dir.path()).unwrap(),
            "explicit-name"
        );
    }

    /// Build a `ResolvedVault` rooted at `dir` (which must already contain an
    /// `onebrain.yml`), as the flag-resolved source.
    fn resolved_at(dir: &Path) -> ResolvedVault {
        ResolvedVault {
            root: VaultRoot::from_path(dir).unwrap(),
            source: VaultSource::Flag,
        }
    }

    #[test]
    fn collection_cache_dir_nests_under_search_root() {
        let dir = collection_cache_dir("my-vault");
        assert!(dir.ends_with("search/my-vault"));
    }

    #[test]
    fn generate_collection_name_is_dir_plus_short_hash() {
        let name = generate_collection_name("ob-1".to_string(), Path::new("/vaults/ob-1"));
        // `<dir>-<6 hex>`.
        assert!(name.starts_with("ob-1-"), "got {name}");
        let hash = name.strip_prefix("ob-1-").unwrap();
        assert_eq!(hash.len(), 6);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // Deterministic per path.
        assert_eq!(
            name,
            generate_collection_name("ob-1".to_string(), Path::new("/vaults/ob-1"))
        );
    }

    #[test]
    fn collection_for_autogenerates_and_persists_when_unset() {
        // Vault whose config has neither `search.collection` nor the legacy
        // `qmd_collection`.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let resolved = resolved_at(dir.path());

        // First call generates `<dir>-<hash>` and persists it.
        let name = collection_for(&resolved).unwrap();
        let expected = generate_collection_name(resolved.root.name(), resolved.root.as_path());
        assert_eq!(name, expected);
        assert!(name.starts_with(&format!("{}-", resolved.root.name())));

        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(
            yaml.contains(&format!("collection: {name}")),
            "persisted config should contain the generated collection:\n{yaml}"
        );
        // Other keys preserved.
        assert!(yaml.contains("inbox: 00-inbox"));

        // Second call reads the persisted value (stable, no re-derivation).
        let again = collection_for(&resolved).unwrap();
        assert_eq!(again, name);
    }

    #[test]
    fn collection_for_uses_configured_value_unchanged() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: my-explicit-name\n",
        )
        .unwrap();
        let resolved = resolved_at(dir.path());
        assert_eq!(collection_for(&resolved).unwrap(), "my-explicit-name");
    }

    #[test]
    fn collection_for_uses_legacy_qmd_collection_without_persisting() {
        // Legacy `qmd_collection` is honoured via `load_vault_config`'s
        // fallback, so no auto-generation happens.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "qmd_collection: legacy-name\n",
        )
        .unwrap();
        let resolved = resolved_at(dir.path());
        assert_eq!(collection_for(&resolved).unwrap(), "legacy-name");
    }

    #[test]
    fn is_indexed_false_when_dir_absent() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(!is_indexed(&missing));
    }

    #[test]
    fn index_size_bytes_sums_index_dirs_and_redb_excluding_models() {
        let dir = tempdir().unwrap();
        // No index yet → None.
        assert!(index_size_bytes(dir.path()).is_none());

        std::fs::create_dir_all(dir.path().join("tantivy")).unwrap();
        std::fs::write(dir.path().join("tantivy/meta.json"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(dir.path().join("vectors")).unwrap();
        std::fs::write(dir.path().join("vectors/data.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(dir.path().join("engine.redb"), vec![0u8; 44]).unwrap();
        // A model dir must NOT be counted toward the index size.
        let model = dir.path().join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("model.onnx"), vec![0u8; 9999]).unwrap();

        assert_eq!(index_size_bytes(dir.path()), Some(344));
    }

    #[test]
    fn is_indexed_true_when_dir_present() {
        let dir = tempdir().unwrap();
        assert!(is_indexed(dir.path()));
    }

    #[test]
    fn embed_model_key_present_true_when_configured() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  embed_model: bge-m3\n",
        )
        .unwrap();
        assert!(embed_model_key_present(dir.path()));
    }

    #[test]
    fn embed_model_key_present_false_when_absent() {
        let dir = tempdir().unwrap();
        // `search:` block with a collection but no embed_model.
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: t\n",
        )
        .unwrap();
        assert!(!embed_model_key_present(dir.path()));
    }

    #[test]
    fn embed_model_key_present_false_when_no_config_file() {
        let dir = tempdir().unwrap();
        assert!(!embed_model_key_present(dir.path()));
    }

    #[test]
    fn embed_model_key_present_false_on_unparseable_yaml() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "search: : : not yaml\n").unwrap();
        assert!(!embed_model_key_present(dir.path()));
    }

    #[test]
    fn any_model_downloaded_false_on_empty_cache() {
        let dir = tempdir().unwrap();
        assert!(!any_model_downloaded(dir.path()));
    }

    #[test]
    fn any_model_downloaded_true_when_one_present() {
        use onebrain_search::embed::model_registry;
        let dir = tempdir().unwrap();
        let m = &model_registry()[0];
        let mdir = dir.path().join(m.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 16]).unwrap();
        assert!(any_model_downloaded(dir.path()));
    }

    #[test]
    fn model_not_chosen_true_on_fresh_vault() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        assert!(model_not_chosen(vault.path(), cache.path()));
    }

    #[test]
    fn model_not_chosen_false_when_key_configured() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  embed_model: bge-m3\n",
        )
        .unwrap();
        assert!(!model_not_chosen(vault.path(), cache.path()));
    }

    #[test]
    fn model_not_chosen_false_when_model_downloaded() {
        use onebrain_search::embed::model_registry;
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        // No embed_model key, but a model already on disk → already committed.
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "folders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        let m = &model_registry()[0];
        let mdir = cache.path().join(m.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 16]).unwrap();
        assert!(!model_not_chosen(vault.path(), cache.path()));
    }

    #[test]
    fn reconcile_clears_a_stale_model_choice() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap(); // empty → the model isn't downloaded
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: c\n  embed_model: bge-m3\n",
        )
        .unwrap();
        reconcile_missing_model(vault.path(), cache.path(), "bge-m3");
        assert!(
            !embed_model_key_present(vault.path()),
            "a configured-but-undownloaded model must be cleared"
        );
        let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("collection: c"), "{yaml}");
    }

    #[test]
    fn reconcile_is_noop_when_no_model_committed() {
        let vault = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: c\n",
        )
        .unwrap();
        reconcile_missing_model(vault.path(), cache.path(), "multilingual-e5-small");
        let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("collection: c"), "{yaml}");
    }

    // ── Warm-daemon routing (Track 2c) ─────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn daemon_routing_disabled_honours_env() {
        // Empty value → routing enabled; any non-empty value → disabled. Each
        // guard is scoped so only one holds the (non-reentrant) env lock at a
        // time.
        {
            let _enabled = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "");
            assert!(!daemon_routing_disabled(), "empty value → enabled");
        }
        {
            let _disabled = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "1");
            assert!(daemon_routing_disabled(), "non-empty value → disabled");
        }
    }

    #[cfg(unix)]
    #[test]
    fn route_to_daemon_none_when_disabled() {
        // With routing disabled, `route_to_daemon` short-circuits to None even
        // before any discovery — the direct-open / honest-EngineBusy path.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: rd-disabled\n",
        )
        .unwrap();
        let _set = crate::test_env::set_var("ONEBRAIN_NO_DAEMON", "1");
        let resolved = resolved_at(dir.path());
        assert!(route_to_daemon(&resolved).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn route_to_daemon_none_when_no_daemon_running() {
        // Routing enabled but no daemon.json under HOME → discover() is None →
        // route_to_daemon is None (fall back to direct open). HOME is redirected
        // to an empty tempdir so we never touch the real ~/.onebrain.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: rd-nodaemon\n",
        )
        .unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("")),
        ]);
        let resolved = resolved_at(dir.path());
        assert!(route_to_daemon(&resolved).is_none());
    }

    /// PASSIVE, no-restart guard (Track 2c): a `daemon.json` bound to vault A,
    /// while the CLI resolves vault B, routes to DIRECT open (None) AND leaves
    /// the vault-A record untouched — the CLI must never stop/restart a daemon
    /// serving another vault (that would disrupt a live MCP session). Decision-
    /// level: no live server needed, since `discover_matching` rejects on the
    /// vault mismatch BEFORE any liveness probe or restart.
    #[cfg(unix)]
    #[test]
    fn route_to_daemon_wrong_vault_goes_direct_and_leaves_daemon_untouched() {
        use crate::commands::daemon_client::{canonical_vault_id, DaemonInfo};

        // Two distinct real vault dirs: A (the daemon's) and B (the CLI's).
        let vault_a = tempdir().unwrap();
        let vault_b = tempdir().unwrap();
        std::fs::write(
            vault_b.path().join("onebrain.yml"),
            "search:\n  collection: rd-vault-b\n",
        )
        .unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("")),
        ]);

        // A daemon.json bound to vault A (port 1 = connection refused; never
        // probed here — the vault check short-circuits first).
        let path = crate::commands::daemon_client::discovery_path().unwrap();
        DaemonInfo {
            port: 1,
            token: "x".repeat(20),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vault: canonical_vault_id(vault_a.path()),
        }
        .write(&path)
        .unwrap();

        // CLI resolves vault B → routes DIRECT (None), no restart.
        let resolved = resolved_at(vault_b.path());
        assert!(
            route_to_daemon(&resolved).is_none(),
            "wrong-vault daemon → direct open"
        );
        // The vault-A record is UNTOUCHED (passive path never removes/stops it).
        let still = DaemonInfo::read(&path).unwrap().expect("record preserved");
        assert_eq!(
            still.vault,
            canonical_vault_id(vault_a.path()),
            "CLI must not disturb a daemon serving another vault"
        );
    }
}

/// Path of the live reindex-progress marker inside a collection cache dir.
/// Written by an in-flight `search reindex`, read by `search status` from
/// any other process, removed when the reindex finishes.
pub(crate) fn reindex_progress_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("reindex-progress.json")
}

/// A live reindex's `(done, total)` doc counts.
#[derive(
    Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub(crate) struct ReindexLiveProgress {
    pub done: usize,
    pub total: usize,
}

/// Read the live progress marker, if a reindex appears to be running.
/// Fresh = touched within the last 30 minutes — a crashed reindex leaves the
/// file behind, so stale markers are ignored (and best-effort removed).
pub(crate) fn read_reindex_progress(cache_dir: &Path) -> Option<ReindexLiveProgress> {
    let path = reindex_progress_path(cache_dir);
    let meta = std::fs::metadata(&path).ok()?;
    let fresh = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|e| e.as_secs() < 30 * 60);
    if !fresh {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
}

#[cfg(test)]
mod live_progress_tests {
    use super::*;

    #[test]
    fn read_reindex_progress_roundtrip_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_reindex_progress(dir.path()).is_none(), "no marker yet");

        let marker = reindex_progress_path(dir.path());
        std::fs::write(&marker, r#"{"done":457,"total":761}"#).unwrap();
        let p = read_reindex_progress(dir.path()).unwrap();
        assert_eq!(
            p,
            ReindexLiveProgress {
                done: 457,
                total: 761
            }
        );

        std::fs::write(&marker, "not json").unwrap();
        assert!(
            read_reindex_progress(dir.path()).is_none(),
            "garbage → None"
        );
    }
}
