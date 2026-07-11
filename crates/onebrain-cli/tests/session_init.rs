use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// v3.1: default mode is text · hook callers pass `--json` for the
// structured envelope. The tests below cover both contracts.

#[test]
fn session_init_emits_required_fields_in_minimal_vault_with_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session-init", "--json"])
        .current_dir(fixture("minimal_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"datetime\":"))
        .stdout(predicate::str::contains("\"session_token\":"))
        .stdout(predicate::str::contains("\"qmd_unembedded\":"))
        .stdout(predicate::str::contains("\"decision\":").not());
}

/// #229: `onebrain session init --vault <path>` must resolve the flagged
/// vault even when the process cwd is OUTSIDE any vault — before the fix,
/// `session init` ignored the global `--vault` flag entirely and always
/// walked up from cwd, so this would have emitted the "not found" block.
#[test]
fn session_init_honors_vault_flag_from_outside_any_vault() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session", "init", "--json", "--vault"])
        .arg(fixture("minimal_vault"))
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session_token\":"))
        .stdout(predicate::str::contains("\"decision\":").not());
}

/// Counterpart via `ONEBRAIN_VAULT` env instead of the flag.
#[test]
fn session_init_honors_onebrain_vault_env_from_outside_any_vault() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session", "init", "--json"])
        .env("ONEBRAIN_VAULT", fixture("minimal_vault"))
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session_token\":"))
        .stdout(predicate::str::contains("\"decision\":").not());
}

#[test]
fn session_init_emits_block_outside_vault_with_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session-init", "--json"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"decision\":\"block\""))
        .stdout(predicate::str::contains(
            "\"reason\":\"onebrain-vault-not-found\"",
        ));
}

/// R1 C2: malformed vault.yml now emits the distinct
/// `onebrain-vault-malformed` reason (was previously collapsed to
/// `onebrain-vault-not-found`). Still exits 0 with a block JSON so
/// SessionStart can surface "fix your vault.yml" instead of "/onboarding".
#[test]
fn session_init_emits_block_on_malformed_vault_yml_with_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session-init", "--json"])
        .current_dir(fixture("malformed_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"decision\":\"block\""))
        .stdout(predicate::str::contains(
            "\"reason\":\"onebrain-vault-malformed\"",
        ))
        .stdout(predicate::str::contains("\"error_detail\":"));
}

// ── v3.1 default-text contract ───────────────────────────────────────────

#[test]
fn default_outside_vault_emits_text_not_json() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("session-init")
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("{").not())
        .stdout(predicate::str::contains("No OneBrain vault found"));
}

#[test]
fn default_inside_vault_emits_text_session_ready() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .arg("session-init")
        .current_dir(fixture("minimal_vault"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("{").not())
        .stdout(predicate::str::contains("Session ready"));
}

#[test]
fn json_pretty_emits_indented_multiline() {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session-init", "--json", "--pretty"])
        .current_dir(fixture("empty_vault"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains('\n'), "expected newlines; got: {stdout}");
    assert!(
        stdout.contains("  \"decision\""),
        "expected 2-space indent on `decision`; got: {stdout}"
    );
    // Still parseable as JSON.
    let _v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("invalid JSON");
}

#[test]
fn yaml_flag_emits_yaml() {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["session-init", "--yaml"])
        .current_dir(fixture("empty_vault"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.trim_start().starts_with('{'),
        "expected YAML; got JSON-shaped: {stdout}"
    );
    let v: serde_yaml::Value = serde_yaml::from_str(&stdout).expect("invalid YAML");
    assert_eq!(
        v.get("reason").and_then(|v| v.as_str()),
        Some("onebrain-vault-not-found")
    );
}
