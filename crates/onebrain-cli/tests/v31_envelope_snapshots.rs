//! v3.1 envelope shape snapshots — lock the canonical JSON contract.
//!
//! Each test invokes a real `onebrain` subcommand against a synthetic temp
//! vault, parses its JSON output, normalises the volatile fields
//! (paths, tempdir basenames, cwd, datetime, session_token), then pins the
//! resulting shape with `insta::assert_json_snapshot!`.
//!
//! Coverage (matches design AC #15 and #16):
//! - `vault current --json` — inside vault · normal envelope
//! - `plugin update --json --dry-run` — envelope wrapper around the report
//!   payload (dry-run avoids both the network fetch and the launchd write)
//! - `session init` (alias `session-init`) — inside vault · v3.0 hook shape
//! - `session init` outside vault — `{decision:"block",reason:"…"}` shape
//!
//! These snapshots LOCK the contract for v3.x. Any future envelope change
//! must update the snapshots intentionally (`cargo insta review`) and CI
//! will surface the drift before merge.

use assert_cmd::Command;
use insta::assert_json_snapshot;
use serde_json::Value;
use std::fs;
use tempfile::tempdir;

/// Recursively walk a JSON value, replacing any string that contains the
/// `needle` substring with `replacement`. Used to scrub temp-directory paths
/// and basenames from a snapshot before assertion — the structure stays
/// intact, only volatile content is masked.
fn scrub_substring(v: &mut Value, needle: &str, replacement: &str) {
    match v {
        Value::String(s) if s.contains(needle) => {
            *s = replacement.to_string();
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_substring(item, needle, replacement);
            }
        }
        Value::Object(obj) => {
            for (_k, val) in obj.iter_mut() {
                scrub_substring(val, needle, replacement);
            }
        }
        _ => {}
    }
}

/// Replace specific top-level keys with a placeholder so snapshots survive
/// non-deterministic data (datetime, session_token, cwd, name, path).
fn normalise_key(v: &mut Value, key: &str, placeholder: &str) {
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key(key) {
            obj.insert(key.into(), Value::String(placeholder.into()));
        }
    }
    // Recurse one level into common containers (`data`, `vault`) — vault
    // envelopes carry `name`/`path` both at the root and inside `data`.
    if let Some(obj) = v.as_object_mut() {
        for nested_key in ["data", "vault"] {
            if let Some(nested) = obj.get_mut(nested_key) {
                normalise_key(nested, key, placeholder);
            }
        }
    }
}

fn make_vault(dir: &std::path::Path) {
    fs::write(dir.join("vault.yml"), "method: onebrain\n").unwrap();
}

fn run_json(args: &[&str], dir: &std::path::Path) -> Value {
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn failed");
    assert!(
        output.status.success(),
        "command {args:?} failed · status {:?} · stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 stdout");
    serde_json::from_str(stdout.trim()).expect("not valid JSON")
}

// ─────────────────────────────────────────────────────────────────────────
// vault current --json
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn vault_current_json_envelope_snapshot() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let mut v = run_json(&["vault", "current", "--json"], dir.path());

    // Scrub the temp path (varies per run) before key normalisation so any
    // PathBuf string under unrelated keys is also masked.
    let tmp_str = dir.path().to_string_lossy().to_string();
    scrub_substring(&mut v, &tmp_str, "<VAULT_PATH>");
    // Scrub the tempdir basename (the vault `name` field).
    let basename = dir
        .path()
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !basename.is_empty() {
        scrub_substring(&mut v, &basename, "<VAULT_NAME>");
    }
    // Cwd is the same temp path, already scrubbed by scrub_substring above.
    assert_json_snapshot!("vault_current_envelope", v);
}

// ─────────────────────────────────────────────────────────────────────────
// plugin update --json (dry-run · no network, no launchd writes)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn plugin_update_json_envelope_snapshot_dry_run() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // No `.claude/settings.json` → hooks_rewritten = 0.
    let mut v = run_json(&["plugin", "update", "--dry-run", "--json"], dir.path());

    // The envelope's `vault` field carries the temp path / basename.
    let tmp_str = dir.path().to_string_lossy().to_string();
    scrub_substring(&mut v, &tmp_str, "<VAULT_PATH>");
    let basename = dir
        .path()
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !basename.is_empty() {
        scrub_substring(&mut v, &basename, "<VAULT_NAME>");
    }
    assert_json_snapshot!("plugin_update_envelope_dry_run", v);
}

// ─────────────────────────────────────────────────────────────────────────
// session init — inside vault (v3.0 hook shape preserved)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn session_init_json_envelope_snapshot_inside_vault() {
    let dir = tempdir().unwrap();
    // Same fixture shape as legacy session_init snapshot — qmd_collection
    // present so the qmd_unembedded probe runs (returns 0 with no qmd
    // binary installed in the test env).
    fs::write(dir.path().join("vault.yml"), "qmd_collection: x\n").unwrap();
    let mut v = run_json(&["session", "init"], dir.path());

    // Volatile fields per the hook contract.
    normalise_key(&mut v, "datetime", "<DATETIME>");
    normalise_key(&mut v, "session_token", "<SESSION_TOKEN>");
    assert_json_snapshot!("session_init_envelope_inside_vault", v);
}

// ─────────────────────────────────────────────────────────────────────────
// session init — outside vault (block JSON per hook contract)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn session_init_json_envelope_snapshot_outside_vault() {
    let no_vault = tempdir().unwrap();
    // No vault.yml — hook protocol emits the block JSON + exit 0.
    let v = run_json(&["session", "init"], no_vault.path());
    // Block shape has no volatile fields — pin verbatim.
    assert_json_snapshot!("session_init_envelope_outside_vault", v);
}
