//! CLI end-to-end multilingual integration test.
//!
//! Proves the full pipeline — CLI parsing -> `onebrain-search` engine ->
//! tantivy/fastembed -> hybrid ranking -> JSON envelope — works through the
//! actual compiled binary (not just the engine crate directly), across
//! English, Thai, and Chinese notes in one vault.
//!
//! Gated behind `ONEBRAIN_TEST_EMBED` because it downloads a real embedding
//! model (`multilingual-e5-small`). Run with:
//!   ONEBRAIN_TEST_EMBED=1 cargo test -p onebrain-cli --test search_integration -- --nocapture

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Build a `Command` for the `onebrain` binary, scoped to `vault_root` and a
/// tempdir-redirected cache (`ONEBRAIN_CACHE_DIR`) so tests never touch the
/// real `~/.cache/onebrain/` or `~/Library/Caches/onebrain/`.
fn onebrain(vault_root: &Path, cache_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    cmd.env("ONEBRAIN_CACHE_DIR", cache_dir)
        .arg("--vault")
        .arg(vault_root)
        .arg("--json");
    cmd
}

/// Regression test for the dims-changing `search model set` bug: switching
/// to a model with different embedding dims used to corrupt `onebrain.yml`
/// (persisted the new model first, then bailed reopening the stale-dims
/// vector store, so `rebuild` never ran). With an EMPTY index (0 chunks) the
/// switch must still succeed — wiping and reopening the vector store at the
/// new dims and recording the new active model — without downloading any
/// model (0 chunks => the embedder is never constructed).
///
/// NON-GATED: runs in normal CI. e5-small is 384-dim, e5-base is 768-dim, so
/// this is a genuine dims change; no reindex is run so there is nothing to
/// re-embed.
#[test]
fn search_model_set_empty_index_dims_change_succeeds() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );

    // No reindex: the index is empty. Switch to a different-dims model.
    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "set", "multilingual-e5-base"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "dims-changing model set on an empty index must succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.model.set");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["already_current"], false);
    assert_eq!(
        v["data"]["chunks_reembedded"], 0,
        "empty index => nothing to re-embed: {v}"
    );

    // Config was updated only after the successful rebuild.
    let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(
        yaml.contains("embed_model: multilingual-e5-base"),
        "config should point at the new model after a successful switch: {yaml}"
    );

    // The vector store must have been recreated at the new dims (768). A
    // SECOND dims-changing switch (e5-base 768 -> e5-large 1024) forces
    // `open_engine` -> `VectorStore::open(768)` against the now-768 on-disk
    // store: if the first switch had left a stale-dims store, this reopen
    // would bail. Its success proves the wipe+reopen+header-update worked.
    let out2 = onebrain(vault.path(), cache.path())
        .args(["search", "model", "set", "multilingual-e5-large"])
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "second dims-changing switch must reopen the store at the prior new dims; stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["data"]["chunks_reembedded"], 0);
    let yaml2 = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(
        yaml2.contains("embed_model: multilingual-e5-large"),
        "{yaml2}"
    );
}

/// Bug B/C/D (v3.4.6): while another process holds the engine's single-process
/// redb lock, the CLI must report contention HONESTLY and UNIFORMLY:
/// - `search status` → exit 0, `busy: true`, `doc_count: null`, a
///   `W_ENGINE_BUSY` warning — NEVER "up to date".
/// - `search query` → non-zero exit (77) + an `E_ENGINE_BUSY` error envelope.
/// - `search reindex --lex-only` (hook path) → exit 0 + `skipped`, reason
///   `engine-busy` (must not break the hook chain).
///
/// The lock is held in-process by opening `Engine` at the exact collection
/// cache dir the CLI will resolve (`<ONEBRAIN_CACHE_DIR>/search/<collection>`),
/// then keeping the handle alive across the subprocess invocations.
// `query` opens redb (semantic path) — in a lex-only build it degrades to
// keyword results and never trips the single-process lock, so this end-to-end
// busy test is semantic-only. (status busy/unreadable is also unit-tested.)
#[cfg(feature = "semantic")]
#[test]
fn engine_busy_is_honest_across_status_query_and_hook() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let collection = "t-busy";
    write(
        vault.path(),
        "onebrain.yml",
        &format!("search:\n  collection: {collection}\n  embed_model: multilingual-e5-small\n"),
    );
    write(vault.path(), "a.md", "# Title\nsome content here");

    // Hold the single-process lock at the exact cache dir the CLI resolves.
    let collection_dir = cache.path().join("search").join(collection);
    let _held = onebrain_search::engine::Engine::open(&collection_dir, "multilingual-e5-small")
        .expect("first open should take the lock");

    // Seed a fake "downloaded" model dir so the `--lex-only` hook's
    // model-not-downloaded gate passes and it actually reaches the engine open
    // (where the lock trips). Mirrors the real MCP scenario: the server that
    // holds the lock already downloaded the model. `model_download_status`
    // treats the mere presence of the `models--*` dir as downloaded.
    let model_dir = collection_dir.join("models--intfloat--multilingual-e5-small");
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("model.onnx"), b"stub").unwrap();

    // --- status: honest busy report, still exit 0 ---
    let status = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "status under a held lock is a valid report → exit 0; stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let sv: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(sv["ok"], true);
    assert_eq!(sv["data"]["busy"], true, "status must flag busy: {sv}");
    assert!(
        sv["data"]["doc_count"].is_null(),
        "doc_count must be null (unknown), not a healthy zero: {sv}"
    );
    let warnings = sv["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w["code"] == "W_ENGINE_BUSY"),
        "status must carry a W_ENGINE_BUSY warning: {sv}"
    );

    // --- query: E_ENGINE_BUSY envelope + non-zero exit (77) ---
    let query = onebrain(vault.path(), cache.path())
        .args(["search", "query", "content"])
        .output()
        .unwrap();
    assert_eq!(
        query.status.code(),
        Some(77),
        "query under a held lock must exit 77; stderr: {}",
        String::from_utf8_lossy(&query.stderr)
    );
    let qv: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(qv["ok"], false, "{qv}");
    assert_eq!(qv["error"]["code"], "E_ENGINE_BUSY", "{qv}");

    // --- hook path: exit 0 + skipped reason engine-busy ---
    let hook = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "a hook path must NEVER fail the calling turn (exit 0); stderr: {}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hv: serde_json::Value = serde_json::from_slice(&hook.stdout).unwrap();
    assert_eq!(hv["ok"], true, "{hv}");
    assert_eq!(hv["data"]["skipped"], true, "{hv}");
    assert_eq!(hv["data"]["reason"], "engine-busy", "{hv}");
}

