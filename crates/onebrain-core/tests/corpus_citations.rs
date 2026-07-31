//! Every corpus fixture a scheduler doc comment cites must exist (#359).
//!
//! `schtasks.rs`'s module header states the repo's evidence contract:
//!
//! > Every claim in this module is backed by a measured corpus case in
//! > `tests/scheduler-corpus/windows/` that CI runs against a real Task
//! > Scheduler on every PR. Where a doc comment cites a filename, that file is
//! > the evidence.
//!
//! Nothing enforced it. `accept-multi-repetition.xml` went missing when PR #320
//! was closed unmerged — the 48-trigger work landed via #341 instead, and the
//! fixture only ever existed on the abandoned branch. The comment kept citing it
//! for two releases and every gate stayed green.
//!
//! A citation that resolves to nothing is worse than a broken link: the reader
//! believes a measurement was made and can be re-run, and neither is true. The
//! corpus exists precisely because two review rounds put blockers into this
//! design by READING the schema instead of measuring it.
//!
//! This is a test rather than a CI step on purpose — it runs in every job that
//! already runs `cargo test`, and on a developer's machine before the push.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Corpus fixtures are named `accept-*` / `reject-*` by convention, which is
/// what makes them recognisable in prose without a heuristic that fires on
/// every `.xml` in a sentence.
fn cited_fixtures(source: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for token in
        source.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
    {
        let is_fixture = (token.starts_with("accept-") || token.starts_with("reject-"))
            && (token.ends_with(".xml")
                || token.ends_with(".service")
                || token.ends_with(".timer")
                || token.ends_with(".expect"));
        if is_fixture {
            found.insert(token.to_string());
        }
    }
    found
}

#[test]
fn every_cited_corpus_fixture_exists() {
    let root = repo_root();
    let corpus = root.join("tests/scheduler-corpus");
    assert!(corpus.is_dir(), "corpus missing at {}", corpus.display());

    // Index the corpus once: name -> the directories it lives in.
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    for platform in ["linux", "windows", "macos"] {
        let dir = corpus.join(platform);
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
            let name = entry.expect("dir entry").file_name();
            on_disk.insert(name.to_string_lossy().to_string());
        }
    }
    assert!(
        !on_disk.is_empty(),
        "no corpus fixtures found — the guard would pass vacuously"
    );

    let src = root.join("crates/onebrain-core/src/scheduler");
    let mut missing: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&src).expect("read scheduler src") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        for fixture in cited_fixtures(&text) {
            checked += 1;
            if !on_disk.contains(&fixture) {
                missing.push(format!(
                    "{} cites {fixture}, which does not exist under tests/scheduler-corpus/",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }

    assert!(
        checked > 0,
        "no fixture citations found at all — either the contract was abandoned or \
         this guard's extractor stopped recognising them, and both mean it is no \
         longer checking anything"
    );
    assert!(
        missing.is_empty(),
        "{} broken evidence citation(s):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn the_extractor_recognises_a_citation_and_ignores_ordinary_prose() {
    // Without this, a regression that makes `cited_fixtures` return nothing
    // would turn the guard above into a vacuous pass — the exact failure mode
    // it exists to prevent.
    let found = cited_fixtures(
        "/// measured in `accept-triggers-048.xml` and reject-weekly-with-months.xml \
         but not in some-notes.xml, config.service, or the word accept-only",
    );
    assert!(found.contains("accept-triggers-048.xml"), "{found:?}");
    assert!(found.contains("reject-weekly-with-months.xml"), "{found:?}");
    assert!(!found.contains("some-notes.xml"), "{found:?}");
    assert!(!found.contains("config.service"), "{found:?}");
    assert_eq!(found.len(), 2, "{found:?}");
}
