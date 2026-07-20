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
//! Deliberately coarse: it greps raw file text rather than parsing Rust, so
//! it stays cheap and has no false negatives from an unusual call shape. A
//! false *positive* (e.g. the word `collection:` appearing only in a doc
//! comment) is resolved the same safe way as a true one — add the env — so
//! the failure mode of the heuristic is a harmless extra `.env(...)`.
//!
//! Not covered on purpose: tests that spawn the binary without naming any
//! collection. Those still resolve a *generated* `<tempdir>-<hash>` name, which
//! cannot collide with a real collection, so they carry no data-loss risk (they
//! can still leave stray empty dirs in the cache root — a separate, non-
//! destructive cleanliness issue tracked on its own).

use std::fs;
use std::path::PathBuf;

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

#[test]
fn binary_invoking_tests_that_name_a_collection_pin_the_cache_dir() {
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let self_name = "cache_isolation_sweep.rs";
    assert!(
        tests_dir.join(self_name).is_file(),
        "sanity check failed: expected {tests_dir:?} to contain {self_name} — is \
         CARGO_MANIFEST_DIR/tests still this crate's integration-test dir?"
    );

    let mut scanned = 0usize;
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
        if !NAMES_A_COLLECTION.iter().any(|m| text.contains(m)) {
            continue;
        }
        scanned += 1;
        if !text.contains(ISOLATION) {
            offenders.push(name);
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
        offenders.is_empty(),
        "these integration tests spawn the binary against a named collection without \
         pinning {ISOLATION} — add `.env(\"{ISOLATION}\", <tempdir>)` to every \
         invocation (see tests/user_flows.rs for the helper shape):\n{}",
        offenders
            .iter()
            .map(|f| format!("  - tests/{f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
