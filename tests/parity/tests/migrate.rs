//! Layer 4 parity tests for `onebrain migrate` (plain-text non-TTY output).
//!
//! ## Status
//!
//! Like `doctor`, `migrate` emits plain text — not JSON — so a true
//! parity assertion needs an `assert_parity_stdout_equals` helper that does
//! not exist yet on `parity::ParityRunner` (every prior slice produced JSON).
//! That helper is deferred until the Bun v2.3.3 release artifact is uploaded
//! and the parity CI job becomes meaningful.
//!
//! The fixtures (`migrate-empty/`, `migrate-backfill/`) are shipped now so
//! the parity job can wire up byte-for-byte stdout comparison the moment the
//! helper lands.

use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn migrate_parity_empty_logs() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    let _ = fixture("migrate-empty");
    eprintln!("SKIP: migrate parity needs assert_parity_stdout_equals helper (deferred to v3.0.1)");
}

#[test]
fn migrate_parity_backfill_one_session() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    let _ = fixture("migrate-backfill");
    eprintln!("SKIP: migrate parity needs assert_parity_stdout_equals helper (deferred to v3.0.1)");
}
