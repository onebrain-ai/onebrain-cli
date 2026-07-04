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
use onebrain_core::{load_vault_config, load_vault_config_at, ResolvedVault, VaultRoot};
use onebrain_search::engine::{short_path_hash, Engine};

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
        .with_context(|| format!("opening search engine at {}", cache_dir.display()))?;
    engine.set_exclude_patterns(config.search.exclude.clone());
    Ok((engine, resolved))
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

/// `true` when the collection's cache dir already exists on disk (used by
/// `status`'s `indexed` field). Pure path check — never touches the engine,
/// so it never triggers a model download.
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

#[cfg(test)]
mod tests {
    use super::*;
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
