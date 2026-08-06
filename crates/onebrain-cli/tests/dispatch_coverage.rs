//! Parametric coverage tests for `v31/dispatch.rs`.
//!
//! Two jobs:
//! 1. `every_removed_verb_is_now_an_unknown_command` — the inverse of v3.4.24's
//!    63-verb removal (#334). Reads the committed frozen list and asserts each
//!    verb now exits 2. This is the only guard that catches an INCOMPLETE
//!    removal; see its doc comment.
//! 2. Direct hits on REAL dispatch arms (completions, schedule, daemon, plugin,
//!    skill, harness, serve, run-skill) so llvm-cov does not count them missed.
//!
//! The old stub-arm tests (`non_vault_stubs_always_exit_72`,
//! `vault_required_stubs_exit_72_inside_64_outside`,
//! `stub_error_message_contains_verb_path`, `session_get_stub_exits_72`) were
//! deleted with the verbs they drove — 64 assertions whose subjects no longer
//! exist.

mod support;

use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Create a minimal vault (onebrain.yml sentinel — the canonical name; using
/// the legacy `vault.yml` would fire a deprecation warning on every subprocess
/// and clutter test stderr).
fn vault_dir() -> tempfile::TempDir {
    let d = tempdir().unwrap();
    fs::write(d.path().join("onebrain.yml"), "method: onebrain\n").unwrap();
    d
}

/// Run `onebrain <args>` in `cwd` and return the exit code. `ONEBRAIN_VAULT`
/// is cleared so walk-up is the only auto-discovery path.
fn exit_in(cwd: &std::path::Path, args: &[&str]) -> i32 {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(cwd)
        .env_remove("ONEBRAIN_VAULT")
        .args(args)
        .output()
        .expect("spawn failed")
        .status
        .code()
        .unwrap_or(-1)
}

// ─────────────────────────────────────────────────────────────────────────────
// Parametric: every REMOVED verb → exit 2 (unknown command)
// ─────────────────────────────────────────────────────────────────────────────

/// The inverse of the removal: every verb on the frozen list must now fail as
/// an unknown command.
///
/// 🔴 This is the ONLY guard that catches an INCOMPLETE removal. The mechanical
/// sweep in the plan searches for stale *references to* removed verbs — but a
/// verb left behind in the dispatcher has no stale reference, because its own
/// arm is where it lives. The compiler protects only 34 of the 63 (those that
/// called `not_implemented_vault_required`, now deleted); the other 29 called
/// `not_implemented`, which SURVIVES for the `plugin uninstall` hybrid, so a
/// forgotten arm among them would compile clean and pass every other test.
///
/// The list is committed at `tests/fixtures/removed-verbs.txt` rather than
/// re-derived, because it is derived from the very call sites this change
/// deletes — regenerating it after the fact yields an empty list and a gate
/// that cannot fail.
#[test]
fn every_removed_verb_is_now_an_unknown_command() {
    let list = include_str!("fixtures/removed-verbs.txt");
    let verbs: Vec<&str> = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        verbs.len(),
        63,
        "the frozen list must hold all 63 removed verbs"
    );
    for v in verbs {
        let (group, verb) = v.split_once(' ').unwrap();
        let out = Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
            .env_remove("ONEBRAIN_VAULT")
            .args([group, verb])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "`{v}` still parses — it was not removed from the dispatcher"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Completions — real arm not yet touched by other tests
// ─────────────────────────────────────────────────────────────────────────────

/// `onebrain completions <shell>` routes through the `Cmd::Completions` arm.
/// All supported shells must succeed (exit 0) and emit non-empty content on
/// stdout.
#[test]
fn completions_arm_exits_0_with_output() {
    for shell in ["bash", "zsh", "fish"] {
        let out = Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
            .args(["completions", shell])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "completions {shell} expected exit 0. got {:?}",
            out.status.code()
        );
        assert!(
            !out.stdout.is_empty(),
            "completions {shell} expected non-empty stdout"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Schedule register arm — real arm (not a stub)
// ─────────────────────────────────────────────────────────────────────────────

/// `schedule register --dry-run` inside a vault exercises the real
/// `ScheduleVerb::Register` arm (not a stub). The dry-run path exits 0 when
/// no schedule entries are present.
#[test]
fn schedule_register_dry_run_exits_0_inside_vault() {
    let vault = vault_dir();
    let code = exit_in(vault.path(), &["schedule", "register", "--dry-run"]);
    assert_eq!(
        code, 0,
        "schedule register --dry-run inside vault should exit 0"
    );
}

/// `schedule list` (#116 bug 3) is wired to the same status path as
/// `schedule register --status` — it must exit 0 inside a vault and print
/// the schedule summary, not fall through to the not-implemented stub.
#[test]
fn schedule_list_exits_0_and_prints_schedule_inside_vault() {
    let vault = vault_dir();
    std::fs::write(
        vault.path().join("onebrain.yml"),
        "schedule:\n  - cron: \"0 9 * * *\"\n    skill: /daily\n",
    )
    .unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["schedule", "list"])
        .output()
        .expect("spawn failed");
    assert_eq!(
        out.status.code(),
        Some(0),
        "schedule list inside vault should exit 0. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Registered schedules: 1"),
        "expected schedule summary in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("[cron]"),
        "expected [cron] tag in stdout, got: {stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon arms — already implemented (not stubs), but may not have been
// exercised by other integration tests. `daemon status` is the least
// disruptive real verb to call (no side-effects if no daemon is running).
// ─────────────────────────────────────────────────────────────────────────────

/// `daemon status` is a real handler. Must exit 0 (no daemon running → reports
/// that cleanly) without panicking.
#[test]
fn daemon_status_exits_0_with_no_running_daemon() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env_remove("ONEBRAIN_VAULT")
        .args(["daemon", "status"])
        .output()
        .unwrap();
    // Daemon status exits 0 when no daemon is running (reports "not running").
    // If the exit is non-zero that's unexpected and we surface the output.
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Allow 0 (not running) or 1 (could not detect). Do NOT allow a panic (101).
    assert!(
        code == 0 || code == 1,
        "daemon status panicked or produced unexpected exit {code}.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("panicked at"),
        "daemon status panicked:\nstderr: {stderr}"
    );
}

/// `daemon stop` with no running daemon should exit gracefully (not panic).
#[test]
fn daemon_stop_graceful_when_not_running() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env_remove("ONEBRAIN_VAULT")
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "daemon stop panicked:\nstderr: {stderr}"
    );
    // Graceful = a clean status code (0 = stopped/none-running, 1 = handled
    // "not running" report), never a crash/abort code.
    let code = out.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "daemon stop should exit 0 or 1 when no daemon runs, got {code}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Real handler arms reachable WITHOUT network / subprocess / a blocking server
// ─────────────────────────────────────────────────────────────────────────────
//
// These arms route to a real handler that completes (or fails its vault/arg
// guard) entirely in-process — no `claude`/`gemini`/`qmd`/`git` spawn, no
// listener bind, no TTY. Each was previously uncovered because only the
// hidden v3.0 *alias* spelling (or no spelling at all) had an integration test.

// `onebrain qmd reindex` was removed in v3.4.5 — `Cmd::Qmd { .. }` is now a
// hidden catch-all that bails with a helpful migration error (see
// `tests/qmd_removed.rs`). The legacy `qmd-reindex` alias still exists but
// now dispatches to the native `search reindex` handler.

/// `plugin install` routes to `register_hooks::run` (the v3.0 `register-hooks`
/// body) against the target vault. Filesystem-only — harness detection is
/// dir/env based, never a `claude` spawn — so it always lands a clean exit 0.
/// Exercises the `PluginVerb::Install` arm (the alias `register-hooks` is
/// covered elsewhere, but this v3.1 spelling was not).
#[test]
fn plugin_install_into_vault_exits_0() {
    let vault = vault_dir();
    let code = exit_in(
        vault.path(),
        &[
            "plugin",
            "install",
            "--vault-dir",
            vault.path().to_str().unwrap(),
        ],
    );
    assert_eq!(
        code, 0,
        "plugin install into a writable vault should exit 0, got {code}"
    );
}

/// `plugin migrate <unknown>` routes to `migrate::run`, which loads the vault
/// config and dispatches the named migration. An unknown name is reported on
/// stderr and — per the internal-command contract — still exits 0. Exercises
/// the `PluginVerb::Migrate` arm (the alias `migrate` is covered elsewhere).
#[test]
fn plugin_migrate_unknown_migration_exits_0() {
    let vault = vault_dir();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args([
            "plugin",
            "migrate",
            "not-a-real-migration",
            "--vault-dir",
            vault.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "plugin migrate (unknown migration) must exit 0 per the internal-command contract"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown migration"),
        "expected an 'unknown migration' note on stderr. got:\n{stderr}"
    );
}

/// `skill show <name>` resolves the vault, then reads the skill's `SKILL.md`.
/// Run inside a vault whose plugin tree has no such skill, the handler returns
/// `66` (EX_NOINPUT). Exercises the `SkillVerb::Show` arm — `vault_ctx::require`
/// succeeds (walk-up finds `onebrain.yml`) so the handler body actually runs.
#[test]
fn skill_show_missing_skill_in_vault_exits_66() {
    let vault = vault_dir();
    let code = exit_in(vault.path(), &["skill", "show", "daily"]);
    assert_eq!(
        code, 66,
        "skill show for a missing skill inside a vault should exit 66 (EX_NOINPUT), got {code}"
    );
}

/// `skill info <name>` mirror of `skill show` — same vault-resolve-then-read
/// path, same `66` for a missing skill. Exercises the `SkillVerb::Info` arm.
#[test]
fn skill_info_missing_skill_in_vault_exits_66() {
    let vault = vault_dir();
    let code = exit_in(vault.path(), &["skill", "info", "daily"]);
    assert_eq!(
        code, 66,
        "skill info for a missing skill inside a vault should exit 66 (EX_NOINPUT), got {code}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Vault / arg guard arms that fail BEFORE any harness or server spawn
// ─────────────────────────────────────────────────────────────────────────────
//
// `harness run` (default `--mode with-context`), `serve`, `skill run`, and the
// `run-skill` alias all reach a guard — vault resolution or a required-arg
// check — that returns `Err` *before* any `claude`/`gemini` process is spawned
// or any TCP listener is bound. That makes the dispatch arm reachable without
// the dangerous side effect: we only ever exercise the early-return path.
// (The real harness/serve bodies — spawn + block — stay genuinely residual.)

/// `harness run <prompt>` with the default `with-context` mode requires a
/// vault; resolution is the FIRST statement in that arm, so outside any vault
/// it returns `E_VAULT_NOT_FOUND` (exit 64) before reaching the harness spawn.
/// (NB: 64 here, not the 78 that `harness_run::run`'s own internal config check
/// would yield — the dispatcher's `vault_ctx::require` fires first.)
#[test]
fn harness_run_with_context_without_vault_exits_64() {
    let neutral = tempdir().unwrap(); // no onebrain.yml anywhere above
    let code = exit_in(neutral.path(), &["harness", "run", "ping"]);
    assert_eq!(
        code, 64,
        "harness run (with-context) outside a vault should exit 64 before spawning, got {code}"
    );
}

/// `serve` is vault-required; `vault_ctx::require` is the first thing the
/// handler does, so outside a vault it exits 64 *before* binding a listener
/// (no hang). Exercises the `Cmd::Serve` arm.
#[test]
fn serve_without_vault_exits_64() {
    let neutral = tempdir().unwrap();
    let code = exit_in(neutral.path(), &["serve"]);
    assert_eq!(
        code, 64,
        "serve outside a vault should exit 64 before binding a server, got {code}"
    );
}

/// `skill run` with neither a positional name nor `--skill` hits the
/// `ok_or_else` guard (before vault resolution and before any spawn), surfacing
/// a usage error → generic exit 1.
#[test]
fn skill_run_without_name_errors_exit_1() {
    let neutral = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(neutral.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["skill", "run"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "skill run with no name should exit 1 (generic)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("skill name"),
        "expected a 'skill name' usage error on stderr. got:\n{stderr}"
    );
}

/// `run-skill` (hidden v3.0 alias), post-#263-Part-2 hardening: with no
/// `--vault`/`--vault-dir` flag and `ONEBRAIN_VAULT` cleared, the alias now
/// resolves through the SAME walk-up chain as the modern `skill run` arm
/// (`vault_ctx::require`) instead of erroring up-front via `ok_or_else`. When
/// nothing is found anywhere above cwd, that walk-up itself fails with
/// `CoreError::VaultNotFound` → exit 64 (E_VAULT_NOT_FOUND) — the same guard
/// the modern arm hits — rather than the old bare "requires --vault" usage
/// error (exit 1). Exercises the `Cmd::RunSkillAlias` missing-vault branch.
#[test]
fn run_skill_alias_without_vault_exits_64() {
    let neutral = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(neutral.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["run-skill", "--skill", "daily"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(64),
        "run-skill without --vault, and no vault anywhere above cwd, should exit 64 (E_VAULT_NOT_FOUND)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no OneBrain vault found"),
        "expected the walk-up 'no OneBrain vault found' error on stderr. got:\n{stderr}"
    );
}

/// Positive counterpart to the above: `run-skill` run from a directory
/// *nested inside* a vault (not the vault root itself), with no
/// `--vault`/`--vault-dir` flag, must resolve via walk-up and actually reach
/// the harness-spawn body — proving the alias genuinely walks up now, rather
/// than only ever erroring when a flag is absent.
///
/// Uses a written mock `claude` script (exits 0 immediately) pointed at by
/// `CLAUDE_BIN`, so the test is hermetic and instant. A bare `CLAUDE_BIN=/bin/true`
/// is NOT portable: `/bin/true` doesn't exist on macOS (only `/usr/bin/true`),
/// so the harness resolver would report it missing and fall through to a real
/// installed `claude` — burning API tokens and taking tens of seconds.
#[cfg(unix)]
#[test]
fn run_skill_alias_resolves_vault_via_walk_up_from_nested_cwd() {
    use std::os::unix::fs::PermissionsExt;
    let vault = vault_dir();
    let nested = vault.path().join("00-inbox").join("nested");
    std::fs::create_dir_all(&nested).unwrap();

    let mock = vault.path().join("mock-claude.sh");
    std::fs::write(&mock, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&mock).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&mock, perms).unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .current_dir(&nested)
        .env_remove("ONEBRAIN_VAULT")
        .env("CLAUDE_BIN", &mock)
        .args(["run-skill", "--skill", "daily"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "run-skill with no --vault, invoked from a directory nested inside a \
         vault, should walk up and succeed rather than hitting the old \
         up-front usage error. stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
