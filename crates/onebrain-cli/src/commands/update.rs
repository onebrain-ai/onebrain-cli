//! `onebrain update` — thin wiring around `onebrain_fs::update::run_update`.
//!
//! v3.0 ships non-TTY plain-text output only. TTY pretty-printing
//! (spinners, color, ascii banner) is deferred to v3.0.1 once the
//! cli-banner / cli-ui equivalents land in the Rust tree.

use anyhow::Result;
use onebrain_fs::update::{run_update, UpdateOptions};

/// Run `onebrain update`. Returns the exit code; the caller is responsible
/// for `std::process::exit`. Mirrors the `doctor` pattern from Slice 6.
pub fn run(check: bool) -> Result<i32> {
    let opts = UpdateOptions {
        check,
        ..Default::default()
    };
    let result = run_update(opts);
    Ok(result.exit_code)
}
