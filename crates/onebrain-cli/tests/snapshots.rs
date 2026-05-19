use assert_cmd::Command;
use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn fixture_orphan(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/orphan_scan")
        .join(name)
}

fn run_json(args: &[&str], dir: &std::path::Path) -> Value {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn failed");
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 stdout");
    serde_json::from_str(stdout.trim()).expect("not valid JSON")
}

fn normalize_volatile(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("datetime") {
            obj.insert("datetime".into(), Value::String("<NORMALIZED>".into()));
        }
        if obj.contains_key("session_token") {
            obj.insert("session_token".into(), Value::String("<NORMALIZED>".into()));
        }
    }
    v
}

#[test]
fn session_init_minimal_vault_snapshot() {
    let raw = run_json(&["session-init"], &fixture("minimal_vault"));
    assert_json_snapshot!("session_init_minimal_vault", normalize_volatile(raw));
}

#[test]
fn session_init_block_snapshot() {
    let raw = run_json(&["session-init"], &fixture("empty_vault"));
    assert_json_snapshot!("session_init_block", normalize_volatile(raw));
}

#[test]
fn orphan_scan_empty_logs_snapshot() {
    let raw = run_json(
        &["orphan-scan", ".", "abc12345"],
        &fixture_orphan("empty_logs"),
    );
    assert_json_snapshot!("orphan_scan_empty_logs", raw);
}

// ---------------------------------------------------------------------------
// `update` non-TTY plain-text snapshots (Slice 11)
// ---------------------------------------------------------------------------

#[test]
fn update_check_dry_run_snapshot() {
    use onebrain_fs::update::{run_update, CurrentVersion, ReleaseInfo, UpdateOptions};
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let lines_c = lines.clone();
    let opts = UpdateOptions {
        check: true,
        fetch_fn: Some(Box::new(|| {
            Ok(ReleaseInfo {
                version: "v9.9.9".to_string(),
                published_at: None,
            })
        })),
        current_version_fn: Some(Box::new(|| CurrentVersion {
            version: "v1.10.18".to_string(),
            published_at: None,
        })),
        stdout_lines: Some(Box::new(move |s| {
            lines_c.lock().unwrap().push(s.to_string());
        })),
        ..Default::default()
    };
    let _ = run_update(opts);
    let joined = lines.lock().unwrap().join("\n");
    insta::assert_snapshot!("update_check_dry_run", joined);
}

#[test]
fn update_full_upgrade_success_snapshot() {
    use onebrain_fs::update::{run_update, CurrentVersion, ReleaseInfo, UpdateOptions};
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let lines_c = lines.clone();
    let opts = UpdateOptions {
        fetch_fn: Some(Box::new(|| {
            Ok(ReleaseInfo {
                version: "v2.0.0".to_string(),
                published_at: None,
            })
        })),
        install_fn: Some(Box::new(|_| Ok(()))),
        validate_fn: Some(Box::new(|| true)),
        current_version_fn: Some(Box::new(|| CurrentVersion {
            version: "v1.10.18".to_string(),
            published_at: None,
        })),
        stdout_lines: Some(Box::new(move |s| {
            lines_c.lock().unwrap().push(s.to_string());
        })),
        ..Default::default()
    };
    let _ = run_update(opts);
    let joined = lines.lock().unwrap().join("\n");
    insta::assert_snapshot!("update_full_upgrade_success", joined);
}

#[test]
fn register_hooks_fresh_install_snapshot() {
    // Snapshot the on-disk settings.json produced from a fresh vault.
    let d = tempdir().unwrap();
    fs::create_dir_all(d.path().join(".claude")).unwrap();
    fs::write(d.path().join("vault.yml"), "method: onebrain\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success();
    let text = fs::read_to_string(d.path().join(".claude").join("settings.json")).unwrap();
    assert_snapshot!("register_hooks_fresh_install", text);
}

#[test]
fn register_hooks_merge_legacy_snapshot() {
    // Snapshot the on-disk settings.json after migrating a vault that has:
    // - legacy shell-form Stop hook
    // - legacy `qmd update -c foo` PostToolUse hook
    // - a user-added Bash() permission
    // - an unknown top-level key (theme)
    // - qmd_collection set in vault.yml
    let d = tempdir().unwrap();
    fs::create_dir_all(d.path().join(".claude")).unwrap();
    fs::write(d.path().join("vault.yml"), "qmd_collection: foo\n").unwrap();
    fs::write(
        d.path().join(".claude").join("settings.json"),
        serde_json::to_string(&serde_json::json!({
            "theme": "dark",
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [{"command": "onebrain checkpoint stop"}]}],
                "PostToolUse": [{
                    "matcher": "Write|Edit",
                    "hooks": [{"type": "command", "command": "qmd update -c foo"}],
                }],
            },
            "permissions": {"allow": ["Bash(my-script *)"]},
        }))
        .unwrap(),
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["register-hooks", "--vault", d.path().to_str().unwrap()])
        .assert()
        .success();
    let text = fs::read_to_string(d.path().join(".claude").join("settings.json")).unwrap();
    assert_snapshot!("register_hooks_merge_legacy", text);
}

#[test]
fn vault_sync_happy_path_stdout_snapshot() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join(".claude")).unwrap();
    std::fs::write(
        vault.join("vault.yml"),
        "update_channel: stable\nfolders:\n  inbox: 00-inbox\n  logs: 07-logs\n",
    )
    .unwrap();

    let mut buf = Vec::new();
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut b = tar::Builder::new(enc);
        let prefix = "onebrain-ai-onebrain-snap";
        for (path, content) in [
            (
                format!("{prefix}/.claude/plugins/onebrain/.claude-plugin/plugin.json"),
                serde_json::json!({"id":"onebrain","version":"1.11.0"}).to_string(),
            ),
            (format!("{prefix}/CONTRIBUTING.md"), "# c\n".into()),
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
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, &path, content.as_bytes()).unwrap();
        }
        b.finish().unwrap();
    }
    let tarball_path = tmp.path().join("fixture.tar.gz");
    std::fs::write(&tarball_path, &buf).unwrap();
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
    let stdout = String::from_utf8(out.stdout).unwrap();
    insta::assert_snapshot!("vault_sync_happy_stdout", stdout);
}
