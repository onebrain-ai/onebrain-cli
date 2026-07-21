//! Guard test: an integration test that **spawns the binary** must pin
//! `ONEBRAIN_CACHE_DIR`. No second condition — spawning is the whole trigger.
//!
//! ## Why spawning alone is the trigger
//!
//! `collection_name_readonly` resolves the collection from the vault config,
//! and `collection_cache_dir` joins it onto the *process-wide* search-cache
//! root — the developer's real `~/Library/Application Support/onebrain/search/`
//! (or its XDG/Windows equivalent) unless `ONEBRAIN_CACHE_DIR` says otherwise.
//!
//! Two distinct hazards live there, and this guard now covers both:
//!
//! 1. **Data loss.** A test that writes `qmd_collection: ob-1` into a tempdir
//!    vault and runs the binary unisolated is one name collision away from
//!    rewriting a real user's index: several commands call `Engine::open` on
//!    that directory whenever a collection of that name already exists, and
//!    `Engine::open` is not read-only — it migrates the layout, and on a schema
//!    mismatch it wipes and repopulates the keyword index.
//!
//! 2. **Unbounded growth in the real cache root** (issue #300). A test that
//!    spawns the binary WITHOUT naming a collection still resolves a generated
//!    `<tempdir-name>-<hash>` name — which cannot collide, so this file used to
//!    exclude it explicitly as "a separate, non-destructive cleanliness issue".
//!    That exclusion is what let #300 through. `tests/serve_bind_order.rs`
//!    spawned `onebrain serve` twice with no isolation, and the daemon's
//!    startup `create_dir_all(<cache>/<collection>/token)` materialized two
//!    permanent directories in the developer's real cache root per workspace
//!    test run. 125 dirs / 124 MB had accumulated by the time it was noticed.
//!    Every tempdir vault has a fresh random name, so the leak is unbounded by
//!    construction: nothing ever reuses or cleans those directories.
//!
//! The second hazard is why the collection condition is gone. "It only creates
//! junk, not damage" is a judgement about today's code paths, and it was
//! already wrong when it was written — a *test* has no business reaching the
//! developer's real state directory at all, whatever the binary does once it
//! gets there.
//!
//! ## Granularity, and what it does and does not see
//!
//! The scan is **per function**, not per file. A whole-file pass let ONE
//! isolated call site exempt every other spawn in the file — not theoretical:
//! it passed green over five sites that wrote a literal collection name into a
//! tempdir vault and then ran the binary against it (`doctor_integration.rs`,
//! `snapshots.rs`, `v31_integration.rs`). Planting a pristine collection under
//! one of those names and running even the **read-only** `doctor` migrated it
//! in place (`heading_path` default → script_aware, tantivy 2884 KB → 3320 KB)
//! while reporting `ok`.
//!
//! Regions are cut on top-level `fn ` boundaries, so each `#[test]` (and each
//! helper) is judged on its own text. The whole-file pass is kept alongside as
//! the backstop for a file where NO spawn is isolated.
//!
//! Deliberately coarse otherwise: it greps raw text rather than parsing Rust,
//! so it stays cheap. A false *positive* is resolved the same safe way as a
//! true one — add the env — so the failure mode of the heuristic is a harmless
//! extra `.env(...)`.
//!
//! ## The isolation to add
//!
//! `.env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())` on the spawn
//! (see `tests/support/mod.rs`), or any other path under the test's own
//! tempdir. When the binary is spawned indirectly (through `bash -c`, say),
//! put the env on the *outer* command — the child inherits it.
//!
//! ## The one thing this env does NOT let a test exercise
//!
//! `migration::default_state_dir` short-circuits on `ONEBRAIN_CACHE_DIR`, and
//! `migrate_search_cache` early-returns on it, so a test that pins it can never
//! exercise the cache→data migration (ADR 0021 / #114). No integration test
//! does today — that migration is covered by unit tests over `migrate_dir_once`,
//! which take path arguments and never touch the env. If one is ever written,
//! it belongs in `ALLOWLIST` with a comment, not in a blanket relaxation of
//! this rule.

use std::fs;
use std::path::PathBuf;

/// A file matching either of these is spawning the real binary.
const SPAWNS_BINARY: [&str; 2] = ["cargo_bin(\"onebrain\")", "CARGO_BIN_EXE_onebrain"];

/// The isolation every such file must carry.
const ISOLATION: &str = "ONEBRAIN_CACHE_DIR";

