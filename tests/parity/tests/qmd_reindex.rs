use parity::ParityRunner;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn qmd_reindex_parity_no_collection() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    runner.assert_parity_empty_stdout(&["qmd-reindex"], &fixture("qmd-no-collection"));
}
