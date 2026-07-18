//! `onebrain update` — thin wiring around `onebrain_fs::update::run_update`.
//!
//! Three output modes:
//! - **JSON** (`--json` / `--plan`): single machine-readable document
//!   suppressing all orchestrator log lines.
//! - **TTY** (interactive terminal, non-JSON): colorized status lines + an
//!   `indicatif` spinner for the install phase (download takes a few seconds
//!   on slow links; silent stdout would feel like a hang).
//! - **Plain** (non-TTY, non-JSON — CI, redirected output): unchanged
//!   line-by-line output, no ANSI escapes, no spinner.

use crate::legacy_output::serialize_for_mode;
use crate::output::OutputMode;
use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use is_terminal::IsTerminal;
use onebrain_fs::update::{
    default_fetch_latest_release, default_install_binary, run_update, FetchFn, UpdateError,
    UpdateOptions,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RELEASES_BASE_URL: &str = "https://github.com/onebrain-ai/onebrain-cli/releases";

// ANSI escape codes — kept inline rather than pulling a `colored`-style dep
// for a handful of distinct color uses.
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_DIM: &str = "\x1b[2m";

/// Run `onebrain update`. Returns the exit code; the caller is responsible
/// for `std::process::exit`.
///
/// `fresh = true` skips the 1-hour on-disk release-info cache so the user
/// always hits GitHub. `json = true` switches stdout to a single JSON
/// document with the version delta. `plan = true` implies dry-run mode and
/// emits a richer plan document (release URL + binary URL) suitable for the
/// `/update` plugin skill — no install happens.
pub fn run(check: bool, fresh: bool, json: bool, plan: bool, mode: &OutputMode) -> Result<i32> {
    // `--plan` implies dry-run — never install when the caller asked for a
    // plan document. `clap`'s `conflicts_with = "check"` blocks the user
    // from setting both flags, so this is the only place `--plan` can flip
    // dry-run on.
    let dry_run = check || plan;
    // v3.1: structured output is triggered by EITHER the local `--json`/
    // `--plan` flags (back-compat with v3.0 callers) OR any global format
    // flag (`--yaml`, `--output yaml`, …). `mode.is_structured()` catches
    // every non-text variant so `--yaml` no longer silently emits text.
    let want_structured = json || plan || mode.is_structured();
    let want_tty = !want_structured && std::io::stdout().is_terminal();

    let mut opts = if want_structured {
        // Suppress the orchestrator's plain-text log lines — structured
        // mode produces exactly one document on stdout.
        UpdateOptions {
            check: dry_run,
            fresh,
            stdout_lines: Some(Box::new(|_| {})),
            stderr_lines: Some(Box::new(|_| {})),
            ..Default::default()
        }
    } else if want_tty {
        build_tty_options(dry_run, fresh)
    } else {
        UpdateOptions {
            check: dry_run,
            fresh,
            ..Default::default()
        }
    };

    // #291: after a real upgrade, the just-swapped binary means every running
    // warm daemon now serves the OLD wire shape until it idles out. Retire them
    // all so the next `daemon start`/`serve`/`mcp` respawns at the new version.
    // Only wired on the real-install path — `run_update` also guards it (never
    // fires on `--check`/plan/no-op/failure), so a dry run never touches
    // daemons even though the closure is present.
    if !dry_run {
        opts.post_update_fn = Some(Box::new(|| {
            crate::commands::daemon::stop_all_slots()
                .map(|(n, _)| n)
                .map_err(|e| e.to_string())
        }));
    }

    let result = run_update(opts);

    if want_structured {
        let doc = build_json_document(&result, plan);
        // When the only signal was the local `--json` / `--plan` flag, fall
        // back to compact JSON so v3.0 callers see byte-identical output.
        // Global format flags (`--yaml`, `--pretty`, …) go through the
        // canonical dispatcher.
        let rendered = if mode.is_structured() {
            serialize_for_mode(&doc, mode)
        } else {
            serde_json::to_string(&doc)?
        };
        println!("{}", rendered);
    }

    Ok(result.exit_code)
}

/// TTY-mode `UpdateOptions`: a framed 🚀 header, colorized phase lines, and a
/// braille spinner (matching `doctor`) on the two phases that take time —
/// `fetch` (the version check, padded to a deliberate beat so it reads as real
/// work even on a warm cache) and `install` (the real download). The install
/// spinner is shared with the sinks via an `Arc<Mutex<Option<ProgressBar>>>`
/// so `pb.println` interleaves cleanly; the fetch spinner stands alone because
/// no log lines are emitted mid-fetch.
fn build_tty_options(dry_run: bool, fresh: bool) -> UpdateOptions {
    let pb_cell: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

    // stdout sink — replace the plain "OneBrain Update" title with the framed
    // 🚀 header, color the well-known phase lines, and route everything via the
    // install spinner's `println` when one is active so output doesn't trample.
    let pb_for_stdout = Arc::clone(&pb_cell);
    let stdout_sink: Box<dyn FnMut(&str) + Send> = Box::new(move |line: &str| {
        let guard = pb_for_stdout.lock().unwrap_or_else(|e| e.into_inner());
        let rendered = render_stdout_line(line);
        let body = rendered.trim_end_matches('\n');
        match guard.as_ref() {
            Some(pb) => pb.println(body),
            None => println!("{body}"),
        }
    });

    // stderr sink — same pattern, but errors always go to stderr (matches
    // the non-TTY behavior the orchestrator already has).
    let pb_for_stderr = Arc::clone(&pb_cell);
    let stderr_sink: Box<dyn FnMut(&str) + Send> = Box::new(move |line: &str| {
        let colored = format!("{ANSI_RED}{line}{ANSI_RESET}");
        let guard = pb_for_stderr.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(pb) => {
                // indicatif has no eprintln equivalent; suspend the spinner
                // briefly so the error line lands on stderr without scramble.
                pb.suspend(|| eprintln!("{colored}"));
            }
            None => eprintln!("{colored}"),
        }
    });

    // fetch_fn — spin while checking GitHub, padded to a deliberate beat so the
    // check reads as real work even when the cache is warm / network is fast.
    // Stands alone (no shared cell): the orchestrator emits no lines mid-fetch.
    let fetch_fn: FetchFn = Box::new(move || {
        let pb = braille_spinner("checking for updates…");
        let started = Instant::now();
        let result = default_fetch_latest_release(fresh);
        if let Some(remaining) = crate::output::random_step_delay().checked_sub(started.elapsed()) {
            std::thread::sleep(remaining);
        }
        pb.finish_and_clear();
        result
    });

    // install_fn wrapper — start the spinner, run the real install, stop.
    let pb_for_install = Arc::clone(&pb_cell);
    let install_fn: InstallFn = Box::new(move |version: &str| {
        let pb = braille_spinner(format!("downloading onebrain {version}…"));
        // Hand the spinner to the sinks while install runs.
        *pb_for_install.lock().unwrap_or_else(|e| e.into_inner()) = Some(pb.clone());

        let result = default_install_binary(version);

        // Release the spinner before the orchestrator's next stdout line so the
        // install summary line ("done: …") doesn't fight for the cursor.
        *pb_for_install.lock().unwrap_or_else(|e| e.into_inner()) = None;
        match &result {
            Ok(_) => pb.finish_and_clear(),
            Err(_) => pb.abandon(),
        }
        result
    });

    UpdateOptions {
        check: dry_run,
        fresh,
        stdout_lines: Some(stdout_sink),
        stderr_lines: Some(stderr_sink),
        fetch_fn: Some(fetch_fn),
        install_fn: Some(install_fn),
        ..Default::default()
    }
}