/// Files exempt from the rule, with a justification. A new entry needs a
/// comment explaining why NOT pinning `ONEBRAIN_CACHE_DIR` is safe there.
///
/// - `cache_root_untouched.rs` is the behavioral half of this same guard: its
///   entire subject is where the binary lands when nothing redirects it, so
///   pinning the env would erase what it measures. It is isolated instead by
///   redirecting `HOME` at a tempdir and asserting against that tempdir, which
///   is why it is `#[cfg(unix)]` — see its module docs.
///
/// The other foreseeable candidate is an end-to-end test of the cache→data
/// migration (ADR 0021 / #114), which cannot set `ONEBRAIN_CACHE_DIR` because
/// `migrate_search_cache` early-returns on it. None exists today.
const ALLOWLIST: [&str; 1] = ["cache_root_untouched.rs"];

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
        "    Command::cargo_bin(\"onebrain\")\n",
        "        .env(\"ONEBRAIN_CACHE_DIR\", cache.path())\n",
        "        .args([\"doctor\"]);\n",
        "}\n",
        "\n",
        "fn unisolated_site() {\n",
        "    Command::cargo_bin(\"onebrain\").args([\"doctor\"]);\n",
        "}\n",
    );

    // The whole-file check is green here — that is the bug being pinned.
    assert!(SPAWNS_BINARY.iter().any(|m| text.contains(m)));
    assert!(
        text.contains(ISOLATION),
        "the whole-file check must PASS here — that is the bug being pinned"
    );

    // The per-region rule flags the one site the whole-file one waved through.
    let offenders: Vec<String> = regions(text)
        .into_iter()
        .filter(|(_, r)| SPAWNS_BINARY.iter().any(|m| r.contains(m)) && !r.contains(ISOLATION))
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

/// The #300 widening, pinned the same way: a spawn that names NO collection is
/// an offender too. Under the old two-condition rule this text was clean —
/// that exemption is exactly what let `serve_bind_order.rs` leak two dirs into
/// the real cache root on every workspace test run.
#[test]
fn a_spawn_that_names_no_collection_is_still_an_offender() {
    let text = concat!(
        "fn spawns_without_naming_a_collection() {\n",
        "    let v = tempdir().unwrap();\n",
        "    Command::cargo_bin(\"onebrain\")\n",
        "        .args([\"serve\", \"--port\", \"0\"])\n",
        "        .current_dir(v.path());\n",
        "}\n",
    );

    // No collection string anywhere — the old rule's second condition.
    assert!(!text.contains("collection:"));
    assert!(!text.contains("qmd_collection:"));

    let offenders: Vec<String> = regions(text)
        .into_iter()
        .filter(|(_, r)| SPAWNS_BINARY.iter().any(|m| r.contains(m)) && !r.contains(ISOLATION))
        .map(|(line, _)| line.to_string())
        .collect();
    assert_eq!(
        offenders.len(),
        1,
        "a spawn with no collection name must still be flagged (#300)"
    );
}

#[test]
fn every_binary_invoking_test_pins_the_cache_dir() {
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let self_name = "cache_isolation_sweep.rs";
    assert!(
        tests_dir.join(self_name).is_file(),
        "sanity check failed: expected {tests_dir:?} to contain {self_name} — is \
         CARGO_MANIFEST_DIR/tests still this crate's integration-test dir?"
    );

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
        scanned += 1;
        // Whole-file pass — the backstop for the shape the per-region pass
        // cannot see (the spawn sits in a helper called from elsewhere).
        if !text.contains(ISOLATION) {
            offenders.push(format!("  - tests/{name} (file: no isolation anywhere)"));
            continue;
        }
        // Per-region pass — this is the granularity that matters.
        for (line, region) in regions(&text) {
            if !SPAWNS_BINARY.iter().any(|m| region.contains(m)) {
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
        scanned >= 15,
        "sweep matched only {scanned} file(s) — the detection strings are stale, \
         so this guard is no longer guarding anything"
    );
    assert!(
        regions_scanned >= 100,
        "sweep matched only {regions_scanned} function region(s) — the `fn ` split is \
         stale, so the per-call-site granularity is no longer guarding anything"
    );
    assert!(
        offenders.is_empty(),
        "these integration tests spawn the binary without pinning {ISOLATION} — add \
         `.env(\"{ISOLATION}\", support::scratch_cache_root())` to every invocation \
         (see tests/support/mod.rs); spawning is the whole trigger, naming a \
         collection is not required (#300):\n{}",
        offenders.join("\n")
    );
}
