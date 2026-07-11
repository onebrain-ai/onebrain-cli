//! `CollectionLayout` — split cache layout with per-artifact fallback
//! resolution.
//!
//! Historically a search collection's cache directory held everything flat at
//! its root: the `tantivy/` BM25 index, the `vectors/` flat vector store,
//! `engine.redb` metadata, and hf-hub's `models--org--name` embedding-model
//! dirs, all side by side. This module splits that into two subdirectories —
//! `<root>/models/` (hf-hub cache base; the `models--org--name` layout inside
//! is hf-hub's own and untouched) and `<root>/index/` (`tantivy/`, `vectors/`,
//! `engine.redb`) — while staying safe for collections that haven't migrated
//! yet, and for the moment mid-migration when only some artifacts moved.
//!
//! # Design invariants
//!
//! - **Per-artifact fallback resolution.** Every read path
//!   ([`CollectionLayout::model_dir`], [`CollectionLayout::index_artifact`])
//!   resolves each artifact independently: new location if present, else
//!   legacy root. A half-migrated collection (some artifacts moved, some
//!   not — including a "split-brain" model cache where one `models--*` dir
//!   moved and another is still at the root) always works — nothing needs to
//!   wait for a full migration before it can be read.
//!   [`CollectionLayout::models_base`] is the one deliberate exception: it is
//!   the hf-hub DOWNLOAD base (always the new `<root>/models`), for write
//!   paths that run after [`CollectionLayout::migrate`] — never use it to
//!   probe whether a specific model is on disk.
//! - **`migrate()` is per-entry, idempotent, and race-safe.** It walks the
//!   root and `fs::rename`s each recognized entry into its new home. If the
//!   target already exists (another process/thread migrated it first, or a
//!   previous partial run already moved it), that entry is left alone — no
//!   error, no double-move. Running `migrate()` again on an already-migrated
//!   collection is a no-op.
//! - **Read-only consumers never mutate.** [`CollectionLayout::detect`],
//!   [`CollectionLayout::models_base`], [`CollectionLayout::index_artifact`],
//!   [`CollectionLayout::models_size_bytes`], and
//!   [`CollectionLayout::index_size_bytes`] only ever read the filesystem;
//!   only [`CollectionLayout::migrate`] writes.
//! - **Size functions count each artifact wherever it actually lives**, so
//!   the numbers they report are correct even mid-migration.

use std::path::{Path, PathBuf};

/// The index artifact names this module knows how to relocate. Exported so
/// every consumer that needs to enumerate index artifacts (size accounting,
/// `--force` wipe) shares this ONE list instead of a private duplicate that
/// would silently miss a future 4th artifact (issue #224).
pub const INDEX_ARTIFACTS: [&str; 3] = ["tantivy", "vectors", "engine.redb"];

/// Where a collection's on-disk cache currently stands relative to the
/// `models/` + `index/` split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheLayoutState {
    /// Fully on the new layout: no legacy artifact sits at the collection
    /// root (this also covers a fresh, empty collection).
    Split,
    /// Fully on the old layout: every artifact that exists is at the
    /// collection root, and none has a new-location counterpart yet.
    Legacy,
    /// Some artifacts have moved to the new layout, some haven't.
    Partial,
}

/// Resolves and migrates a single search collection's cache directory
/// between the legacy flat layout and the split `models/` + `index/` layout.
///
/// See the module docs for the invariants this type upholds.
pub struct CollectionLayout {
    root: PathBuf,
}

impl CollectionLayout {
    /// Wrap a collection's cache root directory. `root` need not exist yet —
    /// every method here tolerates a missing or empty directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The collection's cache root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether any legacy artifact (a `models--*` dir or one of
    /// [`INDEX_ARTIFACTS`]) currently sits directly at `root`.
    fn legacy_entries(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(&self.root) else {
            return found;
        };
        for entry in read_dir.flatten() {
            let name = entry.file_name();
            let n = name.to_string_lossy();
            if n.starts_with("models--") || INDEX_ARTIFACTS.contains(&n.as_ref()) {
                found.push(entry.path());
            }
        }
        found
    }

