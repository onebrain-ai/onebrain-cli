//! `onebrain completions <SHELL>` — emit a shell completion script to stdout.
//! Hidden from `--help`; invoked by the Homebrew formula and surfaced by the
//! post-`init` hint.

use crate::cli::Cli;
use clap::CommandFactory;
use clap::ValueEnum;
use clap_complete::{generate, Shell};

/// Generate the completion script for `shell` to stdout. Returns the process
/// exit code (always 0 — clap already validated `shell` into a known variant).
pub fn run(shell: Shell) -> i32 {
    // `CommandFactory::command()` reconstructs the clap `Command` that the
    // derive macro built for `Cli`. `generate` needs `&mut Command` because it
    // finalizes the command tree (propagating help/version) before walking it.
    let mut cmd = Cli::command();
    let bin = cmd.get_name().to_string();
    generate(shell, &mut cmd, bin, &mut std::io::stdout());
    0
}

/// Map a login-shell path (the value of `$SHELL`, e.g. `/bin/zsh`) to a
/// `clap_complete::Shell`. Returns `None` when unset or unrecognized.
///
/// Takes the path as an argument (rather than reading the env itself) so the
/// detection logic is unit-testable without mutating process env.
pub fn detect_login_shell_from(shell_env: Option<&str>) -> Option<Shell> {
    let path = shell_env?;
    let name = path.trim().trim_end_matches('/').rsplit('/').next().unwrap_or(path).trim();
    // 2-arg form selects ValueEnum::from_str (ignore_case=true), NOT std FromStr (1-arg, case-sensitive).
    Shell::from_str(name, true).ok()
}

/// Build the optional "Shell completions" hint shown after interactive
/// `onebrain init`. When the login shell is detected it is dropped straight
/// into a copy-pasteable command; otherwise a placeholder + the supported
/// shell list is shown.
pub fn hint_line(detected: Option<Shell>) -> String {
    match detected {
        Some(shell) => format!(
            "💡 Shell completions (optional):\n   onebrain completions {shell}"
        ),
        None => "💡 Shell completions (optional):\n   onebrain completions <shell>   \
                 (bash · zsh · fish · powershell · elvish)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_zsh_from_login_path() {
        assert_eq!(detect_login_shell_from(Some("/bin/zsh")), Some(Shell::Zsh));
    }

    #[test]
    fn detects_bash_from_usr_path() {
        assert_eq!(detect_login_shell_from(Some("/usr/bin/bash")), Some(Shell::Bash));
    }

    #[test]
    fn returns_none_when_unset() {
        assert_eq!(detect_login_shell_from(None), None);
    }

    #[test]
    fn returns_none_for_unknown_shell() {
        assert_eq!(detect_login_shell_from(Some("/bin/tcsh")), None);
    }

    #[test]
    fn detects_bare_name_without_path() {
        assert_eq!(detect_login_shell_from(Some("zsh")), Some(Shell::Zsh));
    }

    #[test]
    fn detects_case_insensitively() {
        assert_eq!(detect_login_shell_from(Some("/bin/ZSH")), Some(Shell::Zsh));
    }

    #[test]
    fn tolerates_trailing_slash_and_whitespace() {
        assert_eq!(detect_login_shell_from(Some("/bin/zsh/")), Some(Shell::Zsh));
        assert_eq!(detect_login_shell_from(Some("  /bin/fish  ")), Some(Shell::Fish));
    }

    #[test]
    fn hint_uses_detected_shell() {
        let h = hint_line(Some(Shell::Zsh));
        assert!(h.contains("onebrain completions zsh"), "got: {h}");
        assert!(!h.contains("<shell>"), "should not show placeholder when detected");
    }

    #[test]
    fn hint_falls_back_to_placeholder() {
        let h = hint_line(None);
        assert!(h.contains("onebrain completions <shell>"), "got: {h}");
        assert!(h.contains("bash"), "fallback lists shells");
    }
}
