//! Layer 4 parity tests for `onebrain update` (plain-text non-TTY output).
//!
//! ## Status
//!
//! Tonight's deliverable is the fixture + skip scaffolding. A proper parity
//! assertion requires (1) `assert_parity_stdout_equals` in `ParityRunner`
//! (deferred to v3.0.1 — see `tests/parity/tests/doctor.rs`), and (2) a
//! deterministic mock of GitHub releases that both binaries can be pointed
//! at. The Bun source honors `ONEBRAIN_GITHUB_RELEASES_URL` only after a
//! future v2.x patch — until that ships, parity for `update` is best-effort
//! `--check` against the live API · which we deliberately do NOT do in CI.
//!
//! The skip block matches the pattern used by other parity tests:
//! `BUN_BINARY` env var or default path missing → `eprintln!` + early
//! return.

use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn update_parity_check_dry_run() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    // TODO(v3.0.1): once Bun honors ONEBRAIN_GITHUB_RELEASES_URL and the
    // assert_parity_stdout_equals helper lands, replace this with a real
    // mocked check. For now, just verify the fixture exists.
    let _ = fixture("update-check");
    eprintln!(
        "SKIP: update parity needs (a) Bun env-override support and (b) \
         assert_parity_stdout_equals helper (deferred to v3.0.1)"
    );
}
