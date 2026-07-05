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
    let model_cache = cache.path().join("search").join("t-vault");
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

/// Regression (v3.4.5): `indexed` must reflect whether docs are actually
/// indexed (`doc_count > 0`), NOT merely whether the cache dir exists on disk.
///
/// `search status` opens the engine read-only, which CREATES the cache dir as
/// a side effect. So a *second* `status` on a never-indexed vault used to see
/// the dir left behind by the first run and flip `indexed` false → true, while
/// `doc_count` stayed 0 and `last_indexed_at` stayed null — an incoherent
/// "indexed but empty" report (the same false-positive a purged-then-recreated
/// cache dir produces).
#[test]
fn search_status_indexed_stays_false_until_docs_are_indexed() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    write(
        vault.path(),
        "note.md",
        "---\ntitle: N\n---\nalpha bravo charlie.\n",
    );

    // Run 1: cache dir absent → engine.open creates it as a side effect.
    let out1 = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let v1: serde_json::Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(v1["data"]["indexed"], false, "run 1 data: {}", v1["data"]);

    // Run 2: cache dir now exists (from run 1) but still holds zero docs.
    // `indexed` must stay false — nothing was ever indexed.
    let out2 = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["data"]["doc_count"], 0, "run 2 data: {}", v2["data"]);
    assert!(
        v2["data"]["last_indexed_at"].is_null(),
        "run 2 data: {}",
        v2["data"]
    );
    assert_eq!(
        v2["data"]["indexed"], false,
        "indexed must stay false when doc_count is 0 (dir exists but empty): {}",
        v2["data"]
    );
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
    let model_cache = cache.path().join("search").join("t-vault");
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
    assert_eq!(models.len(), 6);

    let names: Vec<&str> = models.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"multilingual-e5-small"));
    assert!(names.contains(&"multilingual-e5-base"));
    assert!(names.contains(&"multilingual-e5-large"));
    assert!(names.contains(&"bge-m3"));
    assert!(names.contains(&"embeddinggemma-300m-q"));
    assert!(names.contains(&"embeddinggemma-300m-q4"));

    let current: Vec<&str> = models
        .iter()
        .filter(|m| m["current"].as_bool().unwrap())
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(current, vec!["bge-m3"]);

    // `list` must never touch the engine's on-disk state — no download.
    let model_cache = cache.path().join("search").join("t-vault");
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
    let model_cache = cache.path().join("search").join("t-vault");
    assert!(
        !model_cache.exists(),
        "an unsupported model must never trigger engine open / download"
    );
}

#[test]
fn search_model_bare_non_tty_falls_back_to_static_list_without_prompting() {
    // `onebrain search model` with no subcommand, run via `Command::output()`
    // (stdout/stdin are pipes here, never a real TTY under any test harness),
    // must NOT launch the interactive TUI: it falls back to the same static
    // `search model list` output (envelope command `search.model.list`), exits
    // 0, never blocks on stdin, and never touches the engine's on-disk state.
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
    assert_eq!(v["command"], "search.model.list");
    assert_eq!(v["ok"], true);

    // Same rows the explicit `list` verb emits.
    let models = v["data"]["models"].as_array().unwrap();
    let names: Vec<&str> = models.iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"multilingual-e5-small"));
    assert!(names.contains(&"multilingual-e5-base"));
    assert!(names.contains(&"multilingual-e5-large"));
    assert!(names.contains(&"bge-m3"));
    assert!(names.contains(&"embeddinggemma-300m-q"));
    assert!(names.contains(&"embeddinggemma-300m-q4"));
    // The active model is flagged current.
    let bge = models.iter().find(|m| m["name"] == "bge-m3").unwrap();
    assert_eq!(bge["current"], true);

    // No engine cache dir should be created — the static list must never open
    // the engine or trigger a model download.
    let model_cache = cache.path().join("search").join("t-vault");
    assert!(
        !model_cache.exists(),
        "bare model fallback must not touch the engine's on-disk state"
    );
}

