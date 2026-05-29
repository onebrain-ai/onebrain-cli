//! Central dispatcher — maps the v3.1 [`Cmd`] enum to handler functions.
//!
//! Layout:
//! - Root verbs (`init` / `update` / `doctor`) → existing v3.0 handlers.
//! - Hook-protocol verbs (`session init`, `checkpoint *`, `qmd reindex`) →
//!   existing v3.0 handlers (their JSON shape is already the canonical
//!   `{"decision":"block",...}` for the hook contract).
//! - New v3.1 verbs (`vault current`, `plugin update`) → handlers in this
//!   module's siblings.
//! - Hidden v3.0 aliases → call the corresponding new-path handler AFTER
//!   emitting the one-time migration notice.
//! - All other verbs → [`stubs::not_implemented`] (exit 72).

use crate::cli::*;
use crate::output::{resolve_output_mode, OutputMode, TtyInputs};
use crate::v31::{plugin_update, stubs, vault_current};
use crate::{banner, commands, migration};
use anyhow::Result;

/// Sentinel attached as `anyhow::Context` to errors from commands that have
/// ALREADY emitted their canonical envelope to stdout. `main::render_error`
/// checks the error chain for this marker and skips emitting a duplicate
/// envelope; `main::exit::exit_code_for` is unaffected (the inner error
/// chain still drives the exit code).
///
/// R2-H3: previously `plugin update`'s partial-failure path emitted both
/// `Envelope::partial` (from `emit_plugin_update_summary`) AND a second
/// `Envelope::err` (from `render_error`), giving structured-mode consumers
/// two JSON documents on stdout for a single invocation.
#[derive(Debug)]
pub struct AlreadyReported;

impl std::fmt::Display for AlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "envelope already emitted to stdout")
    }
}

impl std::error::Error for AlreadyReported {}

/// Resolve the output mode for the current invocation. Wraps
/// `TtyInputs::from_env` + `resolve_output_mode` so callers stay declarative.
pub fn output_mode(cli: &Cli) -> OutputMode {
    let inputs = TtyInputs::from_env(&cli.output, cli.json, cli.yaml, cli.pretty, cli.no_color);
    resolve_output_mode(&inputs)
}

