//! Layer 2 integration tests for `onebrain init` — spawns the real binary
//! via `assert_cmd` with the cwd set to a fresh tempdir. Uses `--yes` so
//! the run is non-interactive and pulls the Essentials schedule preset by
//! default.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn cli_yes_fresh_vault_writes_files_and_emits_header() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("OneBrain Init"))
        .stdout(predicate::str::contains("vault.yml: written"))
        .stdout(predicate::str::contains("folders:"))
        .stdout(predicate::str::contains("done"));

    // vault.yml created
    assert!(d.path().join("vault.yml").is_file());
    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert!(content.contains("update_channel: stable"));
    assert!(content.contains("schedule:"));

    // All 8 folders + imports
    for folder in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        assert!(d.path().join(folder).is_dir(), "missing folder {folder}");
    }
    assert!(d.path().join("00-inbox").join("imports").is_dir());
}

#[test]
fn cli_yes_existing_vault_yml_returns_non_zero() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("vault.yml exists"))
        .stdout(predicate::str::contains("--force"));

    // Original content preserved
    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert_eq!(content, "old: value\n");
    // No folders created
    assert!(!d.path().join("00-inbox").exists());
}

#[test]
fn cli_yes_creates_schedule_block_with_essentials_entries() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();

    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert!(content.contains("/daily"));
    assert!(content.contains("/weekly"));
    assert!(content.contains("/recap"));
}

#[test]
fn cli_yes_emits_essentials_preset_line() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("preset: Essentials"));
}

#[test]
fn cli_yes_run_twice_second_fails_without_force() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure();
}

/// Regression: `onebrain init --yes` in an empty directory must populate
/// `.claude/settings.json` with the Stop hook. Before the fix, init's
/// best-effort register-hooks call no-oped because `.claude/` did not exist
/// yet, so it printed "hooks: ok" without writing settings.json.
#[test]
fn cli_yes_populates_claude_settings_json_with_stop_hook() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hooks: ok"));

    let settings_path = d.path().join(".claude").join("settings.json");
    assert!(
        settings_path.is_file(),
        ".claude/settings.json missing after init"
    );
    let text = fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("settings.json is valid JSON");
    let stop = v
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
        .expect("hooks.Stop array missing");
    assert!(!stop.is_empty(), "hooks.Stop is empty: {text}");
}

/// Bun v2.3.3-parity: `--force` overrides the existing-vault.yml guard so
/// the run succeeds and the file is rewritten without prompting.
#[test]
fn cli_yes_force_overwrites_existing_vault_yml() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("vault.yml: written"));
    let content = fs::read_to_string(d.path().join("vault.yml")).unwrap();
    assert!(
        content.contains("update_channel"),
        "vault.yml should have been overwritten with the fresh template",
    );
}

// ──────────────────────────────────────────────────────────────────────
// Plugin registration (marketplace.json + enabledPlugins)
// ──────────────────────────────────────────────────────────────────────

/// Without `.claude-plugin/marketplace.json`, Claude Code never discovers
/// the vault-bundled plugin. Fresh init must always write this file.
#[test]
fn init_creates_marketplace_json_at_vault_root() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("marketplace.json: written"));

    let path = d.path().join(".claude-plugin").join("marketplace.json");
    assert!(path.is_file(), ".claude-plugin/marketplace.json missing");
    let text = fs::read_to_string(&path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("marketplace.json valid JSON");
    assert_eq!(v["name"], "onebrain");
    assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
    assert!(v["plugins"][0]["description"]
        .as_str()
        .unwrap()
        .contains("Your AI Thinking Partner"));
}

/// Without `enabledPlugins.onebrain@onebrain = true` in settings.json, the
/// plugin shows up in the marketplace browser but stays dormant. Fresh
/// init must inject the key alongside the existing hooks/permissions block.
#[test]
fn init_adds_enabled_plugins_to_settings() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("plugin: enabled"));

    let settings_path = d.path().join(".claude").join("settings.json");
    let text = fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("settings.json valid JSON");
    assert_eq!(
        v["enabledPlugins"]["onebrain@onebrain"],
        serde_json::Value::Bool(true),
        "enabledPlugins.onebrain@onebrain missing: {text}"
    );
    // Coexist with the register-hooks-managed keys
    assert!(v["hooks"]["Stop"].is_array(), "hooks.Stop missing: {text}");
    assert!(
        v["permissions"]["allow"].is_array(),
        "permissions.allow missing: {text}"
    );
}

