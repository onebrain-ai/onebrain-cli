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