/// Top-level dispatch. Returns `Ok(())` on success or an `anyhow::Error`
/// that the main fn maps to an exit code via `exit::exit_code_for`.
pub fn dispatch(cli: Cli) -> Result<()> {
    let mode = output_mode(&cli);
    // R1 branded banner — silently no-ops for hook-protocol commands,
    // structured output, --quiet, and any non-colour text mode (piped,
    // NO_COLOR, CI, TERM=dumb). See `banner::should_show_banner` for the
    // full gating table.
    banner::emit_banner(std::io::stderr().lock(), &cli, &mode);
    let vault_flag = cli.vault.clone();
    let quiet = cli.quiet;

    match cli.command {
        // ───── Root verbs ────────────────────────────────────────────
        Cmd::Init(a) => {
            // Item D: `init` uses the global `--vault` flag for target dir
            // (was `--vault-dir` as an init-specific arg). Walk-up discovery
            // doesn't apply — init creates a vault, doesn't consume one.
            let code = commands::init::run(
                a.yes,
                vault_flag.clone(),
                a.force,
                a.no_sync,
                mode.is_structured(),
            )?;
            std::process::exit(code);
        }
        Cmd::Update(a) => {
            let code = commands::update::run(a.check, a.fresh, a.json, a.plan, &mode)?;
            std::process::exit(code);
        }
        Cmd::Doctor(a) => {
            let code =
                commands::doctor::run(a.fix, a.json, a.yes, vault_flag.clone(), &mode, quiet)?;
            std::process::exit(code);
        }

        // ───── Session ──────────────────────────────────────────────
        Cmd::Session(SessionCmd { verb }) => match verb {
            SessionVerb::Init { vault_dir } => commands::session_init::run(vault_dir, &mode),
            SessionVerb::Current => stubs::not_implemented("session current"),
            SessionVerb::List => stubs::not_implemented("session list"),
            SessionVerb::Get { .. } => stubs::not_implemented("session get"),
        },

        // ───── Checkpoint ───────────────────────────────────────────
        Cmd::Checkpoint(CheckpointCmd { verb }) => match verb {
            CheckpointVerb::Stop { vault_dir } => commands::checkpoint::run("stop", vault_dir),
            CheckpointVerb::Reset { vault_dir } => commands::checkpoint::run("reset", vault_dir),
            CheckpointVerb::Orphans {
                logs_folder,
                session_token,
            } => commands::orphan_scan::run(&logs_folder, &session_token, &mode),
        },

        // ───── Qmd ──────────────────────────────────────────────────
        // Reindex is a hook-protocol command (handles vault check itself);
        // the other verbs are vault-required and must exit 64 outside a
        // vault before reporting E_NOT_IMPLEMENTED (R1 C3).
        Cmd::Qmd(QmdCmd { verb }) => match verb {
            QmdVerb::Reindex => commands::qmd_reindex::run(),
            QmdVerb::Setup => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "qmd setup")
            }
            QmdVerb::Embed => commands::qmd_embed::run(vault_flag.clone()),
            QmdVerb::Status => commands::qmd_status::run(vault_flag.clone(), &mode),
            QmdVerb::Search { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "qmd search")
            }
        },

        // ───── Schedule ─────────────────────────────────────────────
        Cmd::Schedule(ScheduleCmd { verb }) => match verb {
            ScheduleVerb::Register {
                vault_dir,
                dry_run,
                remove,
                refresh,
                resume,
                status,
                test,
            } => {
                let v = vault_dir.or(vault_flag.clone());
                commands::register_schedule::run(v, dry_run, remove, refresh, resume, status, test)
            }
            // Non-protocol verbs are vault-required (need to know which
            // vault's plists to list / which YAML to modify).
            ScheduleVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "schedule list")
            }
            ScheduleVerb::Add { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "schedule add")
            }
            ScheduleVerb::Remove { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "schedule remove")
            }
            ScheduleVerb::Status => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "schedule status")
            }
        },

        // ───── Plugin ───────────────────────────────────────────────
        Cmd::Plugin(PluginCmd { verb }) => match verb {
            PluginVerb::Install { vault_dir, .. } => {
                // v3.0 `register-hooks` + `vault-sync` together. For v3.1 we
                // expose this as a hidden verb under `plugin install`; full
                // first-install flow runs through `onebrain init`.
                let v = vault_dir.or(vault_flag.clone());
                let code = commands::register_hooks::run(v, false, false)?;
                std::process::exit(code);
            }
            PluginVerb::Uninstall => stubs::not_implemented("plugin uninstall"),
            PluginVerb::Update {
                vault_dir,
                branch,
                dry_run,
            } => {
                let v = vault_dir.or(vault_flag.clone());
                let report = plugin_update::run(v, branch, dry_run)?;
                emit_plugin_update_summary(&report, &mode, quiet)?;
                // Partial failure: hooks were rewritten but plists weren't
                // (or some other mid-flight step bailed). Surface a
                // canonical exit so callers don't treat this as success.
                // We've ALREADY emitted the partial-envelope to stdout via
                // `emit_plugin_update_summary` above, so attach the
                // `AlreadyReported` sentinel — `main::render_error` will
                // skip the duplicate envelope but still propagate the
                // exit code (R2-H3).
                if report.partial_failure.is_some() {
                    return Err(anyhow::anyhow!(
                        "plugin update completed only partially: {}",
                        report.partial_failure.as_deref().unwrap_or("")
                    )
                    .context(AlreadyReported));
                }
                Ok(())
            }
            PluginVerb::Migrate {
                name,
                cutoff_date,
                cutoff,
                vault_dir,
            } => {
                let resolved = cutoff_date.or(cutoff);
                let vault_str = vault_dir
                    .or(vault_flag.clone())
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string());
                commands::migrate::run(&name, resolved.as_deref(), vault_str.as_deref())
            }
            PluginVerb::Status => stubs::not_implemented("plugin status"),
            PluginVerb::Verify => stubs::not_implemented("plugin verify"),
        },

        // ───── Vault ────────────────────────────────────────────────
        Cmd::Vault(VaultCmd { verb }) => match verb {
            VaultVerb::Sync {
                vault_root,
                vault_dir,
                branch,
            } => {
                let root = vault_root.or(vault_dir);
                let code = commands::vault_sync::run(root, branch)?;
                std::process::exit(code);
            }
            // Scan / stats / verify need to know which vault — gate on vault
            // presence so the stub returns 64 outside (R1 C3). `current` is
            // intentionally vault-free (it reports detected:false instead).
            VaultVerb::Scan => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "vault scan")
            }
            VaultVerb::Stats => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "vault stats")
            }
            VaultVerb::Verify => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "vault verify")
            }
            VaultVerb::Current => vault_current::run(vault_flag.clone(), &mode),
        },

        // ───── Skill ────────────────────────────────────────────────
        Cmd::Skill(SkillCmd { verb }) => match verb {
            SkillVerb::Run {
                vault_dir,
                name,
                skill,
                harness,
                model,
                args,
            } => {
                // Accept the skill name either positionally (`skill run daily`)
                // or as `--skill /daily` (parity with the scheduler's
                // `run-skill` form); clap's `conflicts_with` rejects both at once.
                let skill_name = name.or(skill).ok_or_else(|| {
                    anyhow::anyhow!("skill run needs a skill name — `onebrain skill run <NAME>` or `--skill <NAME>`")
                })?;
                // Resolve through the canonical chain (flag > ONEBRAIN_VAULT >
                // walk-up from cwd) so `onebrain skill run NAME` just works from
                // inside a vault — no explicit path required. Errors with exit
                // 64 when no vault is found anywhere.
                let resolved = crate::vault_ctx::require(vault_dir.or(vault_flag.clone()))?;
                let vault = resolved.root.as_path().to_string_lossy();
                let want_json = matches!(mode, OutputMode::Json { .. });
                let code = commands::run_skill::run(
                    &vault,
                    &skill_name,
                    &args,
                    harness,
                    model.as_deref(),
                    want_json,
                )?;
                std::process::exit(code);
            }
            SkillVerb::List => stubs::not_implemented("skill list"),
            SkillVerb::Bootstrap { .. } => stubs::not_implemented("skill bootstrap"),
            SkillVerb::Show { name } => {
                let resolved = crate::vault_ctx::require(vault_flag.clone())?;
                let code =
                    commands::skill_inspect::show_run(resolved.root.as_path(), &name, &mode)?;
                std::process::exit(code);
            }
            SkillVerb::Info { name } => {
                let resolved = crate::vault_ctx::require(vault_flag.clone())?;
                let code =
                    commands::skill_inspect::info_run(resolved.root.as_path(), &name, &mode)?;
                std::process::exit(code);
            }
        },

        // ───── Harness (1-verb · accepted exception) ────────────────
        // Missing verb → silently treat as `detect` for v3.0 back-compat
        // (`onebrain harness` with no verb was the only v3.0 flat invocation
        // remaining after the v3.1 tree rename).
        Cmd::Harness(HarnessCmd { verb }) => match verb {
            HarnessVerb::Detect => commands::harness::run(&mode),
            HarnessVerb::Run {
                vault_dir,
                prompt,
                mode: harness_mode,
                harness,
                model,
            } => {
                use crate::cli::HarnessMode;
                // with-context: require vault, cwd = vault, --add-dir vault.
                // ad-hoc: no vault, cwd = $PWD, no --add-dir.
                let (cwd, context_dir) = match harness_mode {
                    HarnessMode::WithContext => {
                        let resolved = crate::vault_ctx::require(vault_dir.or(vault_flag.clone()))?;
                        let v = resolved.root.as_path().to_path_buf();
                        (v.clone(), Some(v))
                    }
                    HarnessMode::AdHoc => (
                        // Force a neutral cwd ($TMPDIR) so claude / gemini
                        // can't walk up from a vault subdir to find
                        // OneBrain's CLAUDE.md / GEMINI.md. v3.2.8 used $PWD
                        // and documented the cwd-auto-load caveat, but a user
                        // invoking ad-hoc from inside their vault still got
                        // full OneBrain context — defeating the mode. User-
                        // level config (`~/.claude/CLAUDE.md`) loads via its
                        // own path and is unaffected by this cwd.
                        std::env::temp_dir(),
                        None,
                    ),
                };
                let want_json = matches!(mode, OutputMode::Json { .. });
                let code = commands::harness_run::run(
                    &cwd,
                    context_dir.as_deref(),
                    prompt.as_deref(),
                    harness,
                    model.as_deref(),
                    want_json,
                )?;
                std::process::exit(code);
            }
        },

        // ───── Stub-only resource groups ────────────────────────────
        Cmd::Avatar(AvatarCmd { verb }) => match verb {
            AvatarVerb::Start => stubs::not_implemented("avatar start"),
            AvatarVerb::Pair => stubs::not_implemented("avatar pair"),
            AvatarVerb::Status => stubs::not_implemented("avatar status"),
            AvatarVerb::Revoke => stubs::not_implemented("avatar revoke"),
            AvatarVerb::Doctor => stubs::not_implemented("avatar doctor"),
        },
        Cmd::Bookmark(BookmarkCmd { verb }) => match verb {
            BookmarkVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "bookmark list")
            }
            BookmarkVerb::Get { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "bookmark get")
            }
            BookmarkVerb::Import { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "bookmark import")
            }
        },
        Cmd::Bundle(BundleCmd { verb }) => match verb {
            BundleVerb::Install { .. } => stubs::not_implemented("bundle install"),
            BundleVerb::Show { .. } => stubs::not_implemented("bundle show"),
            BundleVerb::Info { .. } => stubs::not_implemented("bundle info"),
            BundleVerb::Init { .. } => stubs::not_implemented("bundle init"),
            BundleVerb::Lint { .. } => stubs::not_implemented("bundle lint"),
            BundleVerb::Update { .. } => stubs::not_implemented("bundle update"),
            BundleVerb::Remove { .. } => stubs::not_implemented("bundle remove"),
            BundleVerb::Doctor => stubs::not_implemented("bundle doctor"),
        },
        Cmd::Config(ConfigCmd { verb }) => match verb {
            ConfigVerb::Get { .. } => stubs::not_implemented("config get"),
            ConfigVerb::Set { .. } => stubs::not_implemented("config set"),
            ConfigVerb::List => stubs::not_implemented("config list"),
            ConfigVerb::Init => stubs::not_implemented("config init"),
        },
        Cmd::Daemon(DaemonCmd { verb }) => match verb {
            DaemonVerb::Start => stubs::not_implemented("daemon start"),
            DaemonVerb::Stop => stubs::not_implemented("daemon stop"),
            DaemonVerb::Status => stubs::not_implemented("daemon status"),
        },
        Cmd::Date(DateCmd { verb }) => match verb {
            DateVerb::Today => stubs::not_implemented("date today"),
            DateVerb::Now => stubs::not_implemented("date now"),
            DateVerb::Format { .. } => stubs::not_implemented("date format"),
            DateVerb::Parse { .. } => stubs::not_implemented("date parse"),
        },
        Cmd::Dream(DreamCmd { verb }) => match verb {
            DreamVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "dream list")
            }
            DreamVerb::Tick { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "dream tick")
            }
            DreamVerb::Done { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "dream done")
            }
            DreamVerb::Snooze { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "dream snooze")
            }
        },
        Cmd::Frontmatter(FrontmatterCmd { verb }) => match verb {
            FrontmatterVerb::Parse { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "frontmatter parse")
            }
            FrontmatterVerb::Extract { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "frontmatter extract")
            }
            FrontmatterVerb::Update { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "frontmatter update")
            }
        },
        Cmd::Gateway(GatewayCmd { verb }) => match verb {
            GatewayVerb::Telegram => stubs::not_implemented("gateway telegram"),
            GatewayVerb::Mcp => stubs::not_implemented("gateway mcp"),
        },
        Cmd::Inbox(InboxCmd { verb }) => match verb {
            InboxVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "inbox list")
            }
            InboxVerb::Next => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "inbox next")
            }
            InboxVerb::Process { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "inbox process")
            }
        },
        Cmd::Log(LogCmd { verb }) => match verb {
            LogVerb::Query { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "log query")
            }
            LogVerb::Append { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "log append")
            }
            LogVerb::Rotate => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "log rotate")
            }
            LogVerb::Stats => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "log stats")
            }
        },
        Cmd::Memory(MemoryCmd { verb }) => match verb {
            MemoryVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "memory list")
            }
            MemoryVerb::Add { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "memory add")
            }
            MemoryVerb::Update { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "memory update")
            }
            MemoryVerb::Remove { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "memory remove")
            }
            MemoryVerb::Promote { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "memory promote")
            }
            MemoryVerb::Index => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "memory index")
            }
        },
        Cmd::Note(NoteCmd { verb }) => match verb {
            NoteVerb::Search(args) => commands::note_search::run(vault_flag.clone(), &mode, &args),
            NoteVerb::List(args) => commands::note_list::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Find(args) => commands::note_find::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Read(args) => commands::note_read::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Append(args) => commands::note_append::run(vault_flag.clone(), &mode, &args),
            NoteVerb::New(args) => commands::note_new::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Move(args) => commands::note_move::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Archive(args) => {
                commands::note_archive::run(vault_flag.clone(), &mode, &args)
            }
            NoteVerb::Backlinks(args) => {
                commands::note_backlinks::run(vault_flag.clone(), &mode, &args)
            }
            NoteVerb::Orphans(args) => {
                commands::note_orphans::run(vault_flag.clone(), &mode, &args)
            }
            NoteVerb::Stat(args) => commands::note_stat::run(vault_flag.clone(), &mode, &args),
        },
        Cmd::Pause(PauseCmd { verb }) => match verb {
            PauseVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "pause list")
            }
            PauseVerb::Snapshot { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "pause snapshot")
            }
            PauseVerb::Resume { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "pause resume")
            }
        },
        Cmd::Serve(ServeCmd { verb }) => match verb {
            ServeVerb::Start => stubs::not_implemented("serve start"),
            ServeVerb::Stop => stubs::not_implemented("serve stop"),
            ServeVerb::Status => stubs::not_implemented("serve status"),
        },
        Cmd::Task(TaskCmd { verb }) => match verb {
            TaskVerb::List => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "task list")
            }
            TaskVerb::Add { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "task add")
            }
            TaskVerb::Done { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "task done")
            }
        },

        // ───── Hidden v3.0 aliases — emit migration notice + dispatch ─
        Cmd::SessionInitAlias(a) => {
            migration::print_once("session-init", "session init");
            commands::session_init::run(a.vault_dir, &mode)
        }
        Cmd::OrphanScanAlias(a) => {
            migration::print_once("orphan-scan", "checkpoint orphans");
            commands::orphan_scan::run(&a.logs_folder, &a.session_token, &mode)
        }
        Cmd::QmdReindexAlias => {
            migration::print_once("qmd-reindex", "qmd reindex");
            commands::qmd_reindex::run()
        }
        Cmd::RegisterHooksAlias(a) => {
            migration::print_once("register-hooks", "plugin update");
            // Honour either the per-command `--vault-dir` (legacy spelling)
            // OR the global `--vault` (v3.1 spelling). Either lands in the
            // same place semantically.
            let vault = a.vault.or(vault_flag.clone());
            let code = commands::register_hooks::run(vault, a.dry_run, a.remove)?;
            std::process::exit(code);
        }
        Cmd::RegisterScheduleAlias(a) => {
            migration::print_once("register-schedule", "schedule register");
            let vault = a.vault.or(vault_flag.clone());
            commands::register_schedule::run(
                vault, a.dry_run, a.remove, a.refresh, a.resume, a.status, a.test,
            )
        }
        Cmd::MigrateAlias(a) => {
            migration::print_once("migrate", "plugin migrate");
            let resolved = a.cutoff_date.or(a.cutoff);
            let vault_str = a.vault.as_ref().map(|p| p.to_string_lossy().to_string());
            commands::migrate::run(&a.name, resolved.as_deref(), vault_str.as_deref())
        }
        Cmd::VaultSyncAlias(a) => {
            migration::print_once("vault-sync", "vault sync");
            let root = a.vault_root.or(a.vault_dir);
            let code = commands::vault_sync::run(root, a.branch)?;
            std::process::exit(code);
        }
        Cmd::RunSkillAlias(a) => {
            migration::print_once("run-skill", "skill run");
            let v = a.vault.or(vault_flag.clone()).ok_or_else(|| {
                anyhow::anyhow!("run-skill requires --vault <PATH> or --vault-dir <PATH>")
            })?;
            // Legacy alias keeps the claude default with no model override;
            // `--harness` / `--model` live on the modern `skill run`.
            let code = commands::run_skill::run(
                &v.to_string_lossy(),
                &a.skill,
                &a.args,
                crate::cli::HarnessArg::Claude,
                None,
                /* want_json */ false,
            )?;
            std::process::exit(code);
        }
    }
}