/// User-customized marketplace.json must survive `--force` re-init — we
/// only write the manifest when it doesn't already exist.
#[test]
fn init_is_idempotent_marketplace_json_not_overwritten() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();

    // Modify the manifest as a user would
    let path = d.path().join(".claude-plugin").join("marketplace.json");
    let mut text = fs::read_to_string(&path).unwrap();
    text = text.replace(
        "Your AI Thinking Partner (vault-bundled plugin)",
        "Your AI Thinking Partner (CUSTOMIZED BY USER)",
    );
    fs::write(&path, &text).unwrap();

    // Re-init with --force (existing vault.yml guard requires it)
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "marketplace.json: ok (pre-existing)",
        ));

    let after = fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("CUSTOMIZED BY USER"),
        "user customization clobbered: {after}"
    );
}

/// Pre-existing settings.json (e.g., from a Claude Code session that ran
/// in this dir before OneBrain init) must keep every key — we only insert
/// `enabledPlugins.onebrain@onebrain`, no other mutation. `--force` is
/// required because the pre-existing `.claude/` makes the target dir
/// non-empty (the safety guard would otherwise prompt).
#[test]
fn init_preserves_existing_settings_keys() {
    let d = tempdir().unwrap();
    fs::create_dir_all(d.path().join(".claude")).unwrap();
    let settings_path = d.path().join(".claude").join("settings.json");
    let custom = serde_json::json!({
        "permissions": { "allow": ["Bash(custom *)"] },
        "model": "claude-sonnet",
        "theme": "dark"
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&custom).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();

    let text = fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["enabledPlugins"]["onebrain@onebrain"], true);
    assert_eq!(v["model"], "claude-sonnet");
    assert_eq!(v["theme"], "dark");
    // register-hooks merges its 14 allow entries in alongside the custom one
    let allow = v["permissions"]["allow"].as_array().expect("allow array");
    assert!(
        allow.iter().any(|x| x == "Bash(custom *)"),
        "custom permission lost: {text}"
    );
}

/// Re-init when enabledPlugins is already true is a no-op for settings —
/// register-hooks still runs (idempotent) but the enable step shouldn't
/// re-write the file. The previous "plugin: enabled" line becomes
/// "plugin: ok (already enabled)".
#[test]
fn init_skips_when_already_enabled() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("plugin: enabled"));

    // Run again with --force; key already true → "ok (already enabled)"
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("plugin: ok (already enabled)"));

    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(d.path().join(".claude").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(v["enabledPlugins"]["onebrain@onebrain"], true);
}

/// Pre-existing malformed settings.json must surface as a hard error
/// (FsError → exit 66 / E_FS_ERROR) rather than silently overwriting the
/// user's data. `--force` skips the non-empty safety check so we actually
/// exercise the read-settings.json code path under test.
#[test]
fn init_fails_on_malformed_settings_json() {
    let d = tempdir().unwrap();
    fs::create_dir_all(d.path().join(".claude")).unwrap();
    let settings_path = d.path().join(".claude").join("settings.json");
    fs::write(&settings_path, "{not valid json").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure();

    // Existing file untouched (atomic write means the malformed contents
    // are preserved on the failure path)
    let after = fs::read_to_string(&settings_path).unwrap();
    assert_eq!(after, "{not valid json");
}

/// Bun v2.3.3-parity: `--vault-dir <path>` targets a directory other than
/// cwd. The binary should write the vault scaffold into that directory.
#[test]
fn cli_yes_vault_dir_targets_explicit_path() {
    let d = tempdir().unwrap();
    let target = d.path().join("nested-vault");
    fs::create_dir_all(&target).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "init",
            "--yes",
            "--no-sync",
            "--vault-dir",
            target.to_str().unwrap(),
        ])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("done"));
    assert!(target.join("vault.yml").is_file());
    assert!(target.join("07-logs").is_dir());
}

