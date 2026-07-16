//! Layer 2 integration tests for `onebrain doctor` — exercises the wired CLI
//! end-to-end (assert_cmd spawns the real binary) against synthetic vaults
//! built in tempdirs. Verifies exit codes, stdout/stderr content, and
//! warning-vs-error distinction.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

/// Build a minimal vault that should pass every check.
///
/// Includes:
/// - `vault.yml` with all 8 folder keys + `update_channel: stable` (so the
///   `onebrain.yml-keys` check has no soft-required warning). The fixture
///   writes the legacy filename on purpose; the check reads it via fallback.
/// - All 8 vault folders.
/// - `.claude/plugins/onebrain/` with the required files + non-empty
///   `agents/` and `skills/foo/`.
/// - `.claude/settings.json` with the canonical exec-form Stop hook and the
///   `Bash(onebrain *)` permission (so `settings-hooks` reports ok).
fn write_minimal_vault(dir: &Path) {
    std::fs::write(
        dir.join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
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
        std::fs::create_dir_all(dir.join(f)).unwrap();
    }
    let plugin = dir.join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = dir.join(".claude");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
}

#[test]
fn doctor_clean_vault_exits_0() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Exit code 0 means no `Error` status emerged from any check.
    // Note: this fixture has never run a reindex, so the native `search` check
    // returns Warn ("no index yet"). That keeps the exit code at 0 (advisory)
    // but means the verdict is the ⚠ glyph (warnings present, 0 fail) rather
    // than ✓. The test asserts the no-error invariant by checking that no fail
    // (`✗`) glyph row is rendered and the footer reports "0 fail" (v3.2.1
    // grouped layout).
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2717}").not())
        .stdout(predicate::str::contains("0 fail"))
        // v3.4 content swap: the native search row is present; the old qmd
        // embeddings row is gone.
        .stdout(predicate::str::contains("search"))
        .stdout(predicate::str::contains("unembedded").not());
}

/// The native `search` check's engine path, exercised without any model
/// download: reindexing an EMPTY vault (zero `.md` docs) never constructs the
/// lazy embedder, but it does create the on-disk index and stamp
/// `last_indexed`. Doctor then opens the engine read-only and reads its
/// status:
///   1. after the empty reindex → up to date on disk but the model is absent
///      → the "model not downloaded" advisory arm;
///   2. after a note appears → the pending-drift arm ("1 pending").
///
/// `ONEBRAIN_CACHE_DIR` isolates the search cache in a tempdir so nothing
/// touches the real user cache.
#[test]
fn doctor_search_check_reads_engine_status_after_reindex() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // Pin the collection so the check and the reindex agree on the cache dir.
    let cfg = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("search:\n  collection: doctor-it-engine\n{existing}"),
    )
    .unwrap();

    // 0. A cache dir that exists but was never reindexed (e.g. wiped index
    //    markers): the engine opens fresh and reports no last_indexed stamp →
    //    the "never reindexed" arm.
    std::fs::create_dir_all(cache.path().join("search/doctor-it-engine")).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("never reindexed"));

    // Empty-vault reindex: builds the index files, downloads nothing.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["search", "reindex"])
        .assert()
        .success();

    // 1. Index exists + up to date + model absent → advisory warn.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success() // warn-only — exit-code contract unchanged
        .stdout(predicate::str::contains("0 indexed · model not downloaded"))
        .stdout(predicate::str::contains("onebrain search reindex"));

    // 2. A new note → pending drift reported from Engine::status.
    std::fs::write(vault.path().join("00-inbox/note.md"), "# hello\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 indexed · 1 pending"));
}

/// Minimal mock daemon: a raw HTTP/1.1 server answering `GET /api/health`
/// (the `discover_matching` liveness probe) with `{}` and any other request
/// (`GET /api/internal/status`) with `status_body`. Same live-test-server
/// approach as `daemon_client`'s own handle tests, without needing a real
/// engine. The serving thread ends with the process.
#[cfg(unix)]
fn spawn_mock_daemon(status_body: &'static str) -> u16 {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain headers until the blank line.
            loop {
                let mut h = String::new();
                match reader.read_line(&mut h) {
                    Ok(0) => break,
                    Ok(_) if h == "\r\n" || h == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let body = if request_line.contains("/api/health") {
                "{}"
            } else {
                status_body
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    port
}

/// The per-vault slot stem (`daemon-<hash>`, #230) for a canonical vault path —
/// a replica of the CLI's `daemon_client::slot_stem` (`short_path_hash` =
/// sha256 of the path, first 6 hex chars). Replicated because the binary-only
/// crate exposes no lib for the integration test to call it directly.
#[cfg(unix)]
fn slot_stem_for(canon_vault: &str) -> String {
    use sha2::{Digest, Sha256};
    let full = format!("{:x}", Sha256::digest(canon_vault.as_bytes()));
    format!("daemon-{}", &full[..6])
}

/// Write a discovery record into `vault`'s SLOT (`daemon-<hash>.json`, #230)
/// under `home`, pointing at `port` and bound to the canonical `vault` at THIS
/// crate's version (`version_decision` requires an exact match with the spawned
/// binary, which shares the workspace version).
#[cfg(unix)]
fn write_daemon_record(home: &Path, vault: &Path, port: u16) {
    let run = home.join(".onebrain/run");
    std::fs::create_dir_all(&run).unwrap();
    let canon = std::fs::canonicalize(vault).unwrap();
    let canon = canon.display().to_string();
    std::fs::write(
        run.join(format!("{}.json", slot_stem_for(&canon))),
        serde_json::json!({
            "port": port,
            "token": "test-token",
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "vault": canon,
        })
        .to_string(),
    )
    .unwrap();
}

/// The doctor search check's warm-daemon path (#200): when a live daemon
/// serves THIS vault, the check's doc/pending counts come from the daemon's
/// `/api/internal/status` — not from a second engine open. A mock daemon
/// reports 42 docs / 6 pending on an index the direct path would read as
/// empty, so the counts appearing in the check message prove the routed
/// branch ran. `#[cfg(unix)]` because the isolation hinges on `$HOME`
/// steering `dirs::home_dir()` (same gate as the plugin-cache test).
#[cfg(unix)]
#[test]
fn doctor_search_check_reads_counts_from_matching_daemon() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    write_minimal_vault(vault.path());
    let cfg = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("search:\n  collection: doctor-it-daemon\n{existing}"),
    )
    .unwrap();
    // An existing cache dir (is_indexed) whose DIRECT read would report an
    // empty, never-reindexed index — so daemon counts are unmistakable.
    std::fs::create_dir_all(cache.path().join("search/doctor-it-daemon")).unwrap();

    let port = spawn_mock_daemon(
        r#"{"doc_count":42,"last_indexed":1700000000,"pending_new":4,"pending_changed":1,"pending_removed":1}"#,
    );
    write_daemon_record(home.path(), vault.path(), port);

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .env_remove("ONEBRAIN_NO_DAEMON")
        .args(["doctor", "--json"])
        .assert()
        .success(); // warn-only run — exit-code contract unchanged
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let search = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "search")
        .expect("search check present");
    let msg = search["message"].as_str().unwrap();
    assert!(
        msg.contains("42 indexed") && msg.contains("6 pending"),
        "daemon counts must drive the check message, got: {msg}"
    );
}

