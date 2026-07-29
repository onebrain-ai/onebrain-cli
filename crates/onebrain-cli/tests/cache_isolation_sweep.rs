//! Guard test: a test that **spawns the binary** must pin `ONEBRAIN_CACHE_DIR`.
//! No second condition — spawning is the whole trigger.
//!
//! ## What the sweep looks at (and the blind spot that used to be there)
//!
//! It scans BOTH `CARGO_MANIFEST_DIR/tests` and `CARGO_MANIFEST_DIR/src`,
//! recursively. `src/` is not decoration: until v3.4.17 the sweep scanned
//! exactly `tests/`, non-recursively, so the `#[cfg(test)]` modules inside
//! `src/` — which spawn the binary 12 times, all in `src/commands/daemon.rs`
//! — were outside its scope entirely, and 7 of those 9 test functions pinned
//! nothing. That blind spot hid a WORSE leak than the one #300 fixed: those
//! tests run `daemon start`, which spawns a detached `daemon __run` with
//! `hold_engine = true`, which reaches `server::internal::try_open_held_engine`
//! → `Engine::open` → a whole collection directory (`index/` + `models/`) in
//! the developer's real cache root, not the `token/`-only shape #300 leaked.
//! A guard that cannot see the worst path is not a guard, so the scan is
//! source-tree-wide now and the non-vacuity floors below are sized for the
//! larger corpus.
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
//! In `tests/`: `.env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())` on
//! the spawn (see `tests/support/mod.rs`), or any other path under the test's
//! own tempdir. When the binary is spawned indirectly (through `bash -c`, say),
//! put the env on the *outer* command — the child inherits it.
//!
//! In `src/commands/daemon.rs`: `.isolate_cache_root(cache.path())`, the
//! extension method that module defines for both `std::process::Command` and
//! `assert_cmd::Command` (plus `StopDaemonOnDrop::new`, which bakes the same
//! env into the teardown spawn). It is accepted here as an isolation marker in
//! its own right — see `ISOLATION` below for why that is a strengthening and
//! not a loophole.
//!
//! ## Why the helper also clears `XDG_DATA_HOME`
//!
//! `ONEBRAIN_CACHE_DIR` is dispositive on its own: `migration::default_state_dir`
//! returns it *before* consulting `dirs::data_dir()`, on every platform. But a
//! test that redirects only `HOME` and assumes that is enough is wrong on
//! Linux, where `dirs::data_dir()` reads `$XDG_DATA_HOME` first and `HOME`
//! redirection does not override it. `tests/cache_root_untouched.rs` — the one
//! allowlisted file, which deliberately cannot pin `ONEBRAIN_CACHE_DIR` —
//! already clears `XDG_DATA_HOME` for exactly that reason. Doing both inside
//! one helper is what makes it impossible for a call site to get the pair
//! half-right; that is the pattern-level fix, as opposed to auditing each of
//! the 12 spawn sites individually and hoping the next one copies the right
//! neighbour.
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
use std::path::{Path, PathBuf};

/// A file matching either of these is spawning the real binary.
const SPAWNS_BINARY: [&str; 2] = ["cargo_bin(\"onebrain\")", "CARGO_BIN_EXE_onebrain"];

/// The isolation every such region must carry — any ONE of these strings.
///
/// The first is the env var itself. The second is `daemon.rs`'s extension
/// method, which sets that env AND clears `XDG_DATA_HOME` in one call. Naming
/// the helper is a strengthening, not a loophole: the alternative is 12 call
/// sites each spelling out a two-part incantation, which is precisely the
/// shape that produced 7 unisolated sites in the first place. A new helper
/// belongs here only if it applies isolation unconditionally, with no
/// caller-supplied opt-out.
const ISOLATION: [&str; 2] = ["ONEBRAIN_CACHE_DIR", "isolate_cache_root"];

/// True when `text` carries any accepted isolation marker.
fn isolated(text: &str) -> bool {
    ISOLATION.iter().any(|m| text.contains(m))
}

/// True when `text` spawns the real binary.
fn spawns(text: &str) -> bool {
    SPAWNS_BINARY.iter().any(|m| text.contains(m))
}

