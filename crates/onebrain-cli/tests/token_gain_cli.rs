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

/// The current-window raw gain dir for the `t-token-gain` collection under a
/// tempdir-redirected cache: `<cache>/search/<collection>/token/gain/`.
fn gain_dir(cache: &Path) -> std::path::PathBuf {
    cache
        .join("search")
        .join("t-token-gain")
        .join("token")
        .join("gain")
}

/// Append one synthetic `GainEvent` JSONL line to the current-window raw log
/// (`<gain>/2026-07.jsonl`). Direct-write seeding stands in for the live
/// funnel traffic that Track 4 will add — the reporting surface only cares
/// that well-formed lines exist to read.
fn seed_event(gdir: &Path, ts: i64, before: u64, after: u64) {
    use std::io::Write;
    std::fs::create_dir_all(gdir).unwrap();
    let line = format!(
        "{{\"ts\":{ts},\"surface\":\"cli_search\",\"transform\":\"whitespace\",\
         \"level\":\"conservative\",\"bytes_before\":{before},\"bytes_after\":{after},\
         \"cache\":\"none\",\"session_token\":null}}\n"
    );
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(gdir.join("2026-07.jsonl"))
        .unwrap();
    f.write_all(line.as_bytes()).unwrap();
}

/// The core baseline-workflow guarantee (ADR 0030): after a `--reset`, the
/// default `token gain` read must reflect ONLY post-reset traffic, while
/// `--all-time` still reaches everything (incl. the archived pre-reset
/// epoch). This is the regression the R2 review caught — the default read
/// used to pivot the full cumulative rollup and return the same all-time
/// total after a reset, decorated with a "(since reset)" label that lied
/// about a scoping that never happened.
#[test]
fn token_gain_default_after_reset_shows_only_post_reset_traffic() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());
    let gdir = gain_dir(cache.path());

    // Pre-reset traffic: 2 events on 2026-07-11.
    seed_event(&gdir, 1_783_728_000, 1000, 400);
    seed_event(&gdir, 1_783_728_060, 1000, 400);

    // Reset → archives the 2 pre-reset events out of the current window.
    let reset = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--reset", "--label", "off", "--json"])
        .output()
        .unwrap();
    assert!(
        reset.status.success(),
        "reset stderr: {}",
        String::from_utf8_lossy(&reset.stderr)
    );

    // Post-reset traffic: 3 events on 2026-07-13 (fresh current window).
    seed_event(&gdir, 1_783_900_800, 500, 100);
    seed_event(&gdir, 1_783_900_860, 500, 100);
    seed_event(&gdir, 1_783_900_920, 500, 100);

    // Populate the cumulative rollup from ALL epochs (archived + current) —
    // this is the all-time state live traffic would maintain, and the exact
    // state that made the buggy default read return 5 instead of 3.
    let rebuild = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--rebuild", "--json"])
        .output()
        .unwrap();
    assert!(
        rebuild.status.success(),
        "rebuild stderr: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
    let rv: serde_json::Value = serde_json::from_slice(&rebuild.stdout).unwrap();
    assert_eq!(
        rv["data"]["rebuilt_events"], 5,
        "rebuild must see all 5 events across both epochs"
    );

    // DEFAULT read: must reflect ONLY the 3 post-reset events.
    let default = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--json"])
        .output()
        .unwrap();
    assert!(default.status.success());
    let dv: serde_json::Value = serde_json::from_slice(&default.stdout).unwrap();
    assert_eq!(
        dv["data"]["totals"]["count"], 3,
        "default read after --reset must be scoped to the post-reset window (3), \
         not the all-time cumulative total (5). data: {}",
        dv["data"]
    );
    assert_eq!(dv["data"]["all_time"], false);
    assert!(
        dv["data"]["since_reset"].is_string(),
        "since_reset marker date must be present in default post-reset read"
    );

    // --all-time read: must reflect BOTH epochs (5).
    let all = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--all-time", "--json"])
        .output()
        .unwrap();
    assert!(
        all.status.success(),
        "all-time stderr: {}",
        String::from_utf8_lossy(&all.stderr)
    );
    let av: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(
        av["data"]["totals"]["count"], 5,
        "--all-time must include the archived pre-reset epoch (5)"
    );
    assert_eq!(av["data"]["all_time"], true);
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

/// #287 end-to-end: a non-zero-padded `--since` must exit 70
/// (`E_INVALID_DATE`) at the process level — before the fix it exited 0 with
/// silently-zero results. The `2026-1-1` value is the exact live-proven case
/// (chrono parses it leniently; only the strict-width validator rejects it).
#[test]
fn token_gain_since_non_zero_padded_exits_70() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_vault(vault.path());

    let out = onebrain(vault.path(), cache.path())
        .args(["token", "gain", "--since", "2026-1-1"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(70),
        "malformed --since must exit 70 (E_INVALID_DATE); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("YYYY-MM-DD"), "{stderr}");
    assert!(stderr.contains("2026-1-1"), "{stderr}");
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