/// Bug C (v3.4.6), `--pending-only` variant: the OTHER hook path must ALSO
/// skip honestly (exit 0 + `reason: "engine-busy"`) when the engine is locked
/// by a live process. Companion to
/// `engine_busy_is_honest_across_status_query_and_hook`, which covers
/// `--lex-only`. Runs the pending-only path in the foreground
/// (`ONEBRAIN_EMBED_FOREGROUND=1`) so it reaches `Engine::open` (where the lock
/// trips) synchronously instead of detaching. Semantic-gated: a lex-only build
/// returns `reason:"semantic-unavailable"` before ever reaching `Engine::open`,
/// so the engine-busy path only exists in semantic builds.
#[cfg(feature = "semantic")]
#[test]
fn pending_only_hook_is_honest_when_engine_busy() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let collection = "t-busy-pending";
    write(
        vault.path(),
        "onebrain.yml",
        &format!("search:\n  collection: {collection}\n  embed_model: multilingual-e5-small\n"),
    );
    write(vault.path(), "a.md", "# Title\nsome content here");

    // Hold the single-process lock at the exact cache dir the CLI resolves.
    let collection_dir = cache.path().join("search").join(collection);
    let _held = onebrain_search::engine::Engine::open(&collection_dir, "multilingual-e5-small")
        .expect("first open should take the lock");

    // Seed a fake "downloaded" model dir so the gate's model-not-downloaded
    // check passes and the hook actually reaches the engine open (where the
    // lock trips). Mirrors the real MCP scenario.
    let model_dir = collection_dir.join("models--intfloat--multilingual-e5-small");
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("model.onnx"), b"stub").unwrap();

    let hook = onebrain(vault.path(), cache.path())
        .env("ONEBRAIN_EMBED_FOREGROUND", "1")
        .args(["search", "reindex", "--pending-only"])
        .output()
        .unwrap();
    assert!(
        hook.status.success(),
        "a --pending-only hook must NEVER fail the calling turn (exit 0); stderr: {}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hv: serde_json::Value = serde_json::from_slice(&hook.stdout).unwrap();
    assert_eq!(hv["ok"], true, "{hv}");
    assert_eq!(hv["data"]["skipped"], true, "{hv}");
    assert_eq!(hv["data"]["reason"], "engine-busy", "{hv}");
}