/// Fallback honesty: a SAME-vault daemon record whose daemon is gone (dead
/// port) must not poison the check — `discover_matching`'s liveness probe
/// fails, the check falls back to the direct engine open, and the message
/// reflects the on-disk (empty) index instead of any daemon counts.
#[cfg(unix)]
#[test]
fn doctor_search_check_falls_back_direct_when_daemon_unreachable() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    write_minimal_vault(vault.path());
    let cfg = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("search:\n  collection: doctor-it-dead-daemon\n{existing}"),
    )
    .unwrap();
    std::fs::create_dir_all(cache.path().join("search/doctor-it-dead-daemon")).unwrap();

    // A port that WAS bindable but is closed now — the liveness probe fails.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    write_daemon_record(home.path(), vault.path(), dead_port);

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .env_remove("ONEBRAIN_NO_DAEMON")
        .args(["doctor", "--json"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let search = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "search")
        .expect("search check present");
    let msg = search["message"].as_str().unwrap();
    assert!(
        !msg.contains("42 indexed"),
        "dead daemon must not contribute counts: {msg}"
    );
    assert!(
        msg.contains("0 indexed"),
        "direct probe of the empty index must drive the message: {msg}"
    );
}

/// The all-green path: index up to date AND the embedding model present (its
/// dir is fabricated — `model_download_status` only checks the `models--*`
/// dir exists, so no download is needed). The `search` row reports `ok`, the
/// footer shows the ✅ verdict with zero warnings, and a `--fix` run finds
/// nothing to do (the `issues.is_empty()` branch).
///
/// The reranker is enabled by default (Task 8), so its `models--*` dir is
/// fabricated here too — otherwise doctor would (correctly) warn that the
/// reranker model isn't downloaded and this wouldn't be an all-green fixture
/// anymore.
// `HOME` is overridden below to isolate the new `qmd-leftovers` doctor check
// (reads `dirs::home_dir()` / `.cache`, `.config`) from whatever's actually
// installed on the machine running the test — `dirs::home_dir()` only
// honors `$HOME` on unix (Windows resolves `%USERPROFILE%` instead), so this
// override — and the test — is unix-only.
#[cfg(unix)]
#[test]
fn doctor_all_green_and_fix_noop_with_fake_model_dir() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // All-green needs the CANONICAL config filename (a legacy vault.yml would
    // trip the vault-config-migration warn) and, since v3.4.8, a fully
    // self-documented config (uncommented keys are a config-values warn that
    // --fix repairs) — so use the commented template with the collection
    // placeholder activated.
    std::fs::remove_file(vault.path().join("vault.yml")).unwrap();
    let template = onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap();
    let config = template.replace(
        "  # collection: <set by onebrain search reindex>",
        "  collection: doctor-it-green",
    );
    assert!(config.contains("collection: doctor-it-green"), "{config}");
    std::fs::write(vault.path().join("onebrain.yml"), config).unwrap();

    // Fabricate the downloaded-model dirs BEFORE the reindex: with no model
    // dir on disk, reindex's missing-model reconcile would drop the
    // `search.embed_model` key via a comment-destroying serde rewrite
    // (tracked with the other structural writers in #200) and un-document
    // the commented fixture. Then run the empty-vault reindex (no docs →
    // no real download).
    std::fs::create_dir_all(
        cache
            .path()
            .join("search/doctor-it-green/models--intfloat--multilingual-e5-small"),
    )
    .unwrap();
    std::fs::create_dir_all(
        cache
            .path()
            .join("search/doctor-it-green/models--onebrain-ai--onebrain-rerank-v1"),
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        // Isolate `$PATH` too (not just `$HOME`) so the new `qmd-leftovers`
        // check can't find a real `qmd` binary on this machine.
        .env("PATH", "/usr/bin:/bin")
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["search", "reindex"])
        .assert()
        .success();

    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        // Isolate `$PATH` too (not just `$HOME`) so the new `qmd-leftovers`
        // check can't find a real `qmd` binary on this machine.
        .env("PATH", "/usr/bin:/bin")
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed · up to date"))
        // All-ok run collapses the Summary box to a single line.
        .stdout(predicate::str::contains("checks · all ok"));

    // All checks pass → --fix has nothing to do.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        // Isolate `$PATH` too (not just `$HOME`) so the new `qmd-leftovers`
        // check can't find a real `qmd` binary on this machine.
        .env("PATH", "/usr/bin:/bin")
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Nothing to fix — all checks pass.",
        ));
}

/// Structured `--fix --json` runs every recipe without prompting and reports
/// the outcomes in the `fix[]` array — here the legacy-qmd-collection
/// migration lands as `fixed`, and the post-fix re-check feeds the final
/// `checks` array (the legacy row flips to ok).
#[test]
fn doctor_fix_json_reports_legacy_qmd_collection_outcome() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());
    let cfg = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("qmd_collection: ob-json\n{existing}")).unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor --fix --json emits one JSON document");
    let fixes = doc["fix"].as_array().expect("fix[] present");
    let legacy = fixes
        .iter()
        .find(|f| f["check"] == "legacy-qmd-collection")
        .expect("legacy-qmd-collection outcome present");
    assert_eq!(legacy["outcome"], "fixed", "outcome: {legacy}");
    // Post-fix re-check: the legacy row is now ok.
    let checks = doc["checks"].as_array().expect("checks[] present");
    let row = checks
        .iter()
        .find(|c| c["check"] == "legacy-qmd-collection")
        .expect("legacy row present");
    assert_eq!(row["status"], "ok", "row: {row}");
}

/// Non-interactive safety: the structured `--fix --json` path (driven by the
/// `/doctor` skill and the scheduler) runs every recipe WITHOUT a
/// confirmation prompt — so the qmd-leftovers recipe must NEVER delete
/// anything there, even on a first encounter (no declined flag). The
/// leftovers must survive on disk, the outcome must be `manual`, and no
/// `stats.qmd_cleanup_declined` flag may be written (only an interactive
/// decline records that).
///
/// unix-only: the `HOME` override that isolates the fake `.cache/qmd` /
/// `.config/qmd` only affects `dirs::home_dir()` on unix.
#[cfg(unix)]
#[test]
fn doctor_fix_json_never_deletes_qmd_leftovers() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    // Vault with native search genuinely configured (real `search.collection`,
    // no legacy `qmd_collection`) → the qmd-leftovers check is gated IN.
    vault_with_config(
        vault.path(),
        &format!(
            "update_channel: stable\n\
             {FULL_FOLDERS_BLOCK}\
             search:\n  collection: doctor-it-qmd-json\n"
        ),
    );
    // Fresh qmd leftovers in the fake home (first encounter — no declined flag).
    let cache_dir = home.path().join(".cache/qmd");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join("index.sqlite"), vec![0u8; 128]).unwrap();
    let config_dir = home.path().join(".config/qmd");
    std::fs::create_dir_all(&config_dir).unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin") // no real qmd binary reachable
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["doctor", "--fix", "--json"])
        .assert();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor --fix --json emits one JSON document");

    // Outcome is manual — the recipe did not execute.
    let fixes = doc["fix"].as_array().expect("fix[] present");
    let qmd = fixes
        .iter()
        .find(|f| f["check"] == "qmd-leftovers")
        .expect("qmd-leftovers outcome present");
    assert_eq!(qmd["outcome"], "manual", "outcome: {qmd}");
    let msg = qmd["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("interactively") && msg.contains("rm -rf"),
        "actionable manual message: {msg}"
    );

    // BOTH leftover dirs survived on disk.
    assert!(
        cache_dir.is_dir() && cache_dir.join("index.sqlite").is_file(),
        "~/.cache/qmd must survive a --fix --json run"
    );
    assert!(
        config_dir.is_dir(),
        "~/.config/qmd must survive a --fix --json run"
    );

    // No declined flag was recorded — only an interactive decline writes it.
    let cfg = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(
        !cfg.contains("qmd_cleanup_declined"),
        "non-interactive run must not write the declined flag:\n{cfg}"
    );
}

