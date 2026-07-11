//! `onebrain token gain` — report token-optimization savings: a default
//! all-time summary (plus "since reset" when a `--reset` epoch marker
//! exists), a `--by <time,dim>` pivot, `--since` custom windows,
//! `--history` (raw JSONL tail), `--reset --label` epoch archiving, and
//! `--rebuild` rollup recovery.
//!
//! No live traffic writes `GainEvent`s yet in this PR — Track 4 wires the
//! MCP/CLI/daemon surfaces through `onebrain_token::run_funnel`. This
//! command is the reporting/administration surface over whatever
//! `token.redb` + the raw JSONL log already contain; against a fresh vault
//! every mode below reports zeroes, which is the correct, honest answer.
//!
//! Renders exclusively through [`emit`] / [`Envelope`] — no hand-rolled
//! printer (design §5d: every new command goes through the canonical
//! dispatcher).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Datelike;
use serde::{Deserialize, Serialize};

use crate::cli::TokenGainArgs;
use crate::commands::search_common::{collection_cache_dir, resolve_collection};
use crate::output::{emit, Envelope, OutputMode};
use onebrain_token::gain::{pivot, rollup};
use onebrain_token::{
    Database, Dim, GainEvent, JsonlGainWriter, PivotQuery, PivotResult, PivotTotals, TimeAxis,
};

/// `<collection_cache>/token/` — a new sibling of `models/` and `index/`
/// (design §1). `token.redb` and `gain/` both live directly under it.
fn token_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("token")
}

fn gain_dir(tok_dir: &Path) -> PathBuf {
    tok_dir.join("gain")
}

fn redb_path(tok_dir: &Path) -> PathBuf {
    tok_dir.join("token.redb")
}

const RESET_MARKER_FILE: &str = ".reset_marker.json";

/// Records the boundary of the most recent `--reset` so the default summary
/// can show "since reset" alongside the all-time total. Rollups themselves
/// are never wiped on reset — see [`run`]'s reset branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResetMarker {
    ts: i64,
    label: String,
}

fn read_reset_marker(gdir: &Path) -> Option<ResetMarker> {
    let content = std::fs::read_to_string(gdir.join(RESET_MARKER_FILE)).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_reset_marker(gdir: &Path, marker: &ResetMarker) -> Result<()> {
    std::fs::create_dir_all(gdir).context("creating gain dir")?;
    let path = gdir.join(RESET_MARKER_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(marker)?)
        .with_context(|| format!("writing {}", path.display()))
}

fn day_string(ts: i64) -> String {
    let dt =
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).unwrap_or_else(chrono::Utc::now);
    format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day())
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Move every top-level `*.jsonl` file in `gdir` into
/// `gdir/archive/<ts>-<label>/` (create · rename — never deletes). Returns
/// the archive directory. Idempotent-safe to call on an empty/missing
/// `gdir` (nothing to move, archive dir still created so the path is real).
fn archive_epoch(gdir: &Path, ts: i64, label: &str) -> Result<PathBuf> {
    let archive_dir = gdir.join("archive").join(format!("{ts}-{label}"));
    std::fs::create_dir_all(&archive_dir)
        .with_context(|| format!("creating {}", archive_dir.display()))?;
    if gdir.exists() {
        for entry in std::fs::read_dir(gdir).context("reading gain dir")? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let dest = archive_dir.join(entry.file_name());
                std::fs::rename(&path, &dest).with_context(|| {
                    format!("archiving {} to {}", path.display(), dest.display())
                })?;
            }
        }
    }
    Ok(archive_dir)
}

/// `--by` axis parsing. Order-independent: each comma-separated token is
/// classified by trying the time-axis vocabulary then the dimension
/// vocabulary (the two never overlap), so `--by month,surface` and `--by
/// surface,month` are equivalent.
fn parse_time_axis(s: &str) -> Option<TimeAxis> {
    match s {
        "day" => Some(TimeAxis::Day),
        "week" => Some(TimeAxis::Week),
        "month" => Some(TimeAxis::Month),
        "year" => Some(TimeAxis::Year),
        _ => None,
    }
}

fn parse_dim(s: &str) -> Option<Dim> {
    match s {
        "surface" => Some(Dim::Surface),
        "transform" => Some(Dim::Transform),
        "level" => Some(Dim::Level),
        "cache" => Some(Dim::Cache),
        _ => None,
    }
}

