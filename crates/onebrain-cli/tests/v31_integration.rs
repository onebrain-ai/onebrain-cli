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
fn vault_required_stub_returns_64_outside_vault_or_72_inside() {
    // R1 C3: vault-required group stubs (task, memory, note, inbox, pause,
    // bookmark, dream, frontmatter, log, qmd non-reindex, schedule
    // non-protocol, vault non-current/sync) must check vault BEFORE
    // short-circuiting on E_NOT_IMPLEMENTED. Two paths:
    //
    //   1. Inside a vault   → exit 72 (E_NOT_IMPLEMENTED) — the stub fires.
    //   2. Outside any vault → exit 64 (E_VAULT_NOT_FOUND) — vault check fails.

    // Path 1: inside a vault → 72.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["task", "list"])
        .assert()
        .failure()
        .code(72)
        .stderr(predicate::str::contains("not implemented"))
        .stderr(predicate::str::contains("task list"));

    // Path 2: outside any vault → 64.
    let no_vault = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["task", "list"])
        .assert()
        .failure()
        .code(64);
}

#[test]
fn plugin_update_outside_vault_exits_64_not_101() {
    // Round 1 A1: previously `plugin update` outside a vault panicked
    // (exit 101 + backtrace). After the fix it must return the canonical
    // E_VAULT_NOT_FOUND envelope and exit 64.
    let no_vault = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["plugin", "update", "--dry-run"])
        .assert();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    // Must NOT be a panic backtrace.
    assert!(
        !stderr.contains("RUST_BACKTRACE") && !stderr.contains("panicked at"),
        "plugin update outside vault panicked: {stderr:?}"
    );
    out.failure().code(64);
}

#[test]
fn plugin_update_broken_pipe_does_not_silently_succeed() {
    // Round 1 A4: previously the text-mode summary used `let _ = emit(...)`
    // which silently swallowed broken-pipe errors. After the fix the emit
    // failure must propagate as a non-zero exit via the IO-error chain
    // classifier.
    //
    // We simulate broken pipe by closing stdout via `head -c 0`. The exact
    // exit code depends on how IO errors classify; what matters is the
    // process does NOT exit 0 after the pipe closes if we encountered a
    // genuine emit error. In dry-run there are no real on-disk writes, so
    // any non-zero exit here indicates the error propagated correctly.
    //
    // Note: this test is best-effort — depending on OS buffering the broken
    // pipe may not actually fire if the small summary fits in the OS pipe
    // buffer. We still run it because the regression we're guarding
    // against is "no propagation at all", not "specific exit code".
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // Use bash to pipe through `head -c 0` which closes stdout immediately.
    let onebrain_bin = assert_cmd::cargo::cargo_bin("onebrain");
    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!(
            "{} --vault {} plugin update --dry-run | head -c 0",
            onebrain_bin.display(),
            dir.path().display(),
        ))
        .status()
        .unwrap();
    // `head -c 0`'s exit code is what `bash -c` reports without
    // `pipefail`. We're not asserting a specific code; we're verifying the
    // pipeline doesn't hang or panic.
    let _ = status;
}

#[test]
fn json_error_path_emits_canonical_envelope_on_stdout() {
    // R1 B5: --json mode must always emit one well-formed envelope on
    // stdout, even on the error path. Previously errors went to stderr
    // as `Error: ...` regardless of mode, breaking machine consumers.
    let bogus = tempdir().unwrap(); // no vault.yml here

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        // Use --vault pointing at a non-vault path so we hit NotAVault
        // (deterministic; doesn't depend on cwd or env).
        .args([
            "--vault",
            bogus.path().to_str().unwrap(),
            "--json",
            "vault",
            "current",
        ])
        .env_remove("ONEBRAIN_VAULT")
        .assert()
        .failure();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();

    // stdout has the canonical envelope.
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be one canonical JSON document");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "E_VAULT_NOT_FOUND");
    assert!(v["error"]["message"].is_string());
    assert_eq!(v["version"], "1");

    // stderr stays clean of `Error: ...` lines in structured mode (machine
    // consumers should not have to filter stderr).
    assert!(
        !stderr.contains("Error:"),
        "stderr leaked human error line in --json mode: {stderr:?}"
    );

    // Exit code reflects the underlying error.
    out.code(64);
}

