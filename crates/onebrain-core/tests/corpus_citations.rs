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
///
/// Bare names count too, and that is not a nicety. The first version of this
/// guard required a file extension — and `accept-multi-repetition`, the exact
/// fixture whose disappearance motivated #359, is cited TWICE in bare form. The
/// guard would have covered every citation style except the one that actually
/// failed. A citation is also written `accept-generated-escaping.{service,timer,xml}`,
/// which tokenises with a trailing dot, so that is normalised away.
///
/// Excluded: a token ending in `-`, which is what a format-string prefix like
/// `accept-generated-{name}.xml` leaves behind. That is a name being BUILT, not
/// a claim about a file.
fn cited_fixtures(source: &str) -> BTreeSet<String> {
    const EXTS: [&str; 4] = [".xml", ".service", ".timer", ".expect"];
    let mut found = BTreeSet::new();
    for raw in
        source.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
    {
        let token = raw.trim_end_matches('.');
        if !(token.starts_with("accept-") || token.starts_with("reject-")) {
            continue;
        }
        if token.ends_with('-') {
            continue;
        }
        // Either an exact filename, or a stem naming the fixture family.
        if EXTS.iter().any(|e| token.ends_with(e)) || !token.contains('.') {
            found.insert(token.to_string());
        }
    }
    found
}

/// Does this citation resolve? An exact filename must exist; a bare stem must
/// name a file, or a FAMILY of them — `accept-monthly` legitimately refers to
/// the `accept-monthly-with-months` / `accept-monthly-dayofweek` pair.
///
/// Bare-name detection is a heuristic and cannot be otherwise: `accept-cases`
/// in running prose is indistinguishable from a fixture name by pattern alone.
/// This errs toward FLAGGING, on the view that in a repo where `accept-*` names
/// evidence files, prose that reads like one is ambiguous to a human too and
/// should be reworded — which is what was done rather than widening the rule
/// until it stopped catching things.
fn resolves(citation: &str, on_disk: &BTreeSet<String>) -> bool {
    if citation.contains('.') {
        return on_disk.contains(citation);
    }
    let family = format!("{citation}-");
    on_disk.iter().any(|f| {
        let stem = f.rsplit_once('.').map(|(s, _)| s).unwrap_or(f.as_str());
        stem == citation || stem.starts_with(&family)
    })
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

    // Both crates. The renderers cite fixtures, and so does the CLI comment
    // that explains why the register-time character ban was removed — a
    // citation this guard did not cover until the fixture it names was added
    // by the same PR that widened the scan.
    let sources = [
        root.join("crates/onebrain-core/src/scheduler"),
        root.join("crates/onebrain-cli/src/commands"),
    ];
    let mut missing: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for src in &sources {
        for entry in std::fs::read_dir(src).expect("read source dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for fixture in cited_fixtures(&text) {
                checked += 1;
                if !resolves(&fixture, &on_disk) {
                    missing.push(format!(
                        "{} cites {fixture}, which does not exist under tests/scheduler-corpus/",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                }
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
         and the accept-multi-repetition pair and accept-generated-escaping.{service,timer} \
         but not in some-notes.xml, config.service, or format!(\"accept-generated-{name}.xml\")",
    );
    // exact filenames
    assert!(found.contains("accept-triggers-048.xml"), "{found:?}");
    assert!(found.contains("reject-weekly-with-months.xml"), "{found:?}");
    // bare stem — the form the fixture that motivated #359 was cited in
    assert!(found.contains("accept-multi-repetition"), "{found:?}");
    // brace list: tokenises with a trailing dot, normalised back to the stem
    assert!(found.contains("accept-generated-escaping"), "{found:?}");
    // not citations
    assert!(!found.contains("some-notes.xml"), "{found:?}");
    assert!(!found.contains("config.service"), "{found:?}");
    assert!(
        !found.iter().any(|f| f.ends_with('-')),
        "a format-string prefix is a name being built, not a claim: {found:?}"
    );
    assert_eq!(found.len(), 4, "{found:?}");
}

#[test]
fn a_bare_stem_resolves_only_when_a_file_is_named_after_it() {
    let on_disk: BTreeSet<String> = [
        "accept-multi-repetition.xml",
        "accept-generated-daily.expect",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert!(resolves("accept-multi-repetition", &on_disk));
    assert!(resolves("accept-multi-repetition.xml", &on_disk));
    assert!(!resolves("accept-multi-repetition.timer", &on_disk));
    assert!(!resolves("accept-never-existed", &on_disk));
    // a family reference resolves through its members
    assert!(resolves("accept-generated", &on_disk));
    assert!(
        !resolves("accept-gen", &on_disk),
        "prefix must stop at a hyphen"
    );
}
