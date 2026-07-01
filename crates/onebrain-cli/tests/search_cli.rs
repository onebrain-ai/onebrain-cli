//! E2E: `onebrain search *` against a real temp vault.
//!
//! `search status` and `search search` (lex-only) must stay FAST and must
//! NOT download an embedding model — both run unconditionally (no
//! `ONEBRAIN_TEST_EMBED` gate). Anything that embeds (`query`, `vsearch`,
//! `reindex`) is gated behind `ONEBRAIN_TEST_EMBED` like the engine's own
//! gated tests.

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
fn search_status_json_reports_collection_and_model_without_downloading() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.status");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["collection"], "t-vault");
    assert!(v["data"]["embed_model"].is_string());
    assert_eq!(v["data"]["indexed"], false);

    // The model cache dir must NOT have been created/populated by `status` —
    // if it were, that would mean a download was attempted.
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    assert!(
        !model_cache.exists(),
        "status must not touch the engine's on-disk state"
    );
}

#[test]
fn search_status_reports_unconfigured_collection() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "folders:\n  inbox: 00-inbox\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["data"]["collection"].is_null());
    assert_eq!(v["data"]["indexed"], false);
}

#[test]
fn search_lex_only_works_without_embedding_model() {
    // `search search` (lex-only) must succeed end-to-end — reindex the tiny
    // vault first (reindex embeds too, so this one specific assertion is
    // gated), then query lex-only. To keep the *lex* half of this test
    // ungated and fast, we instead directly exercise the "no index yet"
    // path: an empty/missing collection returns zero hits without ever
    // touching an embedder.
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "search", "hello"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.lex");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["hits"].as_array().unwrap().len(), 0);

    // Confirm no embedding model was ever fetched by this lex-only path.
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    if model_cache.exists() {
        // The tantivy dir may exist (LexIndex::open creates it), but no
        // fastembed ONNX model files should be there.
        let has_onnx = walkdir_has_onnx(&model_cache);
        assert!(!has_onnx, "lex-only search must not download a model");
    }
}

fn walkdir_has_onnx(root: &Path) -> bool {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path) {
                    return true;
                }
            } else if path.extension().is_some_and(|e| e == "onnx") {
                return true;
            }
        }
        false
    }
    walk(root)
}

#[test]
fn search_get_errors_cleanly_for_missing_doc() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "get", "missing.md"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn search_status_errors_outside_a_vault() {
    let dir = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let out = onebrain(dir.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

// ── Gated: real embeddings (reindex + query + vsearch) ─────────────────────

#[test]
fn search_reindex_then_query_and_vsearch_end_to_end() {
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
    write(
        vault.path(),
        "rust.md",
        "# Rust\nerror handling and memory safety",
    );
    write(
        vault.path(),
        "cook.md",
        "# Cooking\npasta recipe with tomato",
    );

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
    assert_eq!(rv["data"]["added"], 2);

    let query_out = onebrain(vault.path(), cache.path())
        .args(["search", "query", "memory safety"])
        .output()
        .unwrap();
    assert!(query_out.status.success());
    let qv: serde_json::Value = serde_json::from_slice(&query_out.stdout).unwrap();
    assert_eq!(qv["command"], "search.query");
    assert_eq!(qv["data"]["hits"][0]["doc_path"], "rust.md");

    let vsearch_out = onebrain(vault.path(), cache.path())
        .args(["search", "vsearch", "memory safety"])
        .output()
        .unwrap();
    assert!(vsearch_out.status.success());
    let vv: serde_json::Value = serde_json::from_slice(&vsearch_out.stdout).unwrap();
    assert_eq!(vv["command"], "search.vec");
    assert_eq!(vv["data"]["hits"][0]["doc_path"], "rust.md");

    let get_out = onebrain(vault.path(), cache.path())
        .args(["search", "get", "rust.md"])
        .output()
        .unwrap();
    assert!(get_out.status.success());
    let gv: serde_json::Value = serde_json::from_slice(&get_out.stdout).unwrap();
    assert_eq!(gv["command"], "search.get");
    assert!(gv["data"]["content"]
        .as_str()
        .unwrap()
        .contains("memory safety"));
}
