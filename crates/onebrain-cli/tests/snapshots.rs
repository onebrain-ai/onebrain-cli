use assert_cmd::Command;
use insta::assert_json_snapshot;
use serde_json::Value;
use std::path::PathBuf;

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
