mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// v3.1: default mode is text · hook callers pass `--json` for the
// structured envelope. The tests below cover both contracts.
//
// Every spawn against the `minimal_vault` fixture pins `ONEBRAIN_CACHE_DIR`:
// that fixture's checked-in `vault.yml` carries `qmd_collection: ob-1`, and
// `session init` probes the index for the resolved collection — `Engine::open`
// migrates the layout it finds, so an unisolated run reaches (and rewrites)
// a same-named collection in the developer's real cache root. The collection
// name lives outside `.rs`, which is why `tests/cache_isolation_sweep.rs`
// scans the fixture configs too.

#[test]
fn session_init_emits_required_fields_in_minimal_vault_with_json() {
    let cache = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["session-init", "--json"])
        .current_dir(fixture("minimal_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"datetime\":"))
        .stdout(predicate::str::contains("\"session_token\":"))
        .stdout(predicate::str::contains("\"qmd_unembedded\":"))
        .stdout(predicate::str::contains("\"decision\":").not());
}

#[test]
fn codex_chats_with_same_prefix_receive_distinct_session_tokens() {
    let cache = tempdir().unwrap();
    let run = |session_id: &str| {
        let output = Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", cache.path())
            .env("CODEX_SESSION_ID", session_id)
            .args(["session-init", "--json"])
            .current_dir(fixture("minimal_vault"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["session_token"].as_str().unwrap().to_owned()
    };

    let first = run("same-prefix-chat-a");
    let second = run("same-prefix-chat-b");
    assert_ne!(first, second);
    assert_eq!(first.len(), 16);
    assert_eq!(second.len(), 16);
}

#[test]
fn generic_hook_session_id_has_highest_priority_and_hashes_the_full_value() {
    let cache = tempdir().unwrap();
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .env("ONEBRAIN_HOOK_SESSION_ID", "same-session-id")
        .env("CODEX_SESSION_ID", "lower-priority-codex-id")
        .env("CLAUDE_CODE_SESSION_ID", "lower-priority-claude-id")
        .args(["session", "init", "--json"])
        .current_dir(fixture("minimal_vault"))
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session_token"], "958f9d9b674d9298");
}

#[test]
fn explicit_hook_token_survives_startup_metadata_collection() {
    let cache = tempdir().unwrap();
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .env("CODEX_SESSION_ID", "a-different-chat-id")
        .args([
            "session",
            "init",
            "--json",
            "--session-token",
            "0123456789abcdef",
        ])
        .current_dir(fixture("minimal_vault"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session_token"], "0123456789abcdef");
}

/// #229: `onebrain session init --vault <path>` must resolve the flagged
/// vault even when the process cwd is OUTSIDE any vault — before the fix,
/// `session init` ignored the global `--vault` flag entirely and always
/// walked up from cwd, so this would have emitted the "not found" block.
#[test]
fn session_init_honors_vault_flag_from_outside_any_vault() {
    let cache = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", cache.path())
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
    let cache = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", cache.path())
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .arg("session-init")
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("{").not())
        .stdout(predicate::str::contains("No OneBrain vault found"));
}

#[test]
fn default_inside_vault_emits_text_session_ready() {
    let cache = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", cache.path())
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
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
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
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

// ── `session token`: resolve-only, no side effects (round-3 audit) ───────
//
// `session init` calls `clean_stale_state_file`, which deletes the Stop-hook
// cadence state file whenever its mtime predates process start — always true
// mid-session. Recovery callers that just need to re-learn an already-live
// token cannot use `session init` for that without silently wiping the
// counter they're trying to recover. `session token` resolves the identical
// token through the identical chain, with no such side effect.

#[test]
fn session_token_matches_session_init_hash16_token() {
    let cache = tempdir().unwrap();
    let run = |verb_args: &[&str]| {
        let output = Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", cache.path())
            .env("ONEBRAIN_HOOK_SESSION_ID", "recovery-guard-chat")
            .args(verb_args)
            .current_dir(fixture("minimal_vault"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["session_token"].as_str().unwrap().to_owned()
    };

    let from_init = run(&["session", "init", "--json"]);
    let from_token = run(&["session", "token", "--json"]);
    assert_eq!(
        from_init, from_token,
        "session token must resolve the same token session init reports for the same env"
    );
    assert_eq!(
        from_token.len(),
        16,
        "hook session ids resolve as sha256[..16]"
    );
}

/// Unlike `session init`, `session token` never resolves a vault at all — the
/// token chain reads env/process state only — so it must succeed from a cwd
/// that isn't (and has no ancestor that is) a vault, with no hook-protocol
/// block envelope.
#[test]
fn session_token_works_outside_any_vault() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("ONEBRAIN_HOOK_SESSION_ID", "session-token-no-vault")
        .args(["session", "token", "--json"])
        .current_dir(fixture("empty_vault"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"session_token\":"))
        .stdout(predicate::str::contains("\"decision\":").not());
}

#[test]
fn session_token_json_mode_prints_only_the_token_field() {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("ONEBRAIN_HOOK_SESSION_ID", "session-token-json-shape")
        .args(["session", "token", "--json"])
        .current_dir(fixture("minimal_vault"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        value.as_object().map(|o| o.len()),
        Some(1),
        "session token's JSON must carry exactly one field; got: {stdout}"
    );
    assert!(value
        .get("session_token")
        .and_then(|v| v.as_str())
        .is_some());
}

#[test]
fn session_token_yaml_flag_emits_yaml() {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("ONEBRAIN_HOOK_SESSION_ID", "session-token-yaml")
        .args(["session", "token", "--yaml"])
        .current_dir(fixture("empty_vault"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.trim_start().starts_with('{'),
        "expected YAML; got JSON-shaped: {stdout}"
    );
    let v: serde_yaml::Value = serde_yaml::from_str(&stdout).expect("invalid YAML");
    assert!(v.get("session_token").and_then(|v| v.as_str()).is_some());
}

#[test]
fn session_token_text_mode_prints_token_line() {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("ONEBRAIN_HOOK_SESSION_ID", "session-token-text-mode")
        .args(["session", "token"])
        .current_dir(fixture("minimal_vault"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.trim_start().starts_with("token="),
        "expected `token=` line; got: {stdout}"
    );
    assert!(
        !stdout.trim_start().starts_with('{'),
        "default mode must not emit JSON; got: {stdout}"
    );
}

/// The guard this verb exists for. Seeds one Stop-hook cadence tick (via
/// `checkpoint stop`, mirroring what `onebrain hook` does on a real Stop
/// event) under the hash-16 token, then proves `session token` resolves that
/// SAME token without disturbing the state file `checkpoint stop` wrote.
#[test]
fn session_token_leaves_cadence_state_file_untouched() {
    let cache = tempdir().unwrap();
    let state = tempdir().unwrap();
    let pinned = || {
        let mut cmd = Command::cargo_bin("onebrain").unwrap();
        cmd.env("ONEBRAIN_CACHE_DIR", cache.path())
            .env("TMPDIR", state.path())
            .env("TMP", state.path())
            .env("TEMP", state.path())
            .env("ONEBRAIN_HOOK_SESSION_ID", "recovery-guard-untouched")
            .current_dir(fixture("minimal_vault"));
        cmd
    };

    // Learn the token, then seed a cadence tick under it.
    let output = pinned()
        .args(["session", "init", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let token = value["session_token"].as_str().unwrap().to_owned();

    pinned().args(["checkpoint", "stop"]).assert().success();

    let state_path = state.path().join(format!("onebrain-{token}.state"));
    let seeded = fs::read_to_string(&state_path).unwrap();
    assert!(
        seeded.starts_with("1:"),
        "expected one counted stop under the hook token, got {seeded}"
    );

    // The guard: `session token` resolves the identical token …
    let output = pinned()
        .args(["session", "token", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session_token"].as_str().unwrap(), token);

    // … WITHOUT touching the cadence state file `checkpoint stop` wrote.
    let after = fs::read_to_string(&state_path).unwrap();
    assert_eq!(
        after, seeded,
        "session token must not modify the Stop-hook cadence state file — \
         that's the whole reason it exists instead of reusing session init"
    );
}