/// Render a `plugin update` report to the user. JSON mode emits the canonical
/// envelope; text mode emits the framed doctor-style report (since v3.2.13),
/// with per-step spinner + random 800–2000ms pacing on a real-colour TTY
/// (since v3.2.14) so the report reads as live work instead of an instant
/// flash — matches the `doctor` and `update` visual vocabulary.
///
/// On partial failure (mid-flight bail after some on-disk state changed),
/// emits a partial-report envelope with `ok: false`, error code
/// `E_PLUGIN_UPDATE_PARTIAL`, and the partial state visible in `data`. The
/// caller in `dispatch` returns an error after this so the process exit
/// code reflects the failure.
fn emit_plugin_update_summary(
    report: &plugin_update::PluginUpdateReport,
    mode: &OutputMode,
    quiet: bool,
) -> Result<()> {
    use std::io::IsTerminal;
    // Animation gate mirrors `doctor`/`update`: only animate when stdout is a
    // real TTY (real-time `\r` overwrite works), mode is colour-bearing text
    // (spinner relies on ANSI), AND the user did NOT pass `--quiet`. Round-1
    // review caught the missing `quiet` plumb-through — `should_animate`'s
    // own contract is `quiet || !stdout_is_tty || !color → false`, so passing
    // the literal `false` here bypassed the user's `--quiet` request.
    // `doctor::run` already threads `cli.quiet` through; matching that here
    // keeps the command family consistent. Pipes / CI / structured output
    // all fall through to the static `_to` path.
    let stdout_is_tty = std::io::stdout().is_terminal();
    let animate = crate::output::should_animate(mode, stdout_is_tty, quiet);
    if animate {
        render_plugin_update_animated(report, mode)
    } else {
        emit_plugin_update_summary_to(report, mode, std::io::stdout().lock())
    }
}