/// Piped/non-TTY TEXT-mode safety (the reviewer's exact repro:
/// `doctor --fix </dev/null | cat`, i.e. cron command-mode / scripts):
/// `confirm_fix` deliberately auto-proceeds when stdin/stdout aren't TTYs
/// (pre-3.2.4 automation compat, unchanged for vault-scoped recipes) — but
/// that auto-proceed is NOT a real confirmation, so the qmd-leftovers
/// destructive branch must stay locked. Spawning the binary under the test
/// harness gives exactly this shape: stdin is null, stdout is piped.
///
/// unix-only for the same `HOME`-override reason as the json variant above.
#[cfg(unix)]
#[test]
fn doctor_fix_piped_text_mode_never_deletes_qmd_leftovers() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    vault_with_config(
        vault.path(),
        &format!(
            "update_channel: stable\n\
             {FULL_FOLDERS_BLOCK}\
             search:\n  collection: doctor-it-qmd-piped\n"
        ),
    );
    // Fresh qmd leftovers (first encounter — no declined flag).
    let cache_dir = home.path().join(".cache/qmd");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join("index.sqlite"), vec![0u8; 128]).unwrap();
    let config_dir = home.path().join(".config/qmd");
    std::fs::create_dir_all(&config_dir).unwrap();

    // Plain text-mode --fix: no --json, no --yes. Under the harness stdin is
    // null and stdout is piped — the auto-proceed route.
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin") // no real qmd binary reachable
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["doctor", "--fix"])
        .assert();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();

    // The manual guidance is in the output — the recipe reported instead of
    // executing.
    assert!(
        stdout.contains("interactively") && stdout.contains("rm -rf"),
        "piped --fix must print the manual guidance, got:\n{stdout}"
    );

    // BOTH leftover dirs survived on disk.
    assert!(
        cache_dir.is_dir() && cache_dir.join("index.sqlite").is_file(),
        "~/.cache/qmd must survive a piped text-mode --fix run"
    );
    assert!(
        config_dir.is_dir(),
        "~/.config/qmd must survive a piped text-mode --fix run"
    );

    // No declined flag was recorded — auto-proceed is neither a confirmation
    // nor a decline.
    let cfg = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(
        !cfg.contains("qmd_cleanup_declined"),
        "piped run must not write the declined flag:\n{cfg}"
    );
}

#[test]
fn doctor_missing_folder_exits_1() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    std::fs::remove_dir_all(d.path().join("01-projects")).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .code(1)
        // v3.4.8 grouped layout: fail glyph `✗` on the folders row (the folders
        // detail no longer renders inline — it stays in the JSON payload); the
        // missing folder also surfaces as a fail finding in the Summary box.
        .stdout(predicate::str::contains("\u{2717} folders"))
        .stdout(predicate::str::contains("7/8 present"));
}

#[test]
fn doctor_missing_vault_yml_errors_out() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not inside a vault"));
}

/// Structured mode (`--json`) outside a vault emits a JSON error envelope
/// on stdout (not stderr anyhow text) so scripts can parse it.
/// Covers the `want_structured=true` not-in-vault early return in `run()`.
#[test]
fn doctor_json_mode_not_in_vault_emits_json_error_envelope() {
    let d = tempdir().unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["doctor", "--json"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("structured mode must emit JSON on stdout");
    assert_eq!(doc["ok"], false, "ok must be false: {doc}");
    assert_eq!(doc["error"], "not_in_vault", "error field: {doc}");
}

/// v3.4: `doctor --fix` on a vault carrying the deprecated top-level
/// `qmd_collection` key migrates it to `search.collection` and removes the
/// legacy key. End-to-end proof of the `legacy-qmd-collection` check +
/// migration recipe (the old qmd-embeddings `qmd embed` recipe is gone).
/// PATH is scrubbed so no real qmd binary is ever consulted.
#[test]
fn doctor_fix_migrates_legacy_qmd_collection() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Add the deprecated key to the (legacy-named) config the fixture wrote.
    let cfg = d.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("qmd_collection: ob-legacy\n{existing}")).unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // The fix summary must appear (proves the fix pass ran). Non-TTY runs
    // (like this one) auto-proceed past the confirmation prompt.
    assert!(
        stdout.contains("Fix summary:"),
        "expected the fix summary in stdout · got: {stdout}"
    );
    // No qmd-embed recipe exists anymore — it must never spawn.
    assert!(
        !stdout.contains("running: qmd embed"),
        "qmd embed recipe should be gone · got: {stdout}"
    );

    // The config (migrated to canonical onebrain.yml by the config-migration
    // recipe) has the legacy key removed and search.collection seeded.
    let after =
        std::fs::read_to_string(d.path().join("onebrain.yml")).expect("config present after --fix");
    assert!(
        !after.contains("qmd_collection"),
        "legacy qmd_collection must be removed · got:\n{after}"
    );
    assert!(
        after.contains("collection: ob-legacy"),
        "value must be migrated to search.collection · got:\n{after}"
    );
}

#[test]
fn doctor_invalid_yaml_falls_back_to_defaults() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("vault.yml"), "not: : valid").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains("invalid YAML"));
}

#[test]
fn doctor_orphan_checkpoints_warns_without_failing() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let cp = d.path().join("07-logs/checkpoint");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success() // warning-only
        .stdout(predicate::str::contains("1 unmerged"));
}

#[test]
fn doctor_stale_marketplace_warns() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Override settings.json with stale marketplace block (keep the canonical
    // Stop hook + permission so `settings-hooks` still reports ok).
    std::fs::write(
        d.path().join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]},"extraKnownMarketplaces":{"onebrain":{"source":{"repo":"kengio/onebrain"}}}}"#,
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success() // warn-only
        .stdout(predicate::str::contains("stale marketplace repo"));
}

/// Regression — `onebrain doctor --vault PATH` from outside the vault must
/// scan the supplied PATH, not the cwd. The original v3.0 / early-v3.1
/// implementation used `find_vault_root(cwd)` only, so passing `--vault`
/// from anywhere except inside the vault produced
/// `{"error":"not_in_vault",...}`. Vault resolution now goes through the
/// canonical chain (flag > env > walk-up) shared with `vault current`.
#[test]
fn doctor_honors_vault_flag() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // Deliberately run from a DIFFERENT directory that has no vault — if
    // the flag isn't honoured, walk-up fails and the smoke-test envelope
    // (`error: not_in_vault`) is what we'll see.
    let elsewhere = tempdir().unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        .args(["doctor", "--vault"])
        .arg(vault.path())
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor must emit a JSON envelope on stdout");
    // Smoke-test failure mode emitted `{"ok":false,"error":"not_in_vault",...}`
    // with no `checks` array — guard against that shape here.
    assert!(
        doc.get("error").is_none(),
        "must not be the not_in_vault envelope · got: {doc}"
    );
    assert!(
        doc.get("checks").is_some(),
        "must include the checks array · got: {doc}"
    );
    // The config check (named `onebrain.yml`) should report ok against the
    // minimal fixture — even though the fixture writes the legacy `vault.yml`
    // filename, the check reads it via fallback and reports the canonical name.
    let checks = doc["checks"].as_array().expect("checks is array");
    let vault_yml = checks
        .iter()
        .find(|c| c["check"] == "onebrain.yml")
        .expect("onebrain.yml check must be present");
    assert_eq!(vault_yml["status"], "ok", "onebrain.yml check should be ok");
}

