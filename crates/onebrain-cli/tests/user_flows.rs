//! End-to-end user-flow walkthroughs.
//!
//! Each test pretends to be a different user persona and asserts the
//! binary's overall behaviour rather than a single command in isolation.
//! Goal: catch regressions where a unit-level fix breaks the larger
//! "what does the user experience?" story.

use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

/// The binary, with its search-cache root pinned to a throwaway tempdir.
///
/// Mandatory for any test that names a collection and then runs the binary:
/// several commands OPEN a collection that already exists under the resolved
/// cache root, and opening is not read-only. Two flows below write
/// `qmd_collection: ob-1` / `x`, so an unisolated run would reach the
/// developer's real index the moment such a collection existed. Applied to
/// every invocation in this file, not just those two — the hazard is the
/// missing isolation, not the individual name. Enforced by
/// `tests/cache_isolation_sweep.rs`.
fn onebrain_cmd(cache: &tempfile::TempDir) -> Command {
    let mut cmd = Command::cargo_bin("onebrain").unwrap();
    cmd.env("ONEBRAIN_CACHE_DIR", cache.path());
    cmd
}

/// Flow 1 · brand-new user. No vault. Runs the binary in a random dir.
///
/// Expected:
/// - `onebrain --help` succeeds, lists commands.
/// - `onebrain session init` (default text) prints a friendly "no vault
///   found" line, NOT a JSON block — the regression we are guarding
///   against is the old JSON-by-default behaviour that confused users.
/// - The structured envelope is still accessible via `--json`.
#[test]
fn flow_new_user_no_vault() {
    let dir = tempdir().unwrap();
    let cache = tempdir().unwrap();

    // Help must always work.
    let help = onebrain_cmd(&cache)
        .args(["--help"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(help.status.success(), "--help failed");
    let help_out = String::from_utf8_lossy(&help.stdout);
    assert!(
        help_out.contains("Commands") || help_out.contains("commands"),
        "help missing command list: {help_out}"
    );

    // Default `session init` → text, not JSON.
    let init = onebrain_cmd(&cache)
        .args(["session", "init"])
        .current_dir(dir.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(init.status.success(), "session init failed");
    let init_out = String::from_utf8_lossy(&init.stdout);
    assert!(
        !init_out.trim_start().starts_with('{'),
        "default session init emitted JSON (regression!): {init_out}"
    );
    assert!(
        init_out.contains("No OneBrain vault found"),
        "expected friendly not-found message: {init_out}"
    );
    // Must mention what to do next.
    assert!(
        init_out.contains("onebrain init"),
        "expected actionable hint mentioning `onebrain init`: {init_out}"
    );

    // --json still gives the structured envelope.
    let init_json = onebrain_cmd(&cache)
        .args(["session", "init", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let json_out = String::from_utf8_lossy(&init_json.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(json_out.trim()).expect("--json must emit valid JSON");
    assert_eq!(parsed["decision"], "block");
    assert_eq!(parsed["reason"], "onebrain-vault-not-found");
}

/// Flow 2 · established user inside a real vault, running interactively.
///
/// Expected:
/// - `session init` (default) emits a one-line "Session ready · ..." marker.
/// - `--json` returns the full SessionInitOutput shape with datetime +
///   session_token + qmd_unembedded.
#[test]
fn flow_established_user_inside_vault() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    fs::write(vault.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

    // Interactive text.
    let init = onebrain_cmd(&cache)
        .args(["session", "init"])
        .current_dir(vault.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(init.status.success(), "session init failed");
    let out = String::from_utf8_lossy(&init.stdout);
    assert!(
        !out.trim_start().starts_with('{'),
        "inside-vault default must be text, got JSON: {out}"
    );
    assert!(
        out.contains("Session ready"),
        "expected ready marker: {out}"
    );

    // --json envelope.
    let init_json = onebrain_cmd(&cache)
        .args(["session", "init", "--json"])
        .current_dir(vault.path())
        .output()
        .unwrap();
    let json_out = String::from_utf8_lossy(&init_json.stdout);
    let v: serde_json::Value = serde_json::from_str(json_out.trim()).expect("invalid JSON");
    assert!(
        v.get("datetime").and_then(|d| d.as_str()).is_some(),
        "missing datetime field: {v:?}"
    );
    assert!(
        v.get("session_token").and_then(|s| s.as_str()).is_some(),
        "missing session_token: {v:?}"
    );
    // v3.4.5 contract: `search_unembedded` (canonical) is always present, and
    // is `null` when the probe can't determine the count (e.g. no index in CI)
    // or a non-negative integer otherwise. The deprecated `qmd_unembedded`
    // alias is emitted alongside with the same value for one transition
    // version. This test is non-hermetic (PATH not scrubbed), so accept either
    // — never absent.
    let unembedded = v
        .get("search_unembedded")
        .expect("search_unembedded key missing");
    assert!(
        unembedded.is_null() || unembedded.is_u64(),
        "search_unembedded must be null or a u64: {v:?}"
    );
    assert_eq!(
        v.get("qmd_unembedded"),
        Some(unembedded),
        "deprecated qmd_unembedded alias must mirror search_unembedded: {v:?}"
    );
    assert!(
        v.get("decision").is_none(),
        "happy path must not include decision: {v:?}"
    );
}

/// Flow 3 · Claude Code hook consumer. Runs the binary in subprocess +
/// parses --json output the way the hook script does.
#[test]
fn flow_hook_consumer_parses_session_init_json() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    fs::write(vault.path().join("vault.yml"), "qmd_collection: x\n").unwrap();

    // Run with the same shape the hook's argv writes after `plugin update`
    // has run: ["session", "init", "--json"].
    let out = onebrain_cmd(&cache)
        .args(["session", "init", "--json"])
        .current_dir(vault.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "hook subprocess failed");

    // stdout is JSON only — no banner, no extra prose. The hook script
    // pipes stdout through `jq` and expects exactly one JSON document.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with('{') && stdout.trim_end().ends_with('}'),
        "expected pure JSON object, got: {stdout:?}"
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    // Hook contract requires these fields by name (search_unembedded is the
    // canonical count; qmd_unembedded is the deprecated transition alias).
    for k in &[
        "datetime",
        "session_token",
        "search_unembedded",
        "qmd_unembedded",
    ] {
        assert!(
            parsed.get(*k).is_some(),
            "hook contract violated: `{k}` missing from {parsed:?}"
        );
    }
}

/// Flow 4 · error recovery. User has a vault but vault.yml is malformed.
#[test]
fn flow_error_recovery_malformed_vault_yml() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    fs::write(vault.path().join("vault.yml"), "not: : valid yaml\n").unwrap();

    // Default mode — actionable text.
    let init = onebrain_cmd(&cache)
        .args(["session", "init"])
        .current_dir(vault.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(init.status.success(), "malformed yaml hook must exit 0");
    let out = String::from_utf8_lossy(&init.stdout);
    assert!(
        !out.trim_start().starts_with('{'),
        "default must be text: {out}"
    );
    assert!(
        out.contains("malformed"),
        "expected malformed marker: {out}"
    );
    assert!(
        out.contains("doctor") || out.contains("onebrain.yml"),
        "expected actionable hint: {out}"
    );

    // --json: distinct reason for the SessionStart consumer.
    let init_json = onebrain_cmd(&cache)
        .args(["session", "init", "--json"])
        .current_dir(vault.path())
        .output()
        .unwrap();
    let json_out = String::from_utf8_lossy(&init_json.stdout);
    let v: serde_json::Value = serde_json::from_str(json_out.trim()).unwrap();
    assert_eq!(v["decision"], "block");
    assert_eq!(v["reason"], "onebrain-vault-malformed");
    assert!(
        v.get("error_detail").and_then(|d| d.as_str()).is_some(),
        "error_detail must be present and non-empty: {v:?}"
    );
}
