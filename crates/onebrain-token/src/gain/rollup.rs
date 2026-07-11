//! Tier-2 precomputed rollups (design 0030 §5): redb tables in `token.redb`,
//! keyed by `(period, surface, transform, level, cache_kind)`, updated
//! incrementally from raw [`GainEvent`]s in the SAME write batch the Tier-1
//! JSONL append happens in (daemon-side, later tracks). Every read (CLI
//! summary/pivot + the Web API) serves from these tables at constant time
//! regardless of how large the raw log has grown — only `--history` tails
//! the raw JSONL directly.
//!
//! [`rebuild`] reconstructs the tables from scratch by replaying the raw
//! log through the same [`update`] path an incremental write uses, so the
//! two can never disagree (the determinism the design calls out).

use std::path::Path;

use chrono::{DateTime, Datelike, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::event::GainEvent;
use super::writer::JsonlGainWriter;

/// Rollup keyed by calendar day: `YYYY-MM-DD`.
pub const GAIN_DAILY: TableDefinition<&str, &str> = TableDefinition::new("gain_daily");
/// Rollup keyed by calendar month: `YYYY-MM`.
pub const GAIN_MONTHLY: TableDefinition<&str, &str> = TableDefinition::new("gain_monthly");
/// Rollup keyed by calendar year: `YYYY`.
pub const GAIN_YEARLY: TableDefinition<&str, &str> = TableDefinition::new("gain_yearly");

/// Separator between composite-key segments. A control character so it can
/// never collide with a real `transform` name or period string.
const KEY_SEP: char = '\u{1}';

/// Everything that can go wrong opening/reading/writing the rollup tables.
/// Mirrors the specific redb error types (not the umbrella `redb::Error`) so
/// each `?` site converts directly — see redb's per-operation error types.
#[derive(Debug, thiserror::Error)]
pub enum RollupError {
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("io error reading raw gain log: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error decoding a rollup value: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Aggregated counters for one `(period, surface, transform, level,
/// cache_kind)` key. `bytes_before`/`bytes_after` are running sums (a
/// pivot's "gain" is `bytes_before - bytes_after` over the matched rows).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupValue {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub count: u64,
}

impl RollupValue {
    fn plus(mut self, event: &GainEvent) -> Self {
        self.bytes_before += event.bytes_before;
        self.bytes_after += event.bytes_after;
        self.count += 1;
        self
    }
}

/// `YYYY-MM-DD` (UTC) for a unix timestamp — the daily rollup key's period
/// segment. `pub(crate)` so [`super::pivot::query_events`] buckets raw
/// events on the exact same day boundary the rollup does.
pub(crate) fn day_key(ts: i64) -> String {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_else(Utc::now);
    format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day())
}

fn month_key(ts: i64) -> String {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_else(Utc::now);
    format!("{:04}-{:02}", dt.year(), dt.month())
}

fn year_key(ts: i64) -> String {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_else(Utc::now);
    format!("{:04}", dt.year())
}

/// One rollup row's decoded key dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupKey {
    pub period: String,
    pub surface: String,
    pub transform: String,
    pub level: String,
    pub cache: String,
}

fn composite_key(period: &str, event: &GainEvent) -> String {
    format!(
        "{period}{KEY_SEP}{}{KEY_SEP}{}{KEY_SEP}{}{KEY_SEP}{}",
        event.surface, event.transform, event.level, event.cache,
    )
}

/// Parse a composite key string back into its dimensions. `None` on a
/// malformed key (should never happen for keys this module wrote).
pub fn parse_key(raw: &str) -> Option<RollupKey> {
    let mut parts = raw.split(KEY_SEP);
    Some(RollupKey {
        period: parts.next()?.to_string(),
        surface: parts.next()?.to_string(),
        transform: parts.next()?.to_string(),
        level: parts.next()?.to_string(),
        cache: parts.next()?.to_string(),
    })
}

