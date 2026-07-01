//! Shared helpers for the `search` verb group (`query` / `search` / `vsearch`
//! / `get` / `status` / `reindex`) — all backed by `onebrain_search::Engine`.
//!
//! ## Collection resolution
//!
//! The engine's on-disk state (tantivy index, vector store, redb metadata)
//! lives under `<cache_dir>/onebrain/search/<collection>/`, keyed by the
//! `search.collection` config value (falling back to the legacy
//! `qmd_collection` — see `onebrain_core::load_vault_config`). When neither
//! is set, `open_engine` errors with a clear message rather than silently
//! picking an implicit default: the collection name is also the on-disk
//! directory name, so an implicit default (e.g. the vault's directory name)
//! would silently split a vault's index in two if the vault were ever moved
//! or renamed — better to make the user set it once, explicitly, in
//! `onebrain.yml`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use onebrain_core::{load_vault_config, ResolvedVault};
use onebrain_search::engine::Engine;

use crate::vault_ctx;

/// Base cache directory for all native-search state:
/// `<cache_dir>/onebrain/search/`. Shares the same root as
/// `crate::migration::default_state_dir` (`~/Library/Caches/onebrain/` on
/// macOS · `$XDG_CACHE_HOME/onebrain/` or `~/.cache/onebrain/` on Linux ·
/// `%LOCALAPPDATA%\onebrain\` on Windows), honouring the same
/// `ONEBRAIN_CACHE_DIR` test override so search-engine tests can redirect
/// index state into a tempdir exactly like the migration-notice tests do.
pub fn search_cache_root() -> PathBuf {
    crate::migration::default_state_dir().join("search")
}

/// Resolve the vault + config's search collection name, without opening the
/// engine. Used by `status` so it can report `collection` / `cache_dir` even
/// when the collection is unset (engine-opening verbs error instead).
pub fn resolve_collection(vault_flag: Option<PathBuf>) -> Result<(ResolvedVault, Option<String>)> {
    let resolved = vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    Ok((resolved, config.search.collection))
}

/// Collection's on-disk cache dir: `<search_cache_root>/<collection>/`.
pub fn collection_cache_dir(collection: &str) -> PathBuf {
    search_cache_root().join(collection)
}

/// Resolve the vault, require a configured `search.collection`, and open
/// the engine rooted at its cache dir. Returns the resolved vault (for
/// building the envelope's `vault` field) alongside the opened engine.
///
/// Errors with a clear, actionable message when `search.collection` is
/// unset — see the module doc for why no implicit default is derived.
pub fn open_engine(vault_flag: Option<PathBuf>) -> Result<(Engine, ResolvedVault)> {
    let resolved = vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    let Some(collection) = config.search.collection else {
        bail!(
            "❌ no search collection configured\n\
             💡 set `search.collection` in onebrain.yml (or run `onebrain init`), \
             then run `onebrain search reindex`"
        );
    };

    let cache_dir = collection_cache_dir(&collection);
    let engine = Engine::open(&cache_dir, &config.search.embed_model)
        .with_context(|| format!("opening search engine at {}", cache_dir.display()))?;
    Ok((engine, resolved))
}

/// `true` when the collection's cache dir already exists on disk (used by
/// `status`'s `indexed` field). Pure path check — never touches the engine,
/// so it never triggers a model download.
pub fn is_indexed(cache_dir: &Path) -> bool {
    cache_dir.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn collection_cache_dir_nests_under_search_root() {
        let dir = collection_cache_dir("my-vault");
        assert!(dir.ends_with("search/my-vault"));
    }

    #[test]
    fn is_indexed_false_when_dir_absent() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(!is_indexed(&missing));
    }

    #[test]
    fn is_indexed_true_when_dir_present() {
        let dir = tempdir().unwrap();
        assert!(is_indexed(dir.path()));
    }
}
