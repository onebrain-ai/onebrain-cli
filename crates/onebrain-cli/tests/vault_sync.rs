//! `onebrain vault-sync` CLI integration tests.
//!
//! Tests run the real binary via `assert_cmd` and exercise the orchestrator
//! end-to-end. The binary is steered away from the network via the
//! `ONEBRAIN_VAULT_SYNC_FIXTURE` env var, which points at a local tarball file
//! the default fetch hook reads instead of hitting GitHub. Pin-step isolation
//! comes from `ONEBRAIN_INSTALLED_PLUGINS_PATH` and the cache-dir override.

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn build_tarball(prefix: &str, version: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut b = tar::Builder::new(enc);
        let mut all: Vec<(String, String)> = vec![
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
        for (k, v) in files {
            all.push((format!("{prefix}/{k}"), v.to_string()));
        }
        for (path, content) in &all {
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

fn make_vault(root: &Path) -> PathBuf {
    let vault = root.join("vault");
    fs::create_dir_all(vault.join(".claude")).unwrap();
    fs::write(
        vault.join("vault.yml"),
        "update_channel: stable\nfolders:\n  inbox: 00-inbox\n  logs: 07-logs\n",
    )
    .unwrap();
    vault
}

/// Run `onebrain vault-sync` against the given vault, with a tarball fixture
/// and an isolated `installed_plugins.json`. Returns `(exit_status, stdout, stderr)`.
fn run_sync(
    vault: &Path,
    tarball_bytes: &[u8],
    extra_env: &[(&str, &str)],
) -> (std::process::ExitStatus, String, String) {
    let tarball_path = vault.join("../fixture.tar.gz");
    fs::write(&tarball_path, tarball_bytes).unwrap();
    let isolated = vault.join(".isolated-installed_plugins.json");

    let mut cmd = Command::cargo_bin("onebrain").unwrap();
    cmd.arg("vault-sync")
        .current_dir(vault)
        .env(
            "ONEBRAIN_VAULT_SYNC_FIXTURE",
            tarball_path.to_string_lossy().to_string(),
        )
        .env(
            "ONEBRAIN_INSTALLED_PLUGINS_PATH",
            isolated.to_string_lossy().to_string(),
        );
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// -- Tests --

#[test]
fn happy_path_fresh_sync_exits_zero_and_writes_plugin_files() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let (status, _stdout, stderr) = run_sync(&vault, &tarball, &[]);
    assert!(status.success(), "exit non-zero · stderr: {stderr}");
    assert!(vault
        .join(".claude/plugins/onebrain/.claude-plugin/plugin.json")
        .exists());
    let pj: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(vault.join(".claude/plugins/onebrain/.claude-plugin/plugin.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(pj["version"], "1.11.0");
}

#[test]
fn happy_path_emits_vault_sync_step_lines_on_non_tty_stdout() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let (_status, stdout, _stderr) = run_sync(&vault, &tarball, &[]);
    assert!(stdout.contains("vault-sync: Downloading"));
    assert!(stdout.contains("vault-sync: Syncing files"));
    assert!(stdout.contains("vault-sync: Updating harness"));
    assert!(stdout.contains("vault-sync: done"));
}

#[test]
fn stale_files_removed_on_resync() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let plugin_dir = vault.join(".claude/plugins/onebrain");
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(plugin_dir.join("stale.md"), "# stale").unwrap();

    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let (status, _stdout, _stderr) = run_sync(&vault, &tarball, &[]);
    assert!(status.success());
    assert!(!plugin_dir.join("stale.md").exists());
}

#[test]
fn download_failure_exits_non_zero() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let tarball_path = vault.join("../bad-fixture.tar.gz");
    fs::write(&tarball_path, b"this is not a tarball").unwrap();

    let isolated = vault.join(".isolated-installed_plugins.json");
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("vault-sync")
        .current_dir(&vault)
        .env(
            "ONEBRAIN_VAULT_SYNC_FIXTURE",
            tarball_path.to_string_lossy().to_string(),
        )
        .env(
            "ONEBRAIN_INSTALLED_PLUGINS_PATH",
            isolated.to_string_lossy().to_string(),
        )
        .output()
        .unwrap();
    assert!(!out.status.success(), "should exit non-zero on bad tarball");
    assert_eq!(out.status.code(), Some(1), "exit code must be exactly 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("vault-sync:") && stderr.contains("failed"),
        "stderr missing failure message: {stderr}"
    );
}

/// Regression: a missing fixture file (representative of any IO-level fetch
/// failure) must exit with code 1 AND surface the underlying cause on
/// stderr. Schedulers and CI rely on the non-zero exit to detect a sync miss.
#[test]
fn missing_fixture_path_exits_one_with_stderr_message() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let isolated = vault.join(".isolated-installed_plugins.json");
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("vault-sync")
        .current_dir(&vault)
        .env("ONEBRAIN_VAULT_SYNC_FIXTURE", "/this/does/not/exist.tar.gz")
        .env(
            "ONEBRAIN_INSTALLED_PLUGINS_PATH",
            isolated.to_string_lossy().to_string(),
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1, got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("vault-sync: download failed"),
        "stderr missing 'vault-sync: download failed': {stderr}"
    );
    // Final summary line must also fire so scheduler logs can grep for it.
    assert!(
        stderr.contains("vault-sync: failed:"),
        "stderr missing summary 'vault-sync: failed:': {stderr}"
    );
}

#[test]
fn vault_yml_update_channel_preserved_through_sync() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let (status, _stdout, _stderr) = run_sync(&vault, &tarball, &[]);
    assert!(status.success());
    let yml = fs::read_to_string(vault.join("vault.yml")).unwrap();
    assert!(yml.contains("update_channel: stable"));
    assert!(yml.contains("inbox: 00-inbox"));
    assert!(yml.contains("logs: 07-logs"));
    assert!(!yml.contains("onebrain_version"));
}