/// Ensure all three rollup tables exist in `db`. Idempotent — safe to call
/// on every daemon start alongside the engine's own table-creation step.
pub fn ensure_tables(db: &Database) -> Result<(), RollupError> {
    let write_txn = db.begin_write()?;
    write_txn.open_table(GAIN_DAILY)?;
    write_txn.open_table(GAIN_MONTHLY)?;
    write_txn.open_table(GAIN_YEARLY)?;
    write_txn.commit()?;
    Ok(())
}

/// Apply one [`GainEvent`] to all three period tables (daily / monthly /
/// yearly) in a single write transaction — the "one write batch" the design
/// calls for, so a rollup read can never observe two of the three tables
/// updated and the third stale.
pub fn update(db: &Database, event: &GainEvent) -> Result<(), RollupError> {
    let write_txn = db.begin_write()?;
    {
        let mut daily = write_txn.open_table(GAIN_DAILY)?;
        let key = composite_key(&day_key(event.ts), event);
        apply(&mut daily, &key, event)?;
    }
    {
        let mut monthly = write_txn.open_table(GAIN_MONTHLY)?;
        let key = composite_key(&month_key(event.ts), event);
        apply(&mut monthly, &key, event)?;
    }
    {
        let mut yearly = write_txn.open_table(GAIN_YEARLY)?;
        let key = composite_key(&year_key(event.ts), event);
        apply(&mut yearly, &key, event)?;
    }
    write_txn.commit()?;
    Ok(())
}

fn apply(
    table: &mut redb::Table<&str, &str>,
    key: &str,
    event: &GainEvent,
) -> Result<(), RollupError> {
    let existing = table.get(key)?.map(|g| g.value().to_string());
    let current: RollupValue = match existing {
        Some(s) => serde_json::from_str(&s)?,
        None => RollupValue::default(),
    };
    let updated = current.plus(event);
    let serialized = serde_json::to_string(&updated)?;
    table.insert(key, serialized.as_str())?;
    Ok(())
}

/// Stats returned by [`rebuild`] — how many raw events were replayed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildStats {
    pub events_replayed: usize,
}

/// Reconstruct all three rollup tables from the raw JSONL log under
/// `jsonl_dir`, discarding whatever the tables currently hold first. Replays
/// events through the exact same [`update`] path an incremental write uses —
/// rebuilding from a log must always produce tables identical to having
/// applied every event incrementally as it happened (the determinism
/// property the design requires).
///
/// Reads `jsonl_dir` **recursively** (via
/// [`read_all_recursive`](JsonlGainWriter::read_all_recursive)), so an
/// archived epoch under `jsonl_dir/archive/<ts>-<label>/` is included —
/// rollups are the all-time cumulative view; `--reset` only changes which
/// raw log files future writes append to, it never removes history from
/// what a rebuild reconstructs (design §5: "archived epochs remain
/// queryable").
pub fn rebuild(jsonl_dir: &Path, db: &Database) -> Result<RebuildStats, RollupError> {
    let events = JsonlGainWriter::new(jsonl_dir).read_all_recursive()?;

    let write_txn = db.begin_write()?;
    // `delete_table` returns `Ok(false)` (not an error) when the table
    // doesn't exist yet — safe on a brand-new database.
    write_txn.delete_table(GAIN_DAILY)?;
    write_txn.delete_table(GAIN_MONTHLY)?;
    write_txn.delete_table(GAIN_YEARLY)?;
    write_txn.open_table(GAIN_DAILY)?;
    write_txn.open_table(GAIN_MONTHLY)?;
    write_txn.open_table(GAIN_YEARLY)?;
    write_txn.commit()?;

    for event in &events {
        update(db, event)?;
    }

    Ok(RebuildStats {
        events_replayed: events.len(),
    })
}

