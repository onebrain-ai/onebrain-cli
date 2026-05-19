//! Layer 4 parity tests for `onebrain register-hooks`.
//!
//! ## Status
//!
//! Scaffold only — register-hooks mutates `.claude/settings.json` on disk and
//! prints a short status summary on stdout. A proper parity assertion needs
//! a helper that runs both binaries against the same fixture (in throwaway
//! copies of the fixture dir so the Rust run does not see Bun's prior write),
//! then diffs the resulting `settings.json` byte-for-byte. That helper
//! doesn't exist yet — `assert_parity_settings_json_equals` is **deferred**
//! until the Bun v2.3.3 release artifact is uploaded so CI can actually run
//! the parity job.
//!
//! The skip block mirrors the convention in other parity tests:
//! `BUN_BINARY` / `RUST_BINARY` env vars or default paths missing →
//! `eprintln!` + early return.

use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn register_hooks_parity_fresh_vault() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    let _ = fixture("register-hooks-fresh");
    eprintln!(
        "SKIP: register-hooks parity needs assert_parity_settings_json_equals helper (deferred)"
    );
}

#[test]
fn register_hooks_parity_legacy_stop_migration() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    let _ = fixture("register-hooks-legacy-stop");
    eprintln!(
        "SKIP: register-hooks parity needs assert_parity_settings_json_equals helper (deferred)"
    );
}

#[test]
fn register_hooks_parity_qmd_collection_adds_post_tool_use() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    let _ = fixture("register-hooks-qmd-collection");
    eprintln!(
        "SKIP: register-hooks parity needs assert_parity_settings_json_equals helper (deferred)"
    );
}
