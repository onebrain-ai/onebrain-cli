//! Shared helpers for the `onebrain-cli` integration tests.
//!
//! Lives in a subdirectory so cargo does not compile it as its own test
//! binary; each test file pulls it in with a plain `mod support;`.

#![allow(dead_code)]

use std::path::PathBuf;

/// The scratch search-cache root every spawned `onebrain` binary must be
/// pinned to via `ONEBRAIN_CACHE_DIR` — see `tests/cache_isolation_sweep.rs`
/// for the rule and `tests/cache_root_untouched.rs` for the regression guard.
///
/// Why a shared root rather than a per-test `TempDir`: several tests spawn the
/// binary more than once and expect state written by the first spawn to be
/// visible to the second, and a `TempDir` bound per call site would break that
/// while also needing a binding threaded into every chain.
///
/// **What makes the shared root safe is its LOCATION**, not any disjointness
/// of its contents: it sits under `CARGO_TARGET_TMPDIR`, inside `target/`, so
/// it is never in the developer's home, is swept by `cargo clean`, and is
/// obvious in a `du` if it ever grows. That is the whole argument. Anything
/// written here is junk in a build directory; nothing written here can damage
/// real user state.
///
/// **Collection names in this root are NOT guaranteed disjoint.** An earlier
/// version of this comment claimed they were — "every test builds its vault in
/// a fresh random tempdir, so collection names are disjoint" — and both halves
/// of that are wrong:
///
/// - Not every vault is a tempdir. `checkpoint.rs` runs the binary against the
///   checked-in fixture `tests/fixtures/checkpoint/empty_vault/` (which has a
///   `vault.yml`), and `harness.rs` / `session_init.rs` likewise use fixed
///   fixture paths. A fixture path is stable across runs and identical across
///   tests, so its `<dir>-<hash-of-abs-path>` name is deterministic and
///   SHARED, not unique.
/// - The scope claim was too narrow: `CARGO_TARGET_TMPDIR` is one directory
///   per PACKAGE, shared by all of the crate's integration-test binaries,
///   which cargo runs concurrently — so this root is shared across parallel
///   processes as well as across tests within one binary.
///
/// It is benign today only because those fixture-based tests resolve no
/// collection, or never touch the collection directory. So the invariant to
/// hold is a rule, not a fact: **a test that (a) runs against a fixed path
/// rather than a fresh tempdir AND (b) writes into the collection dir must not
/// use this shared root** — give it its own `TempDir` cache root, or a
/// collection name unique to that test. Two such tests would otherwise race
/// over one index, with nothing in the failure pointing at the cause.
pub fn scratch_cache_root() -> PathBuf {
    // #305: export the test-collection marker for every binary this test
    // process spawns — the engine then stamps `.onebrain-test-collection`
    // into any collection dir it creates, making residue enumerable instead
    // of guessable. Process-wide on purpose: children inherit it no matter
    // which helper (or bare `.env`) wired the cache root.
    std::env::set_var("ONEBRAIN_TEST_COLLECTION_MARKER", "1");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("search-cache");
    // Best-effort: consumers `create_dir_all` what they need anyway, and a
    // failure here must not panic a test for a reason unrelated to its subject.
    let _ = std::fs::create_dir_all(&dir);
    dir
}
