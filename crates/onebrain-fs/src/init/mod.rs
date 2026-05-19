//! `onebrain init` core — port of
//! `~/projects/onebrain/src/commands/init.ts`.
//!
//! Steps performed (in order):
//!
//! 1. Resolve `vault_dir` (default = current working directory).
//! 2. Guard against an existing `vault.yml` (unless `--force` or interactive
//!    overwrite confirmation).
//! 3. (Interactive only) prompt "Initialize here?".
//! 4. (Interactive only) pick a [`SchedulePreset`].
//! 5. Write `vault.yml`.
//! 6. Create 8 PARA folders + `00-inbox/imports/`.
//! 7. Call `register-hooks` (best-effort — failure is warned, never fatal).
//! 8. Emit summary lines.
//!
//! All four IO surfaces are injectable through [`InitOptions`]:
//!   - `confirm_fn`: guards against vault.yml overwrites + "init here?" prompt.
//!     When `None`, the wizard runs non-interactively (errors out on existing
//!     vault.yml without `--force`).
//!   - `preset_fn`: returns the chosen [`SchedulePreset`]. When `None`,
//!     defaults to [`SchedulePreset::Skip`].
//!   - `register_hooks_fn`: hook into Slice 7's register-hooks library. When
//!     `None`, calls `onebrain_fs::register_hooks::run`.
//!   - `stdout_lines` / `stderr_lines`: line-oriented output sinks (mirrors
//!     Slice 11's `update.rs`). When `None`, writes to real stdout/stderr.

mod folders;
mod presets;
mod vault_yml;
mod wizard;

pub use folders::{INBOX_IMPORTS_SUBDIR, STANDARD_FOLDERS};
pub use presets::{ScheduleEntry, SchedulePreset};

use crate::error::FsError;
use crate::register_hooks::{self, RegisterHooksOptions};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Closure: ask the user a yes/no question.
pub type ConfirmFn = Box<dyn FnMut(&str) -> bool + Send>;
/// Closure: ask the user to pick a [`SchedulePreset`].
pub type PresetFn = Box<dyn FnMut() -> SchedulePreset + Send>;
/// Closure: invoke `register-hooks` for the given vault. Returns `true` on
/// success, `false` on warning (non-fatal).
pub type RegisterHooksFn = Box<dyn FnMut(&Path) -> bool + Send>;
/// Line-oriented sink — receives one line per call without trailing newline.
type LineSink = Box<dyn FnMut(&str) + Send>;

/// Options driving [`run_init`]. Defaults model a non-interactive run with
/// no preset selection and real `register-hooks`.
#[derive(Default)]
pub struct InitOptions {
    /// Vault root. Defaults to `std::env::current_dir()`.
    pub vault_dir: Option<PathBuf>,
    /// Overwrite existing `vault.yml` without prompting.
    pub force: bool,
    /// Non-interactive: accept all defaults (preset = Essentials, no prompts).
    pub yes: bool,
    /// Injectable confirmation prompt. `None` = non-interactive (no prompts).
    pub confirm_fn: Option<ConfirmFn>,
    /// Injectable preset picker. `None` = no preset (`Skip`).
    pub preset_fn: Option<PresetFn>,
    /// Injectable register-hooks runner. `None` = call the real lib.
    pub register_hooks_fn: Option<RegisterHooksFn>,
    /// Optional stdout sink — defaults to real stdout when `None`.
    pub stdout_lines: Option<LineSink>,
    /// Optional stderr sink — defaults to real stderr when `None`.
    pub stderr_lines: Option<LineSink>,
}

/// Result of an [`run_init`] invocation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub ok: bool,
    pub exit_code: i32,
    /// Human-readable diagnostic. Set on non-TTY error paths so callers can
    /// surface a single line to the user.
    pub message: Option<String>,
    pub folders_created: usize,
    pub vault_yml_written: bool,
    pub preset_installed: Option<SchedulePreset>,
    pub hooks_registered: bool,
    /// True when the user declined an interactive prompt and we exited
    /// cleanly (exit 0, no changes).
    pub aborted: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the init workflow. See module docs for the step list.