/// A braille-frame spinner matching `doctor`'s animation — the shared
/// [`crate::output::SPINNER_FRAMES`] fed into indicatif's live ticker, so both
/// animated commands share one spinner look without duplicating the frames.
fn braille_spinner(msg: impl Into<String>) -> ProgressBar {
    // indicatif renders the last tick string when finished; append a blank so
    // all braille frames animate (the spinner is cleared on finish anyway).
    let mut ticks: Vec<&str> = crate::output::SPINNER_FRAMES.to_vec();
    ticks.push(" ");
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .map(|style| style.tick_strings(&ticks))
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    pb.set_message(msg.into());
    pb.enable_steady_tick(Duration::from_millis(90));
    pb
}

/// The framed 🚀 update header (shared layout with `doctor`), rendered to a
/// string for the stdout sink. The TTY path always has colour on.
/// v3.2.15: emoji set differentiated per command (was 🧠 — collided with
/// `doctor`'s 🧠 AND with the brand wordmark banner). `🚀` reads as
/// "self-update / take off / fast release" for the CLI's own bump path;
/// `doctor` now uses `🔬` (lab), `plugin update` uses `🔄` (sync).
fn framed_update_header() -> String {
    let mut buf = Vec::new();
    let _ = crate::output::write_framed_header(
        &mut buf,
        "🚀",
        "OneBrain Update",
        true,
        crate::output::RULE_WIDTH,
    );
    String::from_utf8(buf).unwrap_or_default()
}

