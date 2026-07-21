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
/// while also needing a binding threaded into every chain. Collections are
/// named `<vault-dir>-<hash-of-abs-path>`, and every test builds its vault in a
/// fresh random tempdir, so two tests sharing this root still land in disjoint
/// collection directories.
///
/// It sits under `CARGO_TARGET_TMPDIR` (cargo's per-crate integration-test
/// scratch dir inside `target/`) so it is: never in the developer's home,
/// swept by `cargo clean`, and obvious in a `du` if it ever grows.
pub fn scratch_cache_root() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("search-cache");
    // Best-effort: consumers `create_dir_all` what they need anyway, and a
    // failure here must not panic a test for a reason unrelated to its subject.
    let _ = std::fs::create_dir_all(&dir);
    dir
}
