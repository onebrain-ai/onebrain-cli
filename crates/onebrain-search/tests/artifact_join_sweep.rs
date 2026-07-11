//! Guard test for [ADR 0027](../../../docs/decisions/0027-collection-cache-layout-split.md):
//! "no production code joins `\"tantivy\"` / `\"vectors\"` / `\"engine.redb\"` paths by
//! hand ... everything routes through [`CollectionLayout`](../src/layout.rs)". That
//! claim was previously enforced only by convention (a manual sweep at review time);
//! this test makes it a real, deterministic gate.
//!
//! Scope: this walks every crate's `src/` tree (not `tests/` dirs, which legitimately
//! build fake legacy layouts for fixtures) and skips `layout.rs` itself — the one file
//! that is *allowed* to know these literal names, since it's the resolver everything
//! else must route through. Scanning only `src/` sidesteps the unreliable problem of
//! excluding just the `#[cfg(test)]` module *inside* a file while still checking its
//! production code; if a future `src/` file legitimately needs one of these literals
//! (e.g. a test-only helper module that isn't worth splitting out), add it to
//! `ALLOWLIST` with a comment explaining why.

use std::fs;
use std::path::{Path, PathBuf};

/// Literal path-join fragments that must only ever appear in `layout.rs`. Any
/// production code that needs one of these artifacts must go through
/// `CollectionLayout::index_artifact` / `model_dir` instead of constructing the path
/// by hand.
const FORBIDDEN_JOINS: [&str; 3] = [
    "join(\"tantivy\")",
    "join(\"vectors\")",
    "join(\"engine.redb\")",
];

/// Explicit exceptions to the sweep, beyond `layout.rs` itself, keyed by path relative
/// to the workspace `crates/` dir (e.g. `"onebrain-search/src/foo.rs"`). Empty today —
/// only add an entry here, with a comment justifying it, if a genuinely new caller
/// needs one of the literals above outside the layout module.
const ALLOWLIST: [&str; 0] = [];

#[test]
fn no_stray_artifact_path_joins_outside_layout() {
    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let layout_rs = crates_dir.join("onebrain-search/src/layout.rs");
    assert!(
        layout_rs.is_file(),
        "sanity check failed: expected {:?} to exist — is CARGO_MANIFEST_DIR/.. still \
         the workspace crates/ dir?",
        layout_rs
    );

    let mut offenders = Vec::new();
    for crate_entry in fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("failed to read {:?}: {e}", crates_dir))
        .flatten()
    {
        let src_dir = crate_entry.path().join("src");
        if src_dir.is_dir() {
            visit_rust_sources(&src_dir, &crates_dir, &mut offenders);
        }
    }

    assert!(
        offenders.is_empty(),
        "found stray artifact-path joins outside layout.rs — route these through \
         CollectionLayout instead (see docs/decisions/0027-collection-cache-layout-split.md):\n{}",
        offenders.join("\n")
    );
}

/// Recursively visits every `.rs` file under `dir`, checking each against
/// [`FORBIDDEN_JOINS`] (skipping `layout.rs` and anything in [`ALLOWLIST`]).
fn visit_rust_sources(dir: &Path, crates_dir: &Path, offenders: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rust_sources(&path, crates_dir, offenders);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            check_file(&path, crates_dir, offenders);
        }
    }
}

/// #224 dedup guard: `search_common::index_size_bytes` and
/// `search_reindex::wipe_index_files` must enumerate
/// `onebrain_search::layout::INDEX_ARTIFACTS` rather than a private duplicate
/// of the same three literal names — a future 4th artifact added only to
/// `INDEX_ARTIFACTS` must be picked up by both size accounting and `--force`
/// wipe without a second edit (see issue #224 / R3 of the v3.4.9 #201 PR).
#[test]
fn size_and_wipe_do_not_duplicate_the_artifact_list() {
    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let targets = [
        "onebrain-cli/src/commands/search_common.rs",
        "onebrain-cli/src/commands/search_reindex.rs",
    ];
    let needle = "\"tantivy\", \"vectors\", \"engine.redb\"";
    for rel in targets {
        let path = crates_dir.join(rel);
        let contents =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"));
        assert!(
            !contents.contains(needle),
            "{rel} hardcodes the index-artifact list — consume \
             onebrain_search::layout::INDEX_ARTIFACTS instead (see issue #224)"
        );
    }
}

fn check_file(path: &Path, crates_dir: &Path, offenders: &mut Vec<String>) {
    if path.file_name().and_then(|n| n.to_str()) == Some("layout.rs") {
        return;
    }
    let rel = path
        .strip_prefix(crates_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if ALLOWLIST.contains(&rel.as_str()) {
        return;
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    for needle in FORBIDDEN_JOINS {
        if contents.contains(needle) {
            offenders.push(format!("{rel}: contains {needle}"));
        }
    }
}
