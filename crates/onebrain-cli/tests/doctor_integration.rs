//! Layer 2 integration tests for `onebrain doctor` — exercises the wired CLI
//! end-to-end (assert_cmd spawns the real binary) against synthetic vaults
//! built in tempdirs. Verifies exit codes, stdout/stderr content, and
//! warning-vs-error distinction.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

/// Build a minimal vault that should pass every check.
///
/// Includes:
/// - `vault.yml` with all 8 folder keys + `update_channel: stable` (so the
///   `vault.yml-keys` check has no soft-required warning).
/// - All 8 vault folders.
/// - `.claude/plugins/onebrain/` with the required files + non-empty
///   `agents/` and `skills/foo/`.
/// - `.claude/settings.json` with the canonical exec-form Stop hook and the
///   `Bash(onebrain *)` permission (so `settings-hooks` reports ok).
fn write_minimal_vault(dir: &Path) {
    std::fs::write(
        dir.join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        std::fs::create_dir_all(dir.join(f)).unwrap();
    }
    let plugin = dir.join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = dir.join(".claude");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
}

#[test]
fn doctor_clean_vault_exits_0() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Exit code 0 means no `Error` status emerged from any check.
    // Note: this fixture omits `qmd_collection` (matching the plan), so the
    // qmd-embeddings check returns Warn per Bun parity. That keeps the exit
    // code at 0 but means the summary line will be "warning(s) — ok to run"
    // rather than "all passed". The test asserts the no-error invariant by
    // checking that no `[✗]` row is rendered.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("[\u{2717}]").not())
        .stdout(predicate::str::contains("ok to run").or(predicate::str::contains("all passed")));
}

#[test]
fn doctor_missing_folder_exits_1() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    std::fs::remove_dir_all(d.path().join("01-projects")).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("[\u{2717}]"))
        .stdout(predicate::str::contains("Missing: 01-projects"));
}

#[test]
fn doctor_missing_vault_yml_errors_out() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not inside a vault"));
}

/// `--fix` on a vault where the only warning is "qmd_collection not set"
/// (the minimal vault used by these integration tests) — the recipe
/// dispatcher must route this to `Manual` rather than spawning `qmd
/// embed`, because no real qmd collection exists to embed against. The
/// test runs with a scrubbed PATH so that even if dispatch is wrong the
/// child can't accidentally execute a real `qmd` on the developer's box.
#[test]
fn doctor_fix_qmd_collection_not_set_routes_to_manual() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        // Scrub PATH so `which::which("qmd")` returns NotFound even on
        // developer machines where `qmd` is on the global PATH — this
        // test must NOT spawn real qmd. The `/usr/bin:/bin` floor keeps
        // basic POSIX utilities available in case any other code path
        // needs them.
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // The Fix block must appear (proves the new pass ran).
    assert!(
        stdout.contains("Fix · attempting") || stdout.contains("Fix · nothing"),
        "expected the Fix block in stdout · got: {stdout}"
    );
    // Manual outcome should fire — no `running: qmd embed` line.
    assert!(
        !stdout.contains("running: qmd embed"),
        "qmd embed should NOT spawn for the 'qmd_collection not set' variant · got: {stdout}"
    );
    // No "deferred to v3.0.1" stub anymore — that was the alpha.4 placeholder.
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap_or_default();
    assert!(
        !stderr.contains("deferred to v3.0.1"),
        "stub message should be gone in alpha.5"
    );
}

#[test]
fn doctor_invalid_yaml_falls_back_to_defaults() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("vault.yml"), "not: : valid").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains("invalid YAML"));
}

#[test]
fn doctor_orphan_checkpoints_warns_without_failing() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let cp = d.path().join("07-logs/checkpoint");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success() // warning-only
        .stdout(predicate::str::contains("1 unmerged"));
}

#[test]
fn doctor_stale_marketplace_warns() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Override settings.json with stale marketplace block (keep the canonical
    // Stop hook + permission so `settings-hooks` still reports ok).
    std::fs::write(
        d.path().join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]},"extraKnownMarketplaces":{"onebrain":{"source":{"repo":"kengio/onebrain"}}}}"#,
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success() // warn-only
        .stdout(predicate::str::contains("stale marketplace repo"));
}
