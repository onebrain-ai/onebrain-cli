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

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use is_terminal::IsTerminal;
use onebrain_fs::update::{default_install_binary, run_update, UpdateError, UpdateOptions};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RELEASES_BASE_URL: &str = "https://github.com/onebrain-ai/onebrain-cli/releases";

// ANSI escape codes — kept inline rather than pulling a `colored`-style dep
// for ~5 distinct color uses.
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
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
pub fn run(check: bool, fresh: bool, json: bool, plan: bool) -> Result<i32> {
    // `--plan` implies dry-run — never install when the caller asked for a
    // plan document. `clap`'s `conflicts_with = "check"` blocks the user
    // from setting both flags, so this is the only place `--plan` can flip
    // dry-run on.
    let dry_run = check || plan;
    let want_json = json || plan;
    let want_tty = !want_json && std::io::stdout().is_terminal();

    let opts = if want_json {
        // Suppress the orchestrator's plain-text log lines — JSON mode
        // produces exactly one document on stdout.
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

    let result = run_update(opts);

    if want_json {
        let doc = build_json_document(&result, plan);
        println!("{}", serde_json::to_string(&doc)?);
    }

    Ok(result.exit_code)
}

/// TTY-mode `UpdateOptions`: colorized line output via an `indicatif`-aware
/// sink + a spinner that ticks during the install download. The spinner is
/// shared between the sink (so `pb.println` interleaves correctly) and the
/// `install_fn` wrapper (so we can `start`/`finish_with_message` around the
/// real install). Both use an `Arc<Mutex<Option<ProgressBar>>>`.
fn build_tty_options(dry_run: bool, fresh: bool) -> UpdateOptions {
    let pb_cell: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

    // stdout sink — color the well-known phase lines, route everything via
    // the spinner's `println` when one is active so output doesn't trample.
    let pb_for_stdout = Arc::clone(&pb_cell);
    let stdout_sink: Box<dyn FnMut(&str) + Send> = Box::new(move |line: &str| {
        let colored = colorize_update_line(line);
        let guard = pb_for_stdout.lock().expect("pb mutex poisoned");
        match guard.as_ref() {
            Some(pb) => pb.println(colored),
            None => println!("{colored}"),
        }
    });

    // stderr sink — same pattern, but errors always go to stderr (matches
    // the non-TTY behavior the orchestrator already has).
    let pb_for_stderr = Arc::clone(&pb_cell);
    let stderr_sink: Box<dyn FnMut(&str) + Send> = Box::new(move |line: &str| {
        let colored = format!("{ANSI_RED}{line}{ANSI_RESET}");
        let guard = pb_for_stderr.lock().expect("pb mutex poisoned");
        match guard.as_ref() {
            Some(pb) => {
                // indicatif has no eprintln equivalent; suspend the spinner
                // briefly so the error line lands on stderr without scramble.
                pb.suspend(|| eprintln!("{colored}"));
            }
            None => eprintln!("{colored}"),
        }
    });

    // install_fn wrapper — start the spinner, run the real install, stop.
    let pb_for_install = Arc::clone(&pb_cell);
    let install_fn: InstallFn = Box::new(move |version: &str| {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        pb.set_message(format!("downloading onebrain {version}…"));
        pb.enable_steady_tick(Duration::from_millis(80));
        // Hand the spinner to the sinks while install runs.
        *pb_for_install.lock().expect("pb mutex poisoned") = Some(pb.clone());

        let result = default_install_binary(version);

        // Release the spinner before the orchestrator's next stdout line
        // so the install summary line ("done: …") doesn't fight for the
        // cursor with finish_with_message.
        *pb_for_install.lock().expect("pb mutex poisoned") = None;
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
        install_fn: Some(install_fn),
        ..Default::default()
    }
}

/// Local alias for the install closure type — clippy complains about the
/// raw `Box<dyn Fn(...) + Send + Sync>` shape inline. The orchestrator's
/// `UpdateOptions::install_fn` field is identically shaped, so the alias is
/// purely a readability win.
type InstallFn = Box<dyn Fn(&str) -> Result<(), UpdateError> + Send + Sync>;

/// Map known orchestrator log-line prefixes to ANSI-colored variants. Lines
/// that don't match a known prefix are passed through untouched.
fn colorize_update_line(line: &str) -> String {
    if line == "OneBrain Update" {
        format!("{ANSI_BOLD_CYAN}{line}{ANSI_RESET}")
    } else if line.starts_with("done:") {
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
        obj.insert(
            "binary_targets".to_string(),
            serde_json::json!([
                {"triple": "aarch64-apple-darwin",        "ext": "tar.gz"},
                {"triple": "x86_64-apple-darwin",         "ext": "tar.gz"},
                {"triple": "aarch64-unknown-linux-gnu",   "ext": "tar.gz"},
                {"triple": "x86_64-unknown-linux-gnu",    "ext": "tar.gz"},
                {"triple": "x86_64-unknown-linux-musl",   "ext": "tar.gz"},
                {"triple": "aarch64-pc-windows-msvc",     "ext": "zip"},
                {"triple": "x86_64-pc-windows-msvc",      "ext": "zip"},
            ]),
        );
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_fs::update::UpdateResult;

    fn result(current: &str, latest: &str, ok: bool) -> UpdateResult {
        UpdateResult {
            ok,
            exit_code: if ok { 0 } else { 1 },
            current_version: Some(current.to_string()),
            latest_version: Some(latest.to_string()),
            error: None,
            latest_published_at: None,
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
}
