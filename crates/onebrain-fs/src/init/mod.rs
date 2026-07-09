//! `onebrain init` core — port of
//! `~/projects/onebrain/src/commands/init.ts`.
//!
//! Steps performed (in order):
//!
//! 1. Resolve `vault_dir` (default = current working directory).
//! 2. Guard against an existing config file (`onebrain.yml` or legacy
//!    `vault.yml`) unless `--force` or interactive overwrite confirmation.
//! 3. (Interactive only) prompt "Initialize here?".
//! 4. (Interactive only) pick a [`SchedulePreset`].
//! 5. Write `onebrain.yml`.
//! 6. Create 8 PARA folders + `00-inbox/imports/`.
//! 7. Call `register-hooks` (best-effort — failure is warned, never fatal).
//! 8. Emit summary lines.
//!
//! All four IO surfaces are injectable through [`InitOptions`]:
//!   - `confirm_fn`: guards against config-file overwrites + "init here?" prompt.
//!     When `None`, the wizard runs non-interactively (errors out on existing
//!     config file without `--force`).
//!   - `preset_fn`: returns the chosen [`SchedulePreset`]. When `None`,
//!     defaults to [`SchedulePreset::Skip`].
//!   - `register_hooks_fn`: hook into Slice 7's register-hooks library. When
//!     `None`, calls `onebrain_fs::register_hooks::run`.
//!   - `stdout_lines` / `stderr_lines`: line-oriented output sinks (mirrors
//!     Slice 11's `update.rs`). When `None`, writes to real stdout/stderr.

mod enable_plugin;
mod folders;
mod marketplace;
mod onebrain_yml;
mod presets;
mod safety;
mod wizard;

pub use folders::{INBOX_IMPORTS_SUBDIR, STANDARD_FOLDERS};
pub use onebrain_yml::{
    config_key_docs, render_onebrain_yml, ConfigKeyDoc, TEMPLATE_RERANK_MIN_SCORE,
};
pub use presets::{ScheduleEntry, SchedulePreset};

use crate::error::FsError;
use crate::harness::detect_harnesses;
use crate::register_hooks::{self, RegisterHooksOptions};
use crate::vault_sync::{run_vault_sync, VaultSyncOptions};
use onebrain_core::{CoreError, Harness, CONFIG_FILENAME, LEGACY_CONFIG_FILENAME};
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
/// Closure: invoke `vault-sync` for the given vault. Returns `true` on
/// success, `false` on warning (non-fatal — init still exits 0). Tests
/// inject a stub that returns true without hitting the network.
pub type VaultSyncFn = Box<dyn FnMut(&Path) -> bool + Send>;
/// Line-oriented sink — receives one line per call without trailing newline.
type LineSink = Box<dyn FnMut(&str) + Send>;

/// Options driving [`run_init`]. Defaults model a non-interactive run with
/// no preset selection and real `register-hooks`.
#[derive(Default)]
pub struct InitOptions {
    /// Vault root. Defaults to `std::env::current_dir()`.
    pub vault_dir: Option<PathBuf>,
    /// Overwrite existing `onebrain.yml` (or legacy `vault.yml`) without
    /// prompting.
    pub force: bool,
    /// Non-interactive: accept all defaults (preset = Essentials, no prompts).
    pub yes: bool,
    /// Injectable confirmation prompt. `None` = non-interactive (no prompts).
    pub confirm_fn: Option<ConfirmFn>,
    /// Injectable preset picker. `None` = no preset (`Skip`).
    pub preset_fn: Option<PresetFn>,
    /// Injectable register-hooks runner. `None` = call the real lib.
    pub register_hooks_fn: Option<RegisterHooksFn>,
    /// Injectable vault-sync runner. `None` = call the real lib. Tests should
    /// inject a closure that returns true without hitting the network.
    pub vault_sync_fn: Option<VaultSyncFn>,
    /// Skip the vault-sync sub-step entirely (offline init or test runs).
    pub skip_vault_sync: bool,
    /// Structured-output mode (json/yaml/table/tsv). When `true`, the
    /// non-empty-directory safety check refuses to prompt interactively and
    /// instead returns a [`CoreError::InitTargetNotEmpty`] error so the
    /// envelope renderer surfaces a canonical machine-readable error.
    pub structured_output: bool,
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
    /// True when the embedded vault-sync sub-call ran AND succeeded · the
    /// fresh vault has plugin files installed. False when skipped, failed, or
    /// not attempted (offline init).
    pub vault_sync_ok: bool,
    /// True when `<vault>/.claude-plugin/marketplace.json` was newly written
    /// by this run. False on re-init (file pre-existed) or non-Claude harness
    /// (gemini/direct — manifest not needed).
    pub marketplace_written: bool,
    /// True when `enabledPlugins.onebrain@onebrain` was newly inserted/updated
    /// in `.claude/settings.json`. False on re-init (key already true) or
    /// non-Claude harness.
    pub plugin_enabled: bool,
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