/// Regression — `onebrain doctor --fix --vault PATH` must run the
/// `vault-config-migration` recipe against the supplied PATH (not cwd).
/// The fixture writes legacy `vault.yml`; after `--fix` the canonical
/// `onebrain.yml` should exist with the same content and `vault.yml`
/// should be gone.
#[test]
fn doctor_fix_migrates_vault_yml_with_vault_flag() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // Sanity: fixture writes legacy filename.
    assert!(vault.path().join("vault.yml").is_file());
    assert!(!vault.path().join("onebrain.yml").exists());
    let original = std::fs::read_to_string(vault.path().join("vault.yml")).unwrap();
    let elsewhere = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        // Minimal, deterministic PATH so `doctor --fix` runs against a
        // predictable environment (no developer-specific binaries leak in).
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--vault"])
        .arg(vault.path())
        .assert()
        // `--fix` exit code mirrors post-fix check status; the minimal
        // fixture has no Error checks, so it returns 0.
        .success();
    // Migration recipe should have renamed legacy → canonical.
    assert!(
        vault.path().join("onebrain.yml").is_file(),
        "expected onebrain.yml at {}",
        vault.path().display()
    );
    assert!(
        !vault.path().join("vault.yml").exists(),
        "expected vault.yml to be gone after --fix"
    );
    let after = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    // The migration preserves every original value line (v3.4.8's --fix also
    // backfills self-documentation comments and stamps `stats:`, so assert on
    // the non-comment lines rather than a strict byte prefix).
    let value_lines = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    };
    let after_values = value_lines(&after);
    for line in value_lines(&original) {
        assert!(
            after_values.contains(&line),
            "migrated config must keep original line {line:?} · got:\n{after}"
        );
    }
    assert!(
        after.contains("stats:")
            && after.contains("last_doctor_run:")
            && after.contains("last_doctor_fix:"),
        "doctor --fix must stamp last_doctor_run + last_doctor_fix · got:\n{after}"
    );
}

/// Regression — when `doctor --fix` runs both the `vault-config-migration`
/// recipe AND the `plugin-files` recipe (because the vault is missing
/// plugin files), vault-sync's "Step 7 update_vault_yml" must not
/// resurrect a legacy `vault.yml` after migration renamed it away.
///
/// Pre-fix bug: the recipes ran in declaration order — migration renamed
/// `vault.yml` → `onebrain.yml`, then plugin-files' vault-sync wrote
/// `update_channel` into a hardcoded `vault.yml` path, leaving BOTH files
/// at vault root.
///
/// Skipped on hosts without network — vault-sync downloads the upstream
/// plugin tarball. The repro runs locally via the worktree's debug binary
/// and CI has full internet.
#[test]
#[ignore = "requires network for vault-sync plugin tarball download"]
fn doctor_fix_does_not_resurrect_vault_yml_after_migration() {
    let vault = tempdir().unwrap();
    // Bare-bones legacy vault: no plugin files (forces plugin-files recipe
    // to spawn vault-sync), legacy vault.yml present (forces migration
    // recipe to run).
    std::fs::write(
        vault.path().join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
        ".claude",
    ] {
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }

    let _ = Command::cargo_bin("onebrain")
        .unwrap()
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--vault"])
        .arg(vault.path())
        .assert();

    assert!(
        vault.path().join("onebrain.yml").is_file(),
        "expected onebrain.yml at {}",
        vault.path().display()
    );
    assert!(
        !vault.path().join("vault.yml").exists(),
        "REGRESSION — vault-sync resurrected vault.yml after migration recipe \
         renamed it away. Step 7 must write to onebrain.yml when canonical \
         is present."
    );
}

/// Text-mode `--fix` with a vault that has only manual issues (the native
/// `search` "no index yet" warning is the one manual-only warning the minimal
/// canonical vault always has). Must not print "Will apply" (no
/// auto-fixable issues) and should confirm via the "manual step" path.
/// The `issues.is_empty()` branch requires a vault with ZERO Warn/Error
/// results — only possible once the search index has been built, which needs a
/// reindex (model download). We cover the manual-only path here and leave the
/// all-clean branch as residual (requires a real index).
#[test]
fn doctor_fix_text_mode_manual_issues_shows_manual_step_section() {
    let vault = tempdir().unwrap();
    // Write canonical onebrain.yml (no migration warning). Since v3.4.8 an
    // UNCOMMENTED config is itself an auto-fixable finding (comment
    // backfill), so this manual-only scenario needs the fully-documented
    // template.
    std::fs::write(
        vault.path().join("onebrain.yml"),
        onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap(),
    )
    .unwrap();
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
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }
    let plugin = vault.path().join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = vault.path().join(".claude");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // The native `search` warning is manual-only: "Nothing to auto-fix" appears.
    assert!(
        stdout.contains("Nothing to auto-fix") || stdout.contains("manual step"),
        "expected manual-only message · got: {stdout}"
    );
    // No auto-fix recipes were applied.
    assert!(
        !stdout.contains("Will apply"),
        "must not show auto-fix plan when only manual issues: {stdout}"
    );
}

/// Text-mode `--fix` with vault that has mixed auto+manual issues. The vault
/// uses legacy vault.yml (auto-fixable via migration) PLUS an orphan
/// checkpoint (manual-only). Both the auto plan AND manual step sections
/// must appear in the output, and the auto fix must apply.
#[test]
fn doctor_fix_text_mode_mixed_auto_and_manual_issues() {
    let vault = tempdir().unwrap();
    // Legacy vault.yml → auto-fixable (vault-config-migration recipe).
    std::fs::write(
        vault.path().join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
    )
    .unwrap();
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
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }
    let plugin = vault.path().join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = vault.path().join(".claude");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
    // Add an orphan checkpoint — manual-only issue.
    let cp = vault.path().join("07-logs/checkpoint");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // Auto plan must be previewed.
    assert!(
        stdout.contains("Will apply") || stdout.contains("automated fix"),
        "expected auto-fix plan · got: {stdout}"
    );
    // Manual section must also appear.
    assert!(
        stdout.contains("manual step") || stdout.contains("wrapup"),
        "expected manual section · got: {stdout}"
    );
    // The fix summary must appear (proves auto recipes ran).
    assert!(
        stdout.contains("Fix summary:"),
        "expected Fix summary · got: {stdout}"
    );
}

/// Structured (`--fix --json`) mode with a vault that has fixable issues.
/// Must emit a single JSON document with `fix[]` array containing outcomes,
/// not text output.
#[test]
fn doctor_fix_json_mode_emits_fix_array_with_outcomes() {
    let vault = tempdir().unwrap();
    // Write legacy vault.yml (triggers vault-config-migration which is auto-fixable).
    std::fs::write(
        vault.path().join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
    )
    .unwrap();
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
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }
    let plugin = vault.path().join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = vault.path().join(".claude");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("expected JSON output · error: {e} · stdout: {stdout}"));
    // `fix[]` must be present (--fix was requested).
    assert!(doc.get("fix").is_some(), "fix key must be present: {doc}");
    let fix_arr = doc["fix"].as_array().expect("fix is array");
    // The migration recipe ran → at least one entry.
    assert!(!fix_arr.is_empty(), "fix array must have entries: {doc}");
    // Each entry has check + outcome + message.
    for entry in fix_arr {
        assert!(entry.get("check").is_some(), "entry missing check: {entry}");
        assert!(
            entry.get("outcome").is_some(),
            "entry missing outcome: {entry}"
        );
    }
}

