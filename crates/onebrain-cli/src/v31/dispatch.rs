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
            let code = commands::doctor::run(a.fix, a.json, vault_flag.clone(), &mode)?;
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
            HarnessVerb::Detect => commands::harness::run(&mode),
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
            NoteVerb::Move { .. } => {
                stubs::not_implemented_vault_required(vault_flag.clone(), "note move")
            }
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
    emit_plugin_update_summary_to(report, mode, std::io::stdout().lock())
}

/// Same as [`emit_plugin_update_summary`] but with an injectable writer for
/// unit tests. R2-M4: the broken-pipe regression test feeds a writer that
/// always returns `io::ErrorKind::BrokenPipe`; the function must surface
/// the failure as `Err` so `exit::exit_code_for` can classify it to 0 (the
/// POSIX-correct exit when downstream hung up). Previously the production
/// code used `let _ = emit(...)` and the integration smoke-test couldn't
/// observe propagation deterministically (OS pipe buffer size varies).
pub(crate) fn emit_plugin_update_summary_to<W: std::io::Write>(
    report: &plugin_update::PluginUpdateReport,
    mode: &OutputMode,
    writer: W,
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

    emit(&env, mode, writer, |e| {
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
        // R2-H1: render soft-warnings under the summary so text-mode users
        // can see them too. Format: one line per warning, prefixed with the
        // warning sigil.
        for w in &e.warnings {
            s.push_str(&format!("  ⚠ {}: {}\n", w.code, w.message));
        }
        s
    })
    .context("plugin update: render summary failed")?;
    Ok(())
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
}