/// Round-2 fix (v3.4.6): a NON-lock open failure — here a corrupt `engine.redb`
/// — must NOT be reported as `busy`, must NOT render "✅ up to date", and MUST
/// surface the failure (a `W_STATUS_UNREADABLE` warning + null counts). This is
/// the CLI-level guard against the resurrected bug B where a broken index used
/// to fall through to the healthy "up to date" branch.
///
/// The failure is deterministic and lock-free: writing garbage to
/// `engine.redb` makes `Engine::open` -> `Database::create` fail on an invalid
/// redb header (not the single-process lock), so no live handle is held.
/// NON-gated: no model download — the open fails before any embed.
#[test]
fn status_over_broken_index_is_not_busy_and_never_up_to_date() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let collection = "t-broken";
    write(
        vault.path(),
        "onebrain.yml",
        &format!("search:\n  collection: {collection}\n  embed_model: multilingual-e5-small\n"),
    );
    write(vault.path(), "a.md", "# Title\nsome content here");

    // Corrupt the redb metadata db at the exact cache dir the CLI resolves, so
    // `Engine::open` fails on an invalid header — a non-lock open error.
    let collection_dir = cache.path().join("search").join(collection);
    std::fs::create_dir_all(&collection_dir).unwrap();
    std::fs::write(
        collection_dir.join("engine.redb"),
        b"not a redb database \x00\x01\x02 garbage header bytes",
    )
    .unwrap();

    let status = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    // A status read is a report, not a failure → exit 0 even when unreadable.
    assert!(
        status.status.success(),
        "status over a broken index is still a report → exit 0; stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let sv: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(sv["ok"], true);
    assert_eq!(
        sv["data"]["busy"], false,
        "a broken index is unreadable, NOT busy (no live lock): {sv}"
    );
    assert!(
        sv["data"]["doc_count"].is_null(),
        "doc_count must be null (unknown), not a healthy zero: {sv}"
    );
    assert_eq!(sv["data"]["indexed"], false, "{sv}");
    assert!(
        sv["data"]["status_error"].is_string(),
        "the failure message must be surfaced: {sv}"
    );
    let warnings = sv["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w["code"] == "W_STATUS_UNREADABLE"),
        "status must carry a W_STATUS_UNREADABLE warning: {sv}"
    );

    // Text output must never claim "up to date" over a broken index. Build a
    // raw command (the `onebrain` helper forces `--json`, which conflicts with
    // text mode); default output is already `text`.
    let mut text_cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    text_cmd
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("--vault")
        .arg(vault.path())
        .args(["search", "status"]);
    let status_text = text_cmd.output().unwrap();
    let text = String::from_utf8_lossy(&status_text.stdout);
    assert!(
        !text.contains("up to date"),
        "broken-index status must never read up to date: {text}"
    );
    assert!(
        text.contains("status read failed"),
        "broken-index status text must surface the failure: {text}"
    );
}

#[test]
fn search_reindex_and_query_multilingual_end_to_end() {
    if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
        return; // gated: downloads a model
    }

    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );

    // Three notes, three languages, three unrelated topics.
    write(
        vault.path(),
        "en.md",
        "# Machine learning\nneural networks and model training",
    );
    write(
        vault.path(),
        "th.md",
        "# การทำอาหาร\nสูตรผัดไทยและส่วนผสม", // Thai: cooking / pad thai recipe
    );
    write(
        vault.path(),
        "zh.md",
        "# 天气\n今天下雨很冷需要带伞", // Chinese: weather / rain, needs an umbrella
    );

    // Reindex through the real CLI binary.
    let reindex_out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex"])
        .output()
        .unwrap();
    assert!(
        reindex_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&reindex_out.stderr)
    );
    let rv: serde_json::Value = serde_json::from_slice(&reindex_out.stdout).unwrap();
    assert_eq!(rv["command"], "search.reindex");
    assert_eq!(rv["ok"], true);
    assert_eq!(rv["data"]["added"], 3);

    // English query -> English ML note.
    let en_out = onebrain(vault.path(), cache.path())
        .args(["search", "query", "neural networks and training"])
        .output()
        .unwrap();
    assert!(
        en_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&en_out.stderr)
    );
    let ev: serde_json::Value = serde_json::from_slice(&en_out.stdout).unwrap();
    assert_eq!(ev["command"], "search.query");
    assert_eq!(ev["ok"], true);
    assert_eq!(
        ev["data"]["hits"][0]["doc_path"], "en.md",
        "english query should rank the english ML note first: {ev}"
    );

    // Thai query -> Thai cooking note.
    let th_out = onebrain(vault.path(), cache.path())
        .args(["search", "query", "สูตรอาหารผัดไทย"])
        .output()
        .unwrap();
    assert!(
        th_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&th_out.stderr)
    );
    let tv: serde_json::Value = serde_json::from_slice(&th_out.stdout).unwrap();
    assert_eq!(tv["command"], "search.query");
    assert_eq!(tv["ok"], true);
    assert_eq!(
        tv["data"]["hits"][0]["doc_path"], "th.md",
        "thai query should rank the thai cooking note first: {tv}"
    );

    // Chinese query -> Chinese weather note.
    let zh_out = onebrain(vault.path(), cache.path())
        .args(["search", "query", "下雨天气"])
        .output()
        .unwrap();
    assert!(
        zh_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&zh_out.stderr)
    );
    let zv: serde_json::Value = serde_json::from_slice(&zh_out.stdout).unwrap();
    assert_eq!(zv["command"], "search.query");
    assert_eq!(zv["ok"], true);
    assert_eq!(
        zv["data"]["hits"][0]["doc_path"], "zh.md",
        "chinese query should rank the chinese weather note first: {zv}"
    );
}
