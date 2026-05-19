//! Layer 4 parity tests for `onebrain run-skill`.
//!
//! ## Status
//!
//! Tonight's deliverable is the fixture + skip scaffolding. A real parity
//! assertion needs `assert_parity_argv_equals` in `parity::ParityRunner` —
//! both binaries must spawn a mock `claude` and we compare the recorded
//! argv. The mock-claude harness is wider than this slice; the helper is
//! **deferred** until the Bun v2.3.3 artifact is uploaded and the parity
//! job can actually run in CI.
//!
//! Skip block matches the pattern used by `doctor.rs`: env-driven binary
//! paths · `eprintln!` + early return when either binary is missing.

use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn run_skill_parity_clean_vault() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    // TODO(v3.0.1): once Bun v2.3.3 artifact is uploaded, replace this with
    // an argv-equality helper backed by a shared mock claude script. Until
    // then, fixture exists and skip block keeps CI green.
    let _ = fixture("run-skill-clean");
    eprintln!("SKIP: run-skill parity needs assert_parity_argv_equals helper (deferred to v3.0.1)");
}