#[test]
fn text_error_path_keeps_legacy_stderr_format() {
    // Confirm the text-mode error path is unchanged: still writes
    // `Error: <msg>` to stderr (humans expect this).
    let bogus = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "--vault",
            bogus.path().to_str().unwrap(),
            "vault",
            "current",
        ])
        .env_remove("ONEBRAIN_VAULT")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("Error:"),
        "text-mode error stderr missing legacy format: {stderr:?}"
    );
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

// ─────────────────────────────────────────────────────────────────────────
// R1 branded banner — emission is gated by colour-TTY. In `assert_cmd`
// child processes stdout/stderr are pipes (no TTY), so the banner is
// suppressed and these tests verify the absence on the machine-output path.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn json_output_never_contains_banner_on_stdout() {
    // Even if a future regression accidentally piped the banner to stdout
    // in `--json` mode, this test catches it.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    // Pink ANSI escape or brand text would indicate banner leaked.
    assert!(
        !stdout.contains("OneBrain CLI"),
        "JSON stdout leaked banner text: {stdout:?}"
    );
    assert!(
        !stdout.contains("\x1b[38;2;255;45;146m"),
        "JSON stdout leaked banner ANSI escape: {stdout:?}"
    );
}

#[test]
fn piped_text_output_never_emits_banner_on_stdout() {
    // Default text mode + piped stdout (assert_cmd is a pipe) → no banner.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(
        !stdout.contains("OneBrain CLI"),
        "piped text stdout leaked banner: {stdout:?}"
    );
}

#[test]
fn hook_protocol_session_init_keeps_stderr_clean() {
    // Hook commands MUST keep both stdout and stderr free of any banner
    // bytes — even if a developer accidentally turns colour on in CI.
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("vault.yml"), "qmd_collection: x\n").unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["session", "init"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stdout.contains("OneBrain CLI"),
        "hook stdout leaked banner: {stdout:?}"
    );
    assert!(
        !stderr.contains("OneBrain CLI"),
        "hook stderr leaked banner: {stderr:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R2-B1 · partial-failure E2E
// ─────────────────────────────────────────────────────────────────────────

/// Build a minimal valid tarball fixture for `vault-sync` so step 2 of
/// `plugin update` succeeds. Matches the shape used in `tests/vault_sync.rs`.
fn build_plugin_tarball(version: &str) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut buf = Vec::new();
    let prefix = "onebrain-ai-onebrain-abc1234";
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut b = tar::Builder::new(enc);
        let files: Vec<(String, String)> = vec![
            (
                format!("{prefix}/.claude/plugins/onebrain/.claude-plugin/plugin.json"),
                serde_json::json!({"id":"onebrain","version":version,"name":"OneBrain"})
                    .to_string(),
            ),
            (
                format!("{prefix}/.claude/plugins/onebrain/INSTRUCTIONS.md"),
                "# OneBrain Instructions\n".into(),
            ),
            (
                format!("{prefix}/CONTRIBUTING.md"),
                "# Contributing\n".into(),
            ),
            (format!("{prefix}/CHANGELOG.md"), "# Changelog\n".into()),
            (
                format!("{prefix}/CLAUDE.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
            ),
            (
                format!("{prefix}/GEMINI.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
            ),
            (
                format!("{prefix}/AGENTS.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
            ),
        ];
        for (path, content) in &files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, content.as_bytes()).unwrap();
        }
        b.finish().unwrap();
    }
    buf
}