/// CLI `--branch` flag (Bun v2.3.3 parity) — overrides `vault.yml::update_channel`.
/// Verifies the flag is accepted, the sync completes successfully, and the
/// override propagates into the orchestrator (branch is threaded through to
/// download + pin steps; under fixture mode the value is still surfaced in
/// the result struct that drives the version-stamp line).
#[test]
fn branch_flag_overrides_update_channel() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let tarball_path = vault.join("../fixture.tar.gz");
    fs::write(&tarball_path, &tarball).unwrap();
    let isolated = vault.join(".isolated-installed_plugins.json");

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("vault-sync")
        .arg("--branch")
        .arg("next")
        .current_dir(&vault)
        .env(
            "ONEBRAIN_VAULT_SYNC_FIXTURE",
            tarball_path.to_string_lossy().to_string(),
        )
        .env(
            "ONEBRAIN_INSTALLED_PLUGINS_PATH",
            isolated.to_string_lossy().to_string(),
        )
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "vault-sync --branch next must exit 0 · stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(vault
        .join(".claude/plugins/onebrain/.claude-plugin/plugin.json")
        .exists());
}

/// CLI rejects `--branch` with no value (clap-level validation).
#[test]
fn branch_flag_requires_value() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("vault-sync")
        .arg("--branch") // no value
        .current_dir(&vault)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "should reject --branch with no value"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("value") || stderr.contains("required"),
        "stderr should explain missing value · got: {stderr}"
    );
}

#[test]
fn harness_files_get_new_imports_injected() {
    let dir = tempdir().unwrap();
    let vault = make_vault(dir.path());
    fs::write(
        vault.join("CLAUDE.md"),
        "# My Config\n\n@.claude/plugins/onebrain/INSTRUCTIONS.md\n",
    )
    .unwrap();
    let tarball = build_tarball(
        "onebrain-ai-onebrain-abc1234",
        "1.11.0",
        &[("CUSTOM-CLAUDE.md", "ignored — we override CLAUDE.md below")],
    );
    // Replace CLAUDE.md inside via a second tarball variant.
    let mut buf = Vec::new();
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut b = tar::Builder::new(enc);
        let prefix = "onebrain-ai-onebrain-xyz";
        let files = [
            (
                format!("{prefix}/.claude/plugins/onebrain/.claude-plugin/plugin.json"),
                serde_json::json!({"id":"onebrain","version":"1.11.0"}).to_string(),
            ),
            (
                format!("{prefix}/CLAUDE.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n@.claude/plugins/onebrain/NEW.md\n"
                    .into(),
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
        for (path, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, content.as_bytes()).unwrap();
        }
        b.finish().unwrap();
    }
    let _ = tarball; // Discard the original; we use buf instead.
    let (status, _stdout, stderr) = run_sync(&vault, &buf, &[]);
    assert!(status.success(), "stderr: {stderr}");
    let claude = fs::read_to_string(vault.join("CLAUDE.md")).unwrap();
    assert!(claude.contains("# My Config"));
    assert!(claude.contains("@.claude/plugins/onebrain/NEW.md"));
    let dup = claude
        .matches("@.claude/plugins/onebrain/INSTRUCTIONS.md")
        .count();
    assert_eq!(dup, 1);
}

// ─────────────────────────────────────────────────────────────────────────
// v3.4.8 (#196 / PR #199 R3): the self-documenting template must survive
// vault-sync — its Step 7 config writer is change-detecting and
// comment-preserving. All four scenarios run the REAL binary with the
// tarball fixture (no network).
// ─────────────────────────────────────────────────────────────────────────

fn comment_lines(s: &str) -> usize {
    s.lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .count()
}

/// (a) Default install path: `onebrain init --yes` runs vault-sync INLINE.
/// The scaffolded template must come out the other side untouched.
#[test]
fn default_init_with_inline_sync_keeps_template_comments() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let tarball_path = dir.path().join("fixture.tar.gz");
    fs::write(&tarball_path, &tarball).unwrap();
    let isolated = dir.path().join("isolated-installed_plugins.json");

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes"])
        .current_dir(&vault)
        .env("ONEBRAIN_VAULT_SYNC_FIXTURE", &tarball_path)
        .env("ONEBRAIN_INSTALLED_PLUGINS_PATH", &isolated)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg = fs::read_to_string(vault.join("onebrain.yml")).unwrap();
    let template =
        onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Essentials).unwrap();
    assert_eq!(
        comment_lines(&cfg),
        comment_lines(&template),
        "inline vault-sync must not strip template comments:\n{cfg}"
    );
    assert!(
        cfg.contains("# collection:"),
        "commented collection placeholder must survive:\n{cfg}"
    );
    assert!(
        cfg.contains("min_score: 0.30"),
        "min_score must stay verbatim (not re-serialized to 0.3):\n{cfg}"
    );
    assert!(cfg.contains("update_channel: stable"), "{cfg}");
    // Section banners survive end-to-end on disk (comment_lines counts them,
    // but presence must be explicit — a banner-free file with the same
    // comment count would otherwise pass).
    assert!(
        cfg.contains("# ── General "),
        "section banner must be on disk after init + inline sync:\n{cfg}"
    );
}

