//! One pivot engine (design 0030 §5), shared by every gain-data consumer:
//! the CLI's `token gain` summary/pivot/`--json`, the later `/api/token/gain`
//! daemon route, and the WebUI dashboard. All three render the exact same
//! [`PivotResult`] — "one pivot engine ... serves all three consumers," not
//! three re-derivations of the same aggregation logic.
//!
//! Always sources from the [`GAIN_DAILY`] rollup table (the finest tier-2
//! table), never the raw JSONL: `week` buckets and `--since` windows both
//! need day-level granularity to compute correctly, and the daily table is
//! still a small, precomputed rollup — scanning it is not the "live scan of
//! the append-only log" the design explicitly reserves for `--history`.
//! `month`/`week`/`year` buckets are re-grouped from daily rows in memory.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use redb::Database;
use serde::{Deserialize, Serialize};

use super::rollup::{scan, RollupError, RollupKey, GAIN_DAILY};

/// Time-axis granularity for a pivot's `time` bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeAxis {
    Day,
    Week,
    Month,
    Year,
}

/// Which `GainEvent` dimension to break a pivot's `dim` bucket out by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dim {
    Surface,
    Transform,
    Level,
    Cache,
}

/// A pivot request. Either axis alone is valid (design §5); both `None`
/// collapses to a single grand-total row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotQuery {
    pub time: Option<TimeAxis>,
    pub dim: Option<Dim>,
    /// Inclusive lower bound (`YYYY-MM-DD`). `None` = no lower bound.
    pub since: Option<String>,
}

/// Summed byte/count counters — the same shape for a pivot row and for the
/// grand total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotTotals {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub count: u64,
}

impl PivotTotals {
    fn add_row(&mut self, other: &PivotTotals) {
        self.bytes_before += other.bytes_before;
        self.bytes_after += other.bytes_after;
        self.count += other.count;
    }
}

/// One bucketed row of a [`PivotResult`]. `time`/`dim` are `None` when the
/// query didn't request that axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    #[serde(flatten)]
    pub totals: PivotTotals,
}

/// The full pivot response — the same struct the CLI `--json`, the daemon
/// route, and the WebUI all consume verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotResult {
    pub rows: Vec<PivotRow>,
    pub totals: PivotTotals,
}

fn time_bucket(axis: TimeAxis, day_period: &str) -> String {
    match axis {
        TimeAxis::Day => day_period.to_string(),
        TimeAxis::Month => day_period.get(0..7).unwrap_or(day_period).to_string(),
        TimeAxis::Year => day_period.get(0..4).unwrap_or(day_period).to_string(),
        TimeAxis::Week => NaiveDate::parse_from_str(day_period, "%Y-%m-%d")
            .map(|d| {
                let iso = d.iso_week();
                format!("{:04}-W{:02}", iso.year(), iso.week())
            })
            // A malformed period (should never happen — this module only
            // ever reads keys it wrote) falls back to the raw string rather
            // than panicking on a reporting path.
            .unwrap_or_else(|_| day_period.to_string()),
    }
}

fn dim_value(dim: Dim, key: &RollupKey) -> String {
    match dim {
        Dim::Surface => key.surface.clone(),
        Dim::Transform => key.transform.clone(),
        Dim::Level => key.level.clone(),
        Dim::Cache => key.cache.clone(),
    }
}