    // The "OneBrain Init" header lands on stdout AFTER the structured-output
    // safety check so a `--json` failure leaves stdout as a single
    // canonical envelope. Text-mode runs still see the header on the next
    // emit (immediately after the safety check passes).

    // ── Step 0: target-directory safety check ──────────────────────────────
    // Guards against running `onebrain init` in the wrong directory. Always
    // runs safety::classify so permission errors / "target is a file" /
    // missing-parent paths surface as a proper FsError BEFORE any partial
    // writes. `--force` only suppresses the interactive PROMPT (B2 / SF-H4);
    // it does not bypass the classification itself.
    match safety::classify(&vault_dir)? {
        safety::DirState::Missing => {
            std::fs::create_dir_all(&vault_dir).map_err(|e| FsError::Io {
                path: vault_dir.clone(),
                source: e,
            })?;
        }
        safety::DirState::Empty | safety::DirState::OneBrainVault => {
            // Empty: nothing to guard. OneBrainVault: vault.yml present,
            // hand off to Step 1's overwrite logic.
        }
        safety::DirState::NonEmptyNonVault { summary } => {
            if opts.force {
                // --force: classification succeeded, skip the prompt and
                // proceed (caller knows what they're doing).
            } else {
                let target_display = vault_dir.display().to_string();
                if opts.structured_output {
                    // Machine consumer can't respond to a TTY prompt —
                    // surface the canonical error envelope so the caller
                    // gets `E_INIT_TARGET_NOT_EMPTY` exit 75 + a message
                    // pointing at `--force`.
                    return Err(FsError::Core(CoreError::InitTargetNotEmpty(format!(
                        "target is not empty ({summary}) at {target_display} · pass --force to proceed"
                    ))));
                }
                if let Some(confirm) = opts.confirm_fn.as_mut() {
                    let q = format!(
                        "Target directory is not empty:\n    {target_display}\n    Contents: {summary}\n\nOneBrain will create folders (00-inbox/, 01-projects/, …) and files (onebrain.yml, .claude/, .claude-plugin/) here.\nExisting files will NOT be deleted, but OneBrain structure will be merged in.\n\nContinue?"
                    );
                    if !confirm(&q) {
                        return Err(FsError::Core(CoreError::InitTargetNotEmpty(format!(
                            "init declined by user ({summary}) · re-run with --force to skip confirmation"
                        ))));
                    }
                } else {
                    // No prompt available (--yes without --force on a
                    // non-empty dir, or pure non-interactive call from a
                    // script). Fail closed: surface E_INIT_TARGET_NOT_EMPTY
                    // so the user has to consciously opt in via --force.
                    return Err(FsError::Core(CoreError::InitTargetNotEmpty(format!(
                        "target is not empty ({summary}) at {target_display} · pass --force to proceed"
                    ))));
                }
            }
        }
    }

    // Safety check passed — emit the text-mode header now. Structured-output
    // mode still gets it on stdout (existing CLI integration tests assert
    // its presence), but only after we've cleared the safety guard.
    stdout("OneBrain Init");

    // ── Step 1: config-file guard ──────────────────────────────────────────
    // Idempotency: if either the canonical `onebrain.yml` or the legacy
    // `vault.yml` already exists, the dir is an existing OneBrain vault
    // and init must not silently re-write it. We deliberately leave a
    // legacy-only vault on disk (no auto-migration here) — the guard
    // surfaces `vault.yml exists` so the user runs `onebrain doctor --fix`
    // and gets a controlled migration with a doctor-log entry.
    let canonical_path = vault_dir.join(CONFIG_FILENAME);
    let legacy_path = vault_dir.join(LEGACY_CONFIG_FILENAME);
    let canonical_exists = canonical_path.exists();
    let legacy_exists = legacy_path.exists();
    let config_exists = canonical_exists || legacy_exists;
    let existing_filename: &str = if canonical_exists {
        CONFIG_FILENAME
    } else if legacy_exists {
        LEGACY_CONFIG_FILENAME
    } else {
        CONFIG_FILENAME
    };