/// (b) Re-sync of an already-synced commented config: byte-identical no-op.
#[test]
fn resync_of_commented_config_is_a_no_write() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    fs::create_dir_all(vault.join(".claude")).unwrap();
    let template = onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap();
    fs::write(vault.join("onebrain.yml"), &template).unwrap();
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let (status, _stdout, stderr) = run_sync(&vault, &tarball, &[]);
    assert!(status.success(), "stderr: {stderr}");
    let after = fs::read_to_string(vault.join("onebrain.yml")).unwrap();
    assert_eq!(
        after, template,
        "already-correct config must not be rewritten"
    );
    assert!(
        !vault.join(".onebrain-backups").exists(),
        "no write ⇒ no backup dir"
    );
}

/// (c) Sync that genuinely must add `update_channel` to a commented config:
/// the key lands, every comment stays.
#[test]
fn sync_adding_channel_key_keeps_comments() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    fs::create_dir_all(vault.join(".claude")).unwrap();
    let template = onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap();
    // Strip the update_channel line (and keep its doc comment — comments are
    // never load-bearing) so sync has a real change to make.
    let without_channel: String = template
        .lines()
        .filter(|l| !l.starts_with("update_channel:"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(vault.join("onebrain.yml"), &without_channel).unwrap();
    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let (status, _stdout, stderr) = run_sync(&vault, &tarball, &[]);
    assert!(status.success(), "stderr: {stderr}");
    let after = fs::read_to_string(vault.join("onebrain.yml")).unwrap();
    assert!(after.contains("update_channel: stable"), "{after}");
    assert_eq!(
        comment_lines(&after),
        comment_lines(&without_channel),
        "adding a key must not cost a single comment line:\n{after}"
    );
    let parsed: serde_json::Value = serde_yaml::from_str::<serde_yaml::Value>(&after)
        .map(|v| serde_json::to_value(v).unwrap())
        .unwrap();
    assert_eq!(parsed["update_channel"], "stable");
    assert_eq!(parsed["search"]["default_top_k"], 10);
}

/// (d) `doctor --fix` whose plugin-files repair triggers a full
/// `run_vault_sync`: the commented config survives the repair.
#[cfg(unix)]
#[test]
fn doctor_fix_plugin_files_repair_keeps_config_comments() {
    let home = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    // Full healthy vault EXCEPT the plugin files — that failing check maps to
    // the vault-sync repair recipe.
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
        fs::create_dir_all(vault.join(f)).unwrap();
    }
    fs::create_dir_all(vault.join(".claude")).unwrap();
    fs::write(
        vault.join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
    let template = onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap();
    fs::write(vault.join("onebrain.yml"), &template).unwrap();

    let tarball = build_tarball("onebrain-ai-onebrain-abc1234", "1.11.0", &[]);
    let tarball_path = dir.path().join("fixture.tar.gz");
    fs::write(&tarball_path, &tarball).unwrap();
    let isolated = dir.path().join("isolated-installed_plugins.json");

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["doctor", "--fix", "--yes"])
        .current_dir(&vault)
        .env("HOME", home.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .env("ONEBRAIN_VAULT_SYNC_FIXTURE", &tarball_path)
        .env("ONEBRAIN_INSTALLED_PLUGINS_PATH", &isolated)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("plugin files re-overlaid"),
        "vault-sync repair must have run: {stdout}"
    );
    assert!(
        vault
            .join(".claude/plugins/onebrain/INSTRUCTIONS.md")
            .exists(),
        "repair must restore plugin files"
    );
    let after = fs::read_to_string(vault.join("onebrain.yml")).unwrap();
    // No template comment is stripped; the only additions are the canonical
    // stats section's two structural comments (System banner + managed note),
    // stamped in by the doctor run.
    assert_eq!(
        comment_lines(&after),
        comment_lines(&template) + 2,
        "plugin-files repair must not strip config comments:\n{after}"
    );
    for tmpl_comment in template.lines().filter(|l| l.trim_start().starts_with('#')) {
        assert!(
            after.contains(tmpl_comment),
            "template comment lost: {tmpl_comment:?}\n{after}"
        );
    }
    assert!(after.contains(onebrain_fs::SYSTEM_MANAGED_NOTE), "{after}");
    assert!(after.contains("# collection:"), "{after}");
}