#[test]
fn search_model_bare_non_tty_text_mode_renders_static_table() {
    // Same fallback path, but in the default text (non-JSON) output mode —
    // renders the same static table as `search model list`.
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
    // The static-list header + rows + footer, not a TUI or a hint string.
    assert!(stdout.contains("MODEL"));
    assert!(stdout.contains("DOWNLOADED"));
    assert!(stdout.contains("multilingual-e5-small"));
    assert!(stdout.contains("📁  Cache dir:"));
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
    let model_cache = cache.path().join("search").join("t-vault");
    assert!(!model_cache.exists());
}

/// The on-disk model dir a downloaded model would occupy under the collection
/// cache. `intfloat/multilingual-e5-small` → `models--intfloat--multilingual-e5-small`.
fn seed_fake_model(
    cache: &Path,
    collection: &str,
    dir_name: &str,
    bytes: usize,
) -> std::path::PathBuf {
    // `ONEBRAIN_CACHE_DIR` replaces the state dir wholesale (see
    // `migration::default_state_dir`), so the search cache root is
    // `<cache>/search/<collection>/`.
    let model_dir = cache.join("search").join(collection).join(dir_name);
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("model.onnx"), vec![0u8; bytes]).unwrap();
    model_dir
}

#[test]
fn search_model_list_json_reports_downloaded_and_disk_fields() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: bge-m3\n",
    );
    seed_fake_model(
        cache.path(),
        "t-vault",
        "models--intfloat--multilingual-e5-small",
        4096,
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let models = v["data"]["models"].as_array().unwrap();

    let small = models
        .iter()
        .find(|m| m["name"] == "multilingual-e5-small")
        .unwrap();
    assert_eq!(small["downloaded"], true);
    assert_eq!(small["disk_bytes"], 4096);

    let bge = models.iter().find(|m| m["name"] == "bge-m3").unwrap();
    assert_eq!(bge["downloaded"], false);
    assert_eq!(bge["disk_bytes"], serde_json::Value::Null);

    assert!(v["data"]["cache_dir"].as_str().unwrap().contains("t-vault"));
}

#[test]
fn search_model_list_sort_by_dim_desc_orders_rows() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "list", "--sort", "dim", "--desc"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dims: Vec<u64> = v["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["dims"].as_u64().unwrap())
        .collect();
    let mut sorted = dims.clone();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(dims, sorted);
}

#[test]
fn search_model_remove_unknown_name_errors() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "remove", "not-a-real-model"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("unsupported embedding model"));
}

#[test]
fn search_model_remove_not_downloaded_is_friendly_noop() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: bge-m3\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "remove", "multilingual-e5-large"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.model.remove");
    assert_eq!(v["data"]["removed"], false);
    assert_eq!(v["data"]["freed_bytes"], serde_json::Value::Null);
}

#[test]
fn search_model_remove_deletes_non_current_downloaded_model() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: bge-m3\n",
    );
    let model_dir = seed_fake_model(
        cache.path(),
        "t-vault",
        "models--intfloat--multilingual-e5-small",
        8192,
    );
    assert!(model_dir.exists());

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "remove", "multilingual-e5-small"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["removed"], true);
    assert_eq!(v["data"]["freed_bytes"], 8192);
    assert_eq!(v["data"]["was_current"], false);
    assert!(!model_dir.exists(), "model dir should be gone");
}

#[test]
fn search_model_remove_current_refuses_without_force_non_tty() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );
    let model_dir = seed_fake_model(
        cache.path(),
        "t-vault",
        "models--intfloat--multilingual-e5-small",
        4096,
    );

    // Non-TTY (piped) + active model + no --force → refuse, exit non-zero,
    // and leave the model dir intact.
    let out = onebrain(vault.path(), cache.path())
        .args(["search", "model", "remove", "multilingual-e5-small"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("--force"));
    assert!(model_dir.exists(), "refused remove must not delete files");
}