    if config_exists && !opts.force {
        // Interactive: ask the user. Non-interactive: error out.
        if let Some(confirm) = opts.confirm_fn.as_mut() {
            let overwrite = confirm(&format!("{existing_filename} already exists. Overwrite?"));
            if !overwrite {
                result.ok = true;
                result.exit_code = 0;
                result.aborted = true;
                stdout(&format!("aborted: {existing_filename} left unchanged"));
                return Ok(result);
            }
        } else {
            let msg = format!("{existing_filename} exists. Re-run with --force to overwrite.");
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

    // ── Step 4: write onebrain.yml ─────────────────────────────────────────
    // CRITICAL · never clobber an existing config. Re-init (--force or a
    // confirmed overwrite) exists to (re)register the plugin + complete the
    // folder/structure scaffold — NOT to reset the config file. Writing the
    // fresh scaffold here would silently drop user keys the scaffold doesn't
    // model (`qmd_collection`, custom `checkpoint`/`folders`/`schedule`
    // values). So when a config already exists we preserve it verbatim; a
    // vault missing required keys is repaired surgically by `doctor --fix`
    // (the `onebrain.yml-keys` recipe backfills without dropping keys).
    if config_exists {
        // Preserved, not written — leave `vault_yml_written` false so the flag
        // never claims a write that didn't happen.
        stdout(&format!(
            "{existing_filename}: preserved (existing config left untouched · run `onebrain doctor --fix` to backfill missing keys)"
        ));
    } else {
        onebrain_yml::write_onebrain_yml(&vault_dir, preset)?;
        result.vault_yml_written = true;
        stdout(&format!("{CONFIG_FILENAME}: written"));
    }

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

    // ── Step 6b: create .claude/ harness directory ─────────────────────────
    // Bun init creates .claude/ implicitly during vault-sync (download plugin
    // files). The Rust port has not yet ported the vault-sync step inside
    // init, so without an explicit mkdir here register-hooks falls through
    // its `detect_harnesses` check and silently no-ops — reporting "hooks: ok"
    // without ever writing settings.json. Bias toward the claude harness
    // because that's the universal default for OneBrain installs; users on
    // gemini or direct can re-run register-hooks themselves.
    let claude_dir = vault_dir.join(".claude");
    if !claude_dir.exists() {
        std::fs::create_dir_all(&claude_dir).map_err(|e| FsError::Io {
            path: claude_dir.clone(),
            source: e,
        })?;
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

    // ── Step 7b: register plugin with Claude Code ──────────────────────────
    // Without these two writes, Claude Code never loads the plugin even
    // though `.claude/plugins/onebrain/` is on disk — /onboarding and every
    // OneBrain skill silently fail at session start. Only emitted on the
    // claude harness; gemini/direct have no equivalent surface.
    if detect_harnesses(&vault_dir).contains(&Harness::Claude) {
        // A1 (SF-B1): marketplace.json write failure must propagate as an
        // error, symmetric with enable_plugin below. Pre-fix it was demoted
        // to a stderr "warning" and execution fell through — leaving
        // enabledPlugins=true with no manifest, which is the exact bug v3.1
        // set out to fix.
        let outcome = marketplace::write_marketplace_json_with_force(&vault_dir, opts.force)?;
        result.marketplace_written = !matches!(outcome, marketplace::MarketplaceOutcome::Skipped);
        let line = match outcome {
            marketplace::MarketplaceOutcome::Written => "marketplace.json: written",
            marketplace::MarketplaceOutcome::Skipped => "marketplace.json: ok (pre-existing)",
            marketplace::MarketplaceOutcome::Repaired => {
                // A2 (SF-B2): visible warning so the user knows --force just
                // rewrote a malformed/wrong-shape manifest.
                stderr("init: marketplace.json repaired (was malformed or wrong-shape)");
                "marketplace.json: repaired"
            }
        };
        stdout(line);
        match enable_plugin::enable_onebrain_plugin_with_force(&vault_dir, opts.force) {
            Ok(outcome) => {
                if let Some(warning) = outcome.warning.as_deref() {
                    // B1 (SF-H3): surface the clobber warning so the user
                    // sees what --force just replaced.
                    stderr(warning);
                }
                result.plugin_enabled = true;
                stdout(&format!(
                    "plugin: {}",
                    if outcome.wrote {
                        "enabled"
                    } else {
                        "ok (already enabled)"
                    }
                ));
            }
            Err(e) => {
                // Hard fail this one — malformed pre-existing settings.json
                // OR a non-bool clobber refusal (B1 / SF-H3); the surrounding
                // FsError already encodes enough context for the caller to
                // map an exit code (75 for InitTargetNotEmpty).
                return Err(e);
            }
        }
    }

    // ── Step 8: vault-sync (best-effort) ───────────────────────────────────
    // Downloads the upstream tarball + populates `.claude/plugins/onebrain/`,
    // CLAUDE.md/GEMINI.md/AGENTS.md, root markdown files, etc. — collapses
    // the previous 2-step bootstrap (`init` then `vault-sync`) into one.
    //
    // Failure is non-fatal: warn the user + tell them to re-run `vault-sync`
    // manually. Init still exits 0 because the scaffold (vault.yml + folders
    // + hooks + schedule preset) is intact and the user can recover.
    let vault_sync_ok = if opts.skip_vault_sync {
        false
    } else {
        match opts.vault_sync_fn.as_mut() {
            Some(f) => f(&vault_dir),
            None => default_vault_sync(&vault_dir, &mut stdout, &mut stderr),
        }
    };
    result.vault_sync_ok = vault_sync_ok;

    // ── Done ───────────────────────────────────────────────────────────────
    if vault_sync_ok {
        // One-step bootstrap — vault is fully populated, user can jump to
        // `/onboarding` immediately.
        stdout("done: run `/onboarding` in Claude to finish setup");
    } else if opts.skip_vault_sync {
        // Caller deliberately opted out — surface the next step explicitly.
        stdout(
            "done: run `onebrain vault-sync` to install plugin files, then `/onboarding` in Claude",
        );
    } else {
        // vault-sync attempted but failed (network, GitHub down, etc.). The
        // scaffold is still intact; tell the user how to recover.
        stdout(
            "done (partial): vault-sync failed — re-run `onebrain vault-sync`, then `/onboarding` in Claude",
        );
    }
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

fn default_vault_sync(
    vault_dir: &Path,
    _stdout: &mut dyn FnMut(&str),
    stderr: &mut dyn FnMut(&str),
) -> bool {
    // `embedded: true` suppresses the framing banner so the output reads as
    // a sub-step of init rather than a top-level command. The orchestrator
    // still prints `▸ Downloading` / `▸ Syncing files` / etc. on its own
    // stdout, which is what the user expects.
    let opts = VaultSyncOptions {
        embedded: true,
        ..Default::default()
    };
    let result = run_vault_sync(vault_dir, opts);
    if result.ok {
        return true;
    }
    if let Some(err) = result.error.as_ref() {
        stderr(&format!("init: vault-sync warning: {err}"));
    } else {
        stderr("init: vault-sync warning (no error detail captured)");
    }
    false
}

// ---------------------------------------------------------------------------
// Re-export wizard fns for the CLI binary
// ---------------------------------------------------------------------------

pub use wizard::{
    ask_continue_nonempty, ask_initialize_here, ask_overwrite_vault_yml, ask_schedule_preset,
};

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
    /// AND vault-sync entirely (the real lib would otherwise hit the harness
    /// detector and the GitHub releases endpoint).
    fn test_opts(vault_dir: &Path) -> (InitOptions, Arc<Mutex<Vec<String>>>) {
        let (stdout_sink, stdout_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(vault_dir.to_path_buf()),
            stdout_lines: Some(stdout_sink),
            register_hooks_fn: Some(Box::new(|_dir: &Path| true)),
            vault_sync_fn: Some(Box::new(|_dir: &Path| true)),
            ..Default::default()
        };
        (opts, stdout_buf)
    }

    #[test]
    fn fresh_vault_creates_folders_and_onebrain_yml() {
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
        // Canonical filename (v3.1+) — NOT vault.yml.
        assert!(d.path().join("onebrain.yml").is_file());
        assert!(!d.path().join("vault.yml").exists());
        // .claude/ must exist so register-hooks has a target on its next run.
        assert!(
            d.path().join(".claude").is_dir(),
            ".claude/ not created by init"
        );

        let lines = stdout_buf.lock().unwrap();
        assert_eq!(lines[0], "OneBrain Init");
        assert!(lines.iter().any(|l| l == "onebrain.yml: written"));
        assert!(lines.iter().any(|l| l == "folders: 9 created"));
        assert!(lines.iter().any(|l| l.contains("done")));
    }

    /// Regression: with the real register-hooks (no stub), init must populate
    /// `.claude/settings.json` with the Stop hook. Before the fix, init would
    /// skip claude detection (no `.claude/` dir yet) and exit reporting
    /// "hooks: ok" without writing anything.
    #[test]
    fn fresh_vault_real_register_hooks_writes_settings_json() {
        let d = tempdir().unwrap();
        let (stdout_sink, _stdout_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(d.path().to_path_buf()),
            yes: true,
            stdout_lines: Some(stdout_sink),
            // No register_hooks_fn override → exercises the real lib.
            ..Default::default()
        };
        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(r.hooks_registered, "register-hooks reported no-op");

        let settings_path = d.path().join(".claude").join("settings.json");
        assert!(
            settings_path.is_file(),
            "settings.json missing — register-hooks no-oped"
        );
        let text = std::fs::read_to_string(&settings_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            v["hooks"]["Stop"].is_array(),
            "Stop hook missing from settings.json: {text}"
        );
    }

    #[test]
    fn existing_onebrain_yml_no_force_no_confirm_returns_exit_1() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "old: value\n").unwrap();
        let (opts, _stdout_buf) = test_opts(d.path());

        let r = run_init(opts).unwrap();
        assert!(!r.ok);
        assert_eq!(r.exit_code, 1);
        let msg = r.message.unwrap();
        assert!(msg.contains("onebrain.yml exists"));
        assert!(msg.contains("--force"));

        // Folders not created
        assert!(!d.path().join("00-inbox").is_dir());
        // Original onebrain.yml not touched
        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(content, "old: value\n");
    }

    /// Idempotency back-compat: a legacy `vault.yml`-only vault must trigger
    /// the same guard (init doesn't auto-migrate; doctor --fix does).
    #[test]
    fn init_skips_if_legacy_vault_yml_exists() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("vault.yml"), "old: legacy\n").unwrap();
        let (opts, _stdout_buf) = test_opts(d.path());

