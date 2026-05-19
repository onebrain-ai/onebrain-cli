use parity::ParityRunner;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn age_checkpoint(fixture_dir: &std::path::Path, name: &str, secs_before_now: u64) {
    let path = fixture_dir.join("checkpoint").join(name);
    let when = SystemTime::now() - Duration::from_secs(secs_before_now);
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(when))
        .expect("setting mtime failed");
}

#[test]
fn orphan_scan_parity_empty() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    runner.assert_parity(&["orphan-scan", ".", "currtok"], &fixture("orphan-empty"));
}

#[test]
fn orphan_scan_parity_stale() {
    let runner = ParityRunner::from_env();
    if !runner.binaries_available() {
        eprintln!("SKIP: parity binaries not available (BUN_BINARY / RUST_BINARY)");
        return;
    }
    let f = fixture("orphan-stale");
    age_checkpoint(&f, "2099-01-01-stale-checkpoint-01.md", 2 * 3600);
    runner.assert_parity(&["orphan-scan", ".", "currtok"], &f);
}
