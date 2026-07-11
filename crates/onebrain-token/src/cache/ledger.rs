//! Session already-sent ledger (design §3b) — lossy, signaled, level 2↑.
//!
//! Key = `(session_token, doc_path)` → the content hash last sent, plus a
//! timestamp for GC. On a repeat delivery of the same content ([`Ledger::check`]
//! returns [`LedgerVerdict::Unchanged`]), a surface may send a reference
//! envelope instead of the full body; an edit (hash changed →
//! [`LedgerVerdict::Changed`]) forces fresh content. A doc never sent in this
//! session is [`LedgerVerdict::FirstSend`]. Entries older than
//! [`DEFAULT_LEDGER_TTL_SECS`] are pruned opportunistically on write.

use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::CacheResult;

/// `session_token \0 doc_path -> LedgerEntry (json)`.
const LEDGER_TABLE: TableDefinition<&str, &str> = TableDefinition::new("ledger");

/// Default GC horizon: entries whose last-send timestamp is older than this
/// are pruned. A week comfortably covers any realistic single working session
/// while keeping the ledger from growing without bound (design §3b GC note).
pub const DEFAULT_LEDGER_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// The stored value for one `(session, doc)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Content hash last delivered to this session for this doc.
    pub hash: String,
    /// Epoch seconds of that delivery — the GC anchor.
    pub ts: i64,
}

/// What [`Ledger::check`] concludes about delivering `doc_path` to `session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerVerdict {
    /// Never sent to this session — deliver full content and record it.
    FirstSend,
    /// Already sent this exact content — a surface may send a reference. The
    /// `sent_hash` is the hash on file (equal to the caller's `current_hash`).
    Unchanged { sent_hash: String },
    /// Sent before, but the content hash changed (doc edited) — deliver fresh
    /// content and re-record.
    Changed,
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compose the composite redb key. A NUL separator is safe: session tokens are
/// `[A-Za-z0-9]` and vault doc paths never contain a NUL byte.
fn ledger_key(session_token: &str, doc_path: &str) -> String {
    format!("{session_token}\u{0}{doc_path}")
}

/// The ledger layer over a [`Database`]. Obtain via
/// [`super::TokenCache::ledger`].
pub struct Ledger<'a> {
    db: &'a Database,
}