#[test]
fn search_model_remove_current_with_force_deletes() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );
    let model_dir = seed_fake_model(
        cache.path(),
        "t-vault",
        "models--intfloat--multilingual-e5-small",
        2048,
    );

    let out = onebrain(vault.path(), cache.path())
        .args([
            "search",
            "model",
            "remove",
            "multilingual-e5-small",
            "--force",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["removed"], true);
    assert_eq!(v["data"]["was_current"], true);
    assert_eq!(v["data"]["freed_bytes"], 2048);
    assert!(!model_dir.exists());
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

#[test]
fn search_reindex_force_rejects_specific_paths() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "a.md", "--force"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--force + paths must error");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("--force"), "mentions the flag: {text}");
}

#[test]
fn search_reindex_force_on_empty_vault_rebuilds_and_status_reports_index_size() {
    // An empty vault embeds nothing, so --force runs the full wipe+rebuild
    // path (LiveProgressFile, wipe_index_files, model-load notice wiring)
    // without ever downloading a model.
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    // Run twice: the second --force exercises the delete-existing arms.
    for _ in 0..2 {
        let out = onebrain(vault.path(), cache.path())
            .args(["search", "reindex", "--force"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "force reindex on empty vault: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status emits a JSON envelope");
    let size = v["data"]["index_size_bytes"]
        .as_u64()
        .expect("index exists after --force → index_size_bytes present");
    assert!(size > 0, "tantivy/redb files have real bytes: {size}");
}

// ── `--lex-only` / `--pending-only` (hook safety gate) ─────────────────────
//
// Both flags are "hook paths": every error/skip case must exit 0 with a skip
// envelope on stdout (`{"skipped":true,"reason":"..."}`) — never a nonzero
// exit, never a `decision` key (that field belongs to a completely different
// hook-output contract this envelope must stay inert against).

/// The on-disk dir name of the first registry model — used to fake a
/// "downloaded" model without a real network fetch, exactly like
/// `seed_fake_model` above but reusable across the gate tests below.
fn first_model_dir_name() -> String {
    onebrain_search::embed::model_registry()[0].cache_dir_name()
}

#[test]
fn lex_only_gate_no_index_skips_cleanly_without_touching_config_or_models() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    let config_before = std::fs::read(vault.path().join("onebrain.yml")).unwrap();

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "hook path must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["skipped"], true);
    assert_eq!(v["data"]["reason"], "no-index");
    assert!(v["data"].get("decision").is_none(), "no decision key: {v}");

    // Vault config byte-unchanged: the readonly collection resolver must
    // never persist `search.collection`.
    let config_after = std::fs::read(vault.path().join("onebrain.yml")).unwrap();
    assert_eq!(config_before, config_after, "config must be untouched");

    // No model download dir anywhere under the cache root.
    assert!(
        !walkdir_has_dir_prefix(cache.path(), "models--"),
        "gate must never construct the embedder"
    );
}

#[test]
fn lex_only_gate_index_present_but_model_missing_skips_model_not_downloaded() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    // Mirror a real installed vault: tantivy dir exists, but no model.
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["skipped"], true);
    assert_eq!(v["data"]["reason"], "model-not-downloaded");
}

#[test]
fn lex_only_gate_reindex_in_flight_skips() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    let collection_dir = cache.path().join("search/t-vault");
    std::fs::create_dir_all(collection_dir.join("tantivy")).unwrap();
    seed_fake_model(cache.path(), "t-vault", &first_model_dir_name(), 16);
    // Fresh live-progress marker (read_reindex_progress treats <30min as live).
    std::fs::write(
        collection_dir.join("reindex-progress.json"),
        r#"{"done":1,"total":2}"#,
    )
    .unwrap();

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["skipped"], true);
    assert_eq!(v["data"]["reason"], "reindex-in-flight");
}