fn parse_by(by: Option<&str>) -> Result<(Option<TimeAxis>, Option<Dim>)> {
    let Some(by) = by else {
        return Ok((None, None));
    };
    let mut time = None;
    let mut dim = None;
    for part in by.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(t) = parse_time_axis(part) {
            if time.replace(t).is_some() {
                anyhow::bail!("--by: multiple time axes given (saw a second {part:?})");
            }
        } else if let Some(d) = parse_dim(part) {
            if dim.replace(d).is_some() {
                anyhow::bail!("--by: multiple dimensions given (saw a second {part:?})");
            }
        } else {
            anyhow::bail!(
                "--by: unrecognized axis {part:?} — time: day|week|month|year, \
                 dim: surface|transform|level|cache"
            );
        }
    }
    Ok((time, dim))
}

/// Recent-log cap for `--history` — "recent per-call log" (design §5), not
/// an unbounded raw-log dump. Oldest-first order is preserved; only the tail
/// is kept.
const HISTORY_LIMIT: usize = 200;

fn filter_since(events: Vec<GainEvent>, since: Option<&str>) -> Vec<GainEvent> {
    let Some(since) = since else {
        return events;
    };
    let Ok(date) = chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d") else {
        return events;
    };
    let Some(boundary) = date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp()) else {
        return events;
    };
    events.into_iter().filter(|e| e.ts >= boundary).collect()
}

fn tail(mut events: Vec<GainEvent>, limit: usize) -> Vec<GainEvent> {
    if events.len() > limit {
        events.drain(0..events.len() - limit);
    }
    events
}

/// `token gain`'s response payload — one struct covers every mode (only the
/// relevant optional fields are populated) so the whole command shares one
/// `Envelope<TokenGainData>` type, per the v3.1 output-stack convention.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TokenGainData {
    #[serde(flatten)]
    pivot: PivotResult,
    /// The boundary date of the most recent `--reset`, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    since_reset: Option<String>,
    /// Populated for `--history`.
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Vec<GainEvent>>,
    /// Populated for `--rebuild`.
    #[serde(skip_serializing_if = "Option::is_none")]
    rebuilt_events: Option<usize>,
    /// Populated for `--reset`.
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_to: Option<String>,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &TokenGainArgs) -> Result<()> {
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let collection = collection.context("no search collection resolved for this vault")?;

    let cache_dir = collection_cache_dir(&collection);
    let tok_dir = token_dir(&cache_dir);
    let gdir = gain_dir(&tok_dir);
    std::fs::create_dir_all(&tok_dir).context("creating token cache dir")?;
    let db = Database::create(redb_path(&tok_dir)).context("opening token.redb")?;
    rollup::ensure_tables(&db).context("ensuring rollup tables")?;

    // `--json` is a local shorthand (mirrors `doctor --json` / `update
    // --json`) — it still renders through the SAME `emit`/`Envelope`
    // dispatcher as every other output mode, never a bespoke printer.
    let effective_mode = if args.json && !mode.is_structured() {
        OutputMode::Json { pretty: false }
    } else {
        mode.clone()
    };

    if args.rebuild {
        let stats =
            rollup::rebuild(&gdir, &db).context("rebuilding rollups from the raw gain log")?;
        let data = TokenGainData {
            pivot: pivot::query(&db, &PivotQuery::default()).context("querying rebuilt rollups")?,
            since_reset: read_reset_marker(&gdir).map(|m| day_string(m.ts)),
            history: None,
            rebuilt_events: Some(stats.events_replayed),
            archived_to: None,
        };
        let envelope = Envelope::ok("token.gain", Some(vault_info), data);
        return emit(
            &envelope,
            &effective_mode,
            std::io::stdout().lock(),
            render_text,
        );
    }

    if args.reset {
        let ts = now_ts();
        let label = args.label.clone().unwrap_or_else(|| "reset".to_string());
        let archived_to = archive_epoch(&gdir, ts, &label)?;
        write_reset_marker(
            &gdir,
            &ResetMarker {
                ts,
                label: label.clone(),
            },
        )?;
        // Rollups are the all-time cumulative view and are NEVER wiped by a
        // reset (design §5: "keep everything" · archived epochs remain
        // queryable via `--rebuild`, which walks `gain/archive/**` too).
        let data = TokenGainData {
            pivot: pivot::query(&db, &PivotQuery::default())
                .context("querying rollups after reset")?,
            since_reset: Some(day_string(ts)),
            history: None,
            rebuilt_events: None,
            archived_to: Some(archived_to.display().to_string()),
        };
        let envelope = Envelope::ok("token.gain", Some(vault_info), data);
        return emit(
            &envelope,
            &effective_mode,
            std::io::stdout().lock(),
            render_text,
        );
    }

    if args.history {
        let events = JsonlGainWriter::new(&gdir)
            .read_all()
            .context("reading raw gain log")?;
        let events = tail(filter_since(events, args.since.as_deref()), HISTORY_LIMIT);
        let data = TokenGainData {
            pivot: PivotResult {
                rows: Vec::new(),
                totals: PivotTotals::default(),
            },
            since_reset: read_reset_marker(&gdir).map(|m| day_string(m.ts)),
            history: Some(events),
            rebuilt_events: None,
            archived_to: None,
        };
        let envelope = Envelope::ok("token.gain", Some(vault_info), data);
        return emit(
            &envelope,
            &effective_mode,
            std::io::stdout().lock(),
            render_text,
        );
    }

    // Default: summary (no axes) or `--by` pivot.
    let (time, dim) = parse_by(args.by.as_deref())?;
    let query = PivotQuery {
        time,
        dim,
        since: args.since.clone(),
    };
    let result = pivot::query(&db, &query).context("querying gain rollups")?;
    let data = TokenGainData {
        pivot: result,
        since_reset: read_reset_marker(&gdir).map(|m| day_string(m.ts)),
        history: None,
        rebuilt_events: None,
        archived_to: None,
    };
    let envelope = Envelope::ok("token.gain", Some(vault_info), data);
    emit(
        &envelope,
        &effective_mode,
        std::io::stdout().lock(),
        render_text,
    )
}

