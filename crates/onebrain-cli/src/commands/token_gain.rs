//! `onebrain token gain` — report token-optimization savings: a default
//! all-time summary (plus "since reset" when a `--reset` epoch marker
//! exists), a `--by <time,dim>` pivot, `--since` custom windows,
//! `--history` (raw JSONL tail), `--reset --label` epoch archiving, and
//! `--rebuild` rollup recovery.
//!
//! Live surfaces (`search get`, the MCP `get`/`query`, the read-hook) write
//! `GainEvent`s through `onebrain_token::run_funnel`; this command is the
//! reporting/administration surface over whatever `token.redb` + the raw JSONL
//! log already contain. Against a fresh vault every mode below reports zeroes,
//! which is the correct, honest answer.
//!
//! Renders exclusively through [`emit`] / [`Envelope`] — no hand-rolled
//! printer (design §5d: every new command goes through the canonical
//! dispatcher).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Datelike;
use serde::{Deserialize, Serialize};

use onebrain_core::path::ResolvedVault;

use crate::cli::TokenGainArgs;
use crate::commands::daemon_client::{self, DaemonHandle};
use crate::commands::search_common::{
    collection_cache_dir, daemon_routing_disabled, resolve_collection,
};
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
    // Fall back to the Unix EPOCH (not the current time) for an out-of-range
    // `ts`, matching `onebrain_token`'s rollup `utc_or_epoch`: a corrupt value
    // buckets under 1970 (self-flagging) instead of silently under today.
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
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

/// Parse a `--by`/`?by=` value (`time,dim` in either order, either alone) into
/// its `(time, dim)` axes. `pub(crate)` so the daemon `/api/token/gain` route
/// reuses the EXACT same parsing as the CLI — one code path for the by-axes,
/// no daemon/CLI drift.
pub(crate) fn parse_by(by: Option<&str>) -> Result<(Option<TimeAxis>, Option<Dim>)> {
    let Some(by) = by else {
        return Ok((None, None));
    };
    let mut time = None;
    let mut dim = None;
    for part in by.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // Contract errors ride a `HintedError`, never a literal-glyph bail:
        // Display = the plain line only, so the JSON error envelope and the
        // daemon's `/api/token/gain` reuse (`e.to_string()` → BadRequest
        // body) stay single-line and glyph-free; the ✗/💡 dressing is added
        // by `main::render_error` in text mode. `{part:?}` (Debug) keeps
        // escaping fidelity for weird input.
        if let Some(t) = parse_time_axis(part) {
            if time.replace(t).is_some() {
                return Err(anyhow::Error::new(crate::output::HintedError::new(
                    format!(
                        "--by takes only one time axis (multiple time axes given: saw a second {part:?})"
                    ),
                    "use exactly one of day, week, month, or year — e.g. `--by month,surface`",
                )));
            }
        } else if let Some(d) = parse_dim(part) {
            if dim.replace(d).is_some() {
                return Err(anyhow::Error::new(crate::output::HintedError::new(
                    format!(
                        "--by takes only one dimension (multiple dimensions given: saw a second {part:?})"
                    ),
                    "use exactly one of surface, transform, level, or cache — e.g. `--by month,surface`",
                )));
            }
        } else {
            return Err(anyhow::Error::new(crate::output::HintedError::new(
                format!(
                    "--by doesn't recognize {part:?} — expected a time axis (day, week, month, \
                     year) or a dimension (surface, transform, level, cache)"
                ),
                "combine at most one of each, e.g. `--by month,surface`",
            )));
        }
    }
    Ok((time, dim))
}

