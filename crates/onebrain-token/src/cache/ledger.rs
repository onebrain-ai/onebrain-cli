//! Session already-sent ledger (design §3b) — lossy, signaled, level 2↑.
//!
//! Key = `(session_token, doc_path)` → the content hash last sent, plus a
//! timestamp for GC. On a repeat delivery of the same content ([`Ledger::check`]
//! returns [`LedgerVerdict::Unchanged`]), a surface may send a reference
//! envelope instead of the full body; an edit (hash changed →
//! [`LedgerVerdict::Changed`]) forces fresh content. A doc never sent in this
//! session is [`LedgerVerdict::FirstSend`]. Entries older than
//! [`DEFAULT_LEDGER_TTL_SECS`] are pruned by the daemon's periodic
//! [`Ledger::gc`] sweep; a per-write prune also runs, but THROTTLED to at most
//! once per [`GC_MIN_INTERVAL_SECS`] so the hot path stays an O(1) insert (MAJOR
//! 3) rather than an O(n) full-table scan per delivery.

use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::CacheResult;

/// `session_token \0 doc_path -> LedgerEntry (json)`.
const LEDGER_TABLE: TableDefinition<&str, &str> = TableDefinition::new("ledger");

/// Small metadata table: `key -> value`. Holds the last-GC wall-clock marker
/// that throttles the per-write prune scan. A SEPARATE table so the
/// [`LEDGER_TABLE`] scan never trips over it.
const LEDGER_META: TableDefinition<&str, &str> = TableDefinition::new("ledger_meta");
const LAST_GC_KEY: &str = "last_gc_ts";

