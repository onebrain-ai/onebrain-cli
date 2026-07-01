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
    // Enriched status fields present, all zero/absent on a fresh vault.
    assert_eq!(v["data"]["doc_count"], 0);
    assert_eq!(v["data"]["pending_new"], 0);
    assert!(v["data"]["last_indexed_at"].is_null());
    assert!(v["data"]["model_size_bytes"].is_null());

    // `status` now opens the engine read-only to read the doc count / drift,
    // so the cache dir (tantivy/vectors/redb) may exist — but NO embedding
    // model must ever be downloaded. Assert the real invariant: no ONNX model
    // files and no `models--*` download dir under the collection cache.
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    if model_cache.exists() {
        assert!(
            !walkdir_has_onnx(&model_cache),
            "status must not download an embedding model"
        );
        let has_model_dir = std::fs::read_dir(&model_cache)
            .map(|rd| {
                rd.filter_map(|e| e.ok()).any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("models--"))
                })
            })
            .unwrap_or(false);
        assert!(
            !has_model_dir,
            "status must not create a model download dir"
        );
    }
}

#[test]
fn search_status_autogenerates_and_persists_collection_when_unset() {
    // A vault with no `search.collection` and no legacy `qmd_collection`:
    // `status` auto-generates `<dir>-<hash>` and persists it to onebrain.yml
    // (headless-safe, no prompt) so the index is stable across runs.
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
    let collection = v["data"]["collection"]
        .as_str()
        .expect("collection auto-generated, not null");
    // `<dir>-<6 hex>`.
    let dir_name = vault.path().file_name().unwrap().to_str().unwrap();
    assert!(
        collection.starts_with(&format!("{dir_name}-")),
        "expected `<dir>-<hash>`, got {collection}"
    );
    assert_eq!(v["data"]["indexed"], false);

    // Persisted to config, and preserving the existing folders key.
    let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(
        yaml.contains(&format!("collection: {collection}")),
        "collection must be persisted: {yaml}"
    );
    assert!(yaml.contains("inbox: 00-inbox"));

    // Second run reads the persisted value (stable, no re-derivation).
    let out2 = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["data"]["collection"].as_str().unwrap(), collection);
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

#[test]
fn search_model_list_json_reports_all_models_with_current_marked() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: bge-m3\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "list"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.model.list");
    assert_eq!(v["ok"], true);
    let models = v["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 5);

    let names: Vec<&str> = models.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"multilingual-e5-small"));
    assert!(names.contains(&"multilingual-e5-base"));
    assert!(names.contains(&"multilingual-e5-large"));
    assert!(names.contains(&"bge-m3"));
    assert!(names.contains(&"embeddinggemma-300m-q"));

    let current: Vec<&str> = models
        .iter()
        .filter(|m| m["current"].as_bool().unwrap())
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(current, vec!["bge-m3"]);

    // `list` must never touch the engine's on-disk state — no download.
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    assert!(
        !model_cache.exists(),
        "model list must not touch the engine's on-disk state"
    );
}

#[test]
fn search_model_list_defaults_current_to_multilingual_e5_small() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let models = v["data"]["models"].as_array().unwrap();
    let current: Vec<&str> = models
        .iter()
        .filter(|m| m["current"].as_bool().unwrap())
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(current, vec!["multilingual-e5-small"]);
}

#[test]
fn search_model_set_unknown_name_errors_without_downloading() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "set", "not-a-real-model"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    // `--json` mode routes the error envelope to stdout, not stderr.
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("unsupported embedding model"),
        "message: {message}"
    );

    // Config must be left untouched, and no model cache created.
    let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(!yaml.contains("not-a-real-model"));
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    assert!(
        !model_cache.exists(),
        "an unsupported model must never trigger engine open / download"
    );
}

#[test]
fn search_model_bare_non_tty_prints_current_and_available_without_prompting() {
    // `onebrain search model` with no subcommand, run via `Command::output()`
    // (stdout/stdin are pipes here, never a real TTY under any test harness),
    // must take the non-interactive fallback branch: print the current model
    // + available model names + the hint, exit 0, and never block waiting on
    // stdin or touch the engine's on-disk state (no download).
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: bge-m3\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.model.bare");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["model"], "bge-m3");

    let available = v["data"]["available"].as_array().unwrap();
    let names: Vec<&str> = available.iter().map(|m| m.as_str().unwrap()).collect();
    assert!(names.contains(&"multilingual-e5-small"));
    assert!(names.contains(&"multilingual-e5-base"));
    assert!(names.contains(&"multilingual-e5-large"));
    assert!(names.contains(&"bge-m3"));
    assert!(names.contains(&"embeddinggemma-300m-q"));

    let hint = v["data"]["hint"].as_str().unwrap();
    assert!(hint.contains("search model list"));
    assert!(hint.contains("search model set"));

    // No engine cache dir should be created — the fallback must never open
    // the engine or trigger a model download.
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    assert!(
        !model_cache.exists(),
        "bare model fallback must not touch the engine's on-disk state"
    );
}

#[test]
fn search_model_bare_non_tty_text_mode_shows_current_and_hint() {
    // Same fallback path, but in the default text (non-JSON) output mode —
    // exercises `render_bare_fallback_text` end-to-end.
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    cmd.env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("--vault")
        .arg(vault.path())
        .args(["search", "model"]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("multilingual-e5-small"));
    assert!(stdout.contains("search model list"));
    assert!(stdout.contains("search model set"));
}

#[test]
fn search_model_set_already_current_is_a_noop() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "set", "multilingual-e5-small"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.model.set");
    assert_eq!(v["data"]["already_current"], true);
    assert_eq!(v["data"]["chunks_reembedded"], serde_json::Value::Null);

    // No engine cache dir should be created for a no-op set.
    let model_cache = cache.path().join("onebrain").join("search").join("t-vault");
    assert!(!model_cache.exists());
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

#[test]
fn search_model_set_rebuilds_index_with_new_model() {
    if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
        return; // gated: downloads models (e5-small + e5-base)
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

    let reindex_out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex"])
        .output()
        .unwrap();
    assert!(reindex_out.status.success());

    let set_out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "set", "multilingual-e5-base"])
        .output()
        .unwrap();
    assert!(
        set_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );
    let sv: serde_json::Value = serde_json::from_slice(&set_out.stdout).unwrap();
    assert_eq!(sv["command"], "search.model.set");
    assert_eq!(sv["data"]["already_current"], false);
    assert_eq!(sv["data"]["chunks_reembedded"], 1);

    // Config now records the new model.
    let yaml = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    assert!(yaml.contains("embed_model: multilingual-e5-base"));

    // Status reflects the switched model too.
    let status_out = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    let stv: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(stv["data"]["embed_model"], "multilingual-e5-base");
}
