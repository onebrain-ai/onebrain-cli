//! Guard test: an integration test that both **spawns the binary** and
//! **names a search collection** must also pin `ONEBRAIN_CACHE_DIR`.
//!
//! Why this is a real hazard and not hygiene. `collection_name_readonly`
//! resolves the collection from the vault config, and `collection_cache_dir`
//! joins it onto the *process-wide* search-cache root — the developer's real
//! `~/.cache/onebrain/search/` unless `ONEBRAIN_CACHE_DIR` says otherwise.
//! Several commands (`session init` among them) then call `Engine::open` on
//! that directory **whenever a collection of that name already exists**, and
//! `Engine::open` is not read-only: it migrates the layout, and on a schema
//! mismatch it wipes and repopulates the keyword index.
//!
//! So a test that writes `qmd_collection: ob-1` into a tempdir vault and runs
//! the binary unisolated is one name collision away from rewriting a real
//! user's index. Renaming individual fixtures (round 3 did that for one
//! string) does not fix this — the hazard is the missing isolation, not any
//! particular name. This test makes the isolation the enforced rule.
//!
//! ## Granularity, and what it does and does not see
//!
//! The scan is **per function**, not per file. The first version tested the
//! three conditions against whole-file text, so ONE isolated call site
//! anywhere in a file exempted every other spawn in it — and that is not a
//! theoretical gap: it passed green over five call sites that wrote a literal
//! collection name into a tempdir vault and then ran the binary against it
//! (`doctor_integration.rs`, `snapshots.rs`, `v31_integration.rs`). Planting a
//! pristine collection under one of those names and running even the
//! **read-only** `doctor` migrated it in place (`heading_path` default →
//! script_aware, tantivy 2884 KB → 3320 KB) while reporting `ok`.
//!
//! Regions are cut on top-level `fn ` boundaries, so each `#[test]` (and each
//! helper) is judged on its own text: a region that spawns the binary AND
//! names a collection must pin the cache dir. That is the shape every real
//! hazard has — the config write and the spawn sit in the same function.
//!
//! Residual limits, stated rather than implied:
//! - A helper that spawns the binary while its *caller* supplies the
//!   collection name is not seen by the per-region pass. The original
//!   whole-file pass is kept alongside precisely for that case, so a file
//!   where NO spawn is isolated still fails.
//! - Collection names living outside `.rs` — the `tests/fixtures/*/…yml`
//!   vaults — are picked up by treating a mention of such a fixture directory
//!   as naming a collection (see `fixture_dirs_naming_a_collection`).
//!
//! Deliberately coarse otherwise: it greps raw text rather than parsing Rust,
//! so it stays cheap. A false *positive* (e.g. the word `collection:`
//! appearing only in a doc comment) is resolved the same safe way as a true
//! one — add the env — so the failure mode of the heuristic is a harmless
//! extra `.env(...)`.
//!
//! Not covered on purpose: tests that spawn the binary without naming any
//! collection. Those still resolve a *generated* `<tempdir>-<hash>` name, which
//! cannot collide with a real collection, so they carry no data-loss risk (they
//! can still leave stray empty dirs in the cache root — a separate, non-
//! destructive cleanliness issue tracked on its own).

use std::fs;
use std::path::{Path, PathBuf};

/// A file matching either of these is spawning the real binary.
const SPAWNS_BINARY: [&str; 2] = ["cargo_bin(\"onebrain\")", "CARGO_BIN_EXE_onebrain"];

/// A file matching either of these is putting a collection name into a config
/// (or otherwise steering collection resolution).
const NAMES_A_COLLECTION: [&str; 2] = ["qmd_collection:", "collection:"];

/// The isolation every such file must carry.
const ISOLATION: &str = "ONEBRAIN_CACHE_DIR";

/// Files exempt from the rule, with a justification. Empty today — a new entry
/// needs a comment explaining why reaching the real cache root is safe there.
const ALLOWLIST: [&str; 0] = [];