/// Run `q` against `db`'s daily rollup table.
pub fn query(db: &Database, q: &PivotQuery) -> Result<PivotResult, RollupError> {
    let daily_rows = scan(db, GAIN_DAILY)?;

    let mut buckets: BTreeMap<(Option<String>, Option<String>), PivotTotals> = BTreeMap::new();
    let mut totals = PivotTotals::default();

    for (key, value) in &daily_rows {
        if let Some(since) = &q.since {
            if &key.period < since {
                continue;
            }
        }
        let row_totals = PivotTotals {
            bytes_before: value.bytes_before,
            bytes_after: value.bytes_after,
            count: value.count,
        };
        totals.add_row(&row_totals);

        let time = q.time.map(|axis| time_bucket(axis, &key.period));
        let dim = q.dim.map(|d| dim_value(d, key));
        buckets.entry((time, dim)).or_default().add_row(&row_totals);
    }

    let mut rows: Vec<PivotRow> = buckets
        .into_iter()
        .map(|((time, dim), totals)| PivotRow { time, dim, totals })
        .collect();
    rows.sort_by(|a, b| (&a.time, &a.dim).cmp(&(&b.time, &b.dim)));

    Ok(PivotResult { rows, totals })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gain::event::{CacheKind, GainEvent, Surface};
    use crate::gain::rollup::update;
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

    fn seeded_db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::create(dir.path().join("token.redb")).unwrap();
        // 2026-07-11T00:00:00Z (Sat, ISO week 28), 2026-07-13T00:00:00Z (Mon,
        // ISO week 29), and 2026-08-01T00:00:00Z (Sat, ISO week 31 — next
        // month, different week).
        update(
            &db,
            &event(1_783_728_000, Surface::CliSearch, "whitespace", 1000, 400),
        )
        .unwrap();
        update(
            &db,
            &event(1_783_900_800, Surface::McpQuery, "snippet", 500, 100),
        )
        .unwrap();
        update(
            &db,
            &event(1_785_542_400, Surface::CliSearch, "whitespace", 2000, 1000),
        )
        .unwrap();
        (dir, db)
    }

    #[test]
    fn no_axes_collapses_to_one_grand_total_row() {
        let (_dir, db) = seeded_db();
        let result = query(&db, &PivotQuery::default()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0].time.is_none());
        assert!(result.rows[0].dim.is_none());
        assert_eq!(result.rows[0].totals, result.totals);
        assert_eq!(result.totals.bytes_before, 3500);
        assert_eq!(result.totals.bytes_after, 1500);
        assert_eq!(result.totals.count, 3);
    }

    #[test]
    fn by_month_and_surface_matrix_includes_totals() {
        let (_dir, db) = seeded_db();
        let q = PivotQuery {
            time: Some(TimeAxis::Month),
            dim: Some(Dim::Surface),
            since: None,
        };
        let result = query(&db, &q).unwrap();
        // Two July rows (cli_search, mcp_query) + one August row (cli_search).
        assert_eq!(result.rows.len(), 3);
        assert!(result.rows.iter().any(
            |r| r.time.as_deref() == Some("2026-07") && r.dim.as_deref() == Some("cli_search")
        ));
        assert!(
            result
                .rows
                .iter()
                .any(|r| r.time.as_deref() == Some("2026-07")
                    && r.dim.as_deref() == Some("mcp_query"))
        );
        assert!(result.rows.iter().any(
            |r| r.time.as_deref() == Some("2026-08") && r.dim.as_deref() == Some("cli_search")
        ));
        assert_eq!(result.totals.count, 3);
    }

    #[test]
    fn single_axis_time_only_is_valid() {
        let (_dir, db) = seeded_db();
        let q = PivotQuery {
            time: Some(TimeAxis::Day),
            dim: None,
            since: None,
        };
        let result = query(&db, &q).unwrap();
        assert_eq!(result.rows.len(), 3, "three distinct days in the fixture");
        assert!(result.rows.iter().all(|r| r.dim.is_none()));
    }

    #[test]
    fn single_axis_dim_only_is_valid() {
        let (_dir, db) = seeded_db();
        let q = PivotQuery {
            time: None,
            dim: Some(Dim::Surface),
            since: None,
        };
        let result = query(&db, &q).unwrap();
        assert_eq!(result.rows.len(), 2, "cli_search and mcp_query");
        assert!(result.rows.iter().all(|r| r.time.is_none()));
    }

    #[test]
    fn since_window_excludes_earlier_rows() {
        let (_dir, db) = seeded_db();
        let q = PivotQuery {
            time: Some(TimeAxis::Day),
            dim: None,
            since: Some("2026-08-01".to_string()),
        };
        let result = query(&db, &q).unwrap();
        assert_eq!(result.rows.len(), 1, "only the August row survives --since");
        assert_eq!(result.totals.count, 1);
    }

    #[test]
    fn week_bucketing_uses_iso_week() {
        let (_dir, db) = seeded_db();
        let q = PivotQuery {
            time: Some(TimeAxis::Week),
            dim: None,
            since: None,
        };
        let result = query(&db, &q).unwrap();
        // The two July events land two days apart (2026-07-11 Sat, 2026-07-13
        // Mon) — different ISO weeks — plus the August event in a third week.
        assert_eq!(result.rows.len(), 3);
        assert!(result.rows.iter().all(|r| r
            .time
            .as_deref()
            .map(|t| t.contains("-W"))
            .unwrap_or(false)));
    }

    #[test]
    fn empty_db_returns_zeroed_totals_and_no_rows_with_axes() {
        let dir = tempdir().unwrap();
        let db = Database::create(dir.path().join("token.redb")).unwrap();
        let q = PivotQuery {
            time: Some(TimeAxis::Day),
            dim: Some(Dim::Surface),
            since: None,
        };
        let result = query(&db, &q).unwrap();
        assert!(result.rows.is_empty());
        assert_eq!(result.totals, PivotTotals::default());
    }
}