#[test]
fn lex_only_end_to_end_indexes_lex_only_and_leaves_vectors_pending() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    write(vault.path(), "note.md", "# Note\nalpha bravo charlie.\n");
    // Mirror an installed vault: tantivy dir + downloaded model pre-exist so
    // the safety gate passes.
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();
    seed_fake_model(cache.path(), "t-vault", &first_model_dir_name(), 16);

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["data"].get("skipped").is_none(), "gate passed: {v}");
    assert_eq!(v["command"], "search.reindex");
    assert_eq!(v["data"]["added"], 1);

    // No SECOND model dir appeared — only the one fake dir seeded above by
    // the test itself, proving the run never constructed a real embedder
    // (which would create/touch its own `models--*` dir).
    let model_dirs: Vec<_> = std::fs::read_dir(cache.path().join("search/t-vault"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("models--"))
        })
        .collect();
    assert_eq!(
        model_dirs.len(),
        1,
        "lex-only run must never construct the embedder"
    );

    // `search status` proves DOC_HASHES is untouched: the doc stays pending.
    let status_out = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    assert!(status_out.status.success());
    let sv: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(sv["data"]["doc_count"], 0, "DOC_HASHES untouched: {sv}");
    assert_eq!(sv["data"]["pending_new"], 1, "{sv}");
}

/// Bug E (v3.4.6) at the CLI layer: the lex `search` verb populates each hit's
/// `heading_path` from the STORED tantivy field (no redb open), while `snippet`
/// stays empty (the `body` field is indexed but NOT STORED — a snippet needs a
/// schema change + reindex migration, deferred). This proves the wiring
/// end-to-end through the compiled binary: index a doc under a known heading
/// with `--lex-only` (which populates tantivy without embedding, so it's
/// ungated), then assert `search search --json` echoes that exact heading and
/// an empty snippet.
#[test]
fn lex_search_verb_populates_heading_path_and_empty_snippet() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    // A single heading so the chunk's heading_path is deterministic.
    write(
        vault.path(),
        "note.md",
        "# Errors Handling\nzebra quokka narwhal.\n",
    );
    // Mirror an installed vault so the `--lex-only` safety gate passes.
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();
    seed_fake_model(cache.path(), "t-vault", &first_model_dir_name(), 16);

    // Populate tantivy (heading_path is STORED) without embedding.
    let lex_out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        lex_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&lex_out.stderr)
    );
    let lv: serde_json::Value = serde_json::from_slice(&lex_out.stdout).unwrap();
    assert!(lv["data"].get("skipped").is_none(), "gate passed: {lv}");

    // Now the lex `search` verb must echo the STORED heading + empty snippet.
    let out = onebrain(vault.path(), cache.path())
        .args(["search", "search", "quokka"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.lex", "{v}");
    let hits = v["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected a lex hit for `quokka`: {v}");
    assert_eq!(
        hits[0]["heading_path"], "Errors Handling",
        "heading_path must come from the STORED tantivy field: {v}"
    );
    assert_eq!(
        hits[0]["snippet"], "",
        "snippet is deferred (body not STORED) — must be empty, NOT 'no match': {v}"
    );
}

/// Regression: past the safety gates, the hook path's live-progress marker
/// used to re-resolve the collection via the persisting `collection_for`,
/// which auto-generates AND writes `search.collection` into `onebrain.yml`
/// when unset — violating the "hooks never mutate config" contract. The
/// marker must come from the gate's read-only cache dir instead, so a
/// gate-passing run on a collection-less vault leaves the config untouched.
#[test]
fn lex_only_run_never_persists_autogenerated_collection() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    // NO `search.collection` key — the whole run must resolve read-only.
    write(
        vault.path(),
        "onebrain.yml",
        "folders:\n  inbox: 00-inbox\n",
    );
    write(vault.path(), "note.md", "# Note\nalpha bravo charlie.\n");
    let config_before = std::fs::read(vault.path().join("onebrain.yml")).unwrap();

    // The auto-generated name the readonly resolver derives: `<dir>-<hash>`
    // of the vault path exactly as passed via `--vault` (no canonicalization).
    let dir_name = vault.path().file_name().unwrap().to_str().unwrap();
    let collection = format!(
        "{dir_name}-{}",
        onebrain_search::engine::short_path_hash(vault.path())
    );
    // Pass gates 2+3: tantivy dir + fake downloaded model under that name.
    std::fs::create_dir_all(
        cache
            .path()
            .join("search")
            .join(&collection)
            .join("tantivy"),
    )
    .unwrap();
    seed_fake_model(cache.path(), &collection, &first_model_dir_name(), 16);

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["data"].get("skipped").is_none(), "gate must pass: {v}");
    assert_eq!(v["data"]["added"], 1);

    // Config byte-unchanged: the full run (gates AND the indexing pass with
    // its live-progress marker) must never persist `search.collection`.
    let config_after = std::fs::read(vault.path().join("onebrain.yml")).unwrap();
    assert_eq!(config_before, config_after, "config must be untouched");
}

