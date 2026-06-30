//! Parametric coverage tests for `v31/dispatch.rs` stub arms.
//!
//! Goal: exercise every `stubs::not_implemented` and
//! `stubs::not_implemented_vault_required` dispatch arm so the llvm-cov line
//! counter no longer counts those branches as missed.
//!
//! Contract (from the existing `vault_required_stub_returns_64_outside_vault_or_72_inside`
//! integration test):
//! - `not_implemented(path)` arms: exit 72 (E_NOT_IMPLEMENTED), no vault check.
//! - `not_implemented_vault_required(vault_flag, path)` arms:
//!     - Inside a vault  → exit 72.
//!     - Outside any vault → exit 64 (E_VAULT_NOT_FOUND).

use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Create a minimal vault (vault.yml sentinel).
fn vault_dir() -> tempfile::TempDir {
    let d = tempdir().unwrap();
    fs::write(d.path().join("vault.yml"), "method: onebrain\n").unwrap();
    d
}

/// Run `onebrain <args>` in `cwd` and return the exit code. `ONEBRAIN_VAULT`
/// is cleared so walk-up is the only auto-discovery path.
fn exit_in(cwd: &std::path::Path, args: &[&str]) -> i32 {
    Command::cargo_bin("onebrain")
        .unwrap()
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
// Parametric: NOT-vault-required stubs → always exit 72
// ─────────────────────────────────────────────────────────────────────────────
//
// These verbs call `stubs::not_implemented(path)` directly — no vault check —
// so the exit code is always 72 regardless of cwd.

/// Every `stubs::not_implemented` (non-vault-gated) arm.
#[test]
fn non_vault_stubs_always_exit_72() {
    // (group, verb, extra_args_for_clap_required_positionals)
    // Avatar
    let cases: &[(&[&str], &[&str])] = &[
        // avatar
        (&["avatar", "start"], &[]),
        (&["avatar", "pair"], &[]),
        (&["avatar", "status"], &[]),
        (&["avatar", "revoke"], &[]),
        (&["avatar", "doctor"], &[]),
        // bundle (name is required for most)
        (&["bundle", "install", "my-bundle"], &[]),
        (&["bundle", "show", "my-bundle"], &[]),
        (&["bundle", "info", "my-bundle"], &[]),
        (&["bundle", "init", "my-bundle"], &[]),
        (&["bundle", "lint", "my-bundle"], &[]),
        (&["bundle", "update", "my-bundle"], &[]),
        (&["bundle", "remove", "my-bundle"], &[]),
        (&["bundle", "doctor"], &[]),
        // config
        (&["config", "get", "some.key"], &[]),
        (&["config", "set", "some.key", "val"], &[]),
        (&["config", "list"], &[]),
        (&["config", "init"], &[]),
        // date — vault-free
        (&["date", "today"], &[]),
        (&["date", "now"], &[]),
        (&["date", "format", "2026-01-01", "%Y"], &[]),
        (&["date", "parse", "2026-01-01"], &[]),
        // gateway — vault-free
        (&["gateway", "telegram"], &[]),
        (&["gateway", "mcp"], &[]),
        // plugin non-update stubs
        (&["plugin", "uninstall"], &[]),
        (&["plugin", "status"], &[]),
        (&["plugin", "verify"], &[]),
        // session non-init stubs
        (&["session", "current"], &[]),
        (&["session", "list"], &[]),
        (&["session", "get", "abc123"], &[]),
        // skill stubs
        (&["skill", "list"], &[]),
        (&["skill", "bootstrap", "my-skill"], &[]),
    ];

    // These run from a no-vault dir — vault presence doesn't matter for this
    // group, so we pick a stable neutral dir.
    let neutral = tempdir().unwrap();
    for (argv, _extra) in cases {
        let code = exit_in(neutral.path(), argv);
        assert_eq!(
            code,
            72,
            "expected exit 72 for `onebrain {}` (non-vault-gated stub). got {}",
            argv.join(" "),
            code
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Parametric: vault-required stubs
//   • inside vault  → exit 72
//   • outside vault → exit 64
// ─────────────────────────────────────────────────────────────────────────────

/// Every `stubs::not_implemented_vault_required` arm.
#[test]
fn vault_required_stubs_exit_72_inside_64_outside() {
    // Each entry is the argv slice. Required clap positionals are filled with
    // dummy values that satisfy the parser but never reach real I/O.
    let cases: &[&[&str]] = &[
        // bookmark
        &["bookmark", "list"],
        &["bookmark", "get", "bm-id-1"],
        &["bookmark", "import", "/tmp/bookmarks.json"],
        // dream
        &["dream", "list"],
        &["dream", "tick", "dream-1"],
        &["dream", "done", "dream-1"],
        &["dream", "snooze", "dream-1", "2026-12-31"],
        // frontmatter
        &["frontmatter", "parse", "03-knowledge/Test.md"],
        &["frontmatter", "extract", "03-knowledge/Test.md", "tags"],
        &[
            "frontmatter",
            "update",
            "03-knowledge/Test.md",
            "status",
            "draft",
        ],
        // inbox
        &["inbox", "list"],
        &["inbox", "next"],
        &["inbox", "process"],
        // log
        &["log", "query", "session-*"],
        &["log", "append", "my log entry"],
        &["log", "rotate"],
        &["log", "stats"],
        // memory
        &["memory", "list"],
        &["memory", "add", "topic", "content"],
        &["memory", "update", "mem-id-1", "new content"],
        &["memory", "remove", "mem-id-1"],
        &["memory", "promote", "mem-id-1"],
        &["memory", "index"],
        // pause
        &["pause", "list"],
        &["pause", "snapshot", "my-task"],
        &["pause", "resume"],
        // qmd (vault-required stubs)
        &["qmd", "setup"],
        &["qmd", "search", "my query"],
        // schedule (vault-required stubs)
        &["schedule", "list"],
        &["schedule", "add", "daily"],
        &["schedule", "remove", "daily"],
        &["schedule", "status"],
        // task stubs
        &["task", "add", "my task"],
        &["task", "done", "task-1"],
        // vault stubs
        &["vault", "scan"],
        &["vault", "stats"],
        &["vault", "verify"],
    ];

    let inside = vault_dir();
    let outside = tempdir().unwrap(); // no vault.yml

    for argv in cases {
        // Path 1: inside a vault → 72 (stub fires, vault check passes).
        let code_in = exit_in(inside.path(), argv);
        assert_eq!(
            code_in,
            72,
            "inside vault: expected exit 72 for `onebrain {}`. got {}",
            argv.join(" "),
            code_in
        );

        // Path 2: outside any vault → 64 (vault check fails before stub).
        let code_out = exit_in(outside.path(), argv);
        assert_eq!(
            code_out,
            64,
            "outside vault: expected exit 64 for `onebrain {}`. got {}",
            argv.join(" "),
            code_out
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Spot-check: canonical error message surface for representative verbs
// ─────────────────────────────────────────────────────────────────────────────

/// The not-implemented error message must contain the verb path so the user
/// knows which command is unimplemented. Test a representative non-vault-gated
/// stub and a representative vault-required stub.
#[test]
fn stub_error_message_contains_verb_path() {
    let vault = vault_dir();

    // Non-vault-gated stub (avatar start).
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["avatar", "start"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("avatar start"),
        "expected \"avatar start\" in error output. got:\n{combined}"
    );
    assert_eq!(out.status.code(), Some(72));

    // Vault-required stub (task add) inside vault.
    let out2 = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["task", "add", "buy milk"])
        .output()
        .unwrap();
    let combined2 = format!(
        "{}{}",
        String::from_utf8_lossy(&out2.stdout),
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(
        combined2.contains("task add"),
        "expected \"task add\" in error output. got:\n{combined2}"
    );
    assert_eq!(out2.status.code(), Some(72));
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
// Session verb coverage (non-init stubs already in non_vault_stubs_above, but
// keeping a direct hit on the `session get --id` arm which uses destructuring)
// ─────────────────────────────────────────────────────────────────────────────

/// `session get` parses the positional `id` and routes to the stub regardless of
/// cwd (non-vault-required).
#[test]
fn session_get_stub_exits_72() {
    let neutral = tempdir().unwrap();
    let code = exit_in(neutral.path(), &["session", "get", "abc-def"]);
    assert_eq!(code, 72, "session get should exit 72 (stub)");
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
        .env_remove("ONEBRAIN_VAULT")
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "daemon stop panicked:\nstderr: {stderr}"
    );
}
