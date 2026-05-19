use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn checkpoint_reset_parity_empty_stdout() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    runner.assert_parity_empty_stdout(&["checkpoint", "reset"], &fixture("checkpoint-empty"));
}

#[test]
fn checkpoint_stop_fresh_parity_empty_stdout() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    // Fresh state · count=1 below threshold · no block emission · empty stdout
    runner.assert_parity_empty_stdout(&["checkpoint", "stop"], &fixture("checkpoint-empty"));
}
