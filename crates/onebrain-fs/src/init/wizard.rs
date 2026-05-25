//! Interactive prompts for `onebrain init`.
//!
//! The real implementations call out to `inquire`. The wizard surface is
//! kept small so injectable closures in [`super::InitOptions`] can replace
//! every prompt in tests. Tests never touch this module directly.

use crate::init::presets::SchedulePreset;
use inquire::{Confirm, Select};
use std::io::IsTerminal;
use std::path::Path;

/// Default confirmation prompt — used by the CLI when no injection is
/// provided. Returns `true` on yes, `false` on no/Esc/IO error (treating
/// "couldn't ask" as a refusal so we never run destructive ops on a stuck
/// stdin).
///
/// **Closed-stdin / non-TTY contract:** when stdin is not a terminal (piped,
/// closed, or redirected from /dev/null), short-circuit to `false` rather
/// than dispatching to `inquire::Confirm::prompt()`. On Unix `prompt()`
/// would EOF-error → `unwrap_or(false)`, but on Windows it can block
/// indefinitely waiting on a closed handle — the explicit guard makes the
/// behaviour cross-platform deterministic.
pub fn default_confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    Confirm::new(question)
        .with_default(false)
        .prompt()
        .unwrap_or(false)
}

/// Convenience: the initial "Initialize OneBrain vault here?" prompt
/// includes the resolved path in the question text.
pub fn ask_initialize_here(vault_dir: &Path) -> bool {
    let q = format!("Initialize OneBrain vault here? ({})", vault_dir.display());
    default_confirm(&q)
}

/// "onebrain.yml already exists. Overwrite?" (function name kept stable
/// for binary-crate callers; the prompt text now reflects the canonical
/// filename).
pub fn ask_overwrite_vault_yml() -> bool {
    default_confirm("onebrain.yml already exists. Overwrite?")
}

/// Non-empty-directory safety prompt. The caller prints the multi-line
/// context block to stderr (so the user sees the target path and a summary
/// of what's already there); this helper reads the y/N answer with the
/// default set to "no" so a stray Enter keypress aborts.
///
/// Non-TTY stdin → silent `false` (same closed-stdin contract as
/// [`default_confirm`]).
pub fn ask_continue_nonempty(context: &str) -> bool {
    // Emit the multi-line context above the y/N prompt so the user reviews
    // it before answering. Stderr keeps stdout reserved for the envelope.
    eprintln!("⚠️  {context}");
    if !std::io::stdin().is_terminal() {
        return false;
    }
    Confirm::new("Continue?")
        .with_default(false)
        .prompt()
        .unwrap_or(false)
}

/// Schedule preset picker — defaults to Essentials. Returns `Skip` on any
/// failure to read stdin (defensive: never auto-install a schedule a user
/// didn't actively accept), including a closed / non-TTY stdin.
pub fn ask_schedule_preset() -> SchedulePreset {
    if !std::io::stdin().is_terminal() {
        return SchedulePreset::Skip;
    }
    let options: Vec<&'static str> = SchedulePreset::all().iter().map(|p| p.label()).collect();
    let result = Select::new("Install a schedule preset?", options)
        .with_starting_cursor(1) // Essentials = index 1
        .prompt();
    match result {
        Ok(label) => SchedulePreset::all()
            .iter()
            .copied()
            .find(|p| p.label() == label)
            .unwrap_or(SchedulePreset::Skip),
        Err(_) => SchedulePreset::Skip,
    }
}