/// Directory names under `tests/fixtures/` whose checked-in vault config names
/// a collection. A test that runs the binary against one of these inherits the
/// hazard without any collection string of its own appearing in the `.rs`
/// file — `fixtures/minimal_vault/vault.yml` carries `qmd_collection: ob-1`.
fn fixture_dirs_naming_a_collection(tests_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let fixtures = tests_dir.join("fixtures");
    let mut stack = vec![fixtures];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "yml" && ext != "yaml" {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if !NAMES_A_COLLECTION.iter().any(|m| text.contains(m)) {
                continue;
            }
            if let Some(name) = path.parent().and_then(|p| p.file_name()) {
                // Quoted: the scan looks for `fixture("minimal_vault")`, the
                // only shape that actually reaches the checked-in vault. A
                // bare substring would also hit local helpers that merely
                // share the name (`fn write_minimal_vault` builds its own
                // tempdir vault with no collection key — safe, and three
                // files have one).
                let name = format!("\"{}\"", name.to_string_lossy());
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// Cut a Rust source file into function-sized regions on top-level `fn `
/// boundaries. Integration tests declare everything at module scope, so a
/// line starting with `fn ` / `pub fn ` / `async fn ` opens a new region.
fn regions(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut start_line = 1usize;
    for (idx, line) in text.lines().enumerate() {
        let opens_fn = line.starts_with("fn ")
            || line.starts_with("pub fn ")
            || line.starts_with("async fn ")
            || line.starts_with("pub async fn ");
        if opens_fn && !current.is_empty() {
            out.push((start_line, std::mem::take(&mut current)));
            start_line = idx + 1;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        out.push((start_line, current));
    }
    out
}

/// The granularity regression itself, pinned on synthetic source so it can
/// never be "fixed" by editing the real tests: a file whose FIRST spawn is
/// isolated and whose SECOND is not passes the whole-file check — that is
/// exactly the shape that hid five live hazards — and must fail the per-region
/// one, naming the offending function.
#[test]
fn per_region_scan_catches_a_site_the_whole_file_check_missed() {
    let text = concat!(
        "fn isolated_site() {\n",
        "    fs::write(v.join(\"vault.yml\"), \"qmd_collection: safe\\n\").unwrap();\n",
        "    Command::cargo_bin(\"onebrain\")\n",
        "        .env(\"ONEBRAIN_CACHE_DIR\", cache.path())\n",
        "        .args([\"doctor\"]);\n",
        "}\n",
        "\n",
        "fn unisolated_site() {\n",
        "    fs::write(v.join(\"vault.yml\"), \"qmd_collection: ob-1\\n\").unwrap();\n",
        "    Command::cargo_bin(\"onebrain\").args([\"doctor\"]);\n",
        "}\n",
    );

    // The OLD rule — all three conditions against whole-file text — is green.
    assert!(SPAWNS_BINARY.iter().any(|m| text.contains(m)));
    assert!(NAMES_A_COLLECTION.iter().any(|m| text.contains(m)));
    assert!(
        text.contains(ISOLATION),
        "the whole-file check must PASS here — that is the bug being pinned"
    );

    // The NEW rule flags the one site the old one waved through.
    let offenders: Vec<String> = regions(text)
        .into_iter()
        .filter(|(_, r)| {
            SPAWNS_BINARY.iter().any(|m| r.contains(m))
                && NAMES_A_COLLECTION.iter().any(|m| r.contains(m))
                && !r.contains(ISOLATION)
        })
        .map(|(line, r)| format!("{line}:{r}"))
        .collect();
    assert_eq!(
        offenders.len(),
        1,
        "expected exactly the unisolated region to be flagged, got: {offenders:?}"
    );
    assert!(
        offenders[0].contains("fn unisolated_site"),
        "the wrong region was flagged: {}",
        offenders[0]
    );
}

#[test]
fn binary_invoking_tests_that_name_a_collection_pin_the_cache_dir() {
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let self_name = "cache_isolation_sweep.rs";
    assert!(
        tests_dir.join(self_name).is_file(),
        "sanity check failed: expected {tests_dir:?} to contain {self_name} — is \
         CARGO_MANIFEST_DIR/tests still this crate's integration-test dir?"
    );

    let fixture_dirs = fixture_dirs_naming_a_collection(&tests_dir);

    let mut scanned = 0usize;
    let mut regions_scanned = 0usize;
    let mut offenders = Vec::new();
    for entry in fs::read_dir(&tests_dir)
        .unwrap_or_else(|e| panic!("failed to read {tests_dir:?}: {e}"))
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == self_name || ALLOWLIST.contains(&name.as_str()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if !SPAWNS_BINARY.iter().any(|m| text.contains(m)) {
            continue;
        }
        let names_a_collection = |t: &str| {
            NAMES_A_COLLECTION.iter().any(|m| t.contains(m))
                || fixture_dirs.iter().any(|d| t.contains(d.as_str()))
        };
        if !names_a_collection(&text) {
            continue;
        }
        scanned += 1;
        // Whole-file pass — kept as the backstop for the one shape the
        // per-region pass cannot see (spawn in a helper, collection name in
        // the caller).
        if !text.contains(ISOLATION) {
            offenders.push(format!("  - tests/{name} (file: no isolation anywhere)"));
            continue;
        }
        // Per-region pass — this is the granularity that matters.
        for (line, region) in regions(&text) {
            if !SPAWNS_BINARY.iter().any(|m| region.contains(m)) {
                continue;
            }
            if !names_a_collection(&region) {
                continue;
            }
            regions_scanned += 1;
            if !region.contains(ISOLATION) {
                offenders.push(format!("  - tests/{name}:{line}"));
            }
        }
    }

    // Non-vacuity: the sweep must actually be looking at something. If a
    // refactor renames the test dir or the spawn helper, this catches it
    // instead of silently passing on zero files.
    assert!(
        scanned >= 5,
        "sweep matched only {scanned} file(s) — the detection strings are stale, \
         so this guard is no longer guarding anything"
    );
    assert!(
        regions_scanned >= 5,
        "sweep matched only {regions_scanned} function region(s) — the `fn ` split is \
         stale, so the per-call-site granularity is no longer guarding anything"
    );
    assert!(
        !fixture_dirs.is_empty(),
        "no tests/fixtures vault names a collection — the fixture arm of the scan is \
         stale (it exists because fixtures/minimal_vault/vault.yml carries one)"
    );

    assert!(
        offenders.is_empty(),
        "these integration tests spawn the binary against a named collection without \
         pinning {ISOLATION} — add `.env(\"{ISOLATION}\", <tempdir>)` to every \
         invocation (see tests/user_flows.rs for the helper shape):\n{}",
        offenders.join("\n")
    );
}