/// TTY-animated text-mode renderer for `plugin update`. Writes the framed
/// report **directly** to stdout (not through `emit`'s text closure) so the
/// per-step spinner sleeps + `\r` overwrites are real-time visible — pre-
/// v3.2.14 the static `_to` path returned a pre-built `String` to `emit`,
/// which collapsed the spinner cycles into a single flash.
///
/// Mirrors `doctor`'s rendering vocabulary: braille spinner frame cycling
/// through `SPINNER_FRAMES` at `SPINNER_FRAME_MS`, total per-step duration
/// drawn from [`crate::output::progress::random_step_delay`] (800–2000ms),
/// then `\r`-clear and write the resolved `✓/⚠/✗ <label>  <detail>` line.
fn render_plugin_update_animated(
    report: &plugin_update::PluginUpdateReport,
    mode: &OutputMode,
) -> Result<()> {
    let stdout = std::io::stdout();
    render_plugin_update_animated_to(report, mode, stdout.lock(), None)
}

/// Inner helper for [`render_plugin_update_animated`] with two extra test
/// seams: an injectable `Write` and a per-step delay override (`None` →
/// production random pacing; `Some(d)` → fixed dwell, set to
/// `Duration::ZERO` in unit tests so the animated branch runs without
/// sleeping). Production always passes `(stdout.lock(), None)`; tests pass
/// `(Vec<u8>, Some(Duration::ZERO))` to assert spinner artefacts
/// deterministically.
pub(crate) fn render_plugin_update_animated_to<W: std::io::Write>(
    report: &plugin_update::PluginUpdateReport,
    mode: &OutputMode,
    mut writer: W,
    step_delay_override: Option<std::time::Duration>,
) -> Result<()> {
    use crate::output::{
        framing_rule_n, is_color_text, write_framed_header, ProgressRenderer, Section, Step,
        StepStatus, RULE_WIDTH,
    };

    let color = is_color_text(mode);
    let rule_width = RULE_WIDTH;
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    // ── Header (no animation — same as static path) ───────────────────
    write_framed_header(&mut writer, "🔄", "Plugin Update", color, rule_width)?;

    // ── Step list (animated) ──────────────────────────────────────────
    // Same per-step Ok/Fail mapping as the static `render_plugin_update_text`
    // path. The visible difference is purely the rendering side: each step
    // shows the braille spinner with its label for ~800–2000ms before the
    // `\r`-clear-and-resolve transition — matches `doctor`/`update`.
    let partial_failed = report.partial_failure.is_some();
    let vault_detail = plugin_update_vault_detail(
        report.vault_synced,
        report.dry_run,
        report.version_before.as_deref(),
        report.version_after.as_deref(),
    );
    let hooks_detail = format!("{} rewritten", report.hooks_rewritten);
    let plists_detail = if partial_failed {
        "failed".to_string()
    } else {
        match report.plists_count {
            Some(0) => "no schedule entries".to_string(),
            Some(n) => format!("{n} refreshed"),
            None => "skipped".to_string(),
        }
    };
    let plists_status = if partial_failed {
        StepStatus::Fail
    } else {
        StepStatus::Ok
    };
    let steps = vec![
        Step::new(
            "vault sync",
            StepStatus::Ok,
            Some(vault_detail.to_string()),
            None,
        ),
        Step::new("hooks", StepStatus::Ok, Some(hooks_detail), None),
        Step::new("launchd plists", plists_status, Some(plists_detail), None),
    ];
    let section_header = if report.dry_run {
        "Update plan (dry-run)"
    } else {
        "Update steps"
    };
    let section = Section::new(section_header, steps);
    // `force_static = false` → spinner animates per step.
    let mut renderer = ProgressRenderer::with_writer(&mut writer, false, color);
    if let Some(d) = step_delay_override {
        renderer.set_step_delay(d);
    }
    renderer.render_section(&section)?;

    // ── Verdict footer ────────────────────────────────────────────────
    // Reuse the static path's footer rendering by manually replicating it
    // against the live writer — keeps the two surfaces byte-identical except
    // for the spinner artifacts. The footer doesn't need animation: doctor's
    // own footer is static, and the user has already absorbed the spinner
    // pacing across the three step rows.
    let verdict_status = if partial_failed {
        StepStatus::Fail
    } else {
        StepStatus::Ok
    };
    let verdict_glyph = verdict_status.glyph();
    let verdict_prefix = verdict_status.ansi_prefix(color);
    let verdict_text = plugin_update_verdict_text(
        partial_failed,
        report.dry_run,
        report.vault_synced,
        report.hooks_rewritten,
        report.plists_rewritten,
        report.version_before.as_deref(),
        report.version_after.as_deref(),
    );
    let total_str = "3 steps";
    let left_cols = 1 + 1 + 2 + verdict_text.chars().count();
    let gap = rule_width
        .saturating_sub(left_cols + total_str.chars().count())
        .max(2);
    let rule = framing_rule_n(rule_width);
    writeln!(writer)?;
    writeln!(writer, "{dim}{rule}{reset}")?;
    writeln!(
        writer,
        " {verdict_prefix}{verdict_glyph}{reset}  {verdict_text}{pad}{total_str}",
        pad = " ".repeat(gap),
    )?;
    if let Some(reason) = report.partial_failure.as_deref() {
        let one_line = reason.replace('\n', " · ");
        if color {
            writeln!(writer, " {dim}└ {one_line}{reset}")?;
        } else {
            writeln!(writer, " └ {one_line}")?;
        }
    }
    // R6: surface the reload next-step whenever a real version change landed —
    // INDEPENDENT of partial_failed. `version_after` is read post-sync, so the
    // new version is already on disk; a later schedule-step failure doesn't
    // un-land it, and the running session still holds the old one. Suppressing
    // the hint on partial failure would lose the guidance exactly when the user
    // most needs it.
    if let Some(hint) = plugin_update_reload_hint(
        report.dry_run,
        report.version_before.as_deref(),
        report.version_after.as_deref(),
    ) {
        if color {
            writeln!(writer, " {dim}↻ {hint}{reset}")?;
        } else {
            writeln!(writer, " ↻ {hint}")?;
        }
    }
    writeln!(writer, "{dim}{rule}{reset}")?;
    Ok(())
}