///
/// Errors from filesystem ops bubble up as [`FsError`]; the caller should
/// translate them to an exit code. Soft warnings (register-hooks failure)
/// are written to the stderr sink and reflected in
/// [`InitResult::hooks_registered`] but do NOT produce an error.
pub fn run_init(mut opts: InitOptions) -> Result<InitResult, FsError> {
    let vault_dir = opts
        .vault_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd readable"));

    let mut result = InitResult::default();
    let mut stdout = take_stdout_sink(&mut opts.stdout_lines);
    let mut stderr = take_stderr_sink(&mut opts.stderr_lines);

    stdout("OneBrain Init");

    // ── Step 1: vault.yml guard ────────────────────────────────────────────
    let vault_yml_path = vault_dir.join("vault.yml");
    let vault_yml_exists = vault_yml_path.exists();

    if vault_yml_exists && !opts.force {
        // Interactive: ask the user. Non-interactive: error out.
        if let Some(confirm) = opts.confirm_fn.as_mut() {
            let overwrite = confirm("vault.yml already exists. Overwrite?");
            if !overwrite {
                result.ok = true;
                result.exit_code = 0;
                result.aborted = true;
                stdout("aborted: vault.yml left unchanged");
                return Ok(result);
            }
        } else {
            let msg = "vault.yml exists. Re-run with --force to overwrite.".to_string();
            stdout(&msg);
            result.ok = false;
            result.exit_code = 1;
            result.message = Some(msg);
            return Ok(result);
        }
    }

    // ── Step 2: directory confirmation (interactive only) ──────────────────
    // Only prompt when interactive AND no --force AND no --yes (force/yes
    // imply "user knows what they're doing").
    if let Some(confirm) = opts.confirm_fn.as_mut() {
        if !opts.force && !opts.yes {
            let q = format!("Initialize OneBrain vault here? ({})", vault_dir.display());
            if !confirm(&q) {
                result.ok = true;
                result.exit_code = 0;
                result.aborted = true;
                stdout("aborted: no vault created");
                return Ok(result);
            }
        }
    }

    // ── Step 3: schedule preset ────────────────────────────────────────────
    let preset = if opts.yes {
        SchedulePreset::Essentials
    } else if let Some(picker) = opts.preset_fn.as_mut() {
        picker()
    } else {
        SchedulePreset::Skip
    };

    // ── Step 4: write vault.yml ────────────────────────────────────────────
    vault_yml::write_vault_yml(&vault_dir, preset)?;
    result.vault_yml_written = true;
    stdout("vault.yml: written");

    // ── Step 5: folders ────────────────────────────────────────────────────
    let n = folders::create_folders(&vault_dir)?;
    result.folders_created = n;
    stdout(&format!("folders: {n} created"));

    // ── Step 6: schedule preset summary ────────────────────────────────────
    if preset != SchedulePreset::Skip {
        let entry_count = preset.entries().len();
        stdout(&format!(
            "preset: {} ({entry_count} entries)",
            preset.label()
        ));
        result.preset_installed = Some(preset);
    }

    // ── Step 7: register-hooks (best-effort) ───────────────────────────────
    let hooks_ok = match opts.register_hooks_fn.as_mut() {
        Some(f) => f(&vault_dir),
        None => default_register_hooks(&vault_dir, &mut stderr),
    };
    result.hooks_registered = hooks_ok;
    stdout(&format!(
        "hooks: {}",
        if hooks_ok {
            "ok"
        } else {
            "warning — run onebrain register-hooks to retry"
        }
    ));

    // ── Done ───────────────────────────────────────────────────────────────
    stdout("done: run /onboarding in Claude to finish setup");
    result.ok = true;
    result.exit_code = 0;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn take_stdout_sink(sink: &mut Option<LineSink>) -> Box<dyn FnMut(&str)> {
    if let Some(s) = sink.take() {
        let mut s = s;
        Box::new(move |line: &str| s(line))
    } else {
        Box::new(|line: &str| println!("{line}"))
    }
}

fn take_stderr_sink(sink: &mut Option<LineSink>) -> Box<dyn FnMut(&str)> {
    if let Some(s) = sink.take() {
        let mut s = s;
        Box::new(move |line: &str| s(line))
    } else {
        Box::new(|line: &str| eprintln!("{line}"))
    }
}

fn default_register_hooks(vault_dir: &Path, stderr: &mut dyn FnMut(&str)) -> bool {
    let opts = RegisterHooksOptions {
        vault_dir: Some(vault_dir.to_path_buf()),
        ..Default::default()
    };
    match register_hooks::run(opts) {
        Ok(r) => r.ok,
        Err(e) => {
            stderr(&format!("init: register-hooks warning: {e}"));
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Re-export wizard fns for the CLI binary
// ---------------------------------------------------------------------------

pub use wizard::{ask_initialize_here, ask_overwrite_vault_yml, ask_schedule_preset};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    fn capture_sink() -> (LineSink, Arc<Mutex<Vec<String>>>) {
        let buf = Arc::new(Mutex::new(Vec::<String>::new()));
        let buf_clone = buf.clone();
        let sink: LineSink = Box::new(move |line: &str| {
            buf_clone.lock().unwrap().push(line.to_string());
        });
        (sink, buf)
    }

    /// Helper: build an `InitOptions` for tests that skips register-hooks
    /// entirely (otherwise the lib hits the harness detector on real disk).
    fn test_opts(vault_dir: &Path) -> (InitOptions, Arc<Mutex<Vec<String>>>) {
        let (stdout_sink, stdout_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(vault_dir.to_path_buf()),
            stdout_lines: Some(stdout_sink),
            register_hooks_fn: Some(Box::new(|_dir: &Path| true)),
            ..Default::default()
        };
        (opts, stdout_buf)
    }

    #[test]
    fn fresh_vault_creates_folders_and_vault_yml() {
        let d = tempdir().unwrap();
        let (opts, stdout_buf) = test_opts(d.path());

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.folders_created, 9);
        assert!(r.vault_yml_written);
        assert!(r.hooks_registered);
        assert!(r.preset_installed.is_none()); // no preset_fn → Skip

        for folder in STANDARD_FOLDERS {
            assert!(d.path().join(folder).is_dir());
        }
        assert!(d.path().join("00-inbox").join("imports").is_dir());
        assert!(d.path().join("vault.yml").is_file());

        let lines = stdout_buf.lock().unwrap();
        assert_eq!(lines[0], "OneBrain Init");
        assert!(lines.iter().any(|l| l == "vault.yml: written"));
        assert!(lines.iter().any(|l| l == "folders: 9 created"));
        assert!(lines.iter().any(|l| l.contains("done")));
    }

    #[test]
    fn existing_vault_yml_no_force_no_confirm_returns_exit_1() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();
        let (opts, _stdout_buf) = test_opts(d.path());

        let r = run_init(opts).unwrap();
        assert!(!r.ok);
        assert_eq!(r.exit_code, 1);
        let msg = r.message.unwrap();
        assert!(msg.contains("vault.yml exists"));
        assert!(msg.contains("--force"));

        // Folders not created
        assert!(!d.path().join("00-inbox").is_dir());
        // Original vault.yml not touched
        let content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert_eq!(content, "old: value\n");
    }

    #[test]
    fn force_overwrites_existing_vault_yml() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.force = true;

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        let content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(content.contains("update_channel: stable"));
        assert!(!content.contains("old:"));
    }

    #[test]
    fn yes_flag_picks_essentials_preset() {
        let d = tempdir().unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.yes = true;

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.preset_installed, Some(SchedulePreset::Essentials));

        let content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(content.contains("schedule:"));
        assert!(content.contains("/daily"));
        assert!(content.contains("/weekly"));
        assert!(content.contains("/recap"));
    }

    #[test]
    fn confirm_fn_overwrite_no_aborts_cleanly() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "old: value\n").unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.confirm_fn = Some(Box::new(|_q: &str| false));

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.exit_code, 0);
        assert!(r.aborted);

        // vault.yml not touched
        let content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert_eq!(content, "old: value\n");
        // Folders not created
        assert!(!d.path().join("00-inbox").is_dir());
    }

    #[test]
    fn confirm_fn_directory_no_aborts_before_creating_anything() {
        let d = tempdir().unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        // Counter to track which question we're on
        let count = Rc::new(RefCell::new(0_u32));
        // FnMut closures with shared state — can't use Rc/RefCell with Send,
        // so use a plain Mutex<u32> + Box::new + move.
        let counter = Arc::new(Mutex::new(0_u32));
        let counter_clone = counter.clone();
        opts.confirm_fn = Some(Box::new(move |_q: &str| {
            let mut c = counter_clone.lock().unwrap();
            *c += 1;
            false // always decline
        }));
        let _ = count; // silence unused

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(r.aborted);
        assert!(!d.path().join("vault.yml").exists());
        assert!(!d.path().join("00-inbox").is_dir());
        // Only the "initialize here?" prompt fires (no vault.yml so no
        // overwrite prompt).
        assert_eq!(*counter.lock().unwrap(), 1);
    }

    #[test]
    fn preset_fn_minimal_writes_one_schedule_entry() {
        let d = tempdir().unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.preset_fn = Some(Box::new(|| SchedulePreset::Minimal));

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.preset_installed, Some(SchedulePreset::Minimal));

        let content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        let entries = content.matches("cron:").count();
        assert_eq!(entries, 1);
    }

    #[test]
    fn register_hooks_failure_warns_but_init_still_ok() {
        let d = tempdir().unwrap();
        let (stdout_sink, _stdout_buf) = capture_sink();
        let (stderr_sink, stderr_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(d.path().to_path_buf()),
            stdout_lines: Some(stdout_sink),
            stderr_lines: Some(stderr_sink),
            register_hooks_fn: Some(Box::new(|_dir: &Path| false)),
            ..Default::default()
        };

        let r = run_init(opts).unwrap();
        // Init still succeeds — hooks failure is a warning, not an error
        assert!(r.ok);
        assert_eq!(r.exit_code, 0);
        assert!(!r.hooks_registered);
        let _ = stderr_buf; // unused but kept for symmetry
    }

    #[test]
    fn existing_folders_not_double_counted() {
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("00-inbox")).unwrap();
        std::fs::create_dir_all(d.path().join("01-projects")).unwrap();
        let (opts, _stdout_buf) = test_opts(d.path());

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        // 9 total - 2 already existing (and inbox/imports still counted as
        // new since it didn't exist) = 7
        assert_eq!(r.folders_created, 7);
    }

    #[test]
    fn stdout_starts_with_onebrain_init() {
        let d = tempdir().unwrap();
        let (opts, stdout_buf) = test_opts(d.path());
        let _ = run_init(opts).unwrap();
        let lines = stdout_buf.lock().unwrap();
        assert!(!lines.is_empty());
        assert_eq!(lines[0], "OneBrain Init");
    }

    #[test]
    fn force_skips_directory_confirmation() {
        let d = tempdir().unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.force = true;
        let counter = Arc::new(Mutex::new(0_u32));
        let counter_clone = counter.clone();
        opts.confirm_fn = Some(Box::new(move |_q: &str| {
            *counter_clone.lock().unwrap() += 1;
            true
        }));

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        // confirm_fn must NOT have been called (--force bypasses both prompts)
        assert_eq!(*counter.lock().unwrap(), 0);
    }
}
