use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn session_init_parity_empty_vault() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    runner.assert_parity(&["session-init"], &fixture("empty-vault"));
}

#[test]
fn session_init_parity_full_vault() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (set BUN_BINARY + RUST_BINARY)");
        return;
    }
    runner.assert_parity(&["session-init"], &fixture("full-vault"));
}
