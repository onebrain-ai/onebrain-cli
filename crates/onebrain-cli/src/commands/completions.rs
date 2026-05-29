//! `onebrain completions <SHELL>` — emit a shell completion script to stdout.
//! Hidden from `--help`; invoked by the Homebrew formula and surfaced by the
//! post-`init` hint.

use crate::cli::Cli;
use clap::CommandFactory;
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