/// Compose the `vault sync` step row's detail string from the captured
/// version delta + run flags. Shared by both the static and animated
/// renderers so a future tweak (e.g. trimming the `v` prefix) lands in
/// exactly one place.
///
/// Decision table (v3.2.15):
/// - dry-run with known current → `current vX · skipped`
/// - dry-run unknown → `skipped`
/// - step didn't run → `skipped`
/// - happy path with versions → `vX → vY` / `vX · up-to-date` / `installed vY`
/// - happy path no version info → `done` (back-compat with pre-3.2.15)
///
/// Partial-failure cases aren't branched here: a later-step failure doesn't
/// change what version landed on disk during the vault-sync step, so the row
/// still reports the same delta. The verdict footer + per-step glyph carry
/// the failure signal.
fn plugin_update_vault_detail(
    vault_synced: bool,
    dry_run: bool,
    before: Option<&str>,
    after: Option<&str>,
) -> String {
    if dry_run {
        return match before {
            Some(v) => format!("current v{v} · skipped"),
            None => "skipped".to_string(),
        };
    }
    if !vault_synced {
        return "skipped".to_string();
    }
    match (before, after) {
        (Some(a), Some(b)) if a == b => format!("v{a} · up-to-date"),
        (Some(a), Some(b)) => format!("v{a} → v{b}"),
        (None, Some(b)) => format!("installed v{b}"),
        _ => "done".to_string(),
    }
}

/// Compose the verdict footer's right-hand text (e.g. `updated v3.1.3 → v3.1.4`,
/// `already up-to-date · v3.1.4`). Mirrors the step-detail helper above with
/// the verdict-summary phrasing.
fn plugin_update_verdict_text(
    partial_failed: bool,
    dry_run: bool,
    vault_synced: bool,
    hooks_rewritten: u32,
    plists_rewritten: bool,
    before: Option<&str>,
    after: Option<&str>,
) -> String {
    if partial_failed {
        return "partial failure".to_string();
    }
    if dry_run {
        return match before {
            Some(v) => format!("dry-run · current v{v}"),
            None => "dry-run · no changes written".to_string(),
        };
    }
    // Real version bump trumps everything — that IS the headline outcome.
    if let (Some(a), Some(b)) = (before, after) {
        if a != b {
            return format!("updated v{a} → v{b}");
        }
    }
    let any_change = vault_synced || hooks_rewritten > 0 || plists_rewritten;
    let suffix = after
        .or(before)
        .map(|v| format!(" · v{v}"))
        .unwrap_or_default();
    if any_change {
        format!("update complete{suffix}")
    } else {
        format!("already up-to-date{suffix}")
    }
}

/// R6 · next-step guidance after a plugin update. When a real version change
/// (or fresh install) landed, the RUNNING Claude session still holds the OLD
/// plugin in memory — skills hot-reload on next use, but hooks/MCP and
/// `INSTRUCTIONS.md`/`CLAUDE.md` (loaded once at session start) do not. Point
/// the user at the reload path. Returns `None` for dry-run or a no-op
/// (`up-to-date`) — nothing to apply, so we stay quiet.
///
/// Shared by both renderers (static + animated) so the wording lives in one
/// place, mirroring `plugin_update_verdict_text`.
fn plugin_update_reload_hint(
    dry_run: bool,
    before: Option<&str>,
    after: Option<&str>,
) -> Option<String> {
    if dry_run {
        return None;
    }
    let changed = match (before, after) {
        (Some(a), Some(b)) => a != b, // real version bump
        (None, Some(_)) => true,      // fresh install
        _ => false,
    };
    changed.then(|| {
        "apply: /reload-plugins (skills · hooks · agents · MCP) — \
         for INSTRUCTIONS/CLAUDE.md changes, /wrapup + reopen the session"
            .to_string()
    })
}

/// On-the-wire payload for `plugin update`. Hoisted out of
/// [`emit_plugin_update_summary_to`] so [`render_plugin_update_text`] can
/// borrow the typed fields without a stringly-typed bridge.
#[derive(serde::Serialize)]
struct PluginUpdateData<'a> {
    vault_synced: bool,
    hooks_rewritten: u32,
    plists_rewritten: bool,
    /// v3.2.13: exact count of launchd plists written this run. `None` means
    /// the step did not run (dry-run); `Some(0)` means there were no
    /// `schedule:` entries to register; `Some(N)` is the standard success
    /// case. `#[serde(skip_serializing_if = "Option::is_none")]` keeps the
    /// JSON envelope shape additive (dry-run runs don't gain a new field).
    #[serde(skip_serializing_if = "Option::is_none")]
    plists_count: Option<u32>,
    /// v3.2.15: plugin version BEFORE the vault-sync step (from
    /// `.claude-plugin/plugin.json::version`). `None` ⇒ fresh install / file
    /// missing / version field missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    version_before: Option<&'a str>,
    /// v3.2.15: plugin version AFTER the vault-sync step. In dry-run this
    /// equals `version_before` (no tarball fetched).
    #[serde(skip_serializing_if = "Option::is_none")]
    version_after: Option<&'a str>,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partial_failure: Option<&'a str>,
}

impl<'a> PluginUpdateTextData for PluginUpdateData<'a> {
    fn vault_synced(&self) -> bool {
        self.vault_synced
    }
    fn hooks_rewritten(&self) -> u32 {
        self.hooks_rewritten
    }
    fn plists_rewritten(&self) -> bool {
        self.plists_rewritten
    }
    fn plists_count(&self) -> Option<u32> {
        self.plists_count
    }
    fn version_before(&self) -> Option<&str> {
        self.version_before
    }
    fn version_after(&self) -> Option<&str> {
        self.version_after
    }
    fn dry_run(&self) -> bool {
        self.dry_run
    }
    fn partial_failure(&self) -> Option<&str> {
        self.partial_failure
    }
}

/// Same as [`emit_plugin_update_summary`] but with an injectable writer for
/// unit tests. R2-M4: the broken-pipe regression test feeds a writer that
/// always returns `io::ErrorKind::BrokenPipe`; the function must surface the
/// failure as `Err` so `exit::exit_code_for` can classify it to 0 (the POSIX-
/// correct exit when downstream hung up). Previously the production code used
/// `let _ = emit(...)` and the integration smoke-test couldn't observe
/// propagation deterministically (OS pipe buffer size varies).
pub(crate) fn emit_plugin_update_summary_to<W: std::io::Write>(
    report: &plugin_update::PluginUpdateReport,
    mode: &OutputMode,
    writer: W,
) -> Result<()> {
    use crate::output::{emit, Envelope, ErrorInfo};
    use anyhow::Context;

    let data = PluginUpdateData {
        vault_synced: report.vault_synced,
        hooks_rewritten: report.hooks_rewritten,
        plists_rewritten: report.plists_rewritten,
        plists_count: report.plists_count,
        version_before: report.version_before.as_deref(),
        version_after: report.version_after.as_deref(),
        dry_run: report.dry_run,
        note: if report.dry_run {
            Some("dry-run · no changes written")
        } else {
            None
        },
        partial_failure: report.partial_failure.as_deref(),
    };

    let mut env = if let Some(reason) = report.partial_failure.as_deref() {
        // Partial-failure envelope: ok=false, but data is preserved so the
        // caller can see exactly which steps succeeded.
        Envelope::partial(
            "plugin.update",
            None,
            data,
            ErrorInfo::new("E_PLUGIN_UPDATE_PARTIAL", reason),
        )
    } else {
        Envelope::ok("plugin.update", None, data)
    };

    // R2-H1: plumb rewriter soft-warnings (e.g. W_MALFORMED_HOOK_ENTRY) into
    // the envelope so machine + human consumers both see them. Empty when
    // settings.json is well-formed (Vec<Warning> serialises as []).
    for w in &report.warnings {
        env = env.with_warning(w.code.clone(), w.message.clone());
    }

    emit(&env, mode, writer, |e| render_plugin_update_text(e, mode))
        .context("plugin update: render summary failed")?;
    Ok(())
}