// ─────────────────────────────────────────────────────────────────────────
// Text rendering
// ─────────────────────────────────────────────────────────────────────────

/// Bytes saved as a percentage of `bytes_before`, `0` when there's nothing
/// to divide by (no calls recorded yet).
fn savings_pct(totals: &PivotTotals) -> f64 {
    if totals.bytes_before == 0 {
        return 0.0;
    }
    let saved = totals.bytes_before.saturating_sub(totals.bytes_after);
    (saved as f64 / totals.bytes_before as f64) * 100.0
}

fn bar(pct: f64) -> String {
    const WIDTH: usize = 20;
    let filled = ((pct / 100.0) * WIDTH as f64)
        .round()
        .clamp(0.0, WIDTH as f64) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled))
}

fn render_summary(totals: &PivotTotals) -> String {
    let saved = totals.bytes_before.saturating_sub(totals.bytes_after);
    let pct = savings_pct(totals);
    let mut out = String::new();
    out.push_str("Token Optimization — Gain Summary\n");
    out.push_str(&format!("  Calls:        {}\n", totals.count));
    out.push_str(&format!("  Bytes before: {}\n", totals.bytes_before));
    out.push_str(&format!("  Bytes after:  {}\n", totals.bytes_after));
    out.push_str(&format!(
        "  Bytes saved:  {saved} ({pct:.1}%)\n  [{}] {pct:.0}%\n",
        bar(pct)
    ));
    out
}

fn render_pivot_table(rows: &[onebrain_token::PivotRow]) -> String {
    let has_time = rows.iter().any(|r| r.time.is_some());
    let has_dim = rows.iter().any(|r| r.dim.is_some());
    let mut out = String::new();
    let header_time = if has_time { "time" } else { "" };
    let header_dim = if has_dim { "dim" } else { "" };
    out.push_str(&format!(
        "{:<12} {:<14} {:>12} {:>12} {:>12} {:>8}\n",
        header_time, header_dim, "before", "after", "saved", "calls"
    ));
    for row in rows {
        let saved = row
            .totals
            .bytes_before
            .saturating_sub(row.totals.bytes_after);
        out.push_str(&format!(
            "{:<12} {:<14} {:>12} {:>12} {:>12} {:>8}\n",
            row.time.as_deref().unwrap_or(""),
            row.dim.as_deref().unwrap_or(""),
            row.totals.bytes_before,
            row.totals.bytes_after,
            saved,
            row.totals.count,
        ));
    }
    out
}

fn render_history(events: &[GainEvent]) -> String {
    if events.is_empty() {
        return "No gain events recorded yet.\n".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<12} {:<20} {:<12} {:>10} {:>10}  {}\n",
        "ts", "surface", "transform", "level", "before", "after", "cache"
    ));
    for e in events {
        out.push_str(&format!(
            "{:<20} {:<12} {:<20} {:<12} {:>10} {:>10}  {}\n",
            day_string(e.ts),
            e.surface,
            e.transform,
            e.level,
            e.bytes_before,
            e.bytes_after,
            e.cache,
        ));
    }
    out
}

