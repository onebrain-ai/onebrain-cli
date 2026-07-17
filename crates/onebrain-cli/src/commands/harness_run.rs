//! `onebrain harness run <PROMPT>` — spawn the chosen agent harness on an
//! ad-hoc prompt, headless. Sibling of `skill run`: same spawn machinery
//! (binary resolution, per-harness argv, `ONEBRAIN_HEADLESS=1`, in-place
//! spinner, captured-stream flush on exit) but the prompt is passed verbatim
//! rather than built from a `/onebrain:<skill>` reference.
//!
//! Two modes:
//!
//! - **with-context** (default) — `cwd = <vault>` and `--add-dir <vault>` /
//!   `--include-directories <vault>` so the harness loads OneBrain's
//!   `CLAUDE.md` / `INSTRUCTIONS.md` / `GEMINI.md`. Vault required (exit 78
//!   if `onebrain.yml` is missing under `context_dir`).
//! - **ad-hoc** — `cwd = $TMPDIR` (forced, so claude / gemini can't auto-walk-
//!   up from a vault subdir and silently re-load OneBrain's `CLAUDE.md` /
//!   `GEMINI.md`); no `--add-dir` / `--include-directories`. The harness
//!   answers the prompt with no OneBrain vault context attached, regardless
//!   of where the user invoked from. No vault required. (User-level config at
//!   `~/.claude/CLAUDE.md` still loads — that's separate from cwd-based
//!   project context.)
//!
//! Reads the prompt from stdin when the positional `<PROMPT>` is omitted, so
//! the command composes with `cat note.md | onebrain harness run --harness
//! gemini`.

use crate::cli::HarnessArg;
use crate::commands::run_skill::{harness_argv, spawn_harness};
use anyhow::{anyhow, Result};
use onebrain_core::find_config_file;
use onebrain_fs::{resolve_claude_bin, resolve_gemini_bin};
use std::io::Read;
use std::path::Path;

/// Entry point invoked from the dispatcher. Returns the exit code the binary
/// should call `std::process::exit` with — mirrors `run_skill::run`:
///
/// - `64` (EX_USAGE)  — empty prompt, or no positional and stdin is a TTY
///   (would otherwise block forever waiting for EOF)
/// - `78` (EX_CONFIG) — with-context + no OneBrain config at `context_dir`
///   (defensive: when invoked via the CLI dispatcher this is unreachable —
///   `vault_ctx::require` returns 64 first — but the inner check protects
///   direct callers and unit tests)
/// - `127`            — couldn't run/track the harness (spawn fail, wait fault)
/// - `128 + signal`   — child terminated by signal (Unix only)
/// - other            — propagated from child
///
/// `context_dir = Some(<vault>)` is with-context (vault-required); `None` is
/// ad-hoc (no vault check, no `--add-dir`).
pub fn run(
    cwd: &Path,
    context_dir: Option<&Path>,
    prompt: Option<&str>,
    harness: HarnessArg,
    model: Option<&str>,
    want_json: bool,
) -> Result<i32> {
    use std::io::IsTerminal;

    // With-context mode requires a usable vault under `context_dir`. Ad-hoc
    // mode skips the check entirely (no vault required).
    if let Some(dir) = context_dir {
        if find_config_file(dir).is_none() {
            eprintln!(
                "✗ Vault not found at {} (no onebrain.yml present)\n\
                 💡 run this from inside a vault, pass `--vault <path>` pointing at one, or use \
                 `--mode ad-hoc` if you don't need vault context",
                dir.display()
            );
            return Ok(78);
        }
    }

    // Resolve the prompt: positional > stdin. Stdin is only read when it is
    // NOT a TTY — `read_to_string` on an interactive terminal would block on
    // EOF, looking indistinguishable from a hang. Mirror `spawn_harness`'s
    // null-stdin guard on the parent side: if both the positional and a pipe
    // are absent, surface a usage hint instead of waiting forever. Refuse an
    // empty prompt — the harnesses would error on `-p ""` and we'd rather
    // surface a clear usage message than a cryptic harness error.
    let resolved_prompt = match prompt {
        Some(p) => p.to_string(),
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!(
                    "harness run: no prompt — pass it positionally (`onebrain harness run \"...\"`) or pipe it on stdin (`echo 'hi' | onebrain harness run`)"
                );
                return Ok(64); // EX_USAGE
            }
            read_stdin_prompt()?
        }
    };
    if resolved_prompt.trim().is_empty() {
        eprintln!(
            "harness run: empty prompt — pass it positionally (`onebrain harness run \"...\"`) or pipe it on stdin"
        );
        return Ok(64); // EX_USAGE
    }

    let env_lookup = |k: &str| std::env::var(k).ok();
    let path_exists = |p: &Path| p.exists();
    let home = std::env::var("HOME").ok();
    let resolution = match harness {
        HarnessArg::Claude => resolve_claude_bin(None, env_lookup, path_exists, home.as_deref()),
        HarnessArg::Gemini => resolve_gemini_bin(None, env_lookup, path_exists, home.as_deref()),
    };
    if let Some(warning) = &resolution.warning {
        // `warning`'s own text is kept stable (Bun-parity, see
        // `resolve_bin`'s doc comment) — only the wrapping hint is new.
        eprintln!("⚠ {warning}\n💡 fix or unset the env var above to silence this");
    }

    let context_str = context_dir.map(|p| p.to_string_lossy().into_owned());
    let argv = harness_argv(
        harness,
        &resolved_prompt,
        context_str.as_deref(),
        model,
        want_json,
    );
    spawn_harness(&resolution.path, &argv, cwd, harness, "the prompt")
}

/// Read the prompt from stdin. Errors only on a real IO failure — an empty
/// stdin yields `Ok("")` and the caller surfaces the usage hint.
fn read_stdin_prompt() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow!("harness run: failed to read prompt from stdin: {e}"))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// With-context mode + vault has no config → exit 78 before touching
    /// stdin or any harness.
    #[test]
    fn with_context_returns_78_when_vault_has_no_config() {
        let dir = tempdir().unwrap();
        let code = run(
            dir.path(),
            Some(dir.path()),
            Some("hi"),
            HarnessArg::Claude,
            None,
            false,
        )
        .unwrap();
        assert_eq!(code, 78);
    }

    /// Empty prompt → EX_USAGE (64), independent of the harness. Use a vault
    /// dir that exists so the config check passes first.
    #[test]
    fn with_context_empty_positional_prompt_is_rejected_with_64() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "qmd_collection: ob\n").unwrap();
        let code = run(
            dir.path(),
            Some(dir.path()),
            Some("   "),
            HarnessArg::Claude,
            None,
            false,
        )
        .unwrap();
        assert_eq!(code, 64);
    }

    /// Ad-hoc mode (context_dir = None) must NOT require a vault — the config
    /// check is skipped. Empty prompt still surfaces 64.
    #[test]
    fn ad_hoc_skips_vault_check_and_still_validates_prompt() {
        let dir = tempdir().unwrap();
        // No onebrain.yml. With-context would return 78; ad-hoc must NOT.
        let code = run(
            dir.path(),
            None,
            Some("   "),
            HarnessArg::Claude,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            code, 64,
            "ad-hoc should reach the empty-prompt branch (64), not the vault check (78)"
        );
    }
}