        let r = run_init(opts).unwrap();
        assert!(!r.ok);
        assert_eq!(r.exit_code, 1);
        let msg = r.message.unwrap();
        assert!(msg.contains("vault.yml exists"), "msg: {msg}");
        // Legacy file untouched · canonical not written.
        let legacy_content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert_eq!(legacy_content, "old: legacy\n");
        assert!(!d.path().join("onebrain.yml").exists());
    }

    #[test]
    fn force_preserves_existing_onebrain_yml_config() {
        // CRITICAL data-safety regression guard: `init --force` re-registers
        // the plugin + completes the folder scaffold but must NEVER clobber an
        // existing config. The fresh scaffold doesn't model `qmd_collection`
        // (or custom checkpoint/folders/schedule values), so overwriting would
        // silently drop them — exactly the bug that lost a user's
        // `qmd_collection`. Re-init preserves the config verbatim; missing keys
        // are repaired separately by `doctor --fix`.
        let d = tempdir().unwrap();
        let original = "qmd_collection: ob-1-441565\nold: value\n";
        std::fs::write(d.path().join("onebrain.yml"), original).unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.force = true;

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        // Config preserved verbatim — custom keys survive.
        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(
            content, original,
            "init --force must not rewrite the config"
        );
        // ...while re-init still did its structural work.
        assert!(d.path().join("00-inbox").is_dir());
    }

    #[test]
    fn yes_flag_picks_essentials_preset() {
        let d = tempdir().unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.yes = true;

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.preset_installed, Some(SchedulePreset::Essentials));

        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert!(content.contains("schedule:"));
        assert!(content.contains("/daily"));
        assert!(content.contains("/weekly"));
        assert!(content.contains("/recap"));
    }

    #[test]
    fn confirm_fn_overwrite_no_aborts_cleanly() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("onebrain.yml"), "old: value\n").unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.confirm_fn = Some(Box::new(|_q: &str| false));

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.exit_code, 0);
        assert!(r.aborted);

        // onebrain.yml not touched
        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
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
        assert!(!d.path().join("onebrain.yml").exists());
        assert!(!d.path().join("vault.yml").exists());
        assert!(!d.path().join("00-inbox").is_dir());
        // Only the "initialize here?" prompt fires (no config file so no
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

        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
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
            // Mock vault-sync as success so we isolate the register-hooks
            // failure path under test.
            vault_sync_fn: Some(Box::new(|_dir: &Path| true)),
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
        let (mut opts, _stdout_buf) = test_opts(d.path());
        // The pre-created OneBrain folders make the target dir non-empty
        // and without a vault.yml, so the Step 0 safety check would
        // otherwise refuse to proceed. --force bypasses it.
        opts.force = true;

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

    /// C3 (T-M1): the safety-prompt branch in `ask_continue_nonempty` has
    /// no end-to-end coverage at the run_init level — only safety::classify
    /// and the CLI dispatcher have unit tests for it. Drive an injected
    /// confirm_fn through the full init path so the "yes" branch lands on
    /// vault.yml creation with existing files preserved.
    #[test]
    fn nonempty_dir_confirm_yes_proceeds() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("README.md"), "hi").unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());

        // Confirm "yes" for the non-empty-directory prompt; the second
        // prompt (Initialize here?) also gets "yes" so init proceeds.
        opts.confirm_fn = Some(Box::new(|q: &str| {
            q.starts_with("Target directory is not empty")
                || q.starts_with("Initialize OneBrain vault here?")
        }));

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert_eq!(r.exit_code, 0);
        // OneBrain structure created — canonical filename
        assert!(d.path().join("onebrain.yml").is_file());
        assert!(d.path().join("00-inbox").is_dir());
        // Existing file preserved (no clobber)
        assert_eq!(
            std::fs::read_to_string(d.path().join("README.md")).unwrap(),
            "hi"
        );
    }

    /// C3 (T-M1) negative: the "no" branch surfaces InitTargetNotEmpty and
    /// leaves the existing files untouched.
    #[test]
    fn nonempty_dir_confirm_no_returns_init_target_not_empty() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("README.md"), "hi").unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());

        opts.confirm_fn = Some(Box::new(|q: &str| {
            // Decline the non-empty-directory prompt; never reaches the
            // "Initialize here?" prompt.
            !q.starts_with("Target directory is not empty")
        }));

        let err = run_init(opts).expect_err("expected E_INIT_TARGET_NOT_EMPTY");
        match err {
            FsError::Core(CoreError::InitTargetNotEmpty(msg)) => {
                assert!(msg.contains("declined") || msg.contains("not empty"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        // Existing file preserved + no vault structure created
        assert_eq!(
            std::fs::read_to_string(d.path().join("README.md")).unwrap(),
            "hi"
        );
        assert!(!d.path().join("onebrain.yml").exists());
        assert!(!d.path().join("vault.yml").exists());
        assert!(!d.path().join("00-inbox").exists());
    }

    /// B2 (SF-H4): `--force` must still run `safety::classify`. Pre-fix the
    /// classify call was guarded by `if !opts.force` so an unreadable target
    /// (EACCES on read_dir, target is a regular file, …) under `--force`
    /// fell through to mid-write garbage. Now classify always runs; only
    /// the PROMPT is suppressed under --force.
    #[test]
    #[cfg(unix)]
    fn force_still_classifies_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let target = d.path().join("locked");
        std::fs::create_dir_all(&target).unwrap();
        // Make the dir unreadable so read_dir() returns EACCES.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();

        let (opts, _stdout_buf) = test_opts(&target);
        let mut opts = opts;
        opts.force = true;

        let result = run_init(opts);
        // Restore perms so tempdir cleanup works.
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));

        let err = result.expect_err("expected EACCES to surface as FsError");
        match err {
            FsError::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        // No partial state written — vault.yml absent under the locked dir.
        // (Can't easily inspect without restoring perms first; the EACCES
        // surface from classify is the contract we care about here.)
    }

    /// B2 follow-up: `--force` against a target path that's a regular file
    /// (not a directory) surfaces as a proper FsError via classify rather
    /// than mid-write garbage. classify reports NonEmptyNonVault with the
    /// "not a directory" summary; --force then short-circuits the prompt
    /// and lets the downstream writes fail with their own FsError when they
    /// try to create_dir_all on the regular-file path.
    #[test]
    fn force_still_classifies_target_is_regular_file() {
        let d = tempdir().unwrap();
        let target = d.path().join("regular-file");
        std::fs::write(&target, "ok").unwrap();

        let (opts, _stdout_buf) = test_opts(&target);
        let mut opts = opts;
        opts.force = true;

        let err = run_init(opts).expect_err("expected error for file-as-vault");
        // Either classify caught it via NonEmptyNonVault (then a downstream
        // create_dir_all fails) or the downstream write surfaces FsError.
        // Either way it's an Err, not a partial-write success.
        match err {
            FsError::Io { .. } | FsError::Core(_) => {}
        }
        // The regular file content is preserved (no clobber).
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ok");
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

    /// `DirState::Missing` branch: when the target vault_dir doesn't exist yet,
    /// init must create it via `create_dir_all` and then proceed normally.
    #[test]
    fn missing_vault_dir_is_created_by_init() {
        let d = tempdir().unwrap();
        let vault_dir = d.path().join("new-vault"); // does not exist
        assert!(!vault_dir.exists());
        let (opts, _stdout_buf) = test_opts(&vault_dir);

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(vault_dir.is_dir(), "init must create the missing vault dir");
        assert!(vault_dir.join("onebrain.yml").is_file());
        assert!(r.vault_yml_written);
        assert_eq!(r.folders_created, 9);
    }

    /// `NonEmptyNonVault` + no `confirm_fn` + no force + not structured_output:
    /// the `else` branch surfaces `InitTargetNotEmpty` so the caller gets a
    /// clear "pass --force" error rather than a silent no-op.
    #[test]
    fn non_empty_dir_no_confirm_fn_returns_init_target_not_empty() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("README.md"), "hello").unwrap();
        // test_opts: no confirm_fn, force=false, structured_output=false
        let (opts, _stdout_buf) = test_opts(d.path());

        let err = run_init(opts).expect_err("expected InitTargetNotEmpty");
        match err {
            FsError::Core(CoreError::InitTargetNotEmpty(msg)) => {
                assert!(
                    msg.contains("not empty") || msg.contains("force"),
                    "error message should mention non-empty or --force: {msg}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        // No partial state written.
        assert!(!d.path().join("onebrain.yml").exists());
        assert!(!d.path().join("00-inbox").exists());
    }

    /// `NonEmptyNonVault` + `structured_output=true`: surfaces the machine-readable
    /// `InitTargetNotEmpty` error so the envelope renderer can emit `E_INIT_TARGET_NOT_EMPTY`
    /// without ever reaching an interactive TTY prompt.
    #[test]
    fn structured_output_non_empty_dir_returns_error() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("existing.txt"), "data").unwrap();
        let (stdout_sink, _stdout_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(d.path().to_path_buf()),
            structured_output: true,
            stdout_lines: Some(stdout_sink),
            register_hooks_fn: Some(Box::new(|_| true)),
            vault_sync_fn: Some(Box::new(|_| true)),
            ..Default::default()
        };

        let err = run_init(opts).expect_err("expected InitTargetNotEmpty for structured output");
        match err {
            FsError::Core(CoreError::InitTargetNotEmpty(msg)) => {
                assert!(
                    msg.contains("force"),
                    "structured-output error must mention --force: {msg}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
        assert!(!d.path().join("onebrain.yml").exists());
    }

    /// Existing config + `confirm_fn` says yes to overwrite: init proceeds past
    /// Step 1 but Step 4 still preserves the config verbatim (no clobber). This
    /// tests the confirm-yes path that the existing `confirm_fn_overwrite_no_aborts`
    /// test misses (it only exercises the "no" path).
    #[test]
    fn existing_config_confirm_yes_proceeds_preserves_config() {
        let d = tempdir().unwrap();
        let original = "qmd_collection: my-collection\n";
        std::fs::write(d.path().join("onebrain.yml"), original).unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.confirm_fn = Some(Box::new(|_q: &str| true)); // always yes

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(!r.aborted);
        // Config preserved — vault_yml_written must be false even though we confirmed.
        assert!(
            !r.vault_yml_written,
            "vault_yml_written must be false when config pre-existed"
        );
        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        assert_eq!(content, original, "existing config must not be overwritten");
        // Structural scaffold still applied.
        assert!(d.path().join("00-inbox").is_dir());
    }

    /// `skip_vault_sync=true`: vault_sync_ok=false and the done message tells
    /// the user to run `vault-sync` manually rather than reporting a failure.
    #[test]
    fn skip_vault_sync_emits_vault_sync_done_message() {
        let d = tempdir().unwrap();
        let (stdout_sink, stdout_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(d.path().to_path_buf()),
            skip_vault_sync: true,
            stdout_lines: Some(stdout_sink),
            register_hooks_fn: Some(Box::new(|_| true)),
            // vault_sync_fn intentionally absent — skip_vault_sync bypasses it.
            ..Default::default()
        };

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(!r.vault_sync_ok, "vault_sync_ok must be false when skipped");
        let lines = stdout_buf.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("vault-sync")),
            "done message must mention vault-sync: {lines:?}"
        );
    }

    /// `vault_sync_fn` returns false: vault_sync_ok=false and the "done (partial)"
    /// message is emitted so the user knows to re-run vault-sync manually.
    #[test]
    fn vault_sync_failure_reports_partial_done_message() {
        let d = tempdir().unwrap();
        let (stdout_sink, stdout_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(d.path().to_path_buf()),
            stdout_lines: Some(stdout_sink),
            register_hooks_fn: Some(Box::new(|_| true)),
            vault_sync_fn: Some(Box::new(|_| false)), // simulate failure
            ..Default::default()
        };

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(!r.vault_sync_ok);
        let lines = stdout_buf.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("partial")),
            "done message must say 'partial' on vault-sync failure: {lines:?}"
        );
    }

    /// `MarketplaceOutcome::Repaired` inside `run_init`: a pre-existing malformed
    /// marketplace.json under `--force` triggers a repair, emits a stderr warning,
    /// and writes "marketplace.json: repaired" to stdout.
    #[test]
    fn marketplace_repaired_emits_stderr_warning_and_repaired_stdout_line() {
        let d = tempdir().unwrap();
        // Pre-create a malformed marketplace.json.
        let plugin_dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("marketplace.json"), "{not valid json}").unwrap();

        let (stdout_sink, stdout_buf) = capture_sink();
        let (stderr_sink, stderr_buf) = capture_sink();
        let opts = InitOptions {
            vault_dir: Some(d.path().to_path_buf()),
            force: true,
            stdout_lines: Some(stdout_sink),
            stderr_lines: Some(stderr_sink),
            register_hooks_fn: Some(Box::new(|_| true)),
            vault_sync_fn: Some(Box::new(|_| true)),
            ..Default::default()
        };

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        assert!(
            r.marketplace_written,
            "Repaired must count as marketplace_written=true"
        );
        let stdout = stdout_buf.lock().unwrap();
        assert!(
            stdout.iter().any(|l| l == "marketplace.json: repaired"),
            "expected 'marketplace.json: repaired' in stdout: {stdout:?}"
        );
        let stderr = stderr_buf.lock().unwrap();
        assert!(
            stderr.iter().any(|l| l.contains("repaired")),
            "expected repair warning in stderr: {stderr:?}"
        );
    }

    /// Step 2 directory prompt is skipped when `--yes` is set, even when a
    /// `confirm_fn` is provided. `--yes` implies "accept all defaults" so no
    /// interactive prompt should fire on a clean empty vault.
    #[test]
    fn yes_flag_with_confirm_fn_does_not_prompt_for_directory() {
        let d = tempdir().unwrap();
        let (mut opts, _stdout_buf) = test_opts(d.path());
        opts.yes = true;
        let counter = Arc::new(Mutex::new(0_u32));
        let counter_clone = counter.clone();
        opts.confirm_fn = Some(Box::new(move |_q: &str| {
            *counter_clone.lock().unwrap() += 1;
            true
        }));

        let r = run_init(opts).unwrap();
        assert!(r.ok);
        // Empty dir + no existing config + yes=true → zero confirm prompts:
        //   Step 0: DirState::Empty → no prompt
        //   Step 1: no config exists → no prompt
        //   Step 2: yes=true → prompt skipped
        assert_eq!(
            *counter.lock().unwrap(),
            0,
            "confirm_fn must not be called when --yes is set on a fresh empty vault"
        );
        // yes=true implies Essentials preset.
        assert_eq!(r.preset_installed, Some(SchedulePreset::Essentials));
    }
}