/// Every row currently in `table`, decoded. Used by the pivot engine — small
/// enough tables (daily/monthly/yearly rollups, not the raw log) that a full
/// scan per query is the right trade for "always constant-time relative to
/// log age, never relative to table size across a vault's lifetime."
pub fn scan(
    db: &Database,
    table: TableDefinition<&str, &str>,
) -> Result<Vec<(RollupKey, RollupValue)>, RollupError> {
    let read_txn = db.begin_read()?;
    let t = match read_txn.open_table(table) {
        Ok(t) => t,
        // A table that was never created (fresh db, nothing written yet)
        // reads as empty, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for row in t.iter()? {
        let (k, v) = row?;
        let key = parse_key(k.value()).expect("this module only ever writes well-formed keys");
        let value: RollupValue = serde_json::from_str(v.value())?;
        out.push((key, value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gain::event::{CacheKind, Surface};
    use crate::level::OptLevel;
    use tempfile::tempdir;

    fn event(ts: i64, surface: Surface, transform: &str, before: u64, after: u64) -> GainEvent {
        GainEvent {
            ts,
            surface,
            transform: transform.to_string(),
            level: OptLevel::Conservative,
            bytes_before: before,
            bytes_after: after,
            cache: CacheKind::None,
            session_token: None,
        }
    }

    fn open_db(dir: &std::path::Path) -> Database {
        Database::create(dir.join("token.redb")).unwrap()
    }

    #[test]
    fn one_event_increments_all_three_period_tables() {
        let dir = tempdir().unwrap();
        let db = open_db(dir.path());
        // 2026-07-13T00:00:00Z
        let ts = 1_783_900_800;
        update(&db, &event(ts, Surface::CliSearch, "whitespace", 1000, 400)).unwrap();

        let daily = scan(&db, GAIN_DAILY).unwrap();
        let monthly = scan(&db, GAIN_MONTHLY).unwrap();
        let yearly = scan(&db, GAIN_YEARLY).unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(monthly.len(), 1);
        assert_eq!(yearly.len(), 1);
        assert_eq!(
            daily[0].1,
            RollupValue {
                bytes_before: 1000,
                bytes_after: 400,
                count: 1
            }
        );
        assert_eq!(daily[0].0.period, "2026-07-13");
        assert_eq!(monthly[0].0.period, "2026-07");
        assert_eq!(yearly[0].0.period, "2026");
    }

    #[test]
    fn same_key_accumulates_across_events() {
        let dir = tempdir().unwrap();
        let db = open_db(dir.path());
        let ts = 1_783_900_800;
        update(&db, &event(ts, Surface::CliSearch, "whitespace", 1000, 400)).unwrap();
        update(
            &db,
            &event(ts + 60, Surface::CliSearch, "whitespace", 500, 200),
        )
        .unwrap();

        let daily = scan(&db, GAIN_DAILY).unwrap();
        assert_eq!(daily.len(), 1, "same dims same day must merge into one row");
        assert_eq!(
            daily[0].1,
            RollupValue {
                bytes_before: 1500,
                bytes_after: 600,
                count: 2
            }
        );
    }

    #[test]
    fn different_dimensions_stay_separate_rows() {
        let dir = tempdir().unwrap();
        let db = open_db(dir.path());
        let ts = 1_783_900_800;
        update(&db, &event(ts, Surface::CliSearch, "whitespace", 1000, 400)).unwrap();
        update(&db, &event(ts, Surface::McpQuery, "whitespace", 1000, 400)).unwrap();
        update(&db, &event(ts, Surface::CliSearch, "snippet", 1000, 400)).unwrap();

        let daily = scan(&db, GAIN_DAILY).unwrap();
        assert_eq!(daily.len(), 3, "distinct surface/transform must not merge");
    }

    #[test]
    fn rebuild_from_jsonl_matches_incremental_result() {
        let db_dir = tempdir().unwrap();
        let jsonl_dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(jsonl_dir.path());

        let events = [
            event(1_783_900_800, Surface::CliSearch, "whitespace", 1000, 400),
            event(1_783_900_900, Surface::McpQuery, "snippet", 800, 300),
            event(1_786_579_200, Surface::McpGet, "get_cap", 2000, 900), // next month
        ];
        for e in &events {
            writer.append(e).unwrap();
        }

        // Incremental path: build one db by calling `update` directly.
        let incremental_db = Database::create(db_dir.path().join("incremental.redb")).unwrap();
        for e in &events {
            update(&incremental_db, e).unwrap();
        }

        // Rebuild path: fresh db, populated only via `rebuild`.
        let rebuilt_db = Database::create(db_dir.path().join("rebuilt.redb")).unwrap();
        let stats = rebuild(jsonl_dir.path(), &rebuilt_db).unwrap();
        assert_eq!(stats.events_replayed, 3);

        for (label, table) in [
            ("daily", GAIN_DAILY),
            ("monthly", GAIN_MONTHLY),
            ("yearly", GAIN_YEARLY),
        ] {
            let mut a = scan(&incremental_db, table).unwrap();
            let mut b = scan(&rebuilt_db, table).unwrap();
            a.sort_by(|x, y| format!("{:?}", x.0).cmp(&format!("{:?}", y.0)));
            b.sort_by(|x, y| format!("{:?}", x.0).cmp(&format!("{:?}", y.0)));
            assert_eq!(a, b, "rebuild must match incremental for {label}");
        }
    }

    #[test]
    fn rebuild_discards_stale_rows_not_in_the_log() {
        let db_dir = tempdir().unwrap();
        let jsonl_dir = tempdir().unwrap();
        let db = Database::create(db_dir.path().join("token.redb")).unwrap();

        // A row written directly (simulating a stale/incremental artifact)
        // that is NOT backed by any raw JSONL event.
        update(
            &db,
            &event(1_783_900_800, Surface::CliSearch, "stale", 1, 1),
        )
        .unwrap();
        assert_eq!(scan(&db, GAIN_DAILY).unwrap().len(), 1);

        // The raw log is empty — rebuild must wipe the stale row.
        let stats = rebuild(jsonl_dir.path(), &db).unwrap();
        assert_eq!(stats.events_replayed, 0);
        assert_eq!(scan(&db, GAIN_DAILY).unwrap().len(), 0);
    }

    #[test]
    fn rebuild_includes_archived_epochs() {
        // A `--reset` moves the current window's raw files under
        // `<gain_dir>/archive/<ts>-<label>/` without deleting them — rebuild
        // must still count them (rollups are the all-time cumulative view).
        let db_dir = tempdir().unwrap();
        let jsonl_dir = tempdir().unwrap();
        let db = Database::create(db_dir.path().join("token.redb")).unwrap();

        let writer = JsonlGainWriter::new(jsonl_dir.path());
        writer
            .append(&event(
                1_783_900_800,
                Surface::CliSearch,
                "whitespace",
                100,
                50,
            ))
            .unwrap();

        let archived = jsonl_dir.path().join("archive").join("1700000000-baseline");
        std::fs::create_dir_all(&archived).unwrap();
        let archived_writer = JsonlGainWriter::new(&archived);
        archived_writer
            .append(&event(
                1_700_000_000,
                Surface::McpQuery,
                "snippet",
                200,
                100,
            ))
            .unwrap();

        let stats = rebuild(jsonl_dir.path(), &db).unwrap();
        assert_eq!(stats.events_replayed, 2, "archived epoch must be counted");

        let result =
            crate::gain::pivot::query(&db, &crate::gain::pivot::PivotQuery::default()).unwrap();
        assert_eq!(result.totals.count, 2);
        assert_eq!(result.totals.bytes_before, 300);
    }

    #[test]
    fn scan_on_fresh_db_returns_empty_not_error() {
        let dir = tempdir().unwrap();
        let db = Database::create(dir.path().join("token.redb")).unwrap();
        assert_eq!(scan(&db, GAIN_DAILY).unwrap(), Vec::new());
    }

    #[test]
    fn parse_key_round_trips_composite_key() {
        let e = event(1_783_900_800, Surface::McpMultiGet, "head_only", 1, 1);
        let raw = composite_key("2026-07-11", &e);
        let parsed = parse_key(&raw).unwrap();
        assert_eq!(parsed.period, "2026-07-11");
        assert_eq!(parsed.surface, "mcp_multi_get");
        assert_eq!(parsed.transform, "head_only");
        assert_eq!(parsed.level, "conservative");
        assert_eq!(parsed.cache, "none");
    }
}