/// Default GC horizon: entries whose last-send timestamp is older than this
/// are pruned. A week comfortably covers any realistic single working session
/// while keeping the ledger from growing without bound (design §3b GC note).
pub const DEFAULT_LEDGER_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Minimum interval between opportunistic per-write prune scans (MAJOR 3). A
/// full-table scan on EVERY delivery is O(n) per write / O(n²) accumulated on
/// a long-lived warm daemon; throttling it to once per this window keeps the
/// hot path O(1) while the daemon's periodic [`Ledger::gc`] sweep does the
/// real bounded pruning. Only when this long has passed since the last scan
/// does a `record` also prune.
pub const GC_MIN_INTERVAL_SECS: i64 = 5 * 60;

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
    /// Runs the opportunistic prune only when throttling allows (see
    /// [`GC_MIN_INTERVAL_SECS`]); the daemon's [`Ledger::gc`] does the bounded
    /// pruning.
    pub fn record(&self, session_token: &str, doc_path: &str, hash: &str) -> CacheResult<()> {
        self.record_at(session_token, doc_path, hash, now_ts())
    }

    /// [`Ledger::record`] with an explicit timestamp — the seam GC tests use to
    /// plant entries at controlled times. Prunes in the same txn only if more
    /// than [`GC_MIN_INTERVAL_SECS`] has passed since the last prune; otherwise
    /// it is a plain O(1) insert (MAJOR 3 throttle).
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

        let write_txn = self.db.begin_write()?;
        {
            // Throttle: prune only if the window has elapsed since the last one.
            let last_gc = read_last_gc(&write_txn)?;
            let should_prune = ts - last_gc >= GC_MIN_INTERVAL_SECS;

            let mut table = write_txn.open_table(LEDGER_TABLE)?;
            if should_prune {
                prune_older_than(&mut table, ts - DEFAULT_LEDGER_TTL_SECS)?;
            }
            table.insert(key.as_str(), value.as_str())?;
            if should_prune {
                let mut meta = write_txn.open_table(LEDGER_META)?;
                meta.insert(LAST_GC_KEY, ts.to_string().as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// The daemon's periodic bounded sweep (MAJOR 3): prune every entry older
    /// than `now_ts - `[`DEFAULT_LEDGER_TTL_SECS`] and stamp `now_ts` as the
    /// last-GC time so per-write scans stay throttled. Returns the number
    /// removed. This — not the per-write path — is the primary pruning
    /// mechanism, wired into the daemon's maintenance loop.
    pub fn gc(&self, now_ts: i64) -> CacheResult<usize> {
        let cutoff = now_ts - DEFAULT_LEDGER_TTL_SECS;
        let write_txn = self.db.begin_write()?;
        let removed;
        {
            let mut table = write_txn.open_table(LEDGER_TABLE)?;
            removed = prune_older_than(&mut table, cutoff)?;
            let mut meta = write_txn.open_table(LEDGER_META)?;
            meta.insert(LAST_GC_KEY, now_ts.to_string().as_str())?;
        }
        write_txn.commit()?;
        Ok(removed)
    }

    /// The stored last-GC wall-clock marker (0 if no prune has run). Read-only.
    /// Surfaces the throttle state for tests and, later, `/doctor`.
    pub fn last_gc_marker(&self) -> CacheResult<i64> {
        let read_txn = self.db.begin_read()?;
        let meta = match read_txn.open_table(LEDGER_META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let stored = meta.get(LAST_GC_KEY)?;
        Ok(stored
            .and_then(|v| v.value().parse::<i64>().ok())
            .unwrap_or(0))
    }
}

/// Read the last-GC wall-clock marker (0 if never pruned / table absent).
fn read_last_gc(write_txn: &redb::WriteTransaction) -> CacheResult<i64> {
    let meta = match write_txn.open_table(LEDGER_META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let stored = meta.get(LAST_GC_KEY)?;
    Ok(stored
        .and_then(|v| v.value().parse::<i64>().ok())
        .unwrap_or(0))
}

/// Remove every ledger entry whose timestamp is `< cutoff`. Returns the count
/// removed. Collects keys first (redb can't remove while iterating).
fn prune_older_than(table: &mut redb::Table<&str, &str>, cutoff: i64) -> CacheResult<usize> {
    let stale: Vec<String> = table
        .iter()?
        .filter_map(|row| {
            let (k, v) = row.ok()?;
            let e: LedgerEntry = serde_json::from_str(v.value()).ok()?;
            (e.ts < cutoff).then(|| k.value().to_string())
        })
        .collect();
    let removed = stale.len();
    for k in stale {
        table.remove(k.as_str())?;
    }
    Ok(removed)
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
    fn gc_prunes_entries_older_than_ttl_and_stamps_marker() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        // Plant two entries BELOW the throttle threshold so neither record runs
        // its own prune — isolating the explicit gc under test.
        ledger.record_at("sess", "old.md", "h1", 100).unwrap();
        ledger.record_at("sess", "new.md", "h2", 200).unwrap();
        assert_eq!(
            ledger.last_gc_marker().unwrap(),
            0,
            "no per-write prune ran"
        );

        // Sweep at a wall-clock where old.md (@100) is older than the TTL but
        // new.md (@200) is not: cutoff = now - TTL = 150.
        let now = DEFAULT_LEDGER_TTL_SECS + 150;
        let removed = ledger.gc(now).unwrap();
        assert_eq!(removed, 1, "only the entry older than TTL is pruned");
        assert_eq!(
            ledger.last_gc_marker().unwrap(),
            now,
            "gc stamps the last-GC marker"
        );
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
    fn record_prunes_stale_entries_opportunistically_when_window_elapsed() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();
        // A very old entry (ts=0) — below threshold, so it does NOT prune itself.
        ledger.record_at("sess", "old.md", "h1", 0).unwrap();
        // A record whose ts is well past the TTL: window elapsed (ts - 0 >=
        // interval) so its opportunistic prune (cutoff = ts - TTL) sweeps ts=0.
        let fresh_ts = DEFAULT_LEDGER_TTL_SECS + 10;
        ledger.record_at("sess", "new.md", "h2", fresh_ts).unwrap();
        assert_eq!(
            ledger.check("sess", "old.md", "h1").unwrap(),
            LedgerVerdict::FirstSend,
            "record's opportunistic GC pruned the stale entry"
        );
    }

    /// MAJOR 3: the per-write prune scan is THROTTLED — it runs only when at
    /// least [`GC_MIN_INTERVAL_SECS`] has elapsed since the last one, so the
    /// hot path isn't an O(n) scan per delivery. Tracked via the last-GC marker.
    #[test]
    fn record_throttles_the_prune_scan() {
        let (_dir, cache) = direct_cache();
        let ledger = cache.ledger();

        // Sub-threshold first record (100 < interval): no prune, marker stays 0.
        ledger.record_at("s", "a.md", "h", 100).unwrap();
        assert_eq!(ledger.last_gc_marker().unwrap(), 0);

        // interval past the last prune (marker 0): prunes, marker advances.
        ledger
            .record_at("s", "b.md", "h", GC_MIN_INTERVAL_SECS)
            .unwrap();
        assert_eq!(ledger.last_gc_marker().unwrap(), GC_MIN_INTERVAL_SECS);

        // Within the window of the last prune: scan SKIPPED, marker unchanged.
        ledger
            .record_at("s", "c.md", "h", 2 * GC_MIN_INTERVAL_SECS - 1)
            .unwrap();
        assert_eq!(
            ledger.last_gc_marker().unwrap(),
            GC_MIN_INTERVAL_SECS,
            "a within-window record must not re-scan (throttled)"
        );

        // Past the window again: prunes, marker advances.
        ledger
            .record_at("s", "d.md", "h", 2 * GC_MIN_INTERVAL_SECS)
            .unwrap();
        assert_eq!(ledger.last_gc_marker().unwrap(), 2 * GC_MIN_INTERVAL_SECS);
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
