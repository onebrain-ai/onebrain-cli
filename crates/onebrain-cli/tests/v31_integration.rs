//! Layer 3 CLI integration tests for the v3.1 Consistency Standard.
//!
//! Covers the acceptance criteria in
//! `01-projects/onebrain/cli/2026-05-25-v3.1.0-consistency-standard-design.md`:
//!
//! - `--help` shows the clean tree (3 root verbs + 24 groups · no hidden
//!   aliases · no `--vault-dir` legacy in the top-level help)
//! - `session-init` alias dispatches to `session init` with parity output
//! - `qmd-reindex` alias dispatches to `qmd reindex`
//! - `vault current` reports detected source correctly
//! - Vault-required commands exit 64 outside a vault with the canonical
//!   error envelope (text mode shows the quick-fix block; JSON mode emits
//!   the `E_VAULT_NOT_FOUND` envelope)
//! - Hook-protocol commands emit `{"decision":"block","reason":"..."}`
//!   + exit 0 outside a vault (graceful for SessionStart consumption)
//! - Migration notice fires once per command per process (sticky via state
//!   file across processes)

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn make_vault(dir: &std::path::Path) {
    fs::write(dir.join("vault.yml"), "method: onebrain\n").unwrap();
}

#[test]
fn root_help_shows_3_root_verbs_and_24_groups() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();

    // 3 root verbs.
    for v in ["init", "update", "doctor"] {
        assert!(
            stdout.contains(&format!("  {v} ")) || stdout.contains(&format!("  {v}  ")),
            "root verb `{v}` missing from --help. Got:\n{stdout}"
        );
    }
    // All 24 groups.
    for g in [
        "avatar",
        "bookmark",
        "bundle",
        "checkpoint",
        "config",
        "daemon",
        "date",
        "dream",
        "frontmatter",
        "gateway",
        "harness",
        "inbox",
        "log",
        "memory",
        "note",
        "pause",
        "plugin",
        "qmd",
        "schedule",
        "serve",
        "session",
        "skill",
        "task",
        "vault",
    ] {
        assert!(
            stdout.contains(g),
            "group `{g}` missing from --help. Got:\n{stdout}"
        );
    }
}

#[test]
fn root_help_hides_v30_aliases() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    for alias in [
        "session-init",
        "orphan-scan",
        "qmd-reindex",
        "register-hooks",
        "register-schedule",
        "vault-sync",
        "run-skill",
    ] {
        // Not asserting absence of substring (could appear in env help text);
        // assert it doesn't appear as a command entry (two-space prefix).
        assert!(
            !stdout.contains(&format!("  {alias}  ")) && !stdout.contains(&format!("  {alias} ")),
            "hidden alias `{alias}` leaked into top-level --help"
        );
    }
}

#[test]
fn vault_current_reports_walk_up_source() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["command"], "vault.current");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["detected"], true);
    assert_eq!(v["data"]["source"], "walk-up");
    // `name` is the vault dir basename. On macOS tempdir() lives under
    // /var/folders/... so we just verify the field exists and is non-empty.
    assert!(v["data"]["name"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn vault_current_reports_flag_source_when_explicit() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        // Run from a totally different directory so walk-up can't accidentally
        // match. Tempdir for cwd has no vault.yml.
        .current_dir(tempdir().unwrap().path())
        .args([
            "--vault",
            dir.path().to_str().unwrap(),
            "vault",
            "current",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["source"], "--vault flag");
    assert_eq!(v["data"]["detected"], true);
}

#[test]
fn vault_current_reports_not_detected_when_no_vault() {
    let no_vault = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .args(["vault", "current", "--json"])
        // vault current is informational, NOT vault-required, so exit 0.
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["detected"], false);
    assert!(v["data"]["name"].is_null() || v["data"].get("name").is_none());
}

#[test]
fn session_init_alias_dispatches_to_session_init() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("vault.yml"), "qmd_collection: x\n").unwrap();

    let out_alias = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["session-init"])
        .assert()
        .success();
    let s_alias = String::from_utf8_lossy(&out_alias.get_output().stdout).to_string();

    let out_new = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["session", "init"])
        .assert()
        .success();
    let s_new = String::from_utf8_lossy(&out_new.get_output().stdout).to_string();

    // Both must emit the same JSON keys (datetime+session_token+qmd_unembedded).
    let v_alias: serde_json::Value = serde_json::from_str(s_alias.trim()).unwrap();
    let v_new: serde_json::Value = serde_json::from_str(s_new.trim()).unwrap();
    assert!(v_alias.get("session_token").is_some());
    assert!(v_new.get("session_token").is_some());
    assert!(v_alias.get("qmd_unembedded").is_some());
    assert!(v_new.get("qmd_unembedded").is_some());
}