/// CRITICAL data-safety regression: `doctor --fix` must NEVER lose config
/// keys. The vault carries a legacy `vault.yml` holding `qmd_collection`, a
/// custom key, and USER COMMENTS, but MISSING `update_channel`, so the
/// `vault-config-migration` rename, the `legacy-qmd-collection` migration, AND
/// the `onebrain.yml-keys` backfill all fire. After --fix the config lives at
/// canonical `onebrain.yml`; the deprecated `qmd_collection` is migrated to
/// `search.collection` and its old key removed (v3.4); the unknown custom key
/// AND every user comment survive the multi-recipe pass — the v3.4.8
/// end-to-end proof that recipe ORDERING no longer destroys comments (#200);
/// the missing `update_channel` is backfilled; and a timestamped backup was
/// written before any destructive write.
#[test]
fn doctor_fix_preserves_custom_keys_and_backs_up() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path()); // folders + plugin files + (complete) vault.yml
                                       // Replace the config: keep all folders, add qmd_collection + a custom key
                                       // + user comments, drop update_channel so the keys-backfill recipe has
                                       // work to do.
                                       // NOTE: the user comments sit above keys that SURVIVE the pass. A comment
                                       // directly above `qmd_collection` itself would leave with the deleted key —
                                       // that's delete_key's pinned lead-comment design, covered by the unit test
                                       // `fix_legacy_qmd_collection_preserves_comments_exactly_and_is_idempotent`.
    std::fs::write(
        vault.path().join("vault.yml"),
        "# hand-tuned by me — keep this header\n\
         custom_key: keepme  # my note on the custom key\n\
         qmd_collection: ob-1-441565\n\
         folders:\n  \
           # inbox is sacred\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    let elsewhere = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        .env("PATH", "/usr/bin:/bin") // scrub qmd so the probe degrades
        .args(["doctor", "--fix", "--vault"])
        .arg(vault.path())
        .assert()
        .success();

    let after = std::fs::read_to_string(vault.path().join("onebrain.yml"))
        .expect("config migrated to canonical onebrain.yml");
    // v3.4: the deprecated qmd_collection is migrated away, not preserved.
    assert!(
        !after.contains("qmd_collection"),
        "legacy qmd_collection must be removed by --fix · got:\n{after}"
    );
    assert!(
        after.contains("collection: ob-1-441565"),
        "qmd_collection value must be migrated to search.collection · got:\n{after}"
    );
    assert!(
        after.contains("custom_key: keepme"),
        "unknown user keys must survive --fix · got:\n{after}"
    );
    assert!(
        after.contains("update_channel"),
        "missing required key should be backfilled (onebrain.yml-keys recipe ran) · got:\n{after}"
    );
    // v3.4.8 (#200): EVERY user comment survives the full multi-recipe --fix
    // pass — key backfill (runs FIRST), qmd migration, comment backfill, and
    // layout restructure are all comment-preserving, so order can't matter.
    for comment in [
        "# hand-tuned by me — keep this header",
        "# my note on the custom key",
        "# inbox is sacred",
    ] {
        assert!(
            after.contains(comment),
            "user comment {comment:?} must survive the full --fix pass · got:\n{after}"
        );
    }
    // A backup was taken before the destructive (rename + re-serialize) writes.
    let backups = vault.path().join(".onebrain-backups");
    assert!(backups.is_dir(), "expected .onebrain-backups/ to exist");
    let count = std::fs::read_dir(&backups).unwrap().count();
    assert!(count >= 1, "expected at least one timestamped backup");
}

/// Safety + coverage for the `plugin-cache` `--fix` recipe (`fix_plugin_cache`).
/// `$HOME` is pinned to a tempdir so the destructive cache sweep operates on a
/// synthetic home and can NEVER touch the real developer cache — the inline
/// unit test this replaces called `fix_plugin_cache` directly against the live
/// `$HOME` and could delete real `~/.claude/plugins/cache` entries on every
/// `cargo test`. A stale orphan version dir is planted under the fake cache;
/// after `--fix` the recipe must report `fixed` and the orphan must be gone.
///
/// `#[cfg(unix)]`: the isolation hinges on `$HOME` steering
/// `dirs::home_dir()`, which only holds on unix. On Windows `home_dir()` reads
/// the profile known-folder (not the env), so the fake-home redirect would not
/// take and the recipe would resolve the real home — gate to the platforms
/// where the sweep provably can't escape the tempdir.
#[cfg(unix)]
#[test]
fn doctor_fix_prunes_stale_plugin_cache_under_fake_home() {
    let home = tempdir().unwrap();
    // Any version dir under `<cache>/<marketplace>/onebrain/` is a prunable
    // orphan (the active plugin is the vault-local pin, never a cache copy).
    let stale = home
        .path()
        .join(".claude/plugins/cache/test-marketplace/onebrain/2.2.4");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("plugin.json"), "{}").unwrap();
    // Registry present but empty — the unconditional `<cache>/*/onebrain/` glob
    // still discovers the orphan under the unregistered marketplace.
    std::fs::write(
        home.path().join(".claude/plugins/installed_plugins.json"),
        r#"{"plugins":{}}"#,
    )
    .unwrap();

    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("expected JSON · error: {e} · stdout: {stdout}"));
    let fix_arr = doc["fix"].as_array().expect("fix is array");
    let pc = fix_arr
        .iter()
        .find(|e| e["check"] == "plugin-cache")
        .unwrap_or_else(|| panic!("no plugin-cache fix entry · fix: {fix_arr:?}"));
    assert_eq!(
        pc["outcome"], "fixed",
        "plugin-cache recipe must report fixed · entry: {pc}"
    );
    // The sweep actually removed the orphan — proves the destructive path ran
    // against the fake home, not just that a finding was reported.
    assert!(
        !stale.exists(),
        "stale cache dir must be pruned by --fix: {}",
        stale.display()
    );
}

// ─────────────────────────────────────────────────────────────────────────
// config-values check + --fix reset-to-default (v3.4.8, #196)
// ─────────────────────────────────────────────────────────────────────────

/// All 8 folder keys present so `onebrain.yml-keys` stays quiet and the
/// assertions isolate the new `config-values` check.
const FULL_FOLDERS_BLOCK: &str = "folders:\n  \
       inbox: 00-inbox\n  \
       projects: 01-projects\n  \
       areas: 02-areas\n  \
       knowledge: 03-knowledge\n  \
       resources: 04-resources\n  \
       agent: 05-agent\n  \
       archive: 06-archive\n  \
       logs: 07-logs\n";

/// Like `write_minimal_vault`, but writes the given text to the canonical
/// `onebrain.yml` (no legacy `vault.yml`) so config-value tests control the
/// exact file content doctor sees.
fn vault_with_config(dir: &Path, config: &str) {
    write_minimal_vault(dir);
    std::fs::remove_file(dir.join("vault.yml")).unwrap();
    std::fs::write(dir.join("onebrain.yml"), config).unwrap();
}

/// Run `doctor --json` (no fix) and return raw stdout. `ONEBRAIN_CACHE_DIR`
/// is pointed at a tempdir so the search check never touches the real cache.
fn run_doctor_json(dir: &Path, cache: &Path) -> String {
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir)
        .env("ONEBRAIN_CACHE_DIR", cache)
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--json"])
        .assert();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default()
}

