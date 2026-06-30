//! Layer 2 integration tests for `onebrain run-skill` — spawns the real
//! CLI binary against a mock `claude` shell script, asserts argv + exit
//! codes match Bun behavior. No real `claude` binary required.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// Build a minimal vault directory with just `onebrain.yml`.
fn write_minimal_vault(dir: &Path) {
    fs::write(dir.join("onebrain.yml"), "folders:\n  inbox: 00-inbox\n").unwrap();
}

/// Write a mock `claude` shell script that logs its argv (one per line) to
/// `$ARGV_LOG` and exits with `$MOCK_EXIT` (defaults to 0). Returns the
/// script path. The script is `chmod +x` so we can point `CLAUDE_BIN` at it.
fn write_mock_claude(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("claude-mock.sh");
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

// Absolute shebang (not `/usr/bin/env bash`) so the script survives tests
// that clear `PATH` to provoke spawn failures.
const ARGV_LOG_SCRIPT: &str = r#"#!/bin/bash
: > "$ARGV_LOG"
for a in "$@"; do
  printf '%s\n' "$a" >> "$ARGV_LOG"
done
echo "$PWD" >> "$ARGV_LOG.cwd"
exit "${MOCK_EXIT:-0}"
"#;

#[test]
fn missing_config_exits_78() {
    let d = tempdir().unwrap();
    let bogus = d.path().join("does-not-exist");
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            bogus.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", "/bin/true")
        .assert()
        .failure()
        .code(78)
        .stderr(predicate::str::contains("Vault not found"));
}

#[cfg(unix)]
#[test]
fn happy_path_passes_canonical_argv_and_cwd() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);

    let scripts = d.path().join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    let mock = write_mock_claude(&scripts, ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0);

    let logged = fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    assert_eq!(
        lines,
        vec![
            "-p",
            "/onebrain:daily",
            "--add-dir",
            vault.to_str().unwrap()
        ]
    );

    // Confirm the child was spawned with the vault as cwd · matches Bun's
    // `cwd: vault` spawn option (so launchd-relative paths resolve correctly).
    let cwd_log = fs::read_to_string(argv_log.with_extension("log.cwd")).unwrap();
    let logged_cwd = PathBuf::from(cwd_log.trim());
    // On macOS, `/var/folders/...` is a symlink to `/private/var/folders/...`.
    // `canonicalize` resolves both sides to the same path so the comparison holds.
    assert_eq!(
        logged_cwd.canonicalize().unwrap(),
        vault.canonicalize().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn args_are_appended_as_key_value_tokens() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/distill",
            "--arg",
            "topic=this-week",
            "--arg",
            "depth=2",
        ])
        .env("CLAUDE_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0);

    let logged = fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    // Insertion order must be preserved.
    assert_eq!(lines[0], "-p");
    assert_eq!(lines[1], "/onebrain:distill topic=this-week depth=2");
    assert_eq!(lines[2], "--add-dir");
}

#[cfg(unix)]
#[test]
fn child_exit_code_is_propagated() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &mock)
        .env("MOCK_EXIT", "42")
        .env("ARGV_LOG", &argv_log)
        .assert()
        .failure()
        .code(42);
}

#[cfg(unix)]
#[test]
fn sigterm_maps_to_143() {
    // SIGTERM = signal 15 on POSIX · POSIX convention is 128 + 15 = 143.
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), "#!/bin/bash\nkill -TERM $$\nsleep 5\nexit 0\n");
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &mock)
        .assert()
        .failure()
        .code(143);
}

#[cfg(unix)]
#[test]
fn spawn_failure_maps_to_127() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);

    // Point CLAUDE_BIN at a file that exists but is NOT executable. The
    // resolver accepts it (path exists), but Command::status returns an
    // ENOEXEC-style error, exercising the spawn-error -> exit 127 path.
    // Using a path on disk this way avoids any reliance on the fallback list
    // (which may contain a real claude on a dev machine) or on PATH (which
    // affects the test runner itself).
    let non_executable = d.path().join("not-executable");
    fs::write(&non_executable, "").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &non_executable)
        .assert()
        .failure()
        .code(127)
        .stderr(predicate::str::contains("Failed to spawn claude"));
}

#[test]
fn empty_skill_returns_error() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/",
        ])
        .env("CLAUDE_BIN", "/bin/true")
        .assert()
        .failure()
        .stderr(predicate::str::contains("skill name must not be empty"));
}

#[test]
fn malformed_arg_returns_error() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
            "--arg",
            "no-equals-here",
        ])
        .env("CLAUDE_BIN", "/bin/true")
        .assert()
        .failure()
        .stderr(predicate::str::contains("key=value"));
}