/// Strict `YYYY-MM-DD` shape check: exactly 4-2-2 ASCII digits joined by two
/// literal `-`s. `chrono::NaiveDate::parse_from_str(_, "%Y-%m-%d")` alone is
/// NOT enough — chrono's `%m`/`%d` specifiers are lenient on width when
/// PARSING (they require zero-padding only when FORMATTING), so `"2026-1-1"`
/// parses clean as 2026-01-01. That's the exact #287 bug: the resulting date
/// compares correctly in-memory but the caller only ever sees a valid
/// `NaiveDate`, never a rejection — the value later fails to line up with the
/// zero-padded `YYYY-MM-DD` keys the gain log buckets by, so `--since 2026-1-1`
/// silently reports zero instead of erroring. Checking the literal width
/// FIRST closes that gap; `NaiveDate::parse_from_str` below still does the
/// calendar-validity check (rejects `2026-13-01`, `2026-02-30`, etc).
fn is_strict_since_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// Validate a `--since` / `?since=` value. `None` or `""` (the "unset" guard
/// both the CLI and the daemon route apply BEFORE calling this — see [`run`]
/// and `server::token_api::get_token_gain`) short-circuit `Ok(())` — an empty
/// value is a deliberate no-op, never a malformed date. Anything else must be
/// a strict `YYYY-MM-DD` per [`is_strict_since_date`].
///
/// The error carries `CoreError::InvalidDate` so `exit::exit_code_for` maps
/// it to exit 70 (`EXIT_INVALID_DATE`) — until #287, that code existed but
/// was unreachable. `HintedError` rides on top via `.context(..)` (never a
/// rebuilt `anyhow!`), the SAME technique [`parse_by`] uses above and
/// `serve::contract_bind_error` uses for the PermissionDenied-66 case: the
/// original `CoreError` stays in the chain so the exit-code walk still finds
/// it, while `HintedError`'s `Display` (→ the JSON envelope's
/// `error.message`) stays the single-line, glyph-free `plain` text. The
/// daemon route reuses this AS-IS (`e.to_string()` → its 400 body), mirroring
/// the existing `?by=bogus` 400 path — one validator, one message, for both
/// the CLI arg-parse error and the wire-level check.
pub(crate) fn validate_since(since: Option<&str>) -> Result<()> {
    let Some(since) = since else { return Ok(()) };
    if since.is_empty() || is_strict_since_date(since) {
        return Ok(());
    }
    Err(
        anyhow::Error::new(onebrain_core::CoreError::InvalidDate(since.to_string())).context(
            crate::output::HintedError::new(
                format!("--since must be a date in YYYY-MM-DD form (got {since:?})"),
                "example: `--since 2026-07-01`",
            ),
        ),
    )
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
    /// `true` when the reported pivot spans **all epochs** (`--all-time` /
    /// `--since` / the `--rebuild` all-time summary); `false` when it is
    /// scoped to the current epoch (traffic since the last `--reset`). This
    /// is the honest scope flag consumers key off — never inferred from the
    /// presence of `since_reset` alone.
    all_time: bool,
    /// The boundary date of the most recent `--reset`, when one exists.
    /// Informational for consumers; drives the human-readable scope label.
    #[serde(skip_serializing_if = "Option::is_none")]
    since_reset: Option<String>,
    /// `true` when a current-epoch (`all_time == false`) report uses
    /// month/year bucketing while a reset epoch exists — those buckets can't
    /// reach the archived epoch, so the renderer nudges toward `--all-time`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cross_epoch_buckets_hidden: bool,
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

/// What+why for the rollup-lock EngineBusy error — single-line, no remedy.
/// This is what `CoreError::EngineBusy`'s own Display carries AND
/// `HintedError::plain` mirrors, so text mode's `✗ {plain}` and a JSON
/// consumer's `error.message` say the exact same (glyph-free) thing.
const ROLLUP_LOCK_WHAT: &str =
    "token.redb is held by another onebrain process (a running daemon owns the rollup DB)";

/// The actionable remedy — #288: previously baked into the SAME string as
/// [`ROLLUP_LOCK_WHAT`] and carried entirely inside `CoreError::EngineBusy`,
/// so it rode into the JSON envelope's `error.message` (remedy text where a
/// consumer expects only "what failed") and text mode showed a bare
/// `Error: search engine busy: ...` with no 💡 dressing — a miss of #279's
/// "no dead-end errors" contract. Wording is fact-corrected (#258/#281: only
/// `--rebuild` still touches `token.redb`; every read mode is JSONL-backed).
const ROLLUP_LOCK_REMEDY: &str =
    "every read mode (the default summary, `--by`, `--history`, `--all-time`, `--since`, and \
     `--reset`) reads the lock-free raw log and works regardless; only `--rebuild` needs the \
     rollup DB — retry once that process exits (e.g. `onebrain daemon stop`)";

/// The actionable, exit-77 error for a genuinely contended `token.redb`. Typed
/// as [`onebrain_core::CoreError::EngineBusy`] so it maps to `E_ENGINE_BUSY` /
/// exit 77 — the SAME transient-lock code `search query`/`vsearch`/`get` use
/// (`search_common::map_engine_open_error`), instead of a plain exit-1 generic
/// error that scripts can't tell apart from a real failure. `HintedError`
/// rides on top via `.context(..)` — never a rebuilt `anyhow::anyhow!` — so
/// the original `CoreError::EngineBusy` stays in the chain and
/// `exit::exit_code_for`'s downcast walk still finds it (exit 77 survives),
/// the same technique `vault_ctx::dress_vault_not_found` uses for exit 64.
fn rollup_busy_error() -> anyhow::Error {
    anyhow::Error::new(onebrain_core::CoreError::EngineBusy(
        ROLLUP_LOCK_WHAT.to_string(),
    ))
    .context(crate::output::HintedError::new(
        ROLLUP_LOCK_WHAT,
        ROLLUP_LOCK_REMEDY,
    ))
}

/// Open the Tier-2 rollup DB (`token.redb`) directly, for the ONE mode that
/// still needs it: `--rebuild` (every read mode is JSONL-backed since #281).
/// redb is single-process, so a running
/// `onebrain daemon` — the sole `token.redb` owner — makes this open fail with
/// `DatabaseAlreadyOpen`. When that's the cause we surface the actionable,
/// exit-77 [`rollup_busy_error`] (issue #258); any other open failure passes
/// through unchanged.
fn open_rollup_db_direct(tok_dir: &Path) -> Result<Database> {
    std::fs::create_dir_all(tok_dir).context("creating token cache dir")?;
    Database::create(redb_path(tok_dir))
        .context("opening token.redb")
        .map_err(|e| {
            if onebrain_search::error::is_redb_lock_error(&e) {
                rollup_busy_error()
            } else {
                e
            }
        })
}

/// Resolve an all-epoch pivot (`--all-time` / `--since`), reading the lock-free
/// gain JSONL raw log — every epoch, archived included ([`JsonlGainWriter::
/// read_all_recursive`]) — the SAME source of truth the default summary reads.
/// The Tier-2 rollup DB is no longer consulted: since #258 nothing populates it
/// automatically, so it read empty and `--all-time`/`--since` (and the dashboard)
/// were dark (#281). `--rebuild` remains its only reader/writer.
///
/// When a same-vault daemon is up we still route through its `GET /api/token/gain`
/// route (it reads the same JSONL, so the answers agree), adopting it REGARDLESS
/// of version ([`daemon_client::discover_same_vault_any_version`]) since the route
/// returns the version-stable [`PivotResult`]. A 404 (route absent) or a transport
/// error simply falls through to the Direct JSONL read — lock-free, so it always
/// works (no more `E_ENGINE_BUSY` catch-22). `ONEBRAIN_NO_DAEMON` forces the
/// Direct leg (operator kill-switch + deterministic tests).
fn resolve_all_epoch_pivot(
    resolved: &ResolvedVault,
    tok_dir: &Path,
    by: Option<&str>,
    all_time: bool,
    query: &PivotQuery,
) -> Result<PivotResult> {
    if !daemon_routing_disabled() {
        if let Ok(Some(handle)) =
            daemon_client::discover_same_vault_any_version(Some(resolved.root.as_path()))
        {
            // `Some(pivot)` = the daemon answered. `None` = route absent (404) or
            // the daemon vanished (transport error); either way its JSONL is the
            // same lock-free files we can read ourselves, so fall THROUGH to Direct.
            if let Some(pivot) =
                daemon_all_epoch_pivot(&handle, by, query.since.as_deref(), all_time)?
            {
                return Ok(pivot);
            }
        }
    }
    let events = JsonlGainWriter::new(gain_dir(tok_dir))
        .read_all_recursive()
        .context("reading the gain log (all epochs) for --all-time/--since")?;
    Ok(pivot::query_events(&events, query))
}

/// Fetch the all-epoch pivot from a same-vault daemon's `GET /api/token/gain`.
///
/// - `Ok(Some(pivot))` — the route answered; decode the version-stable
///   [`PivotResult`] (a decode failure is a hard `Err`, never a silent Direct
///   fallback that could mask a real wire break).
/// - `Ok(None)` — the route is absent (404) or the call failed (transport /
///   status error). The gain JSONL is lock-free, so the caller falls through to
///   a Direct read that reads the exact same files and always works (#281) —
///   no more "daemon too old holds the lock" catch-22.
fn daemon_all_epoch_pivot(
    handle: &DaemonHandle,
    by: Option<&str>,
    since: Option<&str>,
    all_time: bool,
) -> Result<Option<PivotResult>> {
    match handle.token_gain(by, since, all_time, false) {
        Ok(Some(json)) => serde_json::from_value::<PivotResult>(json)
            .map(Some)
            .context("decoding the daemon's /api/token/gain pivot response"),
        // 404 (old daemon) or any transport/status error: fall through to the
        // lock-free Direct JSONL read.
        Ok(None) | Err(_) => Ok(None),
    }
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &TokenGainArgs) -> Result<()> {
    // #287: validate BEFORE any I/O — every branch below that reads `args.since`
    // (the default/`--all-time` pivot AND `--history`'s `filter_since`) must see
    // a value that's either unset or a real `YYYY-MM-DD`, never a string that
    // silently fails to match the log's zero-padded date keys.
    validate_since(args.since.as_deref())?;
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let collection = collection.context("no search collection resolved for this vault")?;

    let cache_dir = collection_cache_dir(&collection);
    let tok_dir = token_dir(&cache_dir);
    let gdir = gain_dir(&tok_dir);
    // `token.redb` (the legacy Tier-2 rollup) is opened LAZILY, only by
    // `--rebuild` — the sole remaining branch that needs it. EVERY read mode
    // (default summary, `--by`, `--history`, `--all-time`, `--since`, `--reset`)
    // reads the lock-free JSONL raw log (the source of truth), so reads work
    // even while a daemon holds the redb lock — the #258 fix, completed by #281.

    // `--json` is a local shorthand (mirrors `doctor --json` / `update
    // --json`) — it still renders through the SAME `emit`/`Envelope`
    // dispatcher as every other output mode, never a bespoke printer.
    let effective_mode = if args.json && !mode.is_structured() {
        OutputMode::Json { pretty: false }
    } else {
        mode.clone()
    };

    if args.rebuild {
        // A rebuild WRITES the rollup tables, so it can't route to the daemon's
        // read-only gain route — it needs exclusive redb access. Under a running
        // daemon this surfaces the actionable "held by another process" message
        // rather than the raw redb lock error.
        let db = open_rollup_db_direct(&tok_dir)?;
        rollup::ensure_tables(&db).context("ensuring rollup tables")?;
        let stats =
            rollup::rebuild(&gdir, &db).context("rebuilding rollups from the raw gain log")?;
        // The post-rebuild summary is the all-time cumulative rollup (every
        // epoch), so it's labeled all-time.
        let data = TokenGainData {
            pivot: pivot::query(&db, &PivotQuery::default()).context("querying rebuilt rollups")?,
            all_time: true,
            since_reset: read_reset_marker(&gdir).map(|m| day_string(m.ts)),
            cross_epoch_buckets_hidden: false,
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
        // queryable via `--all-time` / `--rebuild`, which walk
        // `gain/archive/**` too). The reset RESPONSE, though, reports the
        // fresh current-epoch window — which `archive_epoch` just emptied —
        // so the pivot honestly reads zero, matching what "counting fresh"
        // means. (The archive-confirmation render doesn't surface totals; the
        // empty pivot is for `--json` consumers.)
        let current = JsonlGainWriter::new(&gdir)
            .read_all()
            .context("reading fresh current window after reset")?;
        let data = TokenGainData {
            pivot: pivot::query_events(&current, &PivotQuery::default()),
            all_time: false,
            since_reset: Some(day_string(ts)),
            cross_epoch_buckets_hidden: false,
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
        // `--history` tails the current-epoch raw log (excludes archived
        // epochs), so it's a current-epoch view.
        let data = TokenGainData {
            pivot: PivotResult {
                rows: Vec::new(),
                totals: PivotTotals::default(),
            },
            all_time: false,
            since_reset: read_reset_marker(&gdir).map(|m| day_string(m.ts)),
            cross_epoch_buckets_hidden: false,
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
    //
    // Source selection is the R2-blocker fix. The bare default (no `--since`,
    // no `--all-time`) reports the CURRENT epoch — traffic since the last
    // `--reset` — by pivoting the non-archived raw log directly, so the
    // baseline-comparison workflow (reset → run at X → read → reset → run at
    // Y → read → compare) shows attributable per-epoch numbers instead of the
    // unchanging all-time cumulative total. `--all-time` and `--since` both
    // reach every epoch by also walking the archived JSONL
    // (`read_all_recursive`, #281).
    let (time, dim) = parse_by(args.by.as_deref())?;
    // Mirror the daemon route's empty-value guard: `--since ""` means unset,
    // never a real (vacuous) filter that silently flips the epoch scope.
    let since = args.since.clone().filter(|s| !s.is_empty());
    let query = PivotQuery {
        time,
        dim,
        since: since.clone(),
    };
    let marker = read_reset_marker(&gdir);
    let use_current_epoch = !args.all_time && since.is_none();
    let result = if use_current_epoch {
        let events = JsonlGainWriter::new(&gdir)
            .read_all()
            .context("reading current-epoch gain log")?;
        pivot::query_events(&events, &query)
    } else {
        // All-epoch (`--all-time` / `--since`): read the lock-free JSONL raw log
        // (all epochs), routing through a same-vault daemon when one is up (it
        // reads the same JSONL, so the answers agree) else Direct (#281).
        resolve_all_epoch_pivot(
            &resolved,
            &tok_dir,
            args.by.as_deref(),
            args.all_time,
            &query,
        )?
    };
    // month/year buckets on a current-epoch report can't span the archived
    // epoch — flag it so the report never implies a cross-epoch bucket it
    // didn't compute.
    let cross_epoch_buckets_hidden = use_current_epoch
        && marker.is_some()
        && matches!(time, Some(TimeAxis::Month) | Some(TimeAxis::Year));
    let data = TokenGainData {
        pivot: result,
        all_time: !use_current_epoch,
        since_reset: marker.map(|m| day_string(m.ts)),
        cross_epoch_buckets_hidden,
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
    // Honest scope label — never claim a scoping the number doesn't have.
    match (&data.since_reset, data.all_time) {
        // Current-epoch report and a reset happened → truly since-reset.
        (Some(date), false) => {
            out.push_str(&format!("  (scope: since reset {date})\n"));
        }
        // All-time report that spans a past reset → say so, and point at the
        // current-epoch view.
        (Some(date), true) => {
            out.push_str(&format!(
                "  (scope: all-time — includes traffic before the reset at {date}; \
                 omit --all-time for the current epoch only)\n"
            ));
        }
        // No reset ever: current-epoch == all traffic. Nothing to qualify.
        (None, _) => {}
    }
    if data.cross_epoch_buckets_hidden {
        out.push_str(
            "  Note: month/year buckets cover the current epoch only; \
             use --all-time to include archived epochs.\n",
        );
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

    // ── #287: --since / ?since= strict-format validation ───────────────────

    #[test]
    fn is_strict_since_date_accepts_zero_padded_date() {
        assert!(is_strict_since_date("2026-07-01"));
    }

    #[test]
    fn is_strict_since_date_rejects_non_zero_padded_month_and_day() {
        // The exact #287 live-proven case: chrono's own parser accepts this
        // (lenient on width) and silently produces 2026-01-01, which is why
        // validation can't rely on `NaiveDate::parse_from_str` alone.
        assert!(!is_strict_since_date("2026-1-1"));
    }

    #[test]
    fn is_strict_since_date_rejects_garbage() {
        assert!(!is_strict_since_date("notadate"));
    }

    #[test]
    fn is_strict_since_date_rejects_out_of_range_calendar_date() {
        assert!(!is_strict_since_date("2026-13-01"));
        assert!(!is_strict_since_date("2026-02-30"));
    }

    #[test]
    fn validate_since_accepts_none() {
        assert!(validate_since(None).is_ok());
    }

    #[test]
    fn validate_since_accepts_empty_string_as_unset() {
        // The `--since ""` = unset guard (mirrors the daemon route's
        // key-present-but-empty semantics) must stay a no-op here — the
        // caller applies its own `.filter(|s| !s.is_empty())` before/after,
        // but the validator itself must never reject "".
        assert!(validate_since(Some("")).is_ok());
    }

    #[test]
    fn validate_since_accepts_valid_date() {
        assert!(validate_since(Some("2026-07-01")).is_ok());
    }

    #[test]
    fn validate_since_rejects_non_zero_padded_date_with_invalid_date_exit_code() {
        let err = validate_since(Some("2026-1-1")).unwrap_err();
        // The dressed HintedError's Display (plain) is what both the JSON
        // envelope and the daemon route's 400 body see.
        let hinted = err
            .downcast_ref::<crate::output::HintedError>()
            .expect("must carry the HintedError dressing");
        assert!(hinted.plain.contains("--since"));
        assert!(hinted.plain.contains("2026-1-1"));
        assert!(!hinted.hint.is_empty());
        // The ORIGINAL CoreError::InvalidDate must survive in the chain so
        // `exit::exit_code_for` still maps it to 70 — same technique as
        // `contract_bind_error`'s PermissionDenied-66 preservation.
        assert_eq!(crate::exit::exit_code_for(&err), 70);
    }

    #[test]
    fn validate_since_rejects_garbage_with_invalid_date_exit_code() {
        let err = validate_since(Some("notadate")).unwrap_err();
        assert_eq!(crate::exit::exit_code_for(&err), 70);
    }

    // ── #288: rollup-lock EngineBusy — HintedError dressing, exit 77 ───────

    #[test]
    fn rollup_busy_error_is_dressed_and_keeps_exit_77() {
        let err = rollup_busy_error();
        let hinted = err
            .downcast_ref::<crate::output::HintedError>()
            .expect("must carry the HintedError dressing");
        // plain = what+why only, no remedy.
        assert_eq!(hinted.plain, ROLLUP_LOCK_WHAT);
        assert!(hinted.plain.contains("token.redb"));
        assert!(!hinted.plain.contains("onebrain daemon stop"));
        // hint = the actionable remedy.
        assert_eq!(hinted.hint, ROLLUP_LOCK_REMEDY);
        assert!(hinted.hint.contains("onebrain daemon stop"));
        // The JSON envelope's error.message (top-level Display) must be the
        // plain line only — never the remedy leaking into it.
        assert_eq!(err.to_string(), ROLLUP_LOCK_WHAT);
        // The original CoreError::EngineBusy must survive in the chain so
        // the exit-code walk still maps this to 77.
        assert_eq!(crate::exit::exit_code_for(&err), 77);
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
                all_time: false,
                since_reset: None,
                cross_epoch_buckets_hidden: false,
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
                all_time: false,
                since_reset: None,
                cross_epoch_buckets_hidden: false,
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
                all_time: false,
                since_reset: Some("2026-07-11".to_string()),
                cross_epoch_buckets_hidden: false,
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
                all_time: true,
                since_reset: None,
                cross_epoch_buckets_hidden: false,
                history: None,
                rebuilt_events: Some(42),
                archived_to: None,
            },
        );
        let text = render_text(&env);
        assert!(text.contains("42"), "{text}");
    }

    /// Build a default-mode `TokenGainData` for scope-label assertions.
    fn summary_data(all_time: bool, since_reset: Option<&str>) -> TokenGainData {
        TokenGainData {
            pivot: PivotResult {
                rows: Vec::new(),
                totals: PivotTotals::default(),
            },
            all_time,
            since_reset: since_reset.map(str::to_string),
            cross_epoch_buckets_hidden: false,
            history: None,
            rebuilt_events: None,
            archived_to: None,
        }
    }

    #[test]
    fn render_text_current_epoch_with_reset_labels_since_reset() {
        let env = Envelope::ok("token.gain", None, summary_data(false, Some("2026-07-11")));
        let text = render_text(&env);
        assert!(text.contains("scope: since reset 2026-07-11"), "{text}");
        assert!(!text.contains("all-time"), "{text}");
    }

    #[test]
    fn render_text_all_time_over_a_reset_labels_all_time_not_since_reset() {
        // The honesty fix: an all-time number that spans a reset must NOT
        // claim "since reset" — that was the lie the R2 review caught.
        let env = Envelope::ok("token.gain", None, summary_data(true, Some("2026-07-11")));
        let text = render_text(&env);
        assert!(text.contains("scope: all-time"), "{text}");
        assert!(
            !text.contains("scope: since reset"),
            "all-time report must never claim since-reset scoping:\n{text}"
        );
    }

    #[test]
    fn render_text_no_reset_ever_omits_scope_label() {
        // No reset happened → current-epoch == all traffic; nothing to qualify.
        let env = Envelope::ok("token.gain", None, summary_data(false, None));
        let text = render_text(&env);
        assert!(!text.contains("scope:"), "{text}");
    }

    #[test]
    fn render_text_cross_epoch_bucket_note_appears_only_when_flagged() {
        let mut data = summary_data(false, Some("2026-07-11"));
        data.cross_epoch_buckets_hidden = true;
        let env = Envelope::ok("token.gain", None, data);
        let text = render_text(&env);
        assert!(
            text.contains("month/year buckets cover the current epoch only"),
            "{text}"
        );
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

    // ── #258: token gain under a daemon that holds token.redb ──────────────
    // The daemon is the single owner of `token.redb` (redb is single-process).
    // Before the fix, `token gain` opened the redb eagerly at the top of `run`,
    // so EVERY mode hard-errored under a warm daemon. These prove: (a) the
    // lock-free modes work while the lock is held, (b) the rollup modes route
    // to the daemon, and (c) a genuine Direct lock reports an actionable message
    // instead of the raw redb error.

    use onebrain_token::PivotRow;
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::net::TcpListener;

    fn default_args() -> TokenGainArgs {
        TokenGainArgs {
            by: None,
            all_time: false,
            since: None,
            history: false,
            json: true,
            reset: false,
            label: None,
            rebuild: false,
        }
    }

    /// A vault with a search collection + isolated cache dir — enough for
    /// `resolve_collection` / `collection_cache_dir` to resolve `token.redb`.
    fn gain_vault() -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: token-gain-seam\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        (vault, cache)
    }

    /// The token dir + `token.redb` path for the seam collection under the
    /// currently-set `ONEBRAIN_CACHE_DIR` (call after the env is set).
    fn seam_tok_dir() -> PathBuf {
        token_dir(&collection_cache_dir("token-gain-seam"))
    }

    /// Open + hold `token.redb`, standing in for the running daemon that owns
    /// it. The returned handle keeps the exclusive lock until dropped.
    fn hold_redb_lock(tok_dir: &Path) -> Database {
        std::fs::create_dir_all(tok_dir).unwrap();
        Database::create(redb_path(tok_dir)).unwrap()
    }

    /// Seed raw events into the gain JSONL log under `tok_dir` — the lock-free
    /// source `--all-time`/`--since` now read (#281). Writes into the current
    /// epoch (non-archived) so both `read_all` and `read_all_recursive` see them.
    fn seed_jsonl(tok_dir: &Path, evs: &[(i64, Surface, u64, u64)]) {
        let writer = JsonlGainWriter::new(gain_dir(tok_dir));
        for (ts, surface, before, after) in evs {
            writer
                .append(&GainEvent {
                    ts: *ts,
                    surface: *surface,
                    transform: "whitespace".to_string(),
                    level: OptLevel::Conservative,
                    bytes_before: *before,
                    bytes_after: *after,
                    cache: CacheKind::None,
                    session_token: None,
                })
                .unwrap();
        }
    }

    #[test]
    fn default_summary_succeeds_while_a_holder_locks_redb() {
        let (vault, cache) = gain_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()), // no daemon.json here
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);
        let _held = hold_redb_lock(&seam_tok_dir());

        // Pre-#258 this hard-errored ("Database already open. Cannot acquire
        // lock."). Now the default summary reads the lock-free JSONL → Ok.
        let out = run(
            Some(vault.path().to_path_buf()),
            &OutputMode::Json { pretty: false },
            &default_args(),
        );
        assert!(
            out.is_ok(),
            "default `token gain` must work under a held redb lock: {out:?}"
        );
    }

    #[test]
    fn history_succeeds_while_a_holder_locks_redb() {
        let (vault, cache) = gain_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);
        let _held = hold_redb_lock(&seam_tok_dir());

        let args = TokenGainArgs {
            history: true,
            ..default_args()
        };
        let out = run(
            Some(vault.path().to_path_buf()),
            &OutputMode::Json { pretty: false },
            &args,
        );
        assert!(
            out.is_ok(),
            "`token gain --history` reads the raw log and must work under a lock: {out:?}"
        );
    }

    /// #281: the Direct leg (no daemon) reads the lock-free gain JSONL — a held
    /// redb lock no longer blocks it, because the rollup DB is no longer the
    /// source. Proves the catch-22 is gone: `--all-time` works while a holder
    /// keeps token.redb locked.
    #[test]
    fn all_time_direct_reads_jsonl_even_while_a_holder_locks_redb() {
        let (vault, cache) = gain_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")), // force the Direct leg
        ]);
        let tok_dir = seam_tok_dir();
        seed_jsonl(&tok_dir, &[(1_783_728_000, Surface::CliSearch, 1000, 400)]);
        let _held = hold_redb_lock(&tok_dir); // held, but the JSONL read ignores it

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .expect("the lock-free JSONL read must succeed under a held redb lock");
        assert_eq!(pivot.totals.count, 1);
        assert_eq!(pivot.totals.bytes_before, 1000);
    }

    #[test]
    fn all_time_direct_returns_the_seeded_jsonl_when_no_daemon() {
        let (vault, cache) = gain_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()), // no daemon.json → Direct
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);
        let tok_dir = seam_tok_dir();
        seed_jsonl(
            &tok_dir,
            &[
                (1_783_728_000, Surface::CliSearch, 1000, 400),
                (1_783_900_800, Surface::McpQuery, 500, 100),
            ],
        );

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .unwrap();
        assert_eq!(pivot.totals.count, 2);
        assert_eq!(pivot.totals.bytes_before, 1500);
        assert_eq!(pivot.totals.bytes_after, 500);
    }

    /// #281: `read_all_recursive` on the Direct leg reaches archived epochs, so
    /// `--all-time` includes events archived by `--reset` — parity with the old
    /// cumulative rollup's all-epoch scope.
    #[test]
    fn all_time_direct_includes_archived_epochs() {
        let (vault, cache) = gain_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);
        let tok_dir = seam_tok_dir();
        // One current-epoch event + one archived event.
        seed_jsonl(&tok_dir, &[(1_783_900_800, Surface::McpQuery, 500, 100)]);
        let archive = gain_dir(&tok_dir).join("archive").join("1-baseline");
        JsonlGainWriter::new(&archive)
            .append(&GainEvent {
                ts: 1_700_000_000,
                surface: Surface::CliSearch,
                transform: "whitespace".to_string(),
                level: OptLevel::Conservative,
                bytes_before: 1000,
                bytes_after: 400,
                cache: CacheKind::None,
                session_token: None,
            })
            .unwrap();

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .unwrap();
        assert_eq!(
            pivot.totals.count, 2,
            "all-time must include the archived epoch"
        );
        assert_eq!(pivot.totals.bytes_before, 1500);
    }

    // ── daemon-routing (Complete scope): --all-time/--since via the route ──

    #[cfg(unix)]
    fn write_daemon_json(home: &Path, vault: &Path, port: u16, token: &str) {
        write_daemon_json_versioned(home, vault, port, token, env!("CARGO_PKG_VERSION"));
    }

    /// Like [`write_daemon_json`] but pins the recorded daemon `version` — the
    /// Gap 4 test needs a version DIFFERENT from ours to prove the routed read
    /// adopts a version-skewed same-vault daemon.
    #[cfg(unix)]
    fn write_daemon_json_versioned(
        home: &Path,
        vault: &Path,
        port: u16,
        token: &str,
        version: &str,
    ) {
        let run_dir = home.join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let vault_id = daemon_client::canonical_vault_id(vault);
        let info = daemon_client::DaemonInfo {
            port,
            token: token.to_string(),
            pid: std::process::id(),
            version: version.to_string(),
            vault: vault_id.clone(),
        };
        // Write to the vault's SLOT (`daemon-<hash>.json`), computed from the
        // explicit `home` so it doesn't depend on `$HOME` timing (v3.4.13, #230).
        let stem = format!(
            "daemon-{}",
            onebrain_search::engine::short_path_hash(Path::new(&vault_id.unwrap()))
        );
        std::fs::write(
            run_dir.join(format!("{stem}.json")),
            serde_json::to_vec_pretty(&info).unwrap(),
        )
        .unwrap();
    }

    type CapturedReqs = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// A minimal HTTP/1.1 responder for the two routes `resolve_all_epoch_pivot`
    /// exercises: `GET /api/health` (so daemon discovery adopts it) and `GET
    /// /api/token/gain`. It records each non-health request's first line (so a
    /// test can assert `by=`/`since=`/`all_time=` forwarding) and serves `gain_body` with
    /// `gain_status` — EXCEPT `gain_status == 0`, which drops the connection
    /// with no response, simulating a daemon that died mid-call (a transport
    /// error → Gap 8 Direct-fallback).
    #[cfg(unix)]
    fn start_fake_gain_daemon(gain_status: u16, gain_body: String) -> (u16, CapturedReqs) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let reqs: CapturedReqs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reqs_bg = reqs.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let body = gain_body.clone();
                let reqs = reqs_bg.clone();
                std::thread::spawn(move || {
                    // Accumulate until the end of the HTTP headers (`\r\n\r\n`) so
                    // a request split across TCP segments isn't misread from a
                    // single partial `read`.
                    let mut req_bytes = Vec::with_capacity(8192);
                    let mut chunk = [0u8; 1024];
                    while req_bytes.len() < 8192 {
                        let n = stream.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req_bytes.extend_from_slice(&chunk[..n]);
                        if req_bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&req_bytes).to_string();
                    if req.starts_with("GET /api/health") {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        );
                        return;
                    }
                    reqs.lock()
                        .unwrap()
                        .push(req.lines().next().unwrap_or("").to_string());
                    if gain_status == 0 {
                        // Drop the connection with no response → transport error.
                        return;
                    }
                    let status_line = match gain_status {
                        200 => "HTTP/1.1 200 OK",
                        404 => "HTTP/1.1 404 Not Found",
                        _ => "HTTP/1.1 500 Internal Server Error",
                    };
                    let resp = format!(
                        "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        });
        (port, reqs)
    }

    /// A single-surface pivot body for the fake daemon (`count`/bytes on one
    /// `cli_search` row).
    #[cfg(unix)]
    fn pivot_body(count: u64) -> String {
        serde_json::to_string(&PivotResult {
            rows: vec![PivotRow {
                time: None,
                dim: Some("cli_search".to_string()),
                totals: PivotTotals {
                    bytes_before: 2000,
                    bytes_after: 500,
                    count,
                },
            }],
            totals: PivotTotals {
                bytes_before: 2000,
                bytes_after: 500,
                count,
            },
        })
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn all_time_routes_to_the_daemon_when_local_jsonl_is_empty() {
        let (vault, cache) = gain_vault();
        let (port, _reqs) = start_fake_gain_daemon(200, pivot_body(3));
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "gain-daemon-token-123");

        // Leave the LOCAL JSONL empty: a Direct read would return count=0, so
        // this test can pass ONLY by routing to the daemon (count=3).
        let tok_dir = seam_tok_dir();

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .unwrap();
        assert_eq!(
            pivot.totals.count, 3,
            "must reflect the daemon's answer, not the empty local JSONL"
        );
        assert_eq!(pivot.rows.len(), 1);
        assert_eq!(pivot.rows[0].dim.as_deref(), Some("cli_search"));
    }

    /// #258 Gap 4: after a CLI upgrade the still-running OLD daemon is up.
    /// Routing must adopt the version-skewed same-vault daemon anyway (its gain
    /// route + PivotResult are version-stable). The local JSONL is left empty so
    /// the ONLY way to reach count=7 is routing to the daemon.
    #[cfg(unix)]
    #[test]
    fn all_time_routes_to_a_version_mismatched_same_vault_daemon() {
        let (vault, cache) = gain_vault();
        let (port, _reqs) = start_fake_gain_daemon(200, pivot_body(7));
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        // A daemon at a DIFFERENT version than ours (the upgrade-skew case).
        write_daemon_json_versioned(vault.path(), vault.path(), port, "tok", "0.0.1-old");
        let tok_dir = seam_tok_dir();

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .unwrap();
        assert_eq!(
            pivot.totals.count, 7,
            "must route to the version-skewed same-vault daemon (Gap 4), not read the empty local JSONL"
        );
    }

    /// #258 Gap 8 / #281: the daemon dies between discovery and the gain call →
    /// a transport error. The read must fall through to the Direct JSONL read
    /// (self-healing), NOT surface an error.
    #[cfg(unix)]
    #[test]
    fn daemon_gone_mid_call_falls_back_to_direct() {
        let (vault, cache) = gain_vault();
        let (port, _reqs) = start_fake_gain_daemon(0, String::new()); // drops the gain call
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "tok");
        let tok_dir = seam_tok_dir();
        // Seed the local JSONL the Direct fallback will read.
        seed_jsonl(&tok_dir, &[(1_783_728_000, Surface::CliSearch, 1000, 400)]);

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .unwrap();
        assert_eq!(
            pivot.totals.count, 1,
            "a transport error must fall back to the Direct JSONL read (Gap 8)"
        );
    }

    /// R3: prove `by`/`since`/`all_time` are actually forwarded onto the daemon
    /// route URL (the fake daemon records the request line).
    #[cfg(unix)]
    #[test]
    fn by_since_and_all_time_are_forwarded_to_the_daemon_route() {
        let (vault, cache) = gain_vault();
        let empty = serde_json::to_string(&PivotResult {
            rows: vec![],
            totals: PivotTotals::default(),
        })
        .unwrap();
        let (port, reqs) = start_fake_gain_daemon(200, empty);
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "tok");
        let tok_dir = seam_tok_dir();

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let query = PivotQuery {
            time: None,
            dim: None,
            since: Some("2026-01-01".to_string()),
        };
        let _ =
            resolve_all_epoch_pivot(&resolved, &tok_dir, Some("surface"), true, &query).unwrap();

        let captured = reqs.lock().unwrap().clone();
        assert!(
            captured.iter().any(|r| r.contains("since=2026-01-01")),
            "since must be forwarded: {captured:?}"
        );
        assert!(
            captured.iter().any(|r| r.contains("by=surface")),
            "by must be forwarded: {captured:?}"
        );
        assert!(
            captured.iter().any(|r| r.contains("all_time=true")),
            "all_time must be forwarded: {captured:?}"
        );
    }

    /// #281: a daemon too old to serve the gain route answers 404. Because the
    /// JSONL is lock-free, the read falls through to the Direct JSONL read (which
    /// reads the same files) rather than erroring — the old `E_ENGINE_BUSY`
    /// catch-22 is gone.
    #[cfg(unix)]
    #[test]
    fn daemon_too_old_for_the_gain_route_falls_back_to_direct() {
        let (vault, cache) = gain_vault();
        let (port, _reqs) = start_fake_gain_daemon(404, "not found".to_string());
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "gain-daemon-token-123");
        let tok_dir = seam_tok_dir();
        // Seed the local JSONL the Direct fallback reads after the 404.
        seed_jsonl(&tok_dir, &[(1_783_728_000, Surface::CliSearch, 1000, 400)]);

        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let pivot =
            resolve_all_epoch_pivot(&resolved, &tok_dir, None, true, &PivotQuery::default())
                .expect("a 404 gain route must fall through to the lock-free Direct JSONL read");
        assert_eq!(
            pivot.totals.count, 1,
            "a 404 must fall back to the Direct JSONL read, not error"
        );
    }

    /// R3: `--rebuild` is a redb WRITE that can't route to the read-only daemon
    /// route — under a held lock it must surface the actionable exit-77 error.
    #[test]
    fn rebuild_under_a_lock_reports_engine_busy() {
        let (vault, cache) = gain_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);
        let _held = hold_redb_lock(&seam_tok_dir());
        let args = TokenGainArgs {
            rebuild: true,
            ..default_args()
        };
        let err = run(
            Some(vault.path().to_path_buf()),
            &OutputMode::Json { pretty: false },
            &args,
        )
        .expect_err("rebuild needs exclusive redb; under a lock it must error");
        assert_eq!(
            err.downcast_ref::<onebrain_core::CoreError>()
                .expect("typed CoreError")
                .error_code(),
            "E_ENGINE_BUSY"
        );
    }
}