/// Text-mode renderer for `plugin update` — produces a framed report that
/// mirrors `doctor`'s style (FIGlet-flanked header + sectioned step lines +
/// verdict footer). Returns the rendered block as a `String` so the caller's
/// `emit` writes it through the standard text-mode pipeline.
///
/// Layout (v3.2.13+):
/// ```
/// ────────────────────────────────────────────────
///  🔄  Plugin Update
/// ────────────────────────────────────────────────
///
///  Update steps
///   ✓ vault sync         done
///   ✓ hooks              2 rewritten
///   ✓ launchd plists     done
///
/// ────────────────────────────────────────────────
///  ✓  update complete                       3 steps
/// ────────────────────────────────────────────────
/// ```
///
/// Partial-failure path replaces the verdict glyph with `✗` and prepends an
/// indented `└ <reason>` hint line under the failing step's row (closest to
/// doctor's warn/fail rendering).
fn render_plugin_update_text<D>(env: &crate::output::Envelope<D>, mode: &OutputMode) -> String
where
    D: PluginUpdateTextData + serde::Serialize,
{
    use crate::output::{
        framing_rule_n, is_color_text, write_framed_header, ProgressRenderer, Section, Step,
        StepStatus, RULE_WIDTH,
    };
    use std::io::Write;

    let d = match env.data.as_ref() {
        Some(d) => d,
        None => return String::new(),
    };
    let color = is_color_text(mode);
    let rule_width = RULE_WIDTH;
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut buf: Vec<u8> = Vec::new();

    // ── Header ────────────────────────────────────────────────────────
    let _ = write_framed_header(&mut buf, "🔄", "Plugin Update", color, rule_width);

    // ── Step list ─────────────────────────────────────────────────────
    // Today the only step that can populate `partial_failure` is the launchd
    // plist re-registration (see `plugin_update::run` step 4 — vault-sync
    // and hook-rewriter errors bubble as `Err` instead of leaving on-disk
    // state half-applied). v3.2.13: when `partial_failure` is set we mark
    // the plist step as `Fail` so the per-step glyph matches the verdict —
    // pre-3.2.13 it stayed `Ok` and silently misrepresented the failed step
    // as succeeded.
    let partial_failed = d.partial_failure().is_some();
    let vault_detail = plugin_update_vault_detail(
        d.vault_synced(),
        d.dry_run(),
        d.version_before(),
        d.version_after(),
    );
    let hooks_detail = format!("{} rewritten", d.hooks_rewritten());
    // `plists_count` (v3.2.13): `Some(N>0)` → N refreshed; `Some(0)` →
    // vault has no schedule entries (well-formed no-op); `None` → step
    // didn't run (dry-run, or a prior step failed). Distinguishes the cases
    // the pre-3.2.13 bool collapsed into a single "done"/"skipped".
    let plists_detail = if partial_failed {
        "failed".to_string()
    } else {
        match d.plists_count() {
            Some(0) => "no schedule entries".to_string(),
            Some(n) => format!("{n} refreshed"),
            None => "skipped".to_string(),
        }
    };
    let plists_status = if partial_failed {
        StepStatus::Fail
    } else {
        StepStatus::Ok
    };
    let steps = vec![
        Step::new("vault sync", StepStatus::Ok, Some(vault_detail), None),
        Step::new("hooks", StepStatus::Ok, Some(hooks_detail), None),
        Step::new("launchd plists", plists_status, Some(plists_detail), None),
    ];
    let header = if d.dry_run() {
        "Update plan (dry-run)"
    } else {
        "Update steps"
    };
    let section = Section::new(header, steps);
    // `force_static = true` — no spinner for `plugin update`; the operation
    // is fast (single HTTP fetch + a few file writes) and the user already
    // sees doctor-level animation elsewhere. Skip the artificial pacing.
    let mut renderer = ProgressRenderer::with_writer(&mut buf, true, color);
    let _ = renderer.render_section(&section);

    // ── Verdict footer ────────────────────────────────────────────────
    let _ = writeln!(buf);
    let rule = framing_rule_n(rule_width);
    let _ = writeln!(buf, "{dim}{rule}{reset}");
    let verdict_status = if d.partial_failure().is_some() {
        StepStatus::Fail
    } else {
        StepStatus::Ok
    };
    let verdict_glyph = verdict_status.glyph();
    let verdict_prefix = verdict_status.ansi_prefix(color);
    let verdict_text = plugin_update_verdict_text(
        partial_failed,
        d.dry_run(),
        d.vault_synced(),
        d.hooks_rewritten(),
        d.plists_rewritten(),
        d.version_before(),
        d.version_after(),
    );
    // Trailing count right-aligned to the rule width (matches doctor's
    // " ✓  N ok · M warn · K fail              N checks" layout).
    let total_str = "3 steps";
    let left_cols = 1 + 1 + 2 + verdict_text.chars().count(); // " ✓  " + text
    let gap = rule_width
        .saturating_sub(left_cols + total_str.chars().count())
        .max(2);
    let _ = writeln!(
        buf,
        " {verdict_prefix}{verdict_glyph}{reset}  {verdict_text}{pad}{total_str}",
        pad = " ".repeat(gap),
    );
    // Failure hint — indented `└` line, doctor-style. Multi-line `reason`
    // strings (deep anyhow chains can include newlines via `{:#}`) get
    // flattened to ` · ` so the indented layout stays intact — without this
    // the second line would have no `└ ` prefix and break the visual frame.
    if let Some(reason) = d.partial_failure() {
        let one_line = reason.replace('\n', " · ");
        if color {
            let _ = writeln!(buf, " {dim}└ {one_line}{reset}");
        } else {
            let _ = writeln!(buf, " └ {one_line}");
        }
    }
    // R6: same reload next-step as the animated path (shared helper); fires on
    // a real version change regardless of partial_failed — the version already
    // landed on disk, so the running session needs a reload either way.
    if let Some(hint) =
        plugin_update_reload_hint(d.dry_run(), d.version_before(), d.version_after())
    {
        if color {
            let _ = writeln!(buf, " {dim}↻ {hint}{reset}");
        } else {
            let _ = writeln!(buf, " ↻ {hint}");
        }
    }
    let _ = writeln!(buf, "{dim}{rule}{reset}");

    String::from_utf8(buf).unwrap_or_default()
}

