//! `onebrain init` — thin wiring around `onebrain_fs::init::run_init`.
//!
//! When `--yes` is set, all prompts are skipped and the Essentials schedule
//! preset is installed. Without `--yes`, the binary supplies real `inquire`
//! backed closures via [`onebrain_fs::init::wizard`].

use anyhow::Result;
use onebrain_fs::init::{
    ask_initialize_here, ask_overwrite_vault_yml, ask_schedule_preset, run_init, ConfirmFn,
    InitOptions, PresetFn,
};
use std::cell::RefCell;
use std::path::Path;

pub fn run(yes: bool) -> Result<i32> {
    let opts = if yes {
        // Non-interactive — no prompts, Essentials preset, no force (so an
        // existing vault.yml still errors out without --force).
        InitOptions {
            yes: true,
            ..InitOptions::default()
        }
    } else {
        InitOptions {
            confirm_fn: Some(real_confirm_fn()),
            preset_fn: Some(real_preset_fn()),
            ..InitOptions::default()
        }
    };

    let result = run_init(opts)?;
    Ok(result.exit_code)
}

/// Real confirm closure — dispatches between "initialize here?" and
/// "vault.yml overwrite?" based on the question text. Both prompts are
/// surfaced via `inquire::Confirm`; an unparseable answer counts as "no".
fn real_confirm_fn() -> ConfirmFn {
    // Track which question we're on so we can pick the right helper.
    // `RefCell` is fine here — closure is `FnMut` so we own mutability.
    let state = RefCell::new(0_u32);
    Box::new(move |question: &str| {
        let mut n = state.borrow_mut();
        *n += 1;
        if question.starts_with("vault.yml already exists") {
            ask_overwrite_vault_yml()
        } else if question.starts_with("Initialize OneBrain vault here?") {
            // Pull the path out of the question text — defensive: the lib
            // formats it as "(path)" at the end. If we can't parse it, fall
            // through to the generic prompt helper which just re-asks.
            if let Some(open) = question.find('(') {
                let trimmed = &question[open + 1..];
                if let Some(close) = trimmed.find(')') {
                    let path = Path::new(&trimmed[..close]);
                    return ask_initialize_here(path);
                }
            }
            // Fallback — just emit the same question via inquire.
            onebrain_fs::init::ask_initialize_here(Path::new("."))
        } else {
            false
        }
    })
}

fn real_preset_fn() -> PresetFn {
    Box::new(ask_schedule_preset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_confirm_fn_routes_overwrite_question() {
        // We can't actually drive inquire from a unit test (no TTY) — but we
        // can at least construct the closure and verify it answers `false`
        // on an unknown question.
        let mut f = real_confirm_fn();
        assert!(!f("something totally unrelated"));
    }
}