#[cfg(feature = "semantic")]
#[test]
fn pending_only_with_nothing_pending_skips_fast() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();
    seed_fake_model(cache.path(), "t-vault", &first_model_dir_name(), 16);

    let mut cmd = onebrain(vault.path(), cache.path());
    cmd.env("ONEBRAIN_EMBED_FOREGROUND", "1")
        .args(["search", "reindex", "--pending-only"]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["skipped"], true);
    assert_eq!(v["data"]["reason"], "no-pending");
    assert!(
        v["data"].get("detached").is_none(),
        "foreground env must stay foreground: {v}"
    );
}

/// Task 4: without `ONEBRAIN_EMBED_FOREGROUND`, a gate-passing `--pending-only
/// --json` run must detach a background child and return immediately with
/// `detached: true` — never blocking the calling turn on model load / embed.
#[cfg(feature = "semantic")]
#[test]
fn pending_only_json_detaches() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();
    seed_fake_model(cache.path(), "t-vault", &first_model_dir_name(), 16);

    let start = std::time::Instant::now();
    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--pending-only"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "detach must return immediately, took {elapsed:?}"
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "search.reindex");
    assert_eq!(v["data"]["detached"], true);
    assert!(v["data"].get("decision").is_none(), "no decision key: {v}");

    // Give the detached child (which will itself hit `no-pending` and exit
    // silently) a moment to finish so it doesn't outlive the test process.
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[cfg(not(feature = "semantic"))]
#[test]
fn pending_only_lex_only_build_skips_semantic_unavailable() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();
    seed_fake_model(cache.path(), "t-vault", &first_model_dir_name(), 16);

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--pending-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["skipped"], true);
    assert_eq!(v["data"]["reason"], "semantic-unavailable");
}

#[test]
fn pending_only_embeds_exactly_what_lex_only_left_pending() {
    if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
        return; // gated: downloads a real model
    }
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );
    write(vault.path(), "a.md", "# A\nalpha bravo charlie.\n");
    write(vault.path(), "b.md", "# B\ndelta echo foxtrot.\n");
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();

    // First reindex downloads the real model (gated) so the gate's
    // model-downloaded check passes for the subsequent --lex-only /
    // --pending-only calls without re-downloading.
    let seed = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--force"])
        .output()
        .unwrap();
    assert!(seed.status.success());

    // Touch the docs again so they're pending, then lex-only index them
    // (recording LEX_HASHES, leaving DOC_HASHES stale) before pending-only
    // picks them back up.
    write(vault.path(), "a.md", "# A\nalpha bravo charlie updated.\n");
    write(vault.path(), "c.md", "# C\ngolf hotel india.\n");

    let lex_out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(
        lex_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&lex_out.stderr)
    );

    let mut pending_cmd = onebrain(vault.path(), cache.path());
    pending_cmd
        .env("ONEBRAIN_EMBED_FOREGROUND", "1")
        .args(["search", "reindex", "--pending-only"]);
    let pending_out = pending_cmd.output().unwrap();
    assert!(
        pending_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&pending_out.stderr)
    );
    let pv: serde_json::Value = serde_json::from_slice(&pending_out.stdout).unwrap();
    assert!(pv["data"].get("skipped").is_none(), "gate passed: {pv}");
    let added = pv["data"]["added"].as_u64().unwrap_or(0);
    let updated = pv["data"]["updated"].as_u64().unwrap_or(0);
    assert_eq!(added + updated, 2, "exactly a.md + c.md embedded: {pv}");

    let status_out = onebrain(vault.path(), cache.path())
        .args(["search", "status"])
        .output()
        .unwrap();
    let sv: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    let pending = sv["data"]["pending_new"].as_u64().unwrap()
        + sv["data"]["pending_changed"].as_u64().unwrap()
        + sv["data"]["pending_removed"].as_u64().unwrap();
    assert_eq!(pending, 0, "pending must be 0 after pending-only: {sv}");
}

