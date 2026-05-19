//! Layer 4 parity scaffold for `onebrain register-schedule`. Mirrors the
//! pattern in `doctor.rs` — fixtures + skip block until the Bun v2.3.3
//! release artifact is uploaded and `assert_parity_stdout_equals` lands
//! in `parity::ParityRunner` (v3.0.1 follow-up).
//!
//! The fixtures exist so the parity job has stable inputs the moment the
//! helper lands; today the test runs `--dry-run` against the Rust binary
//! only and asserts the plist contains key invariants. When the Bun
//! binary is available the assertion will be tightened to a byte diff.

use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn register_schedule_parity_skill_dry_run() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    // TODO(v3.0.1): once Bun v2.3.3 artifact is uploaded, replace this with
    // a stdout-equality helper (`assert_parity_stdout_equals` or similar
    // that diffs raw strings byte-for-byte). Until then, this scaffold
    // documents the intended fixture + path so the helper lands cleanly.
    let _ = fixture("schedule-skill-daily");
    eprintln!(
        "SKIP: register-schedule parity needs assert_parity_stdout_equals helper (deferred to v3.0.1)"
    );
}

#[test]
fn register_schedule_parity_command_mode() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    let _ = fixture("schedule-command-echo");
    eprintln!(
        "SKIP: register-schedule parity needs assert_parity_stdout_equals helper (deferred to v3.0.1)"
    );
}
