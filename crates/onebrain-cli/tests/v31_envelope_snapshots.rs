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

mod support;

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
    // Canonical config filename since v3.1.0. Writing the legacy `vault.yml`
    // here would trip the deprecation warning on every run and drown out real
    // warnings in the snapshot/CI output.
    fs::write(dir.join("onebrain.yml"), "method: onebrain\n").unwrap();
}

fn run_json(args: &[&str], dir: &std::path::Path) -> Value {
    // Isolated, never-populated cache dir so the native-search `qmd_unembedded`
    // probe (v3.4.1: reads the local index directly, no more `qmd` subprocess)
    // deterministically finds no index for the fixture's collection → `null`
    // ("unknown"), regardless of what any real vault on this machine has
    // indexed under the same collection name. Mirrors `snapshots.rs`.
    let cache = tempdir().unwrap();
    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .args(args)
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        // Hermetic `headless`: clear ONEBRAIN_HEADLESS so session-init snapshots
        // read `false` even when the test runner is itself under `skill run`.
        .env_remove("ONEBRAIN_HEADLESS")
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
    // present so the qmd_unembedded probe runs; with the cache dir isolated
    // to an empty tempdir, the collection has no index yet and the probe
    // reports `null` (v3.4: "unknown" rather than a false 0).
    fs::write(dir.path().join("vault.yml"), "qmd_collection: x\n").unwrap();
    // v3.1: machine consumers must opt into JSON explicitly now that text
    // is the default for interactive use.
    let mut v = run_json(&["session", "init", "--json"], dir.path());

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
    // No vault.yml — hook protocol emits the block JSON + exit 0. v3.1:
    // explicit --json since default is now text.
    let v = run_json(&["session", "init", "--json"], no_vault.path());
    // Block shape has no volatile fields — pin verbatim.
    assert_json_snapshot!("session_init_envelope_outside_vault", v);
}

// ─────────────────────────────────────────────────────────────────────────
// R2-H4 · hook-protocol byte-shape snapshots
//
// These commands run on the hook-protocol contract (raw JSON object, not
// the v3.1 envelope) — `{"orphan_count":N}` and
// `{"decision":"block","reason":"NN since <context>"}`. The hook-protocol
// shape is consumed by Claude Code's SessionStart/Stop hooks; any field
// rename would break the live consumer silently. Pin the shape here.
//
// `qmd reindex` is intentionally skipped — its current implementation
// emits no stable JSON output (returns immediately, prints a status line).
// When/if it gains a stable shape, add a snapshot here.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn checkpoint_orphans_envelope_snapshot() {
    // `checkpoint orphans <logs_folder> <session_token>` is the hook-protocol
    // command behind the SessionStart orphan-count probe. Empty checkpoint
    // dir → `{"orphan_count": 0}`.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // Pre-create an empty checkpoint folder so the scanner doesn't return
    // an error path; the shape is the same either way but this matches the
    // production "fresh vault" state.
    std::fs::create_dir_all(dir.path().join("checkpoint")).unwrap();
    // v3.1: explicit --json since default is now text.
    let v = run_json(
        &["checkpoint", "orphans", ".", "tokABC123", "--json"],
        dir.path(),
    );
    assert_json_snapshot!("checkpoint_orphans_envelope", v);
}

#[test]
fn checkpoint_stop_envelope_when_threshold_hit() {
    // `checkpoint stop` is the hook-protocol Stop-hook command. When the
    // message-count threshold is met it emits
    // `{"decision":"block","reason":"NN since start"}` — pin this shape.
    //
    // Driving: vault.yml sets messages=2; pre-write the state file with
    // count=2 in an isolated TMPDIR so the increment-to-3 fires the
    // threshold. Empty logs folder → NN = "01" → reason "01 since start".
    let tmpdir = tempdir().unwrap(); // overrides std::env::temp_dir() via TMPDIR/TMP/TEMP
    let vault_dir = tempdir().unwrap();

    // v3.1: use canonical `onebrain.yml` name so the legacy-vault.yml
    // deprecation warning doesn't pollute stderr (only matters on Windows
    // where stream interleaving can mask the panic context).
    std::fs::write(
        vault_dir.path().join("onebrain.yml"),
        "checkpoint:\n  messages: 2\n  minutes: 30\nfolders:\n  logs: 07-logs\n",
    )
    .unwrap();

    // WT_SESSION → deterministic session token (sanitised to alphanumeric,
    // truncated to 8 chars). "tkn12345" stays verbatim.
    let token = "tkn12345";
    let state_path = tmpdir.path().join(format!("onebrain-{token}.state"));
    // Format: "count:last_ts:nn" — count=2 means handle_stop increments to
    // 3 which is >= messages_threshold (2) and >= MIN_ACTIVITY (2), so it
    // emits the block JSON.
    std::fs::write(&state_path, "2:0:00").unwrap();

    let output = assert_cmd::Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .args([
            "checkpoint",
            "stop",
            "--vault-dir",
            vault_dir.path().to_str().unwrap(),
        ])
        // `std::env::temp_dir()` reads `TMPDIR` on Unix, `TMP`/`TEMP` on
        // Windows. Set all three so the pre-written state file at
        // `tmpdir/onebrain-{token}.state` is found cross-platform — without
        // the Windows variants, the binary would look at `%LOCALAPPDATA%\Temp`
        // instead and start state.count from 0 (block JSON never fires).
        .env("TMPDIR", tmpdir.path())
        .env("TMP", tmpdir.path())
        .env("TEMP", tmpdir.path())
        .env("WT_SESSION", token)
        // Clear hook-provided IDs above WT_SESSION so this test is hermetic
        // under both Codex and Claude.
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env("TMUX_PANE", "")
        .env_remove("TERM_SESSION_ID")
        .output()
        .expect("spawn failed");
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 stdout");
    let trimmed = stdout.trim();
    assert!(
        !trimmed.is_empty(),
        "expected block JSON on stdout · stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: Value = serde_json::from_str(trimmed).expect("not valid JSON");

    // Pin the byte shape — no volatile fields in the block JSON.
    assert_json_snapshot!("checkpoint_stop_block_envelope", v);
}