/// Read-only view of the typed `plugin update` envelope payload — narrowing
/// the renderer's coupling to the module-level [`PluginUpdateData`] struct.
/// Lets the renderer stay generic over any payload that provides the same
/// shape (used by the unit tests; production always goes through the real
/// `PluginUpdateData` instance built in [`emit_plugin_update_summary_to`]).
trait PluginUpdateTextData {
    fn vault_synced(&self) -> bool;
    fn hooks_rewritten(&self) -> u32;
    fn plists_rewritten(&self) -> bool;
    fn plists_count(&self) -> Option<u32>;
    fn version_before(&self) -> Option<&str>;
    fn version_after(&self) -> Option<&str>;
    fn dry_run(&self) -> bool;
    fn partial_failure(&self) -> Option<&str>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};

    /// Writer that returns `BrokenPipe` on every write. Mirrors a downstream
    /// `head -c 0` / `| less` quit pattern where the consumer hangs up
    /// immediately.
    struct BrokenPipeWriter;
    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn broken_pipe_writer_propagates_through_emit_helper_and_classifies_to_zero() {
        // R2-M4: PROVE the wiring. The previous integration smoke test
        // discarded the exit code; this unit test directly asserts the
        // function returns Err on a BrokenPipe writer AND that
        // `exit::exit_code_for` classifies the resulting anyhow error to
        // EXIT_OK (POSIX-correct behaviour for downstream-hung-up pipes).
        let report = plugin_update::PluginUpdateReport {
            dry_run: true,
            vault_synced: false,
            hooks_rewritten: 0,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mode = OutputMode::Json { pretty: false };
        let err = emit_plugin_update_summary_to(&report, &mode, BrokenPipeWriter)
            .expect_err("BrokenPipe writer must surface as Err, not be swallowed");
        // BrokenPipe must classify to EXIT_OK so `onebrain ... | head` etc.
        // don't show an error.
        assert_eq!(crate::exit::exit_code_for(&err), crate::exit::EXIT_OK);
    }

    #[test]
    fn already_reported_sentinel_downcasts_from_anyhow_context() {
        // Defensive: regression-guard for the anyhow-context downcast
        // quirk that bit R2-H3. If anyhow ever changes how `.context(C)`
        // is exposed, this test surfaces it before main::render_error
        // silently regresses.
        let inner = anyhow::anyhow!("inner err");
        let wrapped: anyhow::Error = inner.context(AlreadyReported);
        assert!(
            wrapped.downcast_ref::<AlreadyReported>().is_some(),
            "anyhow::Error::downcast_ref must locate `.context()`-attached AlreadyReported"
        );
    }

    #[test]
    fn plain_error_does_not_downcast_as_already_reported() {
        let plain: anyhow::Error = anyhow::anyhow!("plain error without sentinel");
        assert!(plain.downcast_ref::<AlreadyReported>().is_none());

        // also verify .context-wrapping with a different type doesn't false-positive
        let other: anyhow::Error = anyhow::anyhow!("base").context("unrelated context string");
        assert!(other.downcast_ref::<AlreadyReported>().is_none());
    }

    // ──────────────────────────────────────────────────────────────────
    // v3.2.13 — framed-report rendering tests for `plugin update`
    // ──────────────────────────────────────────────────────────────────

    fn text_mode_mono() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: false,
        }
    }

    #[test]
    fn plugin_update_text_renders_framed_report_with_header_and_footer() {
        // v3.2.13: replace the old `plugin update:` key:value summary with a
        // doctor-style framed report. Pin the header glyph, the section
        // marker, all three step lines, and the verdict footer so any future
        // template churn fails loudly.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 3,
            plists_rewritten: true,
            plists_count: Some(3),
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("🔄  Plugin Update"), "header missing:\n{out}");
        assert!(
            out.contains("Update steps"),
            "section marker missing:\n{out}"
        );
        assert!(out.contains("✓ vault sync"), "vault step missing:\n{out}");
        assert!(out.contains("✓ hooks"), "hooks step missing:\n{out}");
        assert!(
            out.contains("✓ launchd plists"),
            "plists step missing:\n{out}"
        );
        assert!(out.contains("3 rewritten"), "hook count missing:\n{out}");
        assert!(out.contains("3 refreshed"), "plist count missing:\n{out}");
        assert!(out.contains("update complete"), "verdict missing:\n{out}");
        assert!(out.contains("3 steps"), "step count missing:\n{out}");
        // No legacy `plugin update:` lead-in or `vault sync       :` colon
        // table — those were the visual marks of the pre-3.2.13 format.
        assert!(
            !out.contains("plugin update:"),
            "legacy key:value summary leaked:\n{out}"
        );
        assert!(
            !out.contains("vault sync       :"),
            "legacy colon-aligned table leaked:\n{out}"
        );
    }

    #[test]
    fn plugin_update_text_dry_run_renders_dry_run_section_header_and_verdict() {
        let report = plugin_update::PluginUpdateReport {
            dry_run: true,
            vault_synced: false,
            hooks_rewritten: 0,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("Update plan (dry-run)"),
            "dry-run section header missing:\n{out}"
        );
        assert!(
            out.contains("dry-run · no changes written"),
            "dry-run verdict missing:\n{out}"
        );
    }

    #[test]
    fn plugin_update_text_partial_failure_renders_fail_glyph_and_hint_line() {
        // R1 B3 + v3.2.13 + Round-2 review HIGH-2: the partial-failure path
        // must surface a ✗ glyph on the verdict footer AND the failing
        // launchd plist step row (previously the step row stayed ✓ + "skipped"
        // — silently misrepresenting the failed step as succeeded). The
        // failure reason hangs off the verdict as an indented `└` hint line.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: Some("schedule re-register failed: launchctl exit 1".to_string()),
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("✗  partial failure"),
            "partial-failure verdict glyph missing:\n{out}"
        );
        assert!(
            out.contains("└ schedule re-register failed"),
            "partial-failure hint line missing:\n{out}"
        );
        assert!(
            out.contains("✗ launchd plists"),
            "partial-failure path must mark the plist step with ✗, not ✓:\n{out}"
        );
        assert!(
            out.contains("failed"),
            "partial-failure plist step detail must read \"failed\":\n{out}"
        );
    }

    #[test]
    fn plugin_update_text_partial_failure_with_multiline_reason_collapses_to_single_line() {
        // Round-2 review MEDIUM: deep anyhow chains can carry newlines via
        // `{e:#}`. A multi-line `reason` would break the indented `└` layout
        // because the second line has no `└ ` prefix. Defensive flatten
        // pinned: every newline becomes ` · ` so the indent stays intact.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: Some(
                "schedule re-register failed\nlaunchctl exit 1\nplist syntax error".to_string(),
            ),
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("└ schedule re-register failed · launchctl exit 1 · plist syntax error"),
            "multi-line reason must flatten to single ` · `-separated line:\n{out}"
        );
        // No raw newlines inside the bracketed reason — guard against the
        // layout-breaking case directly.
        assert!(
            !out.contains("schedule re-register failed\nlaunchctl"),
            "raw newline leaked through the `└ <reason>` hint:\n{out}"
        );
    }

    #[test]
    fn plugin_update_text_no_changes_renders_already_up_to_date_verdict() {
        // Idempotent rerun: every step ran but nothing changed (vault already
        // up-to-date, no hooks to rewrite, plists already current). Verdict
        // must communicate the no-op rather than masquerading as a real
        // update.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: false,
            hooks_rewritten: 0,
            plists_rewritten: false,
            plists_count: Some(0),
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("already up-to-date"),
            "no-op verdict missing:\n{out}"
        );
    }

    #[test]
    fn reload_hint_fires_only_on_real_version_change() {
        // Real bump + fresh install → hint; no-op / dry-run / unknown-after → none.
        assert!(plugin_update_reload_hint(false, Some("3.1.3"), Some("3.1.4")).is_some());
        assert!(plugin_update_reload_hint(false, None, Some("3.1.4")).is_some());
        assert!(plugin_update_reload_hint(false, Some("3.1.4"), Some("3.1.4")).is_none());
        assert!(plugin_update_reload_hint(true, Some("3.1.3"), Some("3.1.4")).is_none());
        assert!(plugin_update_reload_hint(false, Some("3.1.3"), None).is_none());
        let h = plugin_update_reload_hint(false, Some("a"), Some("b")).unwrap();
        assert!(h.contains("/reload-plugins") && h.contains("/wrapup"));
    }

    #[test]
    fn plugin_update_text_real_bump_emits_reload_next_step() {
        // Integration: a real version change must surface the reload guidance
        // in the rendered report (not just in the helper).
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 0,
            plists_rewritten: false,
            plists_count: Some(0),
            version_before: Some("3.1.3".to_string()),
            version_after: Some("3.1.4".to_string()),
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("updated v3.1.3 → v3.1.4"),
            "verdict missing:\n{out}"
        );
        assert!(
            out.contains("/reload-plugins"),
            "reload next-step missing:\n{out}"
        );
    }

    #[test]
    fn plugin_update_text_no_schedule_entries_renders_distinct_detail() {
        // Round-2 review HIGH-1: a vault with zero `schedule:` entries
        // returns `Ok(0)` from `register_schedule::run_quiet` — a well-formed
        // no-op. The plist step must render as "no schedule entries" rather
        // than the misleading "done" the pre-Round-2 bool collapse produced.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 1,
            plists_rewritten: false,
            plists_count: Some(0),
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("no schedule entries"),
            "empty-schedule no-op must render as \"no schedule entries\":\n{out}"
        );
        // The "done" line was the old wording; ensure it doesn't sneak back
        // for this case (regression guard against the bool collapse).
        assert!(
            !out.contains("launchd plists     done"),
            "empty-schedule no-op rendered as misleading \"done\":\n{out}"
        );
    }

    #[test]
    fn plugin_update_text_color_mode_balances_ansi_escapes() {
        // Round-2 review MEDIUM: every dim/reset wrapper in the renderer must
        // be balanced — a dangling `\x1b[2m` (dim) or unmatched `\x1b[31m` (red)
        // would leak ANSI state past the framed report into whatever the
        // terminal prints next. All five new text tests use mono mode (color:
        // false); this one exercises the color path and asserts the open
        // count matches the close (`\x1b[0m`) count.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: Some("schedule re-register failed: launchctl exit 1".to_string()),
            warnings: Vec::new(),
        };
        let color_mode = OutputMode::Text {
            color: true,
            pretty: true,
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &color_mode, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // `\x1b` counts EVERY SGR sequence (openers + resets). Subtract the
        // reset count to isolate the non-reset openers we emit (`[2m`, `[1m`,
        // `[31m`, `[2;32m`, etc.). A balanced wrapper has openers == resets;
        // any imbalance means dim/red/bold leaks past the framed report into
        // whatever the terminal prints next.
        let total_sgr = out.matches('\x1b').count();
        let resets = out.matches("\x1b[0m").count();
        let openers = total_sgr - resets;
        assert_eq!(
            openers, resets,
            "ANSI escape openers ({openers}) ≠ resets ({resets}) — wrapper \
             imbalance will leak terminal state past the framed report.\nstdout:\n{out}"
        );
    }

    #[test]
    fn plugin_update_animated_emits_spinner_artifacts_with_zero_delay() {
        // v3.2.14: the animated text path renders the same framed report as
        // the static path PLUS interleaves a braille spinner with `\r`-clear
        // sequences per step. Inject `Duration::ZERO` so the spinner cycles
        // run instantly (no sleeping in the test). Assert the spinner
        // artefacts are present so a future refactor that accidentally hands
        // the animated path a `force_static = true` renderer fails loudly.
        //
        // Mode is mono so the resolved-line glyph isn't wrapped in
        // `\x1b[2;32m...\x1b[0m` ANSI escapes — keeps the `✓ vault sync`
        // assertion below substring-stable. `\r\x1b[K` is still emitted by
        // the spinner clear pass even under color: false (it's positioning,
        // not color).
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: true,
            plists_count: Some(2),
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_plugin_update_animated_to(
            &report,
            &text_mode_mono(),
            &mut buf,
            Some(std::time::Duration::ZERO),
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        // `\r\x1b[K` is the carriage-return + clear-EOL pair the animated
        // step renderer writes between spinner cycles and the resolved line.
        // Absent ⇒ animation was skipped or the renderer was force-static.
        assert!(
            out.contains("\r\x1b[K"),
            "expected `\\r\\x1b[K` spinner-clear sequence in animated \
             output; the renderer may have collapsed to static.\nstdout:\n{out:?}"
        );
        // At least one braille spinner frame must appear pre-resolve.
        let any_spinner_frame = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            .iter()
            .any(|f| out.contains(f));
        assert!(
            any_spinner_frame,
            "expected at least one braille spinner frame in animated \
             output.\nstdout:\n{out:?}"
        );
        // The resolved lines + verdict footer must still appear after the
        // animation overlay — the spinner only PRECEDES the resolved line,
        // it doesn't replace it.
        assert!(
            out.contains("✓ vault sync"),
            "resolved vault step missing after animation:\n{out:?}"
        );
        assert!(
            out.contains("update complete"),
            "verdict missing after animation:\n{out:?}"
        );
    }

    #[test]
    fn plugin_update_animated_partial_failure_renders_fail_glyph_under_animation() {
        // Cross-cut between the v3.2.13 round-2 fix (✗ glyph on the failing
        // step instead of ✓ + "skipped") and v3.2.14 animation: the animated
        // path must mark the launchd plist row as fail too. Mono mode keeps
        // the glyph substring-stable (no ANSI wrap).
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: Some("schedule re-register failed: launchctl exit 1".to_string()),
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_plugin_update_animated_to(
            &report,
            &text_mode_mono(),
            &mut buf,
            Some(std::time::Duration::ZERO),
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("✗ launchd plists"),
            "animated path must mark failed plist step with ✗, not ✓:\n{out:?}"
        );
        assert!(
            out.contains("└ schedule re-register failed"),
            "partial-failure hint missing under animation:\n{out:?}"
        );
    }

    #[test]
    fn reload_hint_survives_partial_failure_when_version_changed() {
        // B-HIGH1 regression: a real version bump landed on disk, THEN the
        // schedule re-register failed (partial). The new version is live, so the
        // reload guidance MUST still appear — suppressing it on partial failure
        // would lose the guidance exactly when the user needs it.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 1,
            plists_rewritten: false,
            plists_count: None,
            version_before: Some("3.1.3".to_string()),
            version_after: Some("3.1.4".to_string()),
            partial_failure: Some("schedule re-register failed: launchctl exit 1".to_string()),
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("partial failure"), "verdict missing:\n{out}");
        assert!(
            out.contains("/reload-plugins"),
            "reload guidance lost on partial failure despite version change:\n{out}"
        );
    }

    #[test]
    fn reload_hint_renders_in_animated_path() {
        // C-L2: the animated renderer wires the same reload helper — assert the
        // hint actually reaches its output on a real version change (the text
        // path is covered separately).
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 0,
            plists_rewritten: false,
            plists_count: Some(0),
            version_before: Some("3.1.3".to_string()),
            version_after: Some("3.1.4".to_string()),
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_plugin_update_animated_to(
            &report,
            &text_mode_mono(),
            &mut buf,
            Some(std::time::Duration::ZERO),
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("/reload-plugins"),
            "animated path missing reload hint:\n{out}"
        );
    }

    #[test]
    fn plugin_update_json_envelope_unchanged_by_v3_2_13_text_refactor() {
        // The text-mode renderer was rewritten in v3.2.13; the JSON envelope
        // shape MUST stay byte-stable for downstream machine consumers. Pin
        // the field set on a happy-path payload.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: true,
            plists_count: Some(2),
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        let mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &mode, &mut buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(env["ok"], serde_json::json!(true));
        assert_eq!(env["data"]["vault_synced"], serde_json::json!(true));
        assert_eq!(env["data"]["hooks_rewritten"], serde_json::json!(2));
        assert_eq!(env["data"]["plists_rewritten"], serde_json::json!(true));
        assert_eq!(env["data"]["dry_run"], serde_json::json!(false));
    }
}
