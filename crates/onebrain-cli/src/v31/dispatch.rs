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

    match cli.command {
        // ───── Root verbs ────────────────────────────────────────────
        Cmd::Init(a) => {
            let code = commands::init::run(a.yes, a.vault_dir, a.force, a.no_sync)?;
            std::process::exit(code);
        }
        Cmd::Update(a) => {
            let code = commands::update::run(a.check, a.fresh, a.json, a.plan)?;
            std::process::exit(code);
        }
        Cmd::Doctor(a) => {
            let code = commands::doctor::run(a.fix, a.json)?;
            std::process::exit(code);
        }

        // ───── Session ──────────────────────────────────────────────
        Cmd::Session(SessionCmd { verb }) => match verb {
            SessionVerb::Init { vault_dir } => commands::session_init::run(vault_dir),
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
            } => commands::orphan_scan::run(&logs_folder, &session_token),
        },

        // ───── Qmd ──────────────────────────────────────────────────
        Cmd::Qmd(QmdCmd { verb }) => match verb {
            QmdVerb::Reindex => commands::qmd_reindex::run(),
            QmdVerb::Setup => stubs::not_implemented("qmd setup"),
            QmdVerb::Embed => stubs::not_implemented("qmd embed"),
            QmdVerb::Status => stubs::not_implemented("qmd status"),
            QmdVerb::Search { .. } => stubs::not_implemented("qmd search"),
        },

        // ───── Schedule ─────────────────────────────────────────────
        Cmd::Schedule(ScheduleCmd { verb }) => match verb {
            ScheduleVerb::Register {
                vault,
                dry_run,
                remove,
                refresh,
                resume,
                status,
                test,
            } => {
                let v = vault.or(vault_flag.clone());
                commands::register_schedule::run(v, dry_run, remove, refresh, resume, status, test)
            }
            ScheduleVerb::List => stubs::not_implemented("schedule list"),
            ScheduleVerb::Add { .. } => stubs::not_implemented("schedule add"),
            ScheduleVerb::Remove { .. } => stubs::not_implemented("schedule remove"),
            ScheduleVerb::Status => stubs::not_implemented("schedule status"),
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
                if report.partial_failure.is_some() {
                    return Err(anyhow::anyhow!(
                        "plugin update completed only partially: {}",
                        report.partial_failure.as_deref().unwrap_or("")
                    ));
                }
                Ok(())
            }
            PluginVerb::Migrate {
                name,
                cutoff_date,
                cutoff,
                vault,
            } => {
                let resolved = cutoff_date.or(cutoff);
                let vault_str = vault.as_ref().map(|p| p.to_string_lossy().to_string());
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
            VaultVerb::Scan => stubs::not_implemented("vault scan"),
            VaultVerb::Stats => stubs::not_implemented("vault stats"),
            VaultVerb::Verify => stubs::not_implemented("vault verify"),
            VaultVerb::Current => vault_current::run(vault_flag, &mode),
        },

        // ───── Skill ────────────────────────────────────────────────
        Cmd::Skill(SkillCmd { verb }) => match verb {
            SkillVerb::Run { vault, name, args } => {
                let v = vault.or(vault_flag.clone()).ok_or_else(|| {
                    anyhow::anyhow!("skill run requires --vault <PATH> or --vault-dir <PATH>")
                })?;
                let code = commands::run_skill::run(&v.to_string_lossy(), &name, &args)?;
                std::process::exit(code);
            }
            SkillVerb::List => stubs::not_implemented("skill list"),
            SkillVerb::Bootstrap { .. } => stubs::not_implemented("skill bootstrap"),
            SkillVerb::Help { .. } => stubs::not_implemented("skill help"),
            SkillVerb::Info { .. } => stubs::not_implemented("skill info"),
        },

        // ───── Harness (1-verb · accepted exception) ────────────────
        // Missing verb → silently treat as `detect` for v3.0 back-compat
        // (`onebrain harness` with no verb was the only v3.0 flat invocation
        // remaining after the v3.1 tree rename).
        Cmd::Harness(HarnessCmd { verb }) => match verb.unwrap_or(HarnessVerb::Detect) {
            HarnessVerb::Detect => commands::harness::run(),
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
            BookmarkVerb::List => stubs::not_implemented("bookmark list"),
            BookmarkVerb::Get { .. } => stubs::not_implemented("bookmark get"),
            BookmarkVerb::Import { .. } => stubs::not_implemented("bookmark import"),
        },
        Cmd::Bundle(BundleCmd { verb }) => match verb {
            BundleVerb::Install { .. } => stubs::not_implemented("bundle install"),
            BundleVerb::Help { .. } => stubs::not_implemented("bundle help"),
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
            DreamVerb::List => stubs::not_implemented("dream list"),
            DreamVerb::Tick { .. } => stubs::not_implemented("dream tick"),
            DreamVerb::Done { .. } => stubs::not_implemented("dream done"),
            DreamVerb::Snooze { .. } => stubs::not_implemented("dream snooze"),
        },
        Cmd::Frontmatter(FrontmatterCmd { verb }) => match verb {
            FrontmatterVerb::Parse { .. } => stubs::not_implemented("frontmatter parse"),
            FrontmatterVerb::Extract { .. } => stubs::not_implemented("frontmatter extract"),
            FrontmatterVerb::Update { .. } => stubs::not_implemented("frontmatter update"),
        },
        Cmd::Gateway(GatewayCmd { verb }) => match verb {
            GatewayVerb::Telegram => stubs::not_implemented("gateway telegram"),
            GatewayVerb::Mcp => stubs::not_implemented("gateway mcp"),
        },
        Cmd::Inbox(InboxCmd { verb }) => match verb {
            InboxVerb::List => stubs::not_implemented("inbox list"),
            InboxVerb::Next => stubs::not_implemented("inbox next"),
            InboxVerb::Process { .. } => stubs::not_implemented("inbox process"),
        },
        Cmd::Log(LogCmd { verb }) => match verb {
            LogVerb::Query { .. } => stubs::not_implemented("log query"),
            LogVerb::Append { .. } => stubs::not_implemented("log append"),
            LogVerb::Rotate => stubs::not_implemented("log rotate"),
            LogVerb::Stats => stubs::not_implemented("log stats"),
        },
        Cmd::Memory(MemoryCmd { verb }) => match verb {
            MemoryVerb::List => stubs::not_implemented("memory list"),
            MemoryVerb::Add { .. } => stubs::not_implemented("memory add"),
            MemoryVerb::Update { .. } => stubs::not_implemented("memory update"),
            MemoryVerb::Remove { .. } => stubs::not_implemented("memory remove"),
            MemoryVerb::Promote { .. } => stubs::not_implemented("memory promote"),
            MemoryVerb::Index => stubs::not_implemented("memory index"),
        },
        Cmd::Note(NoteCmd { verb }) => match verb {
            NoteVerb::Search { .. } => stubs::not_implemented("note search"),
            NoteVerb::List => stubs::not_implemented("note list"),
            NoteVerb::Find { .. } => stubs::not_implemented("note find"),
            NoteVerb::Read { .. } => stubs::not_implemented("note read"),
            NoteVerb::Append { .. } => stubs::not_implemented("note append"),
            NoteVerb::New { .. } => stubs::not_implemented("note new"),
            NoteVerb::Move { .. } => stubs::not_implemented("note move"),
            NoteVerb::Archive { .. } => stubs::not_implemented("note archive"),
            NoteVerb::Backlinks { .. } => stubs::not_implemented("note backlinks"),
            NoteVerb::Orphans => stubs::not_implemented("note orphans"),
            NoteVerb::Stat { .. } => stubs::not_implemented("note stat"),
        },
        Cmd::Pause(PauseCmd { verb }) => match verb {
            PauseVerb::List => stubs::not_implemented("pause list"),
            PauseVerb::Snapshot { .. } => stubs::not_implemented("pause snapshot"),
            PauseVerb::Resume { .. } => stubs::not_implemented("pause resume"),
        },
        Cmd::Serve(ServeCmd { verb }) => match verb {
            ServeVerb::Start => stubs::not_implemented("serve start"),
            ServeVerb::Stop => stubs::not_implemented("serve stop"),
            ServeVerb::Status => stubs::not_implemented("serve status"),
        },
        Cmd::Task(TaskCmd { verb }) => match verb {
            TaskVerb::List => stubs::not_implemented("task list"),
            TaskVerb::Add { .. } => stubs::not_implemented("task add"),
            TaskVerb::Done { .. } => stubs::not_implemented("task done"),
        },

        // ───── Hidden v3.0 aliases — emit migration notice + dispatch ─
        Cmd::SessionInitAlias(a) => {
            migration::print_once("session-init", "session init");
            commands::session_init::run(a.vault_dir)
        }
        Cmd::OrphanScanAlias(a) => {
            migration::print_once("orphan-scan", "checkpoint orphans");
            commands::orphan_scan::run(&a.logs_folder, &a.session_token)
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
            let code = commands::run_skill::run(&v.to_string_lossy(), &a.skill, &a.args)?;
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
    use crate::output::{emit, Envelope, ErrorInfo};
    use anyhow::Context;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Data<'a> {
        vault_synced: bool,
        hooks_rewritten: u32,
        plists_rewritten: bool,
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_failure: Option<&'a str>,
    }

    let data = Data {
        vault_synced: report.vault_synced,
        hooks_rewritten: report.hooks_rewritten,
        plists_rewritten: report.plists_rewritten,
        dry_run: report.dry_run,
        note: if report.dry_run {
            Some("dry-run · no changes written")
        } else {
            None
        },
        partial_failure: report.partial_failure.as_deref(),
    };

    let env = if let Some(reason) = report.partial_failure.as_deref() {
        // Partial-failure envelope: ok=false, but data is preserved so the
        // caller can see exactly which steps succeeded.
        let mut e = Envelope {
            version: crate::output::envelope::ENVELOPE_VERSION,
            command: "plugin.update".to_string(),
            ok: false,
            vault: None,
            data: Some(data),
            warnings: Vec::new(),
            error: Some(ErrorInfo::new("E_PLUGIN_UPDATE_PARTIAL", reason)),
        };
        // No warnings yet, but preserve the field shape.
        e.warnings.clear();
        e
    } else {
        Envelope::ok("plugin.update", None, data)
    };

    emit(&env, mode, std::io::stdout().lock(), |e| {
        let d = e.data.as_ref().unwrap();
        let mut s = String::from("plugin update:\n");
        s.push_str(&format!(
            "  vault sync       : {}\n",
            if d.vault_synced { "done" } else { "skipped" }
        ));
        s.push_str(&format!("  hooks rewritten  : {}\n", d.hooks_rewritten));
        s.push_str(&format!(
            "  plists refreshed : {}\n",
            if d.plists_rewritten {
                "done"
            } else {
                "skipped"
            }
        ));
        if d.dry_run {
            s.push_str("  (dry-run · no changes written)\n");
        }
        if let Some(reason) = d.partial_failure {
            s.push_str(&format!("  partial failure  : {reason}\n"));
        }
        s
    })
    .context("plugin update: render summary failed")?;
    Ok(())
}
