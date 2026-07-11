//! E2E: `onebrain token gain` against a real temp vault.
//!
//! No live traffic writes `GainEvent`s in this PR (Track 4 wires the
//! MCP/CLI/daemon surfaces through the funnel) — these tests exercise the
//! reporting/administration surface against a fresh, empty token.redb + raw
//! log, which is the honest default state: zeroed totals, empty history.

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn onebrain(vault_root: &Path, cache_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    cmd.env("ONEBRAIN_CACHE_DIR", cache_dir)
        .env("ONEBRAIN_NO_DAEMON", "1")
        .arg("--vault")
        .arg(vault_root);
    cmd
}

fn write_vault(dir: &Path) {
    std::fs::write(
        dir.join("onebrain.yml"),
        "search:\n  collection: t-token-gain\n",
    )
    .unwrap();
}

#[test]
fn token_gain_help_lists_the_verb() {
    let out = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .args(["token", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("gain"), "stdout:\n{stdout}");
}

#[test]
fn token_gain_json_on_fresh_vault_reports_zeroed_totals() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "token.gain");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["totals"]["count"], 0);
    assert_eq!(v["data"]["totals"]["bytes_before"], 0);
    assert_eq!(v["data"]["totals"]["bytes_after"], 0);
    assert_eq!(v["data"]["rows"], serde_json::json!([]));
}

#[test]
fn token_gain_text_on_fresh_vault_shows_summary_and_estimate_note() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Gain Summary"), "{stdout}");
    assert!(stdout.contains("estimate"), "{stdout}");
}

#[test]
fn token_gain_history_json_on_fresh_vault_is_an_empty_list() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--history", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["history"], serde_json::json!([]));
}

#[test]
fn token_gain_rebuild_json_on_fresh_vault_reports_zero_events() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--rebuild", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["rebuilt_events"], 0);
}

#[test]
fn token_gain_reset_json_archives_and_reports_marker() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--reset", "--label", "baseline", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["data"]["archived_to"].is_string());
    assert!(
        v["data"]["archived_to"]
            .as_str()
            .unwrap()
            .contains("baseline"),
        "{v}"
    );
    assert!(v["data"]["since_reset"].is_string());

    // The archive directory actually exists on disk (never-deletes contract).
    let archived_to = v["data"]["archived_to"].as_str().unwrap();
    assert!(Path::new(archived_to).is_dir(), "{archived_to}");
}

#[test]
fn token_gain_reset_requires_reset_flag_for_label() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--label", "baseline"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "clap must reject --label without --reset"
    );
}

#[test]
fn token_gain_by_rejects_unrecognized_axis() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--by", "bogus"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bogus"), "{stderr}");
}

#[test]
fn token_gain_by_pivot_json_on_fresh_vault_has_no_rows() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--by", "month,surface", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["rows"], serde_json::json!([]));
}

#[test]
fn token_gain_outside_a_vault_fails() {
    let cache = tempdir().unwrap();
    let no_vault = tempdir().unwrap();
    let out = onebrain(no_vault.path(), cache.path())
        .args(["token", "gain", "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