#[test]
fn doctor_flags_out_of_range_config_values() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    vault_with_config(
        d.path(),
        &format!(
            "update_channel: weekly-maybe\n\
             {FULL_FOLDERS_BLOCK}\
             checkpoint:\n  \
               messages: 0\n\
             search:\n  \
               default_top_k: 0\n  \
               embed_model: not-a-model\n  \
               reranker:\n    \
                 min_candidates: 0\n    \
                 min_score: 7.5\n"
        ),
    );
    let out = run_doctor_json(d.path(), cache.path());
    for needle in [
        "update_channel",
        "checkpoint.messages",
        "search.default_top_k",
        "search.embed_model",
        "reranker.min_candidates",
        "reranker.min_score",
    ] {
        assert!(out.contains(needle), "missing finding for {needle}:\n{out}");
    }
    // The findings live on the new `config-values` check row.
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = doc["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["check"] == "config-values")
        .expect("config-values row present")
        .clone();
    assert_eq!(row["status"], "warn", "row: {row}");
    // Every finding names the documented default it would reset to.
    let details = row["details"].as_array().expect("details[]");
    assert!(
        details
            .iter()
            .filter(|v| {
                let s = v.as_str().unwrap_or("");
                // Report-only findings, the self-documentation summary line,
                // and the layout-drift line are not value resets.
                !s.contains("never auto-reset")
                    && !s.contains("lack self-documentation")
                    && !s.contains("layout differs")
            })
            .all(|v| v.as_str().unwrap_or("").contains("default:")),
        "each resettable finding must state its default: {details:?}"
    );
}

#[test]
fn doctor_config_values_in_range_reports_ok() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    // Every key carries a (user) comment so the assertion isolates VALUE
    // validation from the undocumented-keys warn. The fixture is passed
    // through the shared restructure so it is already in canonical section
    // layout — otherwise the missing banners would (correctly) report layout
    // drift and this VALUE-only assertion would see a warn.
    let raw = "# c\nupdate_channel: next\n\
         folders:\n  \
           # c\n  \
           inbox: 00-inbox\n  \
           # c\n  \
           projects: 01-projects\n  \
           # c\n  \
           areas: 02-areas\n  \
           # c\n  \
           knowledge: 03-knowledge\n  \
           # c\n  \
           resources: 04-resources\n  \
           # c\n  \
           agent: 05-agent\n  \
           # c\n  \
           archive: 06-archive\n  \
           # c\n  \
           logs: 07-logs\n\
         checkpoint:\n  \
           # c\n  \
           messages: 20\n  \
           # c\n  \
           minutes: 45\n\
         search:\n  \
           # c\n  \
           default_top_k: 25\n  \
           reranker:\n    \
             # c\n    \
             min_score: 0.5\n";
    vault_with_config(
        d.path(),
        &onebrain_fs::restructure_config(raw).expect("fixture is a mapping"),
    );
    let out = run_doctor_json(d.path(), cache.path());
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = doc["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["check"] == "config-values")
        .expect("config-values row present")
        .clone();
    assert_eq!(row["status"], "ok", "in-range values must pass: {row}");
}

#[test]
fn doctor_flags_empty_folder_value_as_report_only() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    vault_with_config(
        d.path(),
        "update_channel: stable\n\
         folders:\n  \
           inbox: \"\"\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    );
    let out = run_doctor_json(d.path(), cache.path());
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = doc["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["check"] == "config-values")
        .expect("config-values row present")
        .clone();
    assert_eq!(row["status"], "warn", "row: {row}");
    let details = row["details"].as_array().expect("details[]").clone();
    let folder_finding = details
        .iter()
        .filter_map(|v| v.as_str())
        .find(|s| s.contains("folders.inbox"))
        .expect("folders.inbox finding present");
    assert!(
        folder_finding.contains("never auto-reset"),
        "folders findings are report-only: {folder_finding}"
    );
}

/// Run `doctor --fix --json` under a fake HOME (so home-based recipes like
/// plugin-cache can't touch the real machine) and return raw stdout.
#[cfg(unix)]
fn run_doctor_fix_json(dir: &Path, cache: &Path, home: &Path) -> String {
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir)
        .env("HOME", home)
        .env("ONEBRAIN_CACHE_DIR", cache)
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default()
}

#[cfg(unix)]
#[test]
fn doctor_fix_resets_tunables_but_never_folders() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    vault_with_config(
        d.path(),
        "# my precious comment\n\
         update_channel: stable\n\
         folders:\n  \
           inbox: my-custom-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n\
         checkpoint:\n  \
           # keep my checkpoint block comment too\n  \
           messages: 0\n\
         search:\n  \
           reranker:\n    \
             min_score: 7.5\n",
    );
    // The customised inbox folder exists on disk so the `folders` check
    // (and its mkdir recipe) stays quiet.
    std::fs::create_dir_all(d.path().join("my-custom-inbox")).unwrap();

    let out = run_doctor_fix_json(d.path(), cache.path(), home.path());
    let cfg = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(
        cfg.contains("# my precious comment"),
        "comments must survive:\n{cfg}"
    );
    assert!(
        cfg.contains("# keep my checkpoint block comment too"),
        "nested comments must survive:\n{cfg}"
    );
    assert!(cfg.contains("messages: 15"), "{cfg}");
    assert!(!cfg.contains("min_score: 7.5"), "{cfg}");
    assert!(
        cfg.contains("inbox: my-custom-inbox"),
        "folders NEVER auto-reset:\n{cfg}"
    );
    assert!(
        out.contains("reset"),
        "fix output must report resets:\n{out}"
    );
    // Post-fix re-check: the config-values row is now clean.
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = doc["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["check"] == "config-values")
        .expect("config-values row present")
        .clone();
    assert_eq!(row["status"], "ok", "post-fix re-check must pass: {row}");
}

#[cfg(unix)]
#[test]
fn doctor_fix_embed_model_reset_warns_reindex_required() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    vault_with_config(
        d.path(),
        &format!(
            "update_channel: stable\n\
             {FULL_FOLDERS_BLOCK}\
             search:\n  \
               embed_model: not-a-model\n"
        ),
    );
    let out = run_doctor_fix_json(d.path(), cache.path(), home.path());
    let cfg = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(
        cfg.contains("embed_model: multilingual-e5-small"),
        "embed_model must reset to the registry default:\n{cfg}"
    );
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let fix = doc["fix"]
        .as_array()
        .expect("fix[]")
        .iter()
        .find(|f| f["check"] == "config-values")
        .expect("config-values fix entry")
        .clone();
    assert_eq!(fix["outcome"], "fixed", "{fix}");
    let msg = fix["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("reindex"),
        "embed_model reset must warn that a reindex is required: {msg}"
    );
}

/// The epic's core promise end-to-end: a plain `onebrain doctor` run on a
/// freshly scaffolded (commented) config never strips the comments — the
/// stats stamp is a comment-preserving line edit and the search check
/// resolves its collection read-only.
#[test]
fn doctor_never_strips_template_comments() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_minimal_vault(d.path());
    std::fs::remove_file(d.path().join("vault.yml")).unwrap();
    let template = onebrain_fs::render_onebrain_yml(onebrain_fs::SchedulePreset::Skip).unwrap();
    std::fs::write(d.path().join("onebrain.yml"), &template).unwrap();

    let out = run_doctor_json(d.path(), cache.path());
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    assert_eq!(doc["ok"], true, "template config must be healthy: {out}");

    let after = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(
        after.starts_with("# onebrain.yml"),
        "header comment must survive a doctor run:\n{after}"
    );
    let comment_lines = |s: &str| {
        s.lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .count()
    };
    // No template comment is lost; the stamp adds exactly the canonical stats
    // section's two structural comments (System banner + managed note).
    assert_eq!(
        comment_lines(&after),
        comment_lines(&template) + 2,
        "only the System banner + managed note may be added:\n{after}"
    );
    for tmpl_comment in template.lines().filter(|l| l.trim_start().starts_with('#')) {
        assert!(
            after.contains(tmpl_comment),
            "template comment lost: {tmpl_comment:?}\n{after}"
        );
    }
    // The stats stamp lands in a canonical, system-managed section.
    assert!(
        after.contains(&onebrain_fs::config_layout::section_banner("System")),
        "{after}"
    );
    assert!(after.contains(onebrain_fs::SYSTEM_MANAGED_NOTE), "{after}");
    assert!(after.contains("stats:"), "{after}");
    assert!(after.contains("last_doctor_run:"), "{after}");
    // The stamped config is now itself canonical — a second doctor run reports
    // no layout drift (idempotent), matching a freshly-init'd vault's steady
    // state.
    assert!(
        onebrain_fs::config_layout_matches(&after),
        "stamped config must stay canonical:\n{after}"
    );
}