    /// Whether any artifact already lives at the new location: at least one
    /// `models--*` dir under `<root>/models/`, or at least one of
    /// [`INDEX_ARTIFACTS`] under `<root>/index/`.
    fn has_new_location_artifact(&self) -> bool {
        let models_dir = self.root.join("models");
        if let Ok(read_dir) = std::fs::read_dir(&models_dir) {
            for entry in read_dir.flatten() {
                if entry.file_name().to_string_lossy().starts_with("models--") {
                    return true;
                }
            }
        }
        let index_dir = self.root.join("index");
        INDEX_ARTIFACTS
            .iter()
            .any(|name| index_dir.join(name).exists())
    }

    /// Detect the collection's current migration state.
    ///
    /// A legacy artifact present at `root` means the collection is at least
    /// partly on the old layout: `Partial` if a new-location artifact also
    /// exists anywhere, else fully `Legacy`. No legacy artifact at `root`
    /// (including a fresh, empty collection) means `Split`.
    pub fn detect(&self) -> CacheLayoutState {
        if self.legacy_entries().is_empty() {
            CacheLayoutState::Split
        } else if self.has_new_location_artifact() {
            CacheLayoutState::Partial
        } else {
            CacheLayoutState::Legacy
        }
    }

    /// The hf-hub DOWNLOAD cache base directory: always `<root>/models`.
    /// hf-hub owns the `models--org--name` layout inside this directory.
    ///
    /// This is a **write-path** resolver for callers that run after
    /// [`CollectionLayout::migrate`] (the engine's lazy embedder/reranker
    /// construction, the model TUI's forced downloads), so fresh downloads
    /// always land in the new location. It deliberately does NOT fall back to
    /// the legacy root: a single base for ALL models cannot represent a
    /// split-brain cache (model A still legacy at root, model B already under
    /// `models/`) — any one choice resolves at least one model to the wrong
    /// place. Read-only "is this model downloaded?" probes must therefore use
    /// the per-model [`CollectionLayout::model_dir`] instead.
    ///
    /// Accepted tradeoff: if `migrate()` partially failed and left a model
    /// stuck at the legacy root, a download through this base re-fetches it
    /// into `models/` rather than reusing the stranded copy — wasteful but
    /// correct, and self-healing (the next `migrate()` retry skips the entry
    /// because the target now exists).
    pub fn models_base(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Resolve one model's hf-hub cache dir (`models--org--name`) to wherever
    /// it actually lives, PER MODEL: `<root>/models/<name>` if present, else
    /// the legacy `<root>/<name>` if present, else the new location (where a
    /// future download will put it). This is the read-probe counterpart of
    /// [`CollectionLayout::index_artifact`] — a split-brain cache (one model
    /// migrated, another still legacy) resolves each model correctly, which a
    /// single shared base cannot do.
    pub fn model_dir(&self, cache_dir_name: &str) -> PathBuf {
        let new_path = self.root.join("models").join(cache_dir_name);
        if new_path.exists() {
            return new_path;
        }
        let legacy_path = self.root.join(cache_dir_name);
        if legacy_path.exists() {
            return legacy_path;
        }
        new_path
    }

    /// Resolve an index artifact (`"tantivy"`, `"vectors"`, or
    /// `"engine.redb"`) to wherever it actually lives: `<root>/index/<name>`
    /// if present, else the legacy `<root>/<name>`. For a fresh collection
    /// (neither exists yet) resolves to the new location.
    pub fn index_artifact(&self, name: &str) -> PathBuf {
        let new_path = self.root.join("index").join(name);
        if new_path.exists() {
            return new_path;
        }
        let legacy_path = self.root.join(name);
        if legacy_path.exists() {
            return legacy_path;
        }
        new_path
    }

    /// Migrate every legacy artifact at `root` into the split layout.
    ///
    /// `models/` and `index/` are created unconditionally (even for a fresh
    /// or already-`Split` collection), so after a successful `migrate()` the
    /// parent of every [`CollectionLayout::index_artifact`] resolution is
    /// guaranteed to exist — consumers like redb's `Database::create` do NOT
    /// create parent directories themselves.
    ///
    /// Per-entry `fs::rename`, so each move is atomic on the same volume.
    /// Idempotent: entries already migrated are skipped, and running this on
    /// an already-`Split` collection is a no-op. Race-safe: if the target
    /// already exists (a concurrent migration/open beat this one to that
    /// entry), the source is left alone rather than erroring or clobbering.
    ///
    /// # Errors and partial failure
    ///
    /// If a rename fails mid-loop (e.g. a permission error), the `Err` is
    /// returned immediately and the collection may be left in **any partial
    /// state** — some entries moved, some not — with no state information in
    /// the error itself. This is safe by design:
    ///
    /// - the partial state stays **fully functional**, because every read
    ///   path ([`CollectionLayout::models_base`],
    ///   [`CollectionLayout::index_artifact`]) resolves per-artifact with
    ///   legacy fallback (the module's core invariant);
    /// - retrying `migrate()` is always safe: already-moved entries are
    ///   skipped (idempotent), and the retry picks up where the failure left
    ///   off. Call [`CollectionLayout::detect`] after an `Err` if the caller
    ///   needs to know where things stand.
    ///
    /// Returns the resulting [`CacheLayoutState`] (normally `Split`, unless a
    /// race left some entries unmoved, in which case `Partial`).
    pub fn migrate(&self) -> std::io::Result<CacheLayoutState> {
        let models = self.root.join("models");
        let index = self.root.join("index");
        std::fs::create_dir_all(&models)?;
        std::fs::create_dir_all(&index)?;
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let n = name.to_string_lossy();
            let target = if n.starts_with("models--") {
                models.join(&name)
            } else if INDEX_ARTIFACTS.contains(&n.as_ref()) {
                index.join(&name)
            } else {
                continue; // models/, index/, live-reindex markers, unknown files
            };
            if target.exists() {
                continue; // lost a concurrent-migration race for this entry — fine
            }
            std::fs::rename(entry.path(), &target)?; // same volume: atomic + instant
        }
        Ok(self.detect())
    }

