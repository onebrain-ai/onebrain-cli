//! Context shared by every scheduler backend: what to run, where the vault and
//! logs live, and the uid the launchd renderer needs.
//!
//! Was `launchd::LaunchdContext`. Renamed as part of the cross-platform seam —
//! the Windows and Linux backends take the same inputs, so the type cannot keep
//! a platform in its name.

use std::path::PathBuf;

/// Emit inputs for any backend's artifact renderer.
pub struct SchedulerContext {
    /// Absolute path to the vault root (passed as `--vault` in skill-mode artifacts).
    pub vault_path: PathBuf,

    /// Absolute path to the `onebrain` binary the scheduler should exec.
    pub skill_cli_path: String,

    /// Absolute path to the scheduler log directory.
    pub log_base_path: PathBuf,

    /// User homedir — drives each backend's artifact location.
    pub homedir: PathBuf,

    /// User's effective UID.
    ///
    /// **Deliberately NOT `#[cfg]`-gated**, though it is only meaningful to
    /// launchd. It is read by `one_shot_skill_block` and `one_shot_command_block`
    /// (`launchd.rs:321`, `:377`), which `generate_plist` dispatches to with no
    /// platform gate — and that rendering must compile everywhere so its
    /// snapshot tests run on any dev machine rather than only in CI.
    ///
    /// Gating the field would therefore break `cargo build` on `ubuntu-latest`
    /// and `windows-latest`, taking out two of three `test` matrix legs plus
    /// `clippy`, the lex-only job, and `coverage`. A `u32` on every platform
    /// costs nothing; gating it costs the build.
    pub uid: u32,
}

#[cfg(test)]
mod tests {
    use super::SchedulerContext;
    use std::path::PathBuf;

    #[test]
    fn context_carries_uid_on_every_platform() {
        let c = SchedulerContext {
            vault_path: PathBuf::from("/vault"),
            skill_cli_path: "/opt/homebrew/bin/onebrain".to_string(),
            log_base_path: PathBuf::from("/vault/07-logs/scheduler"),
            homedir: PathBuf::from("/home/u"),
            uid: 501,
        };
        // The assertion is not the value — it is that this file compiles and
        // reads `uid` on Linux and Windows too.
        assert_eq!(c.uid, 501);
    }
}
