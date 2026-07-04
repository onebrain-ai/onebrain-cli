//! `onebrain qmd …` was removed in v3.4.5 (native search replaces it). It
//! must fail with a helpful "use `onebrain search …`" message, not a bare
//! clap "unrecognized subcommand", and not a panic.

use std::process::Command;

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .args(args)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

#[test]
fn qmd_status_is_removed_with_helpful_error() {
    let (ok, msg) = run(&["qmd", "status"]);
    assert!(!ok, "removed command must exit non-zero; output: {msg}");
    assert!(msg.contains("removed"), "expected 'removed' in: {msg}");
    assert!(
        msg.contains("onebrain search"),
        "expected search hint in: {msg}"
    );
}

#[test]
fn bare_qmd_is_removed_with_helpful_error() {
    let (ok, msg) = run(&["qmd"]);
    assert!(!ok);
    assert!(msg.contains("onebrain search"), "output: {msg}");
}

#[test]
fn qmd_reindex_subcommand_is_removed() {
    let (ok, msg) = run(&["qmd", "reindex"]);
    assert!(!ok);
    assert!(msg.contains("onebrain search"), "output: {msg}");
}
