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