/// End-to-end T2b: `doctor --fix` restructures an EXISTING vault whose config
/// has the pre-v3.4.8 layout — stats mid-file, no banners, recap undocumented,
/// schedule before search (the real-vault shape after #199's backfill). The
/// restructure reorders the top-level blocks into template section order,
/// inserts the banners, backfills the recap docs, and moves stats last —
/// preserving every value and comment byte-for-byte. A second `--fix` is a
/// no-op (byte-identical).
#[cfg(unix)]
#[test]
fn doctor_fix_restructures_existing_vault_end_to_end() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    write_minimal_vault(d.path());
    std::fs::remove_file(d.path().join("vault.yml")).unwrap();
    // Pre-v3.4.8 layout: documented folders/checkpoint/search (from #199),
    // UNDOCUMENTED recap, stats stranded mid-file, schedule before search.
    let legacy = "\
# Release channel for plugin updates: stable | next · default: stable
update_channel: stable
folders:
  # Raw braindumps and quick captures · default: 00-inbox
  inbox: 00-inbox
  projects: 01-projects
  areas: 02-areas
  knowledge: 03-knowledge
  resources: 04-resources
  agent: 05-agent
  archive: 06-archive
  logs: 07-logs
checkpoint:
  # Message count between checkpoint emissions (>= 1) · default: 15
  messages: 15
  minutes: 30
recap:
  min_sessions: 6
  min_frequency: 2
stats:
  last_doctor_run: 2020-01-01
  last_recap: 2020-01-01
schedule:
- cron: 0 9 * * *
  skill: /daily
- cron: 45 8 * * *
  command: onebrain
  args:
  - search
  - reindex
search:
  # Collection name binding this vault to its index · default: unset
  collection: ob-restructure-it
  embed_model: multilingual-e5-small
";
    std::fs::write(d.path().join("onebrain.yml"), legacy).unwrap();
    // Plain doctor (read-only) must REPORT the drift without rewriting the
    // user's layout.
    let plain = run_doctor_json(d.path(), cache.path());
    assert!(plain.contains("layout differs from template"), "{plain}");
    let after_plain = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    // A plain run only stamps stats in place — it never restructures a legacy
    // layout, so the block order is still the scrambled original.
    let plain_keys: Vec<&str> = after_plain
        .lines()
        .filter(|l| {
            !l.starts_with(' ')
                && !l.starts_with('-')
                && !l.trim_start().starts_with('#')
                && l.contains(':')
        })
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(
        plain_keys,
        vec![
            "update_channel",
            "folders",
            "checkpoint",
            "recap",
            "stats",
            "schedule",
            "search"
        ],
        "plain doctor must not reorder:\n{after_plain}"
    );

    // --fix restructures.
    let _ = run_doctor_fix_json(d.path(), cache.path(), home.path());
    let fixed = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();

    // Banners for every present section. `Token optimization` is now
    // present too (issue #247): a legacy vault predating v3.4.10 has no
    // `token_optimization` block at all, so `--fix` backfills it alongside
    // the restructure.
    for title in [
        "General",
        "Vault layout",
        "Agent behavior",
        "Search",
        "Token optimization",
        "Automation",
        "System",
    ] {
        assert!(
            fixed.contains(&onebrain_fs::config_layout::section_banner(title)),
            "missing {title} banner:\n{fixed}"
        );
    }
    // System banner carries the managed note; stats is LAST.
    assert!(fixed.contains(onebrain_fs::SYSTEM_MANAGED_NOTE), "{fixed}");
    let key_order: Vec<&str> = fixed
        .lines()
        .filter(|l| {
            !l.starts_with(' ')
                && !l.starts_with('-')
                && !l.trim_start().starts_with('#')
                && l.contains(':')
        })
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(
        key_order,
        vec![
            "update_channel",
            "folders",
            "checkpoint",
            "recap",
            "search",
            "token_optimization",
            "schedule",
            "stats"
        ],
        "restructured order:\n{fixed}"
    );
    // token_optimization is backfilled too (issue #247), with its own
    // documented defaults — byte-identical to what `init` emits.
    assert!(
        fixed.contains(&onebrain_fs::token_optimization_block_lines().join("\n")),
        "{fixed}"
    );
    // recap keys are now documented with the verified plugin defaults.
    let lines: Vec<&str> = fixed.lines().collect();
    for (key, needle) in [
        ("min_sessions:", "default: 6"),
        ("min_frequency:", "default: 2"),
    ] {
        let idx = lines
            .iter()
            .position(|l| l.trim_start().starts_with(key))
            .unwrap_or_else(|| panic!("{key} missing:\n{fixed}"));
        assert!(
            lines[idx - 1].trim_start().starts_with('#') && lines[idx - 1].contains(needle),
            "recap doc for {key} missing ({needle}):\n{fixed}"
        );
    }
    // The schedule block gained the template's entry-shape header (directly
    // above `schedule:`, under the Automation banner) — never per-entry
    // comments.
    let sched_idx = lines
        .iter()
        .position(|l| *l == "schedule:")
        .unwrap_or_else(|| panic!("schedule: missing:\n{fixed}"));
    assert!(
        lines[sched_idx - 1].contains("`command` + `args` (any CLI)"),
        "schedule entry-shape header missing:\n{fixed}"
    );
    // Values + user/existing comments preserved verbatim.
    assert!(fixed.contains("collection: ob-restructure-it"), "{fixed}");
    assert!(
        fixed.contains("# Raw braindumps and quick captures · default: 00-inbox"),
        "{fixed}"
    );
    // The schedule list body (skill entry + command/args entry) is intact.
    assert!(
        fixed.contains("- cron: 0 9 * * *\n  skill: /daily"),
        "{fixed}"
    );
    assert!(
        fixed.contains("  args:\n  - search\n  - reindex"),
        "{fixed}"
    );

    // Second --fix is byte-identical (idempotent).
    let _ = run_doctor_fix_json(d.path(), cache.path(), home.path());
    let twice = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert_eq!(twice, fixed, "second --fix must be byte-identical");
}

