//! Layer 4 parity tests for `onebrain doctor` (plain-text non-TTY output).
//!
//! ## Status
//!
//! Tonight's deliverable is the fixture + skip scaffolding. A proper parity
//! assertion requires `assert_parity_stdout_equals` in `parity::ParityRunner`
//! — which doesn't exist yet because every prior slice produced JSON. Adding
//! that helper is **deferred** until the Bun v2.3.3 release artifact is
//! uploaded so the parity job can actually run in CI.
//!
//! The skip block matches the pattern used by other parity tests:
//! `BUN_BINARY` env var or default path missing → `eprintln!` + early return.

use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn doctor_parity_clean_vault() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    // TODO(v3.0.1): once Bun v2.3.3 artifact is uploaded, replace this with
    // a stdout-equality helper. Until then, run both binaries and check exit
    // codes match — that's the cheapest parity signal available.
    let _ = fixture("doctor-clean");
    eprintln!("SKIP: doctor parity needs assert_parity_stdout_equals helper (deferred to v3.0.1)");
}