#[cfg(unix)]
#[test]
fn argv_snapshot_canonical_invocation() {
    // Insta snapshot of the exact argv passed to claude · gives a reviewable
    // diff if anyone changes the spawn shape (arg order, flag names, prompt
    // construction). Vault path is stripped to keep the snapshot deterministic.
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/distill",
            "--arg",
            "topic=this-week",
            "--arg",
            "depth=2",
        ])
        .env("CLAUDE_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success();

    let logged = fs::read_to_string(&argv_log).unwrap();
    // Replace the absolute vault path with a stable placeholder.
    let vault_str = vault.to_str().unwrap();
    let normalized = logged.replace(vault_str, "<VAULT>");
    insta::assert_snapshot!("run_skill_argv_canonical", normalized);
}

#[cfg(unix)]
#[test]
fn claude_bin_env_missing_emits_warning() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    // Strategy: point CLAUDE_BIN at a missing path · the resolver should warn
    // and fall through. To still get a successful spawn (so the test exercises
    // the warning path without depending on the fallback list), we put the
    // mock script on PATH as `claude` and clear CLAUDE_BIN to the bad value.
    let path_dir = d.path().join("bin");
    fs::create_dir_all(&path_dir).unwrap();
    let claude_on_path = path_dir.join("claude");
    fs::copy(&mock, &claude_on_path).unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", "/definitely/not/a/real/binary/xyz")
        .env("PATH", &path_dir)
        // Clear HOME so the resolver doesn't probe `~/.local/bin/claude`.
        .env_remove("HOME")
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains(
            "CLAUDE_BIN points to a missing file",
        ));
}

// ── Gemini harness ─────────────────────────────────────────────────────────
//
// These tests use `skill run --harness gemini` (the modern subcommand).
// The legacy `run-skill` alias always forces Claude; `--harness` / `--model` /
// `--json` flags only exist on `skill run`.

/// Minimal mock script for gemini — same argv-log contract as ARGV_LOG_SCRIPT.
const GEMINI_ARGV_LOG_SCRIPT: &str = r#"#!/bin/bash
: > "$ARGV_LOG"
for a in "$@"; do
  printf '%s\n' "$a" >> "$ARGV_LOG"
done
exit "${MOCK_EXIT:-0}"
"#;

#[cfg(unix)]
#[test]
fn gemini_harness_passes_include_directories_and_yolo() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), GEMINI_ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        // `skill run` accepts `--harness`; the legacy `run-skill` does not.
        .args([
            "skill",
            "run",
            "--vault-dir",
            vault.to_str().unwrap(),
            "/daily",
            "--harness",
            "gemini",
        ])
        .env("GEMINI_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0);

    let logged = fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    let joined = lines.join(" ");
    // Scan by flag rather than fixed argv slot, so prepending any flag before
    // `-p` in a future change can't silently break these assertions.
    let p_idx = lines
        .iter()
        .position(|a| *a == "-p")
        .unwrap_or_else(|| panic!("no -p flag in argv: {joined}"));
    assert!(
        lines
            .get(p_idx + 1)
            .is_some_and(|p| p.starts_with("/onebrain:daily")),
        "expected /onebrain:daily prompt after -p: {joined}"
    );
    // Gemini uses `--include-directories <vault>`, NOT `--add-dir`.
    let inc_idx = lines
        .iter()
        .position(|a| *a == "--include-directories")
        .unwrap_or_else(|| panic!("no --include-directories in argv: {joined}"));
    assert_eq!(
        lines.get(inc_idx + 1).copied(),
        Some(vault.to_str().unwrap()),
        "vault must follow --include-directories: {joined}"
    );
    assert!(
        !lines.contains(&"--add-dir"),
        "gemini harness must not use --add-dir: {joined}"
    );
    // `--approval-mode yolo` must be present (stdin is null for headless runs).
    assert!(
        joined.contains("--approval-mode yolo"),
        "expected --approval-mode yolo in argv: {joined}"
    );
}

#[cfg(unix)]
#[test]
fn gemini_harness_propagates_non_zero_exit() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), GEMINI_ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "skill",
            "run",
            "--vault-dir",
            vault.to_str().unwrap(),
            "/daily",
            "--harness",
            "gemini",
        ])
        .env("GEMINI_BIN", &mock)
        .env("MOCK_EXIT", "55")
        .env("ARGV_LOG", &argv_log)
        .assert()
        .failure()
        .code(55);
}