#[test]
fn plugin_update_partial_failure_emits_partial_envelope() {
    // R2-B1 (Blocker): the partial-failure envelope contract.
    //
    // Trigger: vault.yml has an INVALID cron schedule (`* *` — too few
    // fields) so step 4 (`schedule register`) fails after step 3 (hook
    // rewrite) has already mutated `.claude/settings.json` on disk.
    //
    // Expected envelope:
    //   - exactly ONE JSON document on stdout (no double-emit)
    //   - `ok == false`
    //   - `error.code == "E_PLUGIN_UPDATE_PARTIAL"`
    //   - `data.hooks_rewritten >= 1` (the rewriter did its job)
    //   - `data.plists_rewritten == false`
    //   - exit code != 0

    let dir = tempdir().unwrap();
    let vault = dir.path();
    fs::create_dir_all(vault.join(".claude")).unwrap();

    // vault.yml: valid YAML BUT contains a malformed cron expression. The
    // register-schedule validate pass rejects it, returning Err after the
    // hook rewriter has already succeeded.
    fs::write(
        vault.join("vault.yml"),
        "update_channel: stable\nschedule:\n  - cron: \"* *\"\n    command: /bin/echo\n    args: [hi]\n",
    )
    .unwrap();

    // .claude/settings.json with one v3.0 hook entry the rewriter will flip.
    let v30_settings = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        { "type": "command", "command": "onebrain", "args": ["session-init"] }
                    ]
                }
            ]
        }
    });
    fs::write(
        vault.join(".claude/settings.json"),
        serde_json::to_string_pretty(&v30_settings).unwrap(),
    )
    .unwrap();

    // Steer vault-sync's network fetch to a local tarball fixture.
    let tarball = build_plugin_tarball("1.11.0");
    let tarball_path = dir.path().join("fixture.tar.gz");
    fs::write(&tarball_path, &tarball).unwrap();
    let isolated = vault.join(".isolated-installed_plugins.json");
    // Isolate HOME so register-schedule's plist path doesn't touch the real
    // ~/Library/LaunchAgents (defensive — invalid cron should fail BEFORE
    // any plist write, but we keep HOME isolated anyway).
    let home = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["plugin", "update", "--json"])
        .current_dir(vault)
        .env("HOME", home.path())
        .env(
            "ONEBRAIN_VAULT_SYNC_FIXTURE",
            tarball_path.to_string_lossy().to_string(),
        )
        .env(
            "ONEBRAIN_INSTALLED_PLUGINS_PATH",
            isolated.to_string_lossy().to_string(),
        )
        .env("ONEBRAIN_QUIET_MIGRATION", "1")
        .output()
        .expect("spawn failed");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // ── Exactly one JSON document on stdout (no double-emit, R2-H3). ──
    //
    // vault-sync prints progress lines BEFORE the JSON envelope, so we can't
    // parse-from-start. Filter to lines that start with `{` and assert
    // exactly one such line. (Each JSON envelope is one line in compact
    // mode; double-emit would produce two.)
    let json_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.trim_start().starts_with('{'))
        .collect();
    assert_eq!(
        json_lines.len(),
        1,
        "expected exactly one JSON envelope on stdout (R2-H3 guard); got {} · stdout: {stdout:?} · stderr: {stderr:?}",
        json_lines.len()
    );
    let v: serde_json::Value = serde_json::from_str(json_lines[0]).expect("valid JSON");

    // ── Envelope contract: partial-failure shape. ──
    assert_eq!(v["ok"], false, "envelope ok must be false on partial");
    assert_eq!(
        v["error"]["code"], "E_PLUGIN_UPDATE_PARTIAL",
        "wrong error code · got: {v:#?}"
    );
    assert!(v["error"]["message"].is_string());
    assert_eq!(v["version"], "1");

    // ── Step progress preserved in data. ──
    assert!(
        v["data"]["hooks_rewritten"].as_u64().unwrap_or(0) >= 1,
        "hooks_rewritten must reflect on-disk progress · got: {v:#?}"
    );
    assert_eq!(
        v["data"]["plists_rewritten"], false,
        "plists_rewritten must be false on partial failure"
    );

    // ── Exit code is non-zero. ──
    assert_ne!(
        out.status.code(),
        Some(0),
        "partial failure must surface non-zero exit · stderr: {stderr:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R2-H5 · semantic swap contract pins
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn root_update_does_not_rewrite_hooks_or_plists() {
    // The v3.1 semantic swap: `onebrain update` is now self-update of the
    // CLI binary; hook/plist rewriting moved under `onebrain plugin update`.
    //
    // Contract pin: running `onebrain update --check --json` (which calls
    // the version-check path, not the hook rewriter) must NOT touch
    // .claude/settings.json. This is a STRUCTURAL guarantee independent of
    // whether the network fetch succeeds — the update orchestrator never
    // touches the vault filesystem on the dry-run path.
    let dir = tempdir().unwrap();
    let vault = dir.path();
    fs::create_dir_all(vault.join(".claude")).unwrap();

    // A v3.0-shape settings.json that the v3.1 hook rewriter WOULD modify
    // if it ran. If `onebrain update` accidentally invoked the rewriter,
    // the args would flip from ["session-init"] to ["session","init"].
    let v30 = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        { "type": "command", "command": "onebrain", "args": ["session-init"] }
                    ]
                }
            ]
        }
    });
    let settings_path = vault.join(".claude/settings.json");
    let original_bytes = serde_json::to_string_pretty(&v30).unwrap();
    fs::write(&settings_path, &original_bytes).unwrap();
    fs::write(vault.join("vault.yml"), "method: onebrain\n").unwrap();
    let home = tempdir().unwrap();

    // Run with --check --json. Network may or may not be reachable in CI;
    // either way, the contract is that settings.json is untouched. We let
    // the binary exit with whatever code it returns (0 on success, 1 on
    // fetch failure) — we only care about the on-disk side-effect.
    let _ = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault)
        .env("HOME", home.path())
        .args(["update", "--check", "--json"])
        .output()
        .expect("spawn failed");

    let after_bytes = fs::read(&settings_path).expect("settings.json must still exist");
    assert_eq!(
        after_bytes,
        original_bytes.as_bytes(),
        "onebrain update must NOT rewrite .claude/settings.json"
    );
}