// ──────────────────────────────────────────────────────────────────────
// Folder-not-empty safety check (Item H)
// ──────────────────────────────────────────────────────────────────────

/// Empty dir is the happy path — no prompt, no error, no `--force` needed.
#[test]
fn init_empty_dir_succeeds_silently() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();
    assert!(d.path().join("vault.yml").is_file());
}

/// Missing dir: classify=Missing → init creates it + proceeds. No `--force`
/// needed because there's nothing to overwrite.
#[test]
fn init_nonexistent_dir_creates_and_succeeds() {
    let d = tempdir().unwrap();
    let target = d.path().join("brand-new").join("nested");
    assert!(!target.exists());
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "init",
            "--yes",
            "--no-sync",
            "--vault-dir",
            target.to_str().unwrap(),
        ])
        .current_dir(d.path())
        .assert()
        .success();
    assert!(target.is_dir());
    assert!(target.join("vault.yml").is_file());
    assert!(target.join("07-logs").is_dir());
}

/// Dir with non-OneBrain files + no vault.yml + no --force in non-interactive
/// mode → exit 75 (E_INIT_TARGET_NOT_EMPTY) without prompting.
#[test]
fn init_nonempty_dir_yes_without_force_errors_with_exit_75() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("README.md"), "hi").unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure();
    // Exit code 75 = E_INIT_TARGET_NOT_EMPTY (skill-alignment §4.8)
    out.code(75)
        .stderr(predicate::str::contains("target is not empty"))
        .stderr(predicate::str::contains("--force"));

    // README.md preserved, no vault structure created
    assert!(d.path().join("README.md").is_file());
    assert!(!d.path().join("vault.yml").exists());
    assert!(!d.path().join("00-inbox").exists());
}

/// `--force` on a non-empty dir bypasses the safety prompt entirely.
/// Existing files are preserved (we only add, never delete).
#[test]
fn init_nonempty_dir_force_flag_proceeds() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("README.md"), "hi").unwrap();
    fs::create_dir_all(d.path().join("src")).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .success();
    // Existing files preserved
    assert!(d.path().join("README.md").is_file());
    assert!(d.path().join("src").is_dir());
    // OneBrain structure created
    assert!(d.path().join("vault.yml").is_file());
    assert!(d.path().join("00-inbox").is_dir());
}

/// `--json` mode on a non-empty dir without `--force` emits a canonical
/// Envelope::err with `error.code = E_INIT_TARGET_NOT_EMPTY` to stdout.
#[test]
fn init_json_mode_nonempty_errors_without_prompt() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("file.txt"), "x").unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync", "--json"])
        .current_dir(d.path())
        .assert()
        .failure();
    let output = assert.code(75).get_output().clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout is a JSON document");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "E_INIT_TARGET_NOT_EMPTY");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("target is not empty"));
    // No prompt was attempted — file.txt still intact
    assert_eq!(fs::read_to_string(d.path().join("file.txt")).unwrap(), "x");
}

/// `--json --force` overrides the safety check just like in text mode.
#[test]
fn init_json_mode_nonempty_with_force_succeeds() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("file.txt"), "x").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--force", "--no-sync", "--json"])
        .current_dir(d.path())
        .assert()
        .success();
    assert!(d.path().join("vault.yml").is_file());
    assert!(d.path().join("file.txt").is_file());
}

/// Dotfile-only dirs (.DS_Store, .git, etc.) still count as non-empty —
/// the safety check intentionally does not whitelist any pattern.
#[test]
fn init_dotfile_only_dir_still_triggers_safety_check() {
    let d = tempdir().unwrap();
    fs::write(d.path().join(".DS_Store"), "").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure()
        .code(75);
}

/// Re-init on an existing OneBrain vault (vault.yml present) hits the
/// existing vault.yml guard BEFORE the safety check — the message reflects
/// vault.yml-exists, not target-not-empty.
#[test]
fn init_existing_vault_yml_skips_safety_check() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();
    // Also drop a sibling file so the dir is clearly "non-empty" — proves
    // the safety check defers to the existing guard when vault.yml is
    // present, regardless of other entries.
    fs::write(d.path().join("README.md"), "hi").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["init", "--yes", "--no-sync"])
        .current_dir(d.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("vault.yml exists"));
}
