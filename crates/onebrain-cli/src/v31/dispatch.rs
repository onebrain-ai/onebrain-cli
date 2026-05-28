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
                emit_plugin_update_summary(&report, &mode)?;
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

/// Render a `plugin update` report to the user. JSON mode emits the
/// canonical envelope; text mode emits a short bullet list.
///
/// On partial failure (mid-flight bail after some on-disk state changed),
/// emits a partial-report envelope with `ok: false`, error code
/// `E_PLUGIN_UPDATE_PARTIAL`, and the partial state visible in `data`. The
/// caller in `dispatch` returns an error after this so the process exit
/// code reflects the failure.
fn emit_plugin_update_summary(
    report: &plugin_update::PluginUpdateReport,
    mode: &OutputMode,
) -> Result<()> {
    emit_plugin_update_summary_to(report, mode, std::io::stdout().lock())
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
///  ⚡  Plugin Update
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
    let _ = write_framed_header(&mut buf, "⚡", "Plugin Update", color, rule_width);

    // ── Step list ─────────────────────────────────────────────────────
    // Today the only step that can populate `partial_failure` is the launchd
    // plist re-registration (see `plugin_update::run` step 4 — vault-sync
    // and hook-rewriter errors bubble as `Err` instead of leaving on-disk
    // state half-applied). v3.2.13: when `partial_failure` is set we mark
    // the plist step as `Fail` so the per-step glyph matches the verdict —
    // pre-3.2.13 it stayed `Ok` and silently misrepresented the failed step
    // as succeeded.
    let partial_failed = d.partial_failure().is_some();
    let vault_detail = if d.vault_synced() { "done" } else { "skipped" };
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
        Step::new(
            "vault sync",
            StepStatus::Ok,
            Some(vault_detail.to_string()),
            None,
        ),
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
    let verdict_text = match (d.partial_failure(), d.dry_run()) {
        (Some(_), _) => "partial failure".to_string(),
        (None, true) => "dry-run · no changes written".to_string(),
        (None, false) => {
            let any_change = d.vault_synced() || d.hooks_rewritten() > 0 || d.plists_rewritten();
            if any_change {
                "update complete".to_string()
            } else {
                "already up-to-date".to_string()
            }
        }
    };
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
            partial_failure: None,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("⚡  Plugin Update"), "header missing:\n{out}");
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
