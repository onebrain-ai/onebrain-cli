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
        // schedule (vault-required stubs) — `list` is wired to the real
        // status path (see `schedule_list_...` tests below), so it is no
        // longer a stub.
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
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