/// Files exempt from the rule, with a justification. A new entry needs a
/// comment explaining why NOT pinning `ONEBRAIN_CACHE_DIR` is safe there.
///
/// Matched on FILE NAME, not path, and the scan now covers two trees — so an
/// entry exempts that base name under `tests/` and `src/` alike. Keep the
/// names distinctive for that reason.
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

/// Cut a Rust source file into function-sized regions on `fn ` boundaries.
///
/// Integration tests in `tests/` declare everything at module scope, but the
/// `src/` unit tests this sweep now also covers live inside `#[cfg(test)] mod
/// tests { … }` and are therefore indented, so the leading whitespace is
/// trimmed before the check. That makes the cut slightly coarser (a nested
/// helper `fn` inside a test function opens its own region), which is the safe
/// direction: more regions means more places that must carry the marker, never
/// fewer.
fn regions(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut start_line = 1usize;
    for (idx, line) in text.lines().enumerate() {
        let t = line.trim_start();
        let opens_fn = t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("pub(crate) fn ")
            || t.starts_with("async fn ")
            || t.starts_with("pub async fn ");
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
    assert!(spawns(text));
    assert!(
        isolated(text),
        "the whole-file check must PASS here — that is the bug being pinned"
    );

    // The per-region rule flags the one site the whole-file one waved through.
    let offenders: Vec<String> = regions(text)
        .into_iter()
        .filter(|(_, r)| spawns(r) && !isolated(r))
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
        .filter(|(_, r)| spawns(r) && !isolated(r))
        .map(|(line, _)| line.to_string())
        .collect();
    assert_eq!(
        offenders.len(),
        1,
        "a spawn with no collection name must still be flagged (#300)"
    );
}

/// The `src/` blind spot, pinned on synthetic source so it cannot be "fixed"
/// by editing the real tests: a spawn inside an indented `#[cfg(test)] mod
/// tests` block — the shape of every unit test in `src/` — must be cut into
/// its own region and judged. Under the pre-v3.4.17 splitter (which required
/// `fn ` at column 0) the whole module collapsed into ONE region, so a single
/// isolated sibling exempted every other spawn in the file.
#[test]
fn an_indented_unit_test_module_is_still_cut_per_function() {
    let text = concat!(
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    #[test]\n",
        "    fn isolated_site() {\n",
        "        Command::cargo_bin(\"onebrain\")\n",
        "            .isolate_cache_root(cache.path())\n",
        "            .args([\"daemon\", \"start\"]);\n",
        "    }\n",
        "\n",
        "    #[test]\n",
        "    fn unisolated_site() {\n",
        "        Command::cargo_bin(\"onebrain\").args([\"daemon\", \"start\"]);\n",
        "    }\n",
        "}\n",
    );

    // Whole-file green (the isolated sibling supplies the marker), and no
    // `fn ` at column 0 anywhere — the two conditions that made this invisible.
    assert!(isolated(text));
    assert!(!text.lines().any(|l| l.starts_with("fn ")));

    let offenders: Vec<String> = regions(text)
        .into_iter()
        .filter(|(_, r)| spawns(r) && !isolated(r))
        .map(|(_, r)| r)
        .collect();
    assert_eq!(
        offenders.len(),
        1,
        "the indented unisolated unit test must be flagged, got: {offenders:?}"
    );
    assert!(
        offenders[0].contains("fn unisolated_site"),
        "the wrong region was flagged: {}",
        offenders[0]
    );
}

/// Every `.rs` file under `root`, recursively, as (display-path, absolute path).
/// Display paths are relative to the crate root so offender lines read
/// `tests/foo.rs:12` / `src/commands/daemon.rs:2608`.
fn rust_files(crate_root: &Path, root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("failed to read {dir:?}: {e}"));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let rel = path
                    .strip_prefix(crate_root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.push((rel, path));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_binary_invoking_test_pins_the_cache_dir() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tests_dir = crate_root.join("tests");
    let src_dir = crate_root.join("src");
    let self_name = "cache_isolation_sweep.rs";
    assert!(
        tests_dir.join(self_name).is_file(),
        "sanity check failed: expected {tests_dir:?} to contain {self_name} — is \
         CARGO_MANIFEST_DIR/tests still this crate's integration-test dir?"
    );
    // The `src/` half of the scan is the whole point of the v3.4.17 widening;
    // if the crate layout moves, fail loudly instead of scanning nothing.
    assert!(
        src_dir.join("commands").join("daemon.rs").is_file(),
        "sanity check failed: expected {src_dir:?} to contain commands/daemon.rs — the \
         src/ half of this sweep is scanning the wrong tree"
    );

    let mut scanned = 0usize;
    let mut regions_scanned = 0usize;
    let mut src_regions = 0usize;
    let mut offenders = Vec::new();
    let mut files: Vec<(String, PathBuf)> = rust_files(&crate_root, &tests_dir);
    files.extend(rust_files(&crate_root, &src_dir));
    for (rel, path) in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == self_name || ALLOWLIST.contains(&name.as_str()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if !spawns(&text) {
            continue;
        }
        scanned += 1;
        // Whole-file pass — the backstop for the shape the per-region pass
        // cannot see (the spawn sits in a helper called from elsewhere).
        if !isolated(&text) {
            offenders.push(format!("  - {rel} (file: no isolation anywhere)"));
            continue;
        }
        // Per-region pass — this is the granularity that matters.
        for (line, region) in regions(&text) {
            if !spawns(&region) {
                continue;
            }
            regions_scanned += 1;
            if rel.starts_with("src") {
                src_regions += 1;
            }
            if !isolated(&region) {
                offenders.push(format!("  - {rel}:{line}"));
            }
        }
    }

    // Non-vacuity: the sweep must actually be looking at something. If a
    // refactor renames a scanned dir or the spawn helper, this catches it
    // instead of silently passing on zero files.
    //
    // Sized for the widened (tests/ + src/) corpus. Measured at the v3.4.17
    // widening: 30 files / 227 spawn regions, of which `src/` contributes 1
    // file (`commands/daemon.rs`) and 10 regions. The previous floors (15 /
    // 100) were slack enough that the ENTIRE `src/` half could vanish and both
    // still passed, which is how the blind spot stayed invisible — these sit
    // just under the real counts instead, so losing a root or a big test file
    // trips them. Deleting a couple of tests legitimately? Lower them in the
    // same commit, deliberately.
    assert!(
        scanned >= 28,
        "sweep matched only {scanned} file(s) — the detection strings are stale, or the \
         scan lost a root, so this guard is no longer guarding anything"
    );
    assert!(
        regions_scanned >= 215,
        "sweep matched only {regions_scanned} function region(s) — the `fn ` split is \
         stale, so the per-call-site granularity is no longer guarding anything"
    );
    // The `src/` half specifically: a floor on the whole corpus can be met by
    // `tests/` alone, so pin the half that was invisible until v3.4.17.
    assert!(
        src_regions >= 9,
        "sweep matched only {src_regions} spawn region(s) under src/ — the src/ half of \
         the scan (the #300 blind spot: daemon tests reaching Engine::open) is no longer \
         seeing anything"
    );
    assert!(
        offenders.is_empty(),
        "these tests spawn the binary without any of {ISOLATION:?} — add \
         `.env(\"ONEBRAIN_CACHE_DIR\", support::scratch_cache_root())` (tests/, see \
         tests/support/mod.rs) or `.isolate_cache_root(cache.path())` (src/, see \
         src/commands/daemon.rs) to every invocation; spawning is the whole trigger, \
         naming a collection is not required (#300):\n{}",
        offenders.join("\n")
    );
}

/// #305: the two isolation helpers are the single place the test-collection
/// marker var is exported — the engine stamps `.onebrain-test-collection`
/// into every collection it creates under that var, which is what makes any
/// future cache-root cleanup an enumeration instead of a name-based guess.
/// If either helper loses the export, collections silently go back to being
/// unmarked; pin the export at the source level, next to the sweep that pins
/// the helpers' use.
#[test]
fn both_isolation_helpers_export_the_test_collection_marker() {
    let marker = "ONEBRAIN_TEST_COLLECTION_MARKER";
    let support = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/mod.rs"),
    )
    .unwrap();
    let daemon = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/daemon.rs"),
    )
    .unwrap();
    assert!(
        support.contains(marker),
        "tests/support::scratch_cache_root no longer exports {marker}"
    );
    assert!(
        daemon.contains(marker),
        "daemon's isolate_cache_root no longer exports {marker}"
    );
}
