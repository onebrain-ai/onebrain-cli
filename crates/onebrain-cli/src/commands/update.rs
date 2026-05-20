//! `onebrain update` — thin wiring around `onebrain_fs::update::run_update`.
//!
//! v3.0 ships non-TTY plain-text output by default. `--json` and `--plan`
//! emit machine-readable documents instead (intended for scripts and the
//! `/update` plugin skill). TTY pretty-printing (spinners, color, ascii
//! banner) is tracked for alpha.9 → GA.

use anyhow::Result;
use onebrain_fs::update::{run_update, UpdateOptions};

const RELEASES_BASE_URL: &str = "https://github.com/onebrain-ai/onebrain-cli/releases";

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

/// Build the JSON document for `--json` / `--plan`. The plan variant adds
/// `release_url` and `binary_url_template` fields that the `/update` skill
/// uses to present the release notes + a copy-paste download link.
fn build_json_document(
    result: &onebrain_fs::update::UpdateResult,
    plan: bool,
) -> serde_json::Value {
    let current = result.current_version.as_deref().unwrap_or("");
    let latest = result.latest_version.as_deref().unwrap_or("");
    let update_available = !current.is_empty()
        && !latest.is_empty()
        && !version_at_least_str(current, latest);
    let mut doc = serde_json::json!({
        "ok": result.ok,
        "current": current,
        "latest": latest,
        "update_available": update_available,
    });
    if let Some(err) = &result.error {
        doc.as_object_mut()
            .unwrap()
            .insert("error".to_string(), serde_json::Value::String(err.clone()));
    }
    if plan && update_available && !latest.is_empty() {
        let tag = if latest.starts_with('v') {
            latest.to_string()
        } else {
            format!("v{latest}")
        };
        let release_url = format!("{RELEASES_BASE_URL}/tag/{tag}");
        let binary_url_template = format!(
            "{RELEASES_BASE_URL}/download/{tag}/onebrain-<TRIPLE>.<EXT>"
        );
        doc.as_object_mut().unwrap().insert(
            "release_url".to_string(),
            serde_json::Value::String(release_url),
        );
        doc.as_object_mut().unwrap().insert(
            "binary_url_template".to_string(),
            serde_json::Value::String(binary_url_template),
        );
    }
    doc
}

/// Local copy of `update::version_at_least` for the JSON output. We
/// duplicate the semver comparison here (instead of re-exporting from
/// onebrain-fs) so the JSON shape stays decoupled from the orchestrator's
/// internal logic — the CLI-side meaning of "update available" is exactly
/// "remote semver > local semver".
fn version_at_least_str(current: &str, candidate: &str) -> bool {
    let c = current.trim_start_matches('v');
    let r = candidate.trim_start_matches('v');
    match (semver::Version::parse(c), semver::Version::parse(r)) {
        (Ok(curr), Ok(cand)) => curr >= cand,
        _ => current == candidate,
    }
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
        };
        let doc = build_json_document(&r, false);
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["error"], "Fetch failed: network");
    }
}