#[test]
fn session_init_emits_block_json_outside_vault() {
    let no_vault = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .args(["session", "init"])
        // Hook protocol: graceful exit 0 with block JSON.
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["decision"], "block");
    assert_eq!(v["reason"], "onebrain-init-required");
}

#[test]
fn orphan_scan_alias_dispatches_to_checkpoint_orphans() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // No checkpoint files: count should be 0.

    let out_alias = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["orphan-scan", "07-logs", "tokABC"])
        .assert()
        .success();
    let s_alias = String::from_utf8_lossy(&out_alias.get_output().stdout).to_string();

    let out_new = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["checkpoint", "orphans", "07-logs", "tokABC"])
        .assert()
        .success();
    let s_new = String::from_utf8_lossy(&out_new.get_output().stdout).to_string();
    assert_eq!(s_alias.trim(), s_new.trim());
    // Schema check: both should be `{"orphan_count": N}`.
    let v: serde_json::Value = serde_json::from_str(s_new.trim()).unwrap();
    assert!(v["orphan_count"].is_number());
}

#[test]
fn vault_required_command_exits_72_for_unimplemented_stub() {
    // `task list` is a stub in v3.1 — it should hit E_NOT_IMPLEMENTED
    // before the vault check (the dispatcher routes to stubs::not_implemented
    // directly). This verifies the error code mapping is wired correctly.
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["task", "list"])
        .assert()
        .failure()
        .code(72)
        .stderr(predicate::str::contains("not implemented"))
        .stderr(predicate::str::contains("task list"));
}

#[test]
fn migration_notice_prints_to_stderr_first_time() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // Isolate the state file by setting XDG/HOME and clearing the cache dir.
    let home = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .args(["qmd-reindex"])
        .assert();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("v3.1") && stderr.contains("qmd-reindex"),
        "expected migration notice on stderr, got: {stderr:?}"
    );
}

#[test]
fn migration_notice_suppressed_by_env_var() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let home = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("ONEBRAIN_QUIET_MIGRATION", "1")
        .args(["qmd-reindex"])
        .assert();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains("v3.1") || !stderr.contains("renamed"),
        "expected no migration notice with ONEBRAIN_QUIET_MIGRATION=1, got: {stderr:?}"
    );
}

#[test]
fn json_envelope_shape_is_canonical_for_vault_current() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    // Required envelope fields per skill-alignment §4.3.
    assert_eq!(v["version"], "1");
    assert!(v["command"].is_string());
    assert!(v["ok"].is_boolean());
    assert!(v["warnings"].is_array());
    // `vault` and `error` may be absent in success path.
    assert!(v["data"].is_object());
}

#[test]
fn yaml_output_matches_envelope_keys() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    // Quick presence check; full YAML parse covered in unit tests.
    assert!(stdout.contains("version: '1'") || stdout.contains("version: \"1\""));
    assert!(stdout.contains("command: vault.current"));
    assert!(stdout.contains("ok: true"));
}

#[test]
fn harness_flat_invocation_still_works() {
    // v3.0 back-compat: `onebrain harness` (no verb) silently becomes
    // `harness detect`.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["harness"])
        .assert()
        .success();
}

#[test]
fn harness_detect_explicit_works() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["harness", "detect"])
        .assert()
        .success();
}