/// Task 4 end-to-end: a REAL detached child (no `ONEBRAIN_EMBED_FOREGROUND`)
/// actually finishes the embed in the background. Gated behind
/// `ONEBRAIN_TEST_EMBED` — downloads a real model — and generously polls
/// `search status` for up to ~30s (CI slack) before failing.
#[test]
fn pending_only_detach_end_to_end_embeds_in_background() {
    if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
        return; // gated: downloads a real model
    }
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n  embed_model: multilingual-e5-small\n",
    );
    write(vault.path(), "a.md", "# A\nalpha bravo charlie.\n");
    std::fs::create_dir_all(cache.path().join("search/t-vault/tantivy")).unwrap();

    // Seed a downloaded model (foreground) so the gate passes, then lex-only
    // index the doc so it's pending for the pending-only detach run.
    let seed = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--force"])
        .output()
        .unwrap();
    assert!(seed.status.success());
    write(vault.path(), "a.md", "# A\nalpha bravo charlie updated.\n");
    let lex_out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only"])
        .output()
        .unwrap();
    assert!(lex_out.status.success());

    // No env var: this must detach.
    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--pending-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["detached"], true, "{v}");

    // Poll `search status` until pending drops to 0, or time out.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let status_out = onebrain(vault.path(), cache.path())
            .args(["search", "status"])
            .output()
            .unwrap();
        assert!(status_out.status.success());
        let sv: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
        let pending = sv["data"]["pending_new"].as_u64().unwrap_or(0)
            + sv["data"]["pending_changed"].as_u64().unwrap_or(0)
            + sv["data"]["pending_removed"].as_u64().unwrap_or(0);
        if pending == 0 {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("background embed did not finish within 30s: {sv}");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[test]
fn lex_only_and_pending_only_are_clap_conflicts() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only", "--pending-only"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "conflicting flags must error");
}

#[test]
fn lex_only_and_force_are_clap_conflicts() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--lex-only", "--force"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "conflicting flags must error");
}

#[test]
fn pending_only_and_paths_are_clap_conflicts() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );

    let out = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "somepath.md", "--pending-only"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "conflicting flags must error");
}

/// `true` if any dir under `root` (recursively) has a name starting with
/// `prefix`. Used to prove the safety gate never constructs an embedder
/// (which would create a `models--*` dir).
fn walkdir_has_dir_prefix(root: &Path, prefix: &str) -> bool {
    fn walk(dir: &Path, prefix: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name_matches = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix));
                if name_matches || walk(&path, prefix) {
                    return true;
                }
            }
        }
        false
    }
    walk(root, prefix)
}

#[test]
fn search_get_rejects_absolute_path_outside_vault() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write(
        vault.path(),
        "onebrain.yml",
        "search:\n  collection: t-vault\n",
    );
    // Index must exist so the failure is the outside-vault check, not setup.
    let ok = onebrain(vault.path(), cache.path())
        .args(["search", "reindex", "--force"])
        .output()
        .unwrap();
    assert!(ok.status.success());

    let outside = tempdir().unwrap();
    let abs = outside.path().join("x.md");
    let out = onebrain(vault.path(), cache.path())
        .args(["search", "get", abs.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("outside"), "outside-vault error: {text}");
}
