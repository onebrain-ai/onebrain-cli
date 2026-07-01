//! Shared helpers for the `search` verb group (`query` / `search` / `vsearch`
//! / `get` / `status` / `reindex`) — all backed by `onebrain_search::Engine`.
//!
//! ## Collection resolution
//!
//! The engine's on-disk state (tantivy index, vector store, redb metadata)
//! lives under `<cache_dir>/onebrain/search/<collection>/`, keyed by the
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
use onebrain_core::{load_vault_config, ResolvedVault};
use onebrain_search::engine::{short_path_hash, Engine};

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
    let engine = Engine::open(&cache_dir, &config.search.embed_model)
        .with_context(|| format!("opening search engine at {}", cache_dir.display()))?;
    Ok((engine, resolved))
}

/// Persist `search.collection = collection` into the vault's config file
/// (`onebrain.yml`, or legacy `vault.yml` if that's what's present),
/// preserving every other key. Mirrors `search_model::persist_embed_model`'s
/// read → mutate mapping → backup → atomic-write pattern:
/// `onebrain_fs::backup_config_file` is a hard precondition (no write
/// proceeds without a successful backup, or a confirmed-absent file for a
/// fresh vault).
fn persist_collection(vault_root: &Path, collection: &str) -> Result<()> {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};

    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let mut yaml: serde_yaml::Value = if text.trim().is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    };
    if !yaml.is_mapping() {
        yaml = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let mapping = yaml.as_mapping_mut().expect("normalized to mapping above");

    let search_key = serde_yaml::Value::String("search".to_string());
    let needs_replace = match mapping.get(&search_key) {
        Some(v) => !v.is_mapping(),
        None => true,
    };
    if needs_replace {
        mapping.insert(
            search_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let search = mapping
        .get_mut(&search_key)
        .and_then(|v| v.as_mapping_mut())
        .expect("search key was just ensured to be a mapping");
    search.insert(
        serde_yaml::Value::String("collection".to_string()),
        serde_yaml::Value::String(collection.to_string()),
    );

    let serialized = serde_yaml::to_string(&yaml).context("serializing updated config")?;

    // Defense-in-depth: back up the existing config before overwriting it.
    // Hard precondition — refuse the write if the backup couldn't be made.
    onebrain_fs::backup_config_file(&path)
        .with_context(|| format!("backing up {} before write", path.display()))?;

    onebrain_fs::atomic_write_text(&path, &serialized)
        .with_context(|| format!("writing {}", path.display()))
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
    use onebrain_core::{ResolvedVault, VaultRoot, VaultSource};
    use tempfile::tempdir;

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
    fn is_indexed_true_when_dir_present() {
        let dir = tempdir().unwrap();
        assert!(is_indexed(dir.path()));
    }
}
