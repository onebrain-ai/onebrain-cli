//! Guard test (plan step 1.8): every transform registered in
//! `onebrain_token::transform::registry()` must have a non-empty fixtures
//! directory under `tests/fixtures/<name>/`, captured from real content —
//! not hand-typed toy strings. Same enforcement pattern as
//! `every_config_struct_key_has_a_doc_entry` (onebrain_yml.rs:684): the
//! registry is the single source of truth; this test fails the moment a
//! new transform lands without matching fixtures, instead of relying on
//! a human remembering to add them.

use std::fs;
use std::path::PathBuf;

use onebrain_token::transform::registry;

#[test]
fn every_registered_transform_has_fixture_tests() {
    let fixtures_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

    let mut missing = Vec::new();
    for t in registry() {
        let dir = fixtures_root.join(t.name());
        let has_files = fs::read_dir(&dir)
            .map(|entries| entries.filter_map(|e| e.ok()).count() > 0)
            .unwrap_or(false);
        if !has_files {
            missing.push(t.name());
        }
    }

    assert!(
        missing.is_empty(),
        "transform(s) missing tests/fixtures/<name>/ coverage: {missing:?}"
    );
}