impl<'a> Ledger<'a> {
    pub(super) fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Classify a delivery of `current_hash` for `(session_token, doc_path)`
    /// WITHOUT mutating the ledger. Read-only: a caller decides based on the
    /// verdict, then calls [`Ledger::record`] once it actually delivers.
    pub fn check(
        &self,
        session_token: &str,
        doc_path: &str,
        current_hash: &str,
    ) -> CacheResult<LedgerVerdict> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(LEDGER_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(LedgerVerdict::FirstSend),
            Err(e) => return Err(e.into()),
        };
        let key = ledger_key(session_token, doc_path);
        match table.get(key.as_str())? {
            None => Ok(LedgerVerdict::FirstSend),
            Some(v) => {
                let entry: LedgerEntry = serde_json::from_str(v.value())?;
                if entry.hash == current_hash {
                    Ok(LedgerVerdict::Unchanged {
                        sent_hash: entry.hash,
                    })
                } else {
                    Ok(LedgerVerdict::Changed)
                }
            }
        }
    }

    /// Record that `hash` was delivered for `(session_token, doc_path)` now.
    /// Prunes entries older than [`DEFAULT_LEDGER_TTL_SECS`] in the same write
    /// txn (opportunistic GC).
    pub fn record(&self, session_token: &str, doc_path: &str, hash: &str) -> CacheResult<()> {
        self.record_at(session_token, doc_path, hash, now_ts())
    }

    /// [`Ledger::record`] with an explicit timestamp — the seam GC tests use to
    /// plant a deliberately-old entry. Prunes with a cutoff derived from `ts`.
    pub fn record_at(
        &self,
        session_token: &str,
        doc_path: &str,
        hash: &str,
        ts: i64,
    ) -> CacheResult<()> {
        let entry = LedgerEntry {
            hash: hash.to_string(),
            ts,
        };
        let value = serde_json::to_string(&entry)?;
        let key = ledger_key(session_token, doc_path);
        let cutoff = ts - DEFAULT_LEDGER_TTL_SECS;

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(LEDGER_TABLE)?;
            // Opportunistic GC first, then the insert — one txn, so a crash
            // leaves the ledger consistent (either both or neither).
            let stale: Vec<String> = table
                .iter()?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    let e: LedgerEntry = serde_json::from_str(v.value()).ok()?;
                    (e.ts < cutoff).then(|| k.value().to_string())
                })
                .collect();
            for k in stale {
                table.remove(k.as_str())?;
            }
            table.insert(key.as_str(), value.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Prune every entry whose timestamp is older than `cutoff_ts`. Returns the
    /// number removed. Exposed for the daemon's periodic sweep and for tests;
    /// [`Ledger::record`] also prunes opportunistically.
    pub fn gc(&self, cutoff_ts: i64) -> CacheResult<usize> {
        let write_txn = self.db.begin_write()?;
        let removed;
        {
            let mut table = write_txn.open_table(LEDGER_TABLE)?;
            let stale: Vec<String> = table
                .iter()?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    let e: LedgerEntry = serde_json::from_str(v.value()).ok()?;
                    (e.ts < cutoff_ts).then(|| k.value().to_string())
                })
                .collect();
            removed = stale.len();
            for k in stale {
                table.remove(k.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::TokenCache;

    /// Open `token.redb` in-process — exactly the `Backend::Direct` fallback
    /// path (no daemon holding the DB). Every ledger test runs through this
    /// seam, so it doubles as the Direct-mode coverage.
    fn direct_cache() -> (tempfile::TempDir, TokenCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache::open(&dir.path().join("token.redb")).unwrap();
        (dir, cache)
    }

    #[test]
    fn first_send_then_record_then_unchanged() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();

        assert_eq!(
            ledger.check("sess", "a.md", "h1").unwrap(),
            LedgerVerdict::FirstSend,
            "never sent → FirstSend"
        );
        ledger.record("sess", "a.md", "h1").unwrap();
        assert_eq!(
            ledger.check("sess", "a.md", "h1").unwrap(),
            LedgerVerdict::Unchanged {
                sent_hash: "h1".into()
            },
            "same hash after record → Unchanged"
        );
    }

    #[test]
    fn edited_doc_reads_as_changed_then_rerecord() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        ledger.record("sess", "a.md", "h1").unwrap();

        assert_eq!(
            ledger.check("sess", "a.md", "h2").unwrap(),
            LedgerVerdict::Changed,
            "different hash → Changed"
        );
        // Re-record the new hash; now h2 reads as Unchanged.
        ledger.record("sess", "a.md", "h2").unwrap();
        assert_eq!(
            ledger.check("sess", "a.md", "h2").unwrap(),
            LedgerVerdict::Unchanged {
                sent_hash: "h2".into()
            }
        );
    }

    #[test]
    fn ledger_is_scoped_per_session() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        ledger.record("sess-A", "a.md", "h1").unwrap();
        // A different session has never seen this doc.
        assert_eq!(
            ledger.check("sess-B", "a.md", "h1").unwrap(),
            LedgerVerdict::FirstSend
        );
    }

    #[test]
    fn gc_prunes_entries_older_than_cutoff() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        // One ancient entry, one fresh — planted with explicit timestamps.
        ledger.record_at("sess", "old.md", "h1", 1_000).unwrap();
        ledger.record_at("sess", "new.md", "h2", 2_000).unwrap();

        let removed = ledger.gc(1_500).unwrap();
        assert_eq!(removed, 1, "only the pre-cutoff entry is pruned");
        assert_eq!(
            ledger.check("sess", "old.md", "h1").unwrap(),
            LedgerVerdict::FirstSend,
            "pruned entry reverts to FirstSend"
        );
        assert_eq!(
            ledger.check("sess", "new.md", "h2").unwrap(),
            LedgerVerdict::Unchanged {
                sent_hash: "h2".into()
            },
            "fresh entry survives GC"
        );
    }

    #[test]
    fn record_prunes_stale_entries_opportunistically() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        // A very old entry (ts=0).
        ledger.record_at("sess", "old.md", "h1", 0).unwrap();
        // A record whose ts is well past the TTL — its opportunistic GC pass
        // (cutoff = ts - TTL) sweeps the ts=0 entry.
        let fresh_ts = DEFAULT_LEDGER_TTL_SECS + 10;
        ledger.record_at("sess", "new.md", "h2", fresh_ts).unwrap();
        assert_eq!(
            ledger.check("sess", "old.md", "h1").unwrap(),
            LedgerVerdict::FirstSend,
            "record's opportunistic GC pruned the stale entry"
        );
    }

    #[test]
    fn check_is_read_only() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        // check on a cold ledger must not create/record anything.
        assert_eq!(
            ledger.check("sess", "a.md", "h1").unwrap(),
            LedgerVerdict::FirstSend
        );
        assert_eq!(
            ledger.check("sess", "a.md", "h1").unwrap(),
            LedgerVerdict::FirstSend,
            "a second check still sees FirstSend — check never records"
        );
    }
}