/// Local alias for the install closure type — clippy complains about the
/// raw `Box<dyn Fn(...) + Send + Sync>` shape inline. The orchestrator's
/// `UpdateOptions::install_fn` field is identically shaped, so the alias is
/// purely a readability win.
type InstallFn = Box<dyn Fn(&str) -> Result<(), UpdateError> + Send + Sync>;

/// Render one orchestrator stdout line for the TTY sink: the `"OneBrain Update"`
/// title becomes the framed 🧠 header; every other line is colorized by prefix.
/// Split out from the sink closure so both branches are unit-testable.
fn render_stdout_line(line: &str) -> String {
    if line == "OneBrain Update" {
        framed_update_header()
    } else {
        colorize_update_line(line)
    }
}

/// Map known orchestrator log-line prefixes to ANSI-colored variants. Lines
/// that don't match a known prefix are passed through untouched.
fn colorize_update_line(line: &str) -> String {
    if line.starts_with("done:") {
        format!("{ANSI_GREEN}{line}{ANSI_RESET}")
    } else if line.starts_with("already up to date") {
        format!("{ANSI_DIM}{line}{ANSI_RESET}")
    } else {
        line.to_string()
    }
}

/// Build the JSON document for `--json` / `--plan`. The plan variant adds
/// `release_url` and `binary_url_template` fields that the `/update` skill
/// uses to present the release notes + a copy-paste download link.
///
/// Schema notes:
/// - `update_available` is `null` (JSON) when the remote fetch failed —
///   consumers must not interpret missing-latest as "no update".
/// - `released_at` is emitted as RFC-3339 when the GitHub release payload
///   carried `published_at`; absent otherwise.
fn build_json_document(
    result: &onebrain_fs::update::UpdateResult,
    plan: bool,
) -> serde_json::Value {
    let current = result.current_version.as_deref().unwrap_or("");
    let latest = result.latest_version.as_deref().unwrap_or("");
    // When fetch failed, `latest` is empty — emit JSON null so consumers
    // can distinguish "fetch failed" from "you're current".
    let update_available_value = if latest.is_empty() {
        serde_json::Value::Null
    } else if current.is_empty() {
        // No local version detected — treat as "available" to be safe.
        serde_json::Value::Bool(true)
    } else {
        serde_json::Value::Bool(!onebrain_fs::update::version_at_least(current, latest))
    };
    let update_available_bool = matches!(update_available_value, serde_json::Value::Bool(true));
    let mut doc = serde_json::json!({
        "ok": result.ok,
        "current": current,
        "latest": latest,
        "update_available": update_available_value,
    });
    if let Some(ts) = result.latest_published_at {
        doc.as_object_mut().unwrap().insert(
            "released_at".to_string(),
            serde_json::Value::String(ts.to_rfc3339()),
        );
    }
    if let Some(err) = &result.error {
        doc.as_object_mut()
            .unwrap()
            .insert("error".to_string(), serde_json::Value::String(err.clone()));
    }
    // #291: how many warm daemons the post-upgrade retire stopped. Absent on
    // dry-run / no-op / failure (the closure never ran → `daemons_retired` is
    // `None`), present as a number only after a real upgrade.
    if let Some(n) = result.daemons_retired {
        doc.as_object_mut().unwrap().insert(
            "daemons_retired".to_string(),
            serde_json::Value::Number(n.into()),
        );
    }
    if plan && update_available_bool {
        let tag = if latest.starts_with('v') {
            latest.to_string()
        } else {
            format!("v{latest}")
        };
        let release_url = format!("{RELEASES_BASE_URL}/tag/{tag}");
        let binary_url_template =
            format!("{RELEASES_BASE_URL}/download/{tag}/onebrain-<TRIPLE>.<EXT>");
        let obj = doc.as_object_mut().unwrap();
        obj.insert(
            "release_url".to_string(),
            serde_json::Value::String(release_url),
        );
        obj.insert(
            "binary_url_template".to_string(),
            serde_json::Value::String(binary_url_template),
        );
        // Enumerate the placeholders so callers don't have to guess the
        // published target set. Each entry pairs a target triple with the
        // archive extension that triple ships with.
        //
        // Windows triples are intentionally omitted in v3.0.0 GA: the
        // self-update path is unix-only this cycle (zip extraction lands in
        // v3.0.1). Releases still publish Windows .zip archives for manual
        // download, but `--plan` only advertises auto-installable targets —
        // otherwise a Windows user running --plan would see their triple
        // listed and then hit a confusing error on actual install
        // (Reviewer A-H1, alpha.9).
        obj.insert(
            "binary_targets".to_string(),
            serde_json::json!([
                {"triple": "aarch64-apple-darwin",      "ext": "tar.gz"},
                {"triple": "x86_64-apple-darwin",       "ext": "tar.gz"},
                {"triple": "aarch64-unknown-linux-gnu", "ext": "tar.gz"},
                {"triple": "x86_64-unknown-linux-gnu",  "ext": "tar.gz"},
                {"triple": "aarch64-unknown-linux-musl","ext": "tar.gz"},
                {"triple": "x86_64-unknown-linux-musl", "ext": "tar.gz"},
            ]),
        );
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_fs::update::UpdateResult;

    // ── TTY stdout-line rendering (framed header + colorized phases) ──────

    #[test]
    fn render_stdout_line_frames_the_title() {
        // The "OneBrain Update" title becomes the framed 🚀 header (two spaces
        // after the glyph, full-width rules) — same layout as doctor's header
        // (v3.2.15: doctor uses 🔬, update uses 🚀, plugin update uses 🔄 —
        // each command gets its own distinct glyph instead of the previous
        // shared 🧠 that doubled as the brand banner).
        let out = render_stdout_line("OneBrain Update");
        assert!(out.contains("🚀  OneBrain Update"), "framed title: {out:?}");
        assert!(out.contains('─'), "framed rule: {out:?}");
    }

    #[test]
    fn render_stdout_line_colorizes_known_phases_and_passes_others_through() {
        assert!(
            render_stdout_line("done: upgraded").starts_with(ANSI_GREEN),
            "done → green"
        );
        assert!(
            render_stdout_line("already up to date: …").starts_with(ANSI_DIM),
            "up-to-date → dim"
        );
        // Unknown lines pass through untouched (no ANSI added).
        assert_eq!(render_stdout_line("downloading…"), "downloading…");
    }

    fn result(current: &str, latest: &str, ok: bool) -> UpdateResult {
        UpdateResult {
            ok,
            exit_code: if ok { 0 } else { 1 },
            current_version: Some(current.to_string()),
            latest_version: Some(latest.to_string()),
            error: None,
            latest_published_at: None,
            daemons_retired: None,
        }
    }

    #[test]
    fn json_doc_reports_update_available() {
        let r = result("3.0.0-alpha.7", "3.0.0-alpha.8", true);
        let doc = build_json_document(&r, false);
        assert_eq!(doc["update_available"], true);
        assert_eq!(doc["current"], "3.0.0-alpha.7");
        assert_eq!(doc["latest"], "3.0.0-alpha.8");
        assert!(doc.get("release_url").is_none(), "plan-only field");
    }

    #[test]
    fn json_doc_reports_no_update_when_at_latest() {
        let r = result("3.0.0-alpha.8", "3.0.0-alpha.8", true);
        let doc = build_json_document(&r, false);
        assert_eq!(doc["update_available"], false);
    }

    #[test]
    fn json_doc_reports_no_update_when_ahead_of_remote() {
        // Prerelease ahead of stable — semver-aware "at least" check
        // refuses the downgrade.
        let r = result("3.0.0-alpha.8", "2.3.3", true);
        let doc = build_json_document(&r, false);
        assert_eq!(doc["update_available"], false);
    }

    #[test]
    fn plan_doc_includes_release_and_binary_urls() {
        let r = result("3.0.0-alpha.7", "3.0.0-alpha.8", true);
        let doc = build_json_document(&r, true);
        let release_url = doc["release_url"].as_str().unwrap();
        let bin_url = doc["binary_url_template"].as_str().unwrap();
        assert!(release_url.ends_with("/releases/tag/v3.0.0-alpha.8"));
        assert!(bin_url.contains("/download/v3.0.0-alpha.8/"));
        assert!(bin_url.contains("<TRIPLE>"));
        assert!(bin_url.contains("<EXT>"));
    }

    #[test]
    fn plan_doc_omits_urls_when_no_update() {
        let r = result("3.0.0-alpha.8", "3.0.0-alpha.8", true);
        let doc = build_json_document(&r, true);
        assert!(doc.get("release_url").is_none());
        assert!(doc.get("binary_url_template").is_none());
    }

    #[test]
    fn json_doc_includes_error_field_on_failure() {
        let r = UpdateResult {
            ok: false,
            exit_code: 1,
            current_version: Some("3.0.0-alpha.7".into()),
            latest_version: None,
            error: Some("Fetch failed: network".into()),
            latest_published_at: None,
            daemons_retired: None,
        };
        let doc = build_json_document(&r, false);
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["error"], "Fetch failed: network");
        // When fetch fails the latest version is unknown → emit JSON null
        // (not false) so consumers don't read it as "you're current".
        assert!(doc["update_available"].is_null());
    }

    #[test]
    fn json_doc_emits_released_at_when_present() {
        use chrono::TimeZone;
        let mut r = result("3.0.0-alpha.7", "3.0.0-alpha.8", true);
        r.latest_published_at = Some(chrono::Utc.with_ymd_and_hms(2026, 5, 21, 10, 0, 0).unwrap());
        let doc = build_json_document(&r, false);
        let released_at = doc["released_at"].as_str().unwrap();
        // RFC-3339 with explicit timezone — chrono renders Utc as "+00:00".
        assert!(released_at.starts_with("2026-05-21T10:00:00"));
    }

    #[test]
    fn json_doc_omits_released_at_when_absent() {
        let r = result("3.0.0-alpha.7", "3.0.0-alpha.8", true);
        let doc = build_json_document(&r, false);
        assert!(doc.get("released_at").is_none());
    }

    #[test]
    fn json_doc_emits_daemons_retired_when_present() {
        // #291: after a real upgrade the post-update closure reports how many
        // warm daemons it retired → surfaced as a JSON number.
        let mut r = result("3.4.14", "3.4.15", true);
        r.daemons_retired = Some(2);
        let doc = build_json_document(&r, false);
        assert_eq!(doc["daemons_retired"], 2);
    }

    #[test]
    fn json_doc_omits_daemons_retired_when_absent() {
        // Dry-run / no-op / failure never runs the retire closure → field absent
        // (distinct from `Some(0)`, which would mean "ran, nothing to stop").
        let r = result("3.4.15", "3.4.15", true);
        let doc = build_json_document(&r, false);
        assert!(doc.get("daemons_retired").is_none());
    }
}