#[cfg(unix)]
#[test]
fn gemini_harness_spawn_failure_maps_to_127() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);

    let non_executable = d.path().join("not-executable-gemini");
    fs::write(&non_executable, "").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "skill",
            "run",
            "--vault-dir",
            vault.to_str().unwrap(),
            "/daily",
            "--harness",
            "gemini",
        ])
        .env("GEMINI_BIN", &non_executable)
        .assert()
        .failure()
        .code(127)
        .stderr(predicate::str::contains("Failed to spawn gemini"));
}

#[cfg(unix)]
#[test]
fn gemini_harness_appends_model_flag() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), GEMINI_ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "skill",
            "run",
            "--vault-dir",
            vault.to_str().unwrap(),
            "/daily",
            "--harness",
            "gemini",
            "--model",
            "gemini-2.5-flash",
        ])
        .env("GEMINI_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0);

    let logged = fs::read_to_string(&argv_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    // `-m gemini-2.5-flash` must appear (Gemini uses short `-m`, not `--model`).
    let joined = lines.join(" ");
    assert!(
        joined.contains("-m gemini-2.5-flash"),
        "expected -m gemini-2.5-flash in argv: {joined}"
    );
}

// ── Non-interactive spawn path ──────────────────────────────────────────────
//
// `cargo test` pipes stderr to the test harness, so `std::io::stderr().is_terminal()`
// is false. Every `spawn_harness` call in these tests therefore exercises the
// non-interactive branch (lines 179-186), not the spinner branch. The spinner /
// indicatif path (lines 188-295) genuinely requires an interactive tty, which
// cannot be provided in `cargo test` without a pty crate; it is listed in the
// residual section of the coverage report.

#[cfg(unix)]
#[test]
fn non_interactive_path_propagates_child_stdout() {
    // Verify that in non-interactive mode the child's stdout reaches our
    // stdout without truncation. The mock writes a fixed string to stdout;
    // assert_cmd captures it.
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), "#!/bin/bash\nprintf 'hello from child'\nexit 0\n");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &mock)
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from child"));
}

#[cfg(unix)]
#[test]
fn non_interactive_path_child_stderr_reaches_stderr() {
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(
        d.path(),
        "#!/bin/bash\nprintf 'warning from child' >&2\nexit 0\n",
    );

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &mock)
        .assert()
        .success()
        .stderr(predicate::str::contains("warning from child"));
}

#[cfg(unix)]
#[test]
fn headless_env_var_is_set_on_child() {
    // The child must see ONEBRAIN_HEADLESS=1 so session-init skips the
    // interactive startup ceremony. We verify this by having the mock echo
    // the variable to stdout.
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(
        d.path(),
        "#!/bin/bash\nprintf '%s' \"$ONEBRAIN_HEADLESS\"\nexit 0\n",
    );

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "run-skill",
            "--vault",
            vault.to_str().unwrap(),
            "--skill",
            "/daily",
        ])
        .env("CLAUDE_BIN", &mock)
        .assert()
        .success()
        .stdout(predicate::str::contains("1"));
}

#[cfg(unix)]
#[test]
fn skill_run_passes_json_flag_to_claude() {
    // `skill run --json` maps to `--output-format json` in the claude argv.
    // Must use `skill run` (not `run-skill`): the legacy alias has no --json flag.
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "skill",
            "run",
            "--vault-dir",
            vault.to_str().unwrap(),
            "/daily",
            "--json",
        ])
        .env("CLAUDE_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0);

    let logged = fs::read_to_string(&argv_log).unwrap();
    let joined = logged.lines().collect::<Vec<_>>().join(" ");
    assert!(
        joined.contains("--output-format json"),
        "expected --output-format json in argv: {joined}"
    );
}

#[cfg(unix)]
#[test]
fn skill_run_passes_model_to_claude() {
    // `skill run --model <m>` injects `--model <m>` into the claude argv.
    // Must use `skill run`: the legacy `run-skill` alias has no `--model` flag.
    let d = tempdir().unwrap();
    let vault = d.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    write_minimal_vault(&vault);
    let mock = write_mock_claude(d.path(), ARGV_LOG_SCRIPT);
    let argv_log = d.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "skill",
            "run",
            "--vault-dir",
            vault.to_str().unwrap(),
            "/daily",
            "--model",
            "claude-haiku-4-5",
        ])
        .env("CLAUDE_BIN", &mock)
        .env("ARGV_LOG", &argv_log)
        .assert()
        .success()
        .code(0);

    let logged = fs::read_to_string(&argv_log).unwrap();
    let joined = logged.lines().collect::<Vec<_>>().join(" ");
    assert!(
        joined.contains("--model claude-haiku-4-5"),
        "expected --model claude-haiku-4-5 in argv: {joined}"
    );
}