/// Regression (PR #199 review R1-4a): a vault whose index dir EXISTS while
/// `search.collection` is absent used to get silently rewritten by a plain
/// doctor run — `open_engine` → `collection_for` persisted the generated
/// name via a comment-destroying whole-file serde rewrite. The engine path
/// must now be strictly read-only.
///
/// Two-phase, platform-proof design: the generated collection name is NOT
/// recomputed in the test (an earlier version hashed the test's own
/// `canonicalize()` output, which diverges from the runtime's resolved path
/// on Windows — `\\?\` verbatim prefixes — and behind macOS `/var` symlinks).
/// Instead, phase 1 runs the real binary and reads the name from the search
/// row's own `collection: <name>` detail — the exact value the production
/// resolver derived — then phase 2 pre-creates that cache dir and re-runs.
/// Same binary, same helper, same value on every platform by construction.
#[test]
fn doctor_engine_path_never_persists_generated_collection() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_minimal_vault(d.path());
    std::fs::remove_file(d.path().join("vault.yml")).unwrap();
    let original = "# precious header\nupdate_channel: stable\nfolders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n";
    std::fs::write(d.path().join("onebrain.yml"), original).unwrap();

    // Phase 1 — no index yet: doctor reports the generated collection name
    // in the search row's details. Capture it from the binary itself.
    let out = run_doctor_json(d.path(), cache.path());
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let search_row = |doc: &serde_json::Value| -> serde_json::Value {
        doc["checks"]
            .as_array()
            .expect("checks[]")
            .iter()
            .find(|c| c["check"] == "search")
            .expect("search row")
            .clone()
    };
    let row = search_row(&doc);
    assert!(
        row["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no index for"),
        "phase 1 must be the no-index arm: {row}"
    );
    let name = row["details"]
        .as_array()
        .expect("details[]")
        .iter()
        .filter_map(|v| v.as_str())
        .find_map(|s| s.strip_prefix("collection: "))
        .expect("search row reports its collection name")
        .to_string();
    // Phase 1 also stamps `stats.last_doctor_run` (comment-preserving,
    // same-day idempotent) — snapshot the file now as the byte baseline.
    let before = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(
        before.starts_with(original),
        "phase 1 must only append the stats stamp:\n{before}"
    );

    // Phase 2 — pre-create that exact cache dir so `is_indexed` is true and
    // doctor takes the engine-open path.
    std::fs::create_dir_all(cache.path().join("search").join(&name)).unwrap();
    let out = run_doctor_json(d.path(), cache.path());
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = search_row(&doc);
    // Non-vacuous: the engine path must actually have been exercised — if
    // the pre-created dir didn't match the runtime collection name the row
    // would still say "no index for …" and this test must fail.
    let msg = row["message"].as_str().unwrap_or_default();
    assert!(
        !msg.contains("no index for"),
        "engine path not exercised (collection name mismatch?): {msg}"
    );

    let after = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(
        !after.contains("collection:"),
        "doctor must never persist a generated collection:\n{after}"
    );
    // The engine-path run makes NO mutation at all (the same-day stats stamp
    // is already present) — byte-identical to the phase-1 snapshot.
    assert_eq!(
        after, before,
        "config rewritten by a read path:\nBEFORE:\n{before}\nAFTER:\n{after}"
    );
}

/// Text-mode mixed outcome: one value reset on disk, one stuck in an inline
/// mapping → the recipe reports ◐ Partial (not ✗ Failed), the summary line
/// counts it, and the process still exits non-zero (manual edit remains).
#[cfg(unix)]
#[test]
fn doctor_fix_text_mode_partial_outcome_renders_distinct_glyph() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    vault_with_config(
        d.path(),
        &format!(
            "update_channel: stable\n\
             {FULL_FOLDERS_BLOCK}\
             checkpoint: {{messages: 0}}\n\
             search:\n  \
               default_top_k: 0\n"
        ),
    );
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .env("HOME", home.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--yes"])
        .assert()
        .failure(); // exit 1: the inline mapping still needs a manual edit
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    assert!(
        stdout.contains("◐ config-values"),
        "expected partial glyph · got: {stdout}"
    );
    assert!(
        stdout.contains("1 partial"),
        "summary counts partial: {stdout}"
    );
    let cfg = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(cfg.contains("default_top_k: 10"), "reset landed: {cfg}");
    assert!(
        cfg.contains("checkpoint: {messages: 0}"),
        "inline shape untouched: {cfg}"
    );
}

/// Line index of the `segments.last()` key, scoped to the entry's own
/// top-level block (`segments[0]`) — tracks the current top-level key while
/// scanning so a bare leaf-name match under the WRONG top-level block is
/// never returned. Needed because `config_key_docs()` has more than one entry
/// sharing a leaf name across different blocks (e.g. `search.reranker.model`
/// / `token_optimization.model`).
fn find_scoped_key_line(lines: &[&str], segments: &[&str]) -> Option<usize> {
    let top = *segments.first()?;
    let key = segments.last()?;
    let key_prefix = format!("{key}:");
    let mut current_top: Option<&str> = None;
    for (i, l) in lines.iter().enumerate() {
        if !l.starts_with(' ') && !l.trim().is_empty() && !l.trim_start().starts_with('#') {
            current_top = l.split(':').next();
        }
        if current_top == Some(top) && l.trim_start().starts_with(&key_prefix) {
            return Some(i);
        }
    }
    None
}

/// End-to-end comment backfill for an EXISTING (legacy, uncommented) vault:
/// plain doctor reports the undocumented keys read-only; `--fix` inserts the
/// template's comments; the next plain doctor is clean and a second `--fix`
/// is byte-identical (idempotent).
#[cfg(unix)]
#[test]
fn doctor_fix_backfills_comments_on_legacy_vault_end_to_end() {
    let d = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let home = tempdir().unwrap();
    let legacy = format!(
        "update_channel: stable\n\
         {FULL_FOLDERS_BLOCK}\
         checkpoint:\n  \
           messages: 15\n  \
           minutes: 30\n"
    );
    vault_with_config(d.path(), &legacy);

    // Plain doctor: discovers, never writes.
    let out = run_doctor_json(d.path(), cache.path());
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = doc["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["check"] == "config-values")
        .expect("config-values row")
        .clone();
    assert_eq!(row["status"], "warn", "row: {row}");
    assert!(
        row["message"]
            .as_str()
            .unwrap_or_default()
            .contains("undocumented key(s)"),
        "row: {row}"
    );
    let cfg_after_plain = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert!(
        cfg_after_plain.starts_with(&legacy),
        "plain doctor must not write (stats stamp appended only):\n{cfg_after_plain}"
    );

    // --fix: every known key gains the exact template comment.
    let out = run_doctor_fix_json(d.path(), cache.path(), home.path());
    assert!(
        out.contains("self-documentation comment(s)"),
        "fix must report the backfill: {out}"
    );
    let cfg = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    for doc_entry in onebrain_fs::config_key_docs() {
        let key = doc_entry.segments.last().unwrap();
        let lines: Vec<&str> = cfg.lines().collect();
        // Scoped to the entry's own TOP-LEVEL block — a bare `starts_with`
        // scan would find the wrong occurrence when two `config_key_docs`
        // entries share a leaf name in different blocks (e.g.
        // `search.reranker.model` / `token_optimization.model`, since #247)
        // and only one member of the pair is present in this minimal
        // fixture (no `search:` block at all).
        let Some(idx) = find_scoped_key_line(&lines, doc_entry.segments) else {
            continue; // absent keys stay absent
        };
        assert_eq!(
            lines[idx - 1].trim_start(),
            doc_entry.comment,
            "comment above {key}:\n{cfg}"
        );
    }
    // Post-fix re-check row is clean.
    let doc: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    let row = doc["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["check"] == "config-values")
        .expect("config-values row")
        .clone();
    assert_eq!(row["status"], "ok", "post-fix: {row}");

    // Idempotency: a second --fix changes nothing.
    let _ = run_doctor_fix_json(d.path(), cache.path(), home.path());
    let cfg2 = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
    assert_eq!(cfg2, cfg, "second --fix must be byte-identical");
}
