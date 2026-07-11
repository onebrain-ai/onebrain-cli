//! The two token-cache layers backed by `token.redb` (design §3):
//!
//! - [`memo`] — lossless query-result **memoization**. Key folds the index
//!   `generation` (Track 3, `onebrain-search`) in, so a reindex invalidates
//!   every prior entry by construction — stale service is impossible.
//! - [`ledger`] — the lossy, signaled, session **already-sent ledger**
//!   (level 2↑). Keyed on `(session_token, doc_path)`, it detects a repeat
//!   delivery of unchanged content so a surface can send a reference instead.
//!
//! ## Ownership / Direct mode
//! The warm daemon is the sole owner of `token.redb` and serves these layers
//! over HTTP. In the daemon-less fallback (`Backend::Direct`), a CLI/MCP
//! process opens the DB in-process via [`TokenCache::open`] — safe because in
//! that mode no daemon holds it (redb's single-writer lock is respected either
//! way). Everything here is I/O; the pure transform logic lives elsewhere in
//! the crate.

use std::path::Path;

use redb::Database;

pub mod ledger;
pub mod memo;

pub use ledger::{
    Ledger, LedgerEntry, LedgerVerdict, DEFAULT_LEDGER_TTL_SECS, GC_MIN_INTERVAL_SECS,
};
pub use memo::{Memo, MemoKey};

/// Error surface for the redb-backed cache layers. Wraps redb's per-stage
/// error types (open / txn / table / storage / commit) plus the JSON codec
/// used for stored values, so callers get one `?`-friendly type.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Database(#[from] redb::DatabaseError),
    #[error(transparent)]
    Transaction(#[from] redb::TransactionError),
    #[error(transparent)]
    Table(#[from] redb::TableError),
    #[error(transparent)]
    Storage(#[from] redb::StorageError),
    #[error(transparent)]
    Commit(#[from] redb::CommitError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Convenience alias for cache operations.
pub type CacheResult<T> = Result<T, CacheError>;

/// A handle on `token.redb`, exposing the [`memo`] and [`ledger`] layers.
///
/// Cheap to hold; all operations open their own short transaction so a single
/// `TokenCache` can be shared behind a lock (the daemon) or opened per-process
/// (Direct mode).
pub struct TokenCache {
    db: Database,
}

impl TokenCache {
    /// Open (creating if absent) the `token.redb` file at `path`. The parent
    /// directory must already exist — the daemon creates
    /// `<collection_cache>/token/` before calling this.
    pub fn open(path: &Path) -> CacheResult<Self> {
        let db = Database::create(path)?;
        Ok(Self { db })
    }

    /// Adopt an already-open [`Database`] (e.g. one the daemon opened at boot).
    pub fn from_database(db: Database) -> Self {
        Self { db }
    }

    /// The lossless query-result memoization layer.
    pub fn memo(&self) -> Memo<'_> {
        Memo::new(&self.db)
    }

    /// The session already-sent ledger layer.
    pub fn ledger(&self) -> Ledger<'_> {
        Ledger::new(&self.db)
    }

    /// The underlying `token.redb` handle — for the shared gain pivot engine
    /// ([`crate::gain::pivot::query`]), which reads the Tier-2 rollup tables
    /// this same DB owns. Exposed so the daemon's `/api/token/gain` route and
    /// the CLI `token gain` command run the ONE pivot engine over the ONE
    /// store, instead of re-deriving the aggregation.
    pub fn database(&self) -> &Database {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_and_reopens_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.redb");
        {
            let cache = TokenCache::open(&path).unwrap();
            cache.ledger().record("sess", "a.md", "hash-1").unwrap();
        } // drop releases the redb lock
          // Reopen in a fresh process-like scope: the entry is durable.
        let cache = TokenCache::open(&path).unwrap();
        let verdict = cache.ledger().check("sess", "a.md", "hash-1").unwrap();
        assert_eq!(
            verdict,
            LedgerVerdict::Unchanged {
                sent_hash: "hash-1".into()
            }
        );
    }
}