    /// Total size in bytes of the hf-hub model cache, wherever its entries
    /// currently live (new `models/` location, legacy root, or a split
    /// between the two mid-migration).
    pub fn models_size_bytes(&self) -> u64 {
        let mut total = dir_size(&self.root.join("models"));
        for entry in self.legacy_entries() {
            if entry
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("models--"))
                .unwrap_or(false)
            {
                total += dir_size(&entry);
            }
        }
        total
    }

    /// Total size in bytes of the search index artifacts (`tantivy/`,
    /// `vectors/`, `engine.redb`), wherever each currently lives.
    pub fn index_size_bytes(&self) -> u64 {
        INDEX_ARTIFACTS
            .iter()
            .map(|name| dir_size(&self.index_artifact(name)))
            .sum()
    }
}

/// Size in bytes of a file, or the recursive total of a directory's
/// contents. Missing paths contribute `0`.
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return 0;
    };
    read_dir
        .flatten()
        .map(|entry| dir_size(&entry.path()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fresh_dir_detects_split_and_resolves_new_paths() {
        let dir = tempdir().unwrap();
        let layout = CollectionLayout::new(dir.path());

        assert_eq!(layout.detect(), CacheLayoutState::Split);
        assert_eq!(
            layout.index_artifact("tantivy"),
            dir.path().join("index").join("tantivy")
        );
        assert_eq!(layout.models_base(), dir.path().join("models"));
    }

    #[test]
    fn legacy_dir_detects_legacy_and_resolves_old_paths() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("tantivy")).unwrap();
        fs::create_dir(dir.path().join("vectors")).unwrap();
        fs::write(dir.path().join("engine.redb"), b"db").unwrap();
        fs::create_dir(dir.path().join("models--org--name")).unwrap();
        let layout = CollectionLayout::new(dir.path());

        assert_eq!(layout.detect(), CacheLayoutState::Legacy);
        assert_eq!(layout.index_artifact("tantivy"), dir.path().join("tantivy"));
        assert_eq!(layout.index_artifact("vectors"), dir.path().join("vectors"));
        assert_eq!(
            layout.index_artifact("engine.redb"),
            dir.path().join("engine.redb")
        );
        // Per-model probe follows the legacy dir; the download base is always
        // the new location regardless of migration state.
        assert_eq!(
            layout.model_dir("models--org--name"),
            dir.path().join("models--org--name")
        );
        assert_eq!(layout.models_base(), dir.path().join("models"));
    }

    /// The split-brain case that motivates PER-MODEL resolution: model A is
    /// still legacy at the root while model B already moved under `models/`.
    /// A single shared base would resolve at least one of them to the wrong
    /// place; `model_dir` must find each where it actually lives.
    #[test]
    fn model_dir_resolves_split_brain_models_individually() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("models--org--legacy")).unwrap();
        fs::create_dir_all(dir.path().join("models").join("models--org--migrated")).unwrap();
        let layout = CollectionLayout::new(dir.path());

        assert_eq!(layout.detect(), CacheLayoutState::Partial);
        assert_eq!(
            layout.model_dir("models--org--legacy"),
            dir.path().join("models--org--legacy"),
            "legacy model must resolve to its root location"
        );
        assert_eq!(
            layout.model_dir("models--org--migrated"),
            dir.path().join("models").join("models--org--migrated"),
            "migrated model must resolve under models/"
        );
        // A model present in NEITHER location resolves to the new location
        // (where a future download puts it).
        assert_eq!(
            layout.model_dir("models--org--absent"),
            dir.path().join("models").join("models--org--absent")
        );
        // Consistency with models_size_bytes, which sums BOTH locations.
        fs::write(dir.path().join("models--org--legacy").join("w"), [0u8; 3]).unwrap();
        fs::write(
            dir.path()
                .join("models")
                .join("models--org--migrated")
                .join("w"),
            [0u8; 4],
        )
        .unwrap();
        assert_eq!(layout.models_size_bytes(), 7);
    }

    #[test]
    fn migrate_moves_all_entries_and_is_idempotent() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("tantivy")).unwrap();
        fs::write(dir.path().join("tantivy").join("seg"), b"x").unwrap();
        fs::create_dir(dir.path().join("vectors")).unwrap();
        fs::write(dir.path().join("engine.redb"), b"db").unwrap();
        fs::create_dir(dir.path().join("models--org--name")).unwrap();
        let layout = CollectionLayout::new(dir.path());

        let state = layout.migrate().unwrap();
        assert_eq!(state, CacheLayoutState::Split);
        assert!(dir.path().join("models").join("models--org--name").is_dir());
        assert!(dir.path().join("index").join("tantivy").is_dir());
        assert!(dir.path().join("index").join("vectors").is_dir());
        assert!(dir.path().join("index").join("engine.redb").is_file());
        assert!(!dir.path().join("tantivy").exists());
        assert!(!dir.path().join("models--org--name").exists());
        assert_eq!(layout.detect(), CacheLayoutState::Split);

        // Idempotent: running again on an already-split collection is a no-op.
        let state2 = layout.migrate().unwrap();
        assert_eq!(state2, CacheLayoutState::Split);
        assert!(dir.path().join("index").join("tantivy").is_dir());
    }

    #[test]
    fn partial_state_resolves_each_artifact_where_it_lives() {
        let dir = tempdir().unwrap();
        // "models" already migrated...
        fs::create_dir_all(dir.path().join("models").join("models--org--name")).unwrap();
        // ...but "tantivy" is still at the legacy root.
        fs::create_dir(dir.path().join("tantivy")).unwrap();
        let layout = CollectionLayout::new(dir.path());

        assert_eq!(layout.detect(), CacheLayoutState::Partial);
        assert_eq!(layout.index_artifact("tantivy"), dir.path().join("tantivy"));
        assert_eq!(layout.models_base(), dir.path().join("models"));
    }

    #[test]
    fn migrate_skips_existing_targets() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("tantivy")).unwrap();
        fs::write(dir.path().join("tantivy").join("legacy-marker"), b"old").unwrap();
        // Simulate a concurrent migration that already won the race for
        // "index/tantivy" — the target already exists.
        fs::create_dir_all(dir.path().join("index").join("tantivy")).unwrap();
        fs::write(
            dir.path().join("index").join("tantivy").join("new-marker"),
            b"new",
        )
        .unwrap();
        let layout = CollectionLayout::new(dir.path());

        let result = layout.migrate();
        assert!(result.is_ok());
        // The legacy source was left alone rather than erroring or clobbering.
        assert!(dir.path().join("tantivy").exists());
        assert!(dir.path().join("tantivy").join("legacy-marker").exists());
        assert!(dir
            .path()
            .join("index")
            .join("tantivy")
            .join("new-marker")
            .exists());
    }

    /// `migrate()` on a fresh empty collection must still create `models/`
    /// and `index/`, so that the parent of every `index_artifact` resolution
    /// exists — redb's `Database::create` does NOT create parents, and
    /// without this guarantee a fresh collection would hit ENOENT.
    #[test]
    fn migrate_on_fresh_dir_creates_index_artifact_parents() {
        let dir = tempdir().unwrap();
        let layout = CollectionLayout::new(dir.path());

        let state = layout.migrate().unwrap();
        assert_eq!(state, CacheLayoutState::Split);

        let redb_path = layout.index_artifact("engine.redb");
        assert_eq!(redb_path, dir.path().join("index").join("engine.redb"));
        assert!(
            redb_path.parent().unwrap().is_dir(),
            "index_artifact parent must exist after migrate() on a fresh dir"
        );
        assert!(layout.models_base().is_dir());
    }

    /// A rename failure mid-migration returns `Err` and may leave a partial
    /// state — but that state must stay fully readable via per-artifact
    /// fallback, and retrying `migrate()` must complete the job.
    #[cfg(unix)]
    #[test]
    fn migrate_partial_failure_stays_functional_and_retry_completes() {
        extern "C" {
            fn geteuid() -> u32;
        }
        // chmod 000 is a no-op under root (the rename still succeeds), so
        // the test would spuriously see Ok. Skip when running as root.
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("models--org--name")).unwrap();
        fs::write(dir.path().join("engine.redb"), b"db").unwrap();
        // Pre-create index/ with no permissions: renaming into it fails.
        fs::create_dir(dir.path().join("index")).unwrap();
        fs::set_permissions(dir.path().join("index"), fs::Permissions::from_mode(0o000)).unwrap();
        let layout = CollectionLayout::new(dir.path());

        let result = layout.migrate();
        assert!(result.is_err(), "rename into unwritable index/ must fail");

        // The blocked artifact is still at the legacy root, and the read
        // path resolves it there — the partial state is fully functional.
        assert!(dir.path().join("engine.redb").is_file());
        assert_eq!(
            layout.index_artifact("engine.redb"),
            dir.path().join("engine.redb")
        );

        // Restore permissions: retrying is safe and completes the migration.
        fs::set_permissions(dir.path().join("index"), fs::Permissions::from_mode(0o755)).unwrap();
        let state = layout.migrate().unwrap();
        assert_eq!(state, CacheLayoutState::Split);
        assert!(dir.path().join("index").join("engine.redb").is_file());
        assert!(dir.path().join("models").join("models--org--name").is_dir());
    }

    /// The "correct mid-migration" invariant: size functions must count each
    /// artifact wherever it actually lives. Here models are already moved
    /// but the index artifacts are still legacy at the root.
    #[test]
    fn size_fns_count_artifacts_mid_migration() {
        let dir = tempdir().unwrap();
        // Models migrated: 10 bytes under models/models--org--name/.
        let model_dir = dir.path().join("models").join("models--org--name");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("weights.bin"), [0u8; 10]).unwrap();
        // Index still legacy at root: 7 bytes in tantivy/ + 5 in engine.redb.
        fs::create_dir(dir.path().join("tantivy")).unwrap();
        fs::write(dir.path().join("tantivy").join("seg"), [0u8; 7]).unwrap();
        fs::write(dir.path().join("engine.redb"), [0u8; 5]).unwrap();
        let layout = CollectionLayout::new(dir.path());

        assert_eq!(layout.detect(), CacheLayoutState::Partial);
        assert_eq!(layout.models_size_bytes(), 10);
        assert_eq!(layout.index_size_bytes(), 12);
    }
}
