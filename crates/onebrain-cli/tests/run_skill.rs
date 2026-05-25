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
