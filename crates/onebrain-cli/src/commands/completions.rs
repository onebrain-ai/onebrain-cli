//! `onebrain completions <SHELL>` — emit a shell completion script to stdout.
//! Hidden from `--help`; invoked by the Homebrew formula and surfaced by the
//! post-`init` hint.

use crate::cli::Cli;
use clap::CommandFactory;
use clap::ValueEnum;
use clap_complete::{generate, Shell};

/// Rebuild `src` keeping only non-hidden subcommands, recursively. clap_complete's
/// aot generators emit every subcommand (they don't honor `hide`), and clap has no
/// remove-subcommand API — so we reconstruct the tree. Only the config that affects
/// completion output is copied (about/version/args/visible aliases); the auto
/// `help`/`version` args are skipped so clap re-adds `help` without a duplicate-id panic.
fn visible_tree(src: &clap::Command) -> clap::Command {
    let mut out = clap::Command::new(src.get_name().to_string());
    if let Some(about) = src.get_about() {
        out = out.about(about.clone());
    }
    if let Some(version) = src.get_version() {
        out = out.version(version.to_string());
    }
    for arg in src.get_arguments() {
        let id = arg.get_id().as_str();
        if id == "help" || id == "version" {
            continue; // clap auto-adds help; avoid duplicate-definition panic
        }
        out = out.arg(arg.clone());
    }
    for alias in src.get_visible_aliases() {
        out = out.visible_alias(alias.to_string());
    }
    for sub in src.get_subcommands().filter(|s| !s.is_hide_set()) {
        out = out.subcommand(visible_tree(sub));
    }
    out
}

/// Generate the completion script for `shell` to stdout. Returns the process
/// exit code (always 0 — clap already validated `shell` into a known variant).
pub fn run(shell: Shell) -> i32 {
    // `CommandFactory::command()` reconstructs the clap `Command` that the
    // derive macro built for `Cli`. clap_complete's aot generators don't honor
    // `#[command(hide = true)]`, so we regenerate from a tree with hidden
    // subcommands recursively stripped (`visible_tree`). `bin` is taken from the
    // original command's name (`onebrain`) so the script binds to the real binary.
    let full = Cli::command();
    let bin = full.get_name().to_string();
    let mut cmd = visible_tree(&full);
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
    let name = path
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim();
    // 2-arg form selects ValueEnum::from_str (ignore_case=true), NOT std FromStr (1-arg, case-sensitive).
    Shell::from_str(name, true).ok()
}

/// Build the optional "Shell completions" hint shown after interactive
/// `onebrain init`. When the login shell is detected it is dropped straight
/// into a copy-pasteable command; otherwise a placeholder + the supported
/// shell list is shown.
pub fn hint_line(detected: Option<Shell>) -> String {
    match detected {
        Some(shell) => format!("💡 Shell completions (optional):\n   onebrain completions {shell}"),
        None => "💡 Shell completions (optional):\n   onebrain completions <shell>   \
                 (bash · zsh · fish · powershell · elvish)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Command;

    fn sample() -> Command {
        Command::new("app")
            .subcommand(Command::new("visible-a").subcommand(Command::new("kid")))
            .subcommand(Command::new("hidden-top").hide(true))
            .subcommand(
                Command::new("group")
                    .subcommand(Command::new("vis-verb"))
                    .subcommand(Command::new("hid-verb").hide(true)),
            )
    }

    fn names(cmd: &Command) -> Vec<String> {
        cmd.get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect()
    }

    #[test]
    fn visible_tree_drops_hidden_top_level() {
        let t = visible_tree(&sample());
        let n = names(&t);
        assert!(n.contains(&"visible-a".to_string()));
        assert!(n.contains(&"group".to_string()));
        assert!(
            !n.contains(&"hidden-top".to_string()),
            "hidden top-level kept: {n:?}"
        );
    }

    #[test]
    fn visible_tree_drops_hidden_nested_verbs() {
        let t = visible_tree(&sample());
        let group = t
            .get_subcommands()
            .find(|c| c.get_name() == "group")
            .unwrap();
        let verbs = names(group);
        assert!(verbs.contains(&"vis-verb".to_string()));
        assert!(
            !verbs.contains(&"hid-verb".to_string()),
            "hidden nested verb kept: {verbs:?}"
        );
    }

    #[test]
    fn visible_tree_keeps_deep_visible_children() {
        let t = visible_tree(&sample());
        let a = t
            .get_subcommands()
            .find(|c| c.get_name() == "visible-a")
            .unwrap();
        assert!(names(a).contains(&"kid".to_string()));
    }

    #[test]
    fn detects_zsh_from_login_path() {
        assert_eq!(detect_login_shell_from(Some("/bin/zsh")), Some(Shell::Zsh));
    }

    #[test]
    fn detects_bash_from_usr_path() {
        assert_eq!(
            detect_login_shell_from(Some("/usr/bin/bash")),
            Some(Shell::Bash)
        );
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
        assert_eq!(
            detect_login_shell_from(Some("  /bin/fish  ")),
            Some(Shell::Fish)
        );
    }

    #[test]
    fn hint_uses_detected_shell() {
        let h = hint_line(Some(Shell::Zsh));
        assert!(h.contains("onebrain completions zsh"), "got: {h}");
        assert!(
            !h.contains("<shell>"),
            "should not show placeholder when detected"
        );
    }

    #[test]
    fn hint_falls_back_to_placeholder() {
        let h = hint_line(None);
        assert!(h.contains("onebrain completions <shell>"), "got: {h}");
        assert!(h.contains("bash"), "fallback lists shells");
    }
}