const ESTIMATE_NOTE: &str = "Note: byte counts are exact; per-model token figures are an estimate — see `docs/token-optimization.md`.";

fn render_text(env: &Envelope<TokenGainData>) -> String {
    let Some(data) = env.data.as_ref() else {
        return String::new();
    };

    if let Some(n) = data.rebuilt_events {
        let mut out = format!("Rebuilt rollups from {n} raw event(s).\n\n");
        out.push_str(&render_summary(&data.pivot.totals));
        return out;
    }

    if let Some(archived_to) = &data.archived_to {
        return format!(
            "Archived current window to {archived_to} (never deleted).\n\
             Counting fresh from now — \"since reset\" starts at {}.\n",
            data.since_reset.as_deref().unwrap_or("today")
        );
    }

    if let Some(history) = &data.history {
        return render_history(history);
    }

    let mut out = if data.pivot.rows.len() <= 1 {
        render_summary(&data.pivot.totals)
    } else {
        let mut s = render_pivot_table(&data.pivot.rows);
        s.push('\n');
        s.push_str(&render_summary(&data.pivot.totals));
        s
    };
    if let Some(since_reset) = &data.since_reset {
        out.push_str(&format!("  (since reset: {since_reset})\n"));
    }
    out.push_str(ESTIMATE_NOTE);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_token::{CacheKind, OptLevel, Surface};

    #[test]
    fn parse_by_none_yields_no_axes() {
        assert_eq!(parse_by(None).unwrap(), (None, None));
    }

    #[test]
    fn parse_by_time_only() {
        assert_eq!(
            parse_by(Some("month")).unwrap(),
            (Some(TimeAxis::Month), None)
        );
    }

    #[test]
    fn parse_by_dim_only() {
        assert_eq!(
            parse_by(Some("surface")).unwrap(),
            (None, Some(Dim::Surface))
        );
    }

    #[test]
    fn parse_by_both_axes_order_independent() {
        assert_eq!(
            parse_by(Some("month,surface")).unwrap(),
            (Some(TimeAxis::Month), Some(Dim::Surface))
        );
        assert_eq!(
            parse_by(Some("surface,month")).unwrap(),
            (Some(TimeAxis::Month), Some(Dim::Surface))
        );
    }

    #[test]
    fn parse_by_rejects_unrecognized_axis() {
        let err = parse_by(Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn parse_by_rejects_duplicate_time_axis() {
        let err = parse_by(Some("day,week")).unwrap_err();
        assert!(err.to_string().contains("multiple time axes"));
    }

    #[test]
    fn parse_by_rejects_duplicate_dim() {
        let err = parse_by(Some("surface,transform")).unwrap_err();
        assert!(err.to_string().contains("multiple dimensions"));
    }

    fn sample_event(ts: i64) -> GainEvent {
        GainEvent {
            ts,
            surface: Surface::CliSearch,
            transform: "whitespace".to_string(),
            level: OptLevel::Conservative,
            bytes_before: 1000,
            bytes_after: 400,
            cache: CacheKind::None,
            session_token: None,
        }
    }

    #[test]
    fn filter_since_keeps_events_on_or_after_boundary() {
        let events = vec![sample_event(1_783_728_000), sample_event(1_785_542_400)];
        // 1_783_728_000 = 2026-07-11, 1_785_542_400 = 2026-08-01.
        let filtered = filter_since(events, Some("2026-08-01"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].ts, 1_785_542_400);
    }

    #[test]
    fn filter_since_none_returns_everything() {
        let events = vec![sample_event(1_783_728_000), sample_event(1_785_542_400)];
        assert_eq!(filter_since(events, None).len(), 2);
    }

    #[test]
    fn filter_since_unparseable_date_returns_everything() {
        let events = vec![sample_event(1_783_728_000)];
        assert_eq!(filter_since(events, Some("not-a-date")).len(), 1);
    }

    #[test]
    fn tail_caps_to_the_most_recent_n_keeping_order() {
        let events: Vec<GainEvent> = (0..5).map(|i| sample_event(1_783_728_000 + i)).collect();
        let capped = tail(events, 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].ts, 1_783_728_000 + 3);
        assert_eq!(capped[1].ts, 1_783_728_000 + 4);
    }

    #[test]
    fn tail_under_limit_is_a_no_op() {
        let events: Vec<GainEvent> = (0..2).map(|i| sample_event(1_783_728_000 + i)).collect();
        assert_eq!(tail(events, 5).len(), 2);
    }

    #[test]
    fn savings_pct_zero_before_is_zero_not_nan() {
        let totals = PivotTotals::default();
        assert_eq!(savings_pct(&totals), 0.0);
    }

    #[test]
    fn savings_pct_computes_expected_ratio() {
        let totals = PivotTotals {
            bytes_before: 1000,
            bytes_after: 400,
            count: 1,
        };
        assert!((savings_pct(&totals) - 60.0).abs() < 0.001);
    }

    #[test]
    fn bar_is_full_width_at_100_percent() {
        let b = bar(100.0);
        assert_eq!(b.chars().count(), 20);
        assert!(!b.contains('░'));
    }

    #[test]
    fn bar_is_empty_at_0_percent() {
        let b = bar(0.0);
        assert_eq!(b.chars().count(), 20);
        assert!(!b.contains('█'));
    }

    #[test]
    fn render_summary_labels_estimate_via_caller_note() {
        // The estimate note lives in `render_text`, not `render_summary` —
        // this pins that `render_text`'s default path always appends it.
        let env = Envelope::ok(
            "token.gain",
            None,
            TokenGainData {
                pivot: PivotResult {
                    rows: Vec::new(),
                    totals: PivotTotals::default(),
                },
                since_reset: None,
                history: None,
                rebuilt_events: None,
                archived_to: None,
            },
        );
        let text = render_text(&env);
        assert!(text.contains("estimate"), "{text}");
    }

    #[test]
    fn render_text_history_mode_lists_events() {
        let env = Envelope::ok(
            "token.gain",
            None,
            TokenGainData {
                pivot: PivotResult {
                    rows: Vec::new(),
                    totals: PivotTotals::default(),
                },
                since_reset: None,
                history: Some(vec![sample_event(1_783_728_000)]),
                rebuilt_events: None,
                archived_to: None,
            },
        );
        let text = render_text(&env);
        assert!(text.contains("cli_search"), "{text}");
        assert!(text.contains("whitespace"), "{text}");
    }

    #[test]
    fn render_text_reset_mode_mentions_archive_path() {
        let env = Envelope::ok(
            "token.gain",
            None,
            TokenGainData {
                pivot: PivotResult {
                    rows: Vec::new(),
                    totals: PivotTotals::default(),
                },
                since_reset: Some("2026-07-11".to_string()),
                history: None,
                rebuilt_events: None,
                archived_to: Some("/vault/.cache/token/gain/archive/1-baseline".to_string()),
            },
        );
        let text = render_text(&env);
        assert!(text.contains("archive/1-baseline"), "{text}");
        assert!(text.contains("2026-07-11"), "{text}");
    }

    #[test]
    fn render_text_rebuild_mode_reports_event_count() {
        let env = Envelope::ok(
            "token.gain",
            None,
            TokenGainData {
                pivot: PivotResult {
                    rows: Vec::new(),
                    totals: PivotTotals::default(),
                },
                since_reset: None,
                history: None,
                rebuilt_events: Some(42),
                archived_to: None,
            },
        );
        let text = render_text(&env);
        assert!(text.contains("42"), "{text}");
    }

    #[test]
    fn archive_epoch_moves_top_level_jsonl_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let gdir = dir.path().join("gain");
        std::fs::create_dir_all(&gdir).unwrap();
        std::fs::write(gdir.join("2026-07.jsonl"), "{}\n").unwrap();

        let archive_dir = archive_epoch(&gdir, 1_700_000_000, "baseline").unwrap();
        assert!(archive_dir.join("2026-07.jsonl").is_file());
        assert!(!gdir.join("2026-07.jsonl").exists());
    }

    #[test]
    fn archive_epoch_on_missing_gain_dir_still_creates_archive_dir() {
        let dir = tempfile::tempdir().unwrap();
        let gdir = dir.path().join("gain-does-not-exist");
        let archive_dir = archive_epoch(&gdir, 1_700_000_000, "baseline").unwrap();
        assert!(archive_dir.is_dir());
    }

    #[test]
    fn reset_marker_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ResetMarker {
            ts: 1_700_000_000,
            label: "baseline".to_string(),
        };
        write_reset_marker(dir.path(), &marker).unwrap();
        let read = read_reset_marker(dir.path()).unwrap();
        assert_eq!(read.ts, marker.ts);
        assert_eq!(read.label, marker.label);
    }

    #[test]
    fn reset_marker_absent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_reset_marker(dir.path()).is_none());
    }
}
