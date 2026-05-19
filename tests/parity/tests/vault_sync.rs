//! Parity scaffold for `vault-sync`. Both Bun and Rust binaries are run against
//! an isolated temp vault with a mock tarball fixture; the resulting non-TTY
//! stdout and the on-disk filesystem are compared.
//!
//! This is wired against the upstream Bun release artifact (BUN_BINARY). The
//! test is `#[ignore]` until v3.0.0-alpha.1 ships the matching Bun build —
//! same approach as other slices' parity tests.

#[test]
#[ignore = "wired in v3.0.0-alpha.1 — requires upstream BUN_BINARY"]
fn vault_sync_parity_happy_path() {
    // Intentionally empty body. The parity harness is set up here per the
    // slice-13 plan — once BUN_BINARY is available on CI the body fills in:
    //   1. build a mock tarball
    //   2. run BUN_BINARY vault-sync against vault_a
    //   3. run RUST_BINARY vault-sync against vault_b
    //   4. assert step-line stdout matches byte-for-byte
    //   5. assert file trees under .claude/plugins/onebrain/ match
}