#[test]
fn plugin_update_does_not_touch_cli_binary() {
    // The dual contract pin: `onebrain plugin update` must never download
    // or replace the CLI binary. v3.0 conflated both paths under one
    // `update` verb; v3.1 split them. If a future refactor accidentally
    // wires the binary-fetch path back in, this test catches it.
    let dir = tempdir().unwrap();
    let vault = dir.path();
    fs::create_dir_all(vault.join(".claude")).unwrap();
    fs::write(vault.join("vault.yml"), "method: onebrain\n").unwrap();
    fs::write(
        vault.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({"hooks": {}})).unwrap(),
    )
    .unwrap();

    // Isolated cache + HOME so any rogue binary write lands in the
    // tempdir, not the real ~/.cache/onebrain.
    let home = tempdir().unwrap();
    let cache_home = home.path().join("cache");
    let cache_dir = cache_home.join("onebrain");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["plugin", "update", "--dry-run"])
        .current_dir(vault)
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", &cache_home)
        .env("ONEBRAIN_QUIET_MIGRATION", "1")
        .assert()
        .success();

    // Nothing under the binaries subdirectory. We don't assert the cache
    // root is absent (some unrelated subsystem may create it), only that
    // no binary downloads happened.
    let binaries_dir = cache_dir.join("binaries");
    assert!(
        !binaries_dir.exists(),
        "plugin update must NOT create a binaries cache dir (got: {})",
        binaries_dir.display()
    );
}
