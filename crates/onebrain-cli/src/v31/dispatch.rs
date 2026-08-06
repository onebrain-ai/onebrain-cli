//! Central dispatcher — maps the v3.1 [`Cmd`] enum to handler functions.
//!
//! Layout:
//! - Root verbs (`init` / `update` / `doctor`) → existing v3.0 handlers.
//! - Hook-protocol verbs (`session init`, `checkpoint *`) → existing v3.0
//!   handlers (their JSON shape is already the canonical
//!   `{"decision":"block",...}` for the hook contract). The legacy
//!   `qmd-reindex` alias now repoints to the native `search reindex` handler.
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
    // One-time relocation of the native-search state out of the OS-purgeable
    // cache dir into the persistent data dir (issue #114 · ADR 0021). Runs
    // before any command touches the search cache; idempotent + cheap (a single
    // stat once the move is done), so it's safe on every invocation including
    // the hot-path hook verbs (`session init`, `checkpoint`).
    migration::migrate_search_cache();
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
        Cmd::Completions(a) => {
            let code = commands::completions::run(a.shell);
            std::process::exit(code);
        }

        // ───── Session ──────────────────────────────────────────────
        Cmd::Session(SessionCmd { verb }) => match verb {
            SessionVerb::Init {
                vault_dir,
                session_token,
            } => commands::session_init::run(
                vault_dir,
                vault_flag.clone(),
                session_token.as_deref(),
                &mode,
            ),
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

        // ───── Qmd — removed v3.4.5 (native search replaces it) ──────
        Cmd::Qmd { .. } => Err(anyhow::anyhow!(
            "`onebrain qmd` was removed in v3.4.5 — use `onebrain search …` \
             (reindex · status · query · search · vsearch)"
        )),

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
            // `List` reuses the existing `--status` path (reads onebrain.yml
            // schedule block, prints each entry with cron/at + installed
            // ✓/✗). Previously only reachable via `schedule register
            // --status`; #116 asked for a plain `schedule list` too.
            ScheduleVerb::List => {
                let v = vault_flag.clone();
                commands::register_schedule::run(v, false, false, false, None, true, None)
            } // Non-protocol verbs are vault-required (need to know which
              // vault's plists to list / which YAML to modify).
        },

        // ───── Plugin ───────────────────────────────────────────────
        Cmd::Plugin(PluginCmd { verb }) => match verb {
            PluginVerb::Install {
                vault_dir,
                harness,
                dry_run,
                ..
            } => {
                // v3.0 `register-hooks` + `vault-sync` together. For v3.1 we
                // expose this as a hidden verb under `plugin install`; full
                // first-install flow runs through `onebrain init`.
                let v = vault_dir.or(vault_flag.clone());
                let code = if harness == HarnessArg::Codex {
                    let resolved = crate::vault_ctx::require(v)?;
                    commands::codex_plugin::install(resolved.root.as_path(), dry_run)?
                } else {
                    commands::register_hooks::run(v, dry_run, false)?
                };
                std::process::exit(code);
            }
            PluginVerb::Uninstall { harness, dry_run } => {
                if harness == HarnessArg::Codex {
                    let resolved = crate::vault_ctx::require(vault_flag.clone())?;
                    let code = commands::codex_plugin::uninstall(resolved.root.as_path(), dry_run)?;
                    std::process::exit(code);
                }
                stubs::not_implemented("plugin uninstall")
            }
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

        Cmd::Daemon(DaemonCmd { verb }) => match verb {
            DaemonVerb::Start { vault } => commands::daemon::run_start(&mode, vault.as_deref()),
            DaemonVerb::Stop { vault, all } => {
                // Accept `--vault` either after `stop` (local) or at the global
                // position (`onebrain --vault X daemon stop`) — fall back to the
                // global flag so both spellings target the same vault's slot.
                let v = vault.as_deref().or(vault_flag.as_deref());
                commands::daemon::run_stop(&mode, v, all)
            }
            DaemonVerb::Status => commands::daemon::run_status(&mode),
            // Hidden internal verb — the detached child's body.
            DaemonVerb::Run { vault } => commands::daemon::run_internal(vault.as_deref()),
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
            NoteVerb::Edit(args) => commands::note_edit::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Delete(args) => commands::note_delete::run(vault_flag.clone(), &mode, &args),
            NoteVerb::Mkdir(args) => commands::note_mkdir::run(vault_flag.clone(), &mode, &args),
        },
        Cmd::Search(SearchCmd { verb }) => match verb {
            SearchVerb::Query(args) => {
                commands::search_query::run_query(vault_flag.clone(), &mode, &args)
            }
            SearchVerb::Search(args) => {
                commands::search_query::run_lex(vault_flag.clone(), &mode, &args)
            }
            SearchVerb::Vsearch(args) => {
                commands::search_query::run_vsearch(vault_flag.clone(), &mode, &args)
            }
            SearchVerb::Get(args) => commands::search_get::run(vault_flag.clone(), &mode, &args),
            SearchVerb::Status => commands::search_status::run(vault_flag.clone(), &mode),
            SearchVerb::Reindex(args) => {
                commands::search_reindex::run(vault_flag.clone(), &mode, &args)
            }
            SearchVerb::Model(SearchModelCmd { verb }) => match verb {
                None => commands::search_model::run_bare(vault_flag.clone(), &mode),
                Some(SearchModelVerb::List(args)) => {
                    commands::search_model::run_list(vault_flag.clone(), &mode, &args)
                }
                Some(SearchModelVerb::Set(args)) => {
                    commands::search_model::run_set(vault_flag.clone(), &mode, &args)
                }
                Some(SearchModelVerb::Remove(args)) => {
                    commands::search_model::run_remove(vault_flag.clone(), &mode, &args)
                }
            },
        },
        Cmd::Mcp => commands::mcp::run(vault_flag.clone()),
        Cmd::Serve(args) => {
            // Fold the global `--vault` into the serve-local override so
            // `onebrain serve --vault PATH` and `--vault-dir PATH` both work.
            let mut args = args;
            if args.vault_dir.is_none() {
                args.vault_dir = vault_flag.clone();
            }
            commands::serve::run(&args, &mode)
        }
        Cmd::Task(TaskCmd { verb }) => match verb {
            TaskVerb::List(args) => commands::task_list::run(vault_flag.clone(), &mode, &args),
        },
        Cmd::Token(TokenCmd { verb }) => match verb {
            TokenVerb::Gain(args) => commands::token_gain::run(vault_flag.clone(), &mode, &args),
            TokenVerb::Check(args) => {
                let code = commands::token_check::run(vault_flag.clone(), &args.path)?;
                std::process::exit(code);
            }
            TokenVerb::Discover(args) => {
                commands::token_discover::run(vault_flag.clone(), &mode, &args)
            }
        },

        // ───── Hidden v3.0 aliases — emit migration notice + dispatch ─
        Cmd::SessionInitAlias(a) => {
            migration::print_once("session-init", "session init");
            commands::session_init::run(a.vault_dir, vault_flag.clone(), None, &mode)
        }
        Cmd::OrphanScanAlias(a) => {
            migration::print_once("orphan-scan", "checkpoint orphans");
            commands::orphan_scan::run(&a.logs_folder, &a.session_token, &mode)
        }
        Cmd::QmdReindexAlias => {
            migration::print_once("qmd-reindex", "search reindex");
            commands::search_reindex::run(
                vault_flag.clone(),
                &mode,
                &crate::cli::SearchReindexArgs {
                    paths: Vec::new(),
                    force: false,
                    lex_only: false,
                    pending_only: false,
                },
            )
        }
        Cmd::RegisterHooksAlias(a) => {
            migration::print_once("register-hooks", "plugin install");
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
            // #263 Part 2: resolve through the canonical chain (flag >
            // ONEBRAIN_VAULT > walk-up from cwd) instead of requiring an
            // explicit --vault/--vault-dir — matches the modern `skill run`
            // arm above. Without this, a bare/relative legacy call that
            // found no vault anywhere would fall through to
            // `run_skill::run`'s own internal config check and surface a
            // bare `Ok(78)` (EX_CONFIG) instead of the canonical
            // `E_VAULT_NOT_FOUND` (exit 64).
            let resolved = crate::vault_ctx::require(a.vault.or(vault_flag.clone()))?;
            let vault = resolved.root.as_path().to_string_lossy();
            // Legacy alias keeps the claude default with no model override;
            // `--harness` / `--model` live on the modern `skill run`.
            let code = commands::run_skill::run(
                &vault,
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
    writer: W,
    step_delay_override: Option<std::time::Duration>,
) -> Result<()> {
    // Animated path: `force_static = false` → the per-step braille spinner
    // animates before each row resolves. Shares the whole render body with the
    // static text path via `render_plugin_update_inner` (v3.2.18).
    let fields = PluginUpdateFields {
        vault_synced: report.vault_synced,
        hooks_rewritten: report.hooks_rewritten,
        plists_rewritten: report.plists_rewritten,
        plists_count: report.plists_count,
        version_before: report.version_before.as_deref(),
        version_after: report.version_after.as_deref(),
        dry_run: report.dry_run,
        partial_failure: report.partial_failure.as_deref(),
        daemon_retired: report.daemon_retired,
    };
    render_plugin_update_inner(writer, &fields, mode, false, step_delay_override)
}

/// Plain, borrow-only view of the fields the `plugin update` renderer needs.
/// Built from either the live [`plugin_update::PluginUpdateReport`] (animated
/// path) or the serialized [`PluginUpdateData`] envelope payload (static text
/// path), so [`render_plugin_update_inner`] stays agnostic to its data source.
/// Replaces the old `PluginUpdateTextData` trait (v3.2.18 · Code-Simplifier
/// finding 1.2/1.3 on PR #57 — the trait existed only to make the static
/// renderer generic over a test double, which a plain struct does for free).
struct PluginUpdateFields<'a> {
    vault_synced: bool,
    hooks_rewritten: u32,
    plists_rewritten: bool,
    plists_count: Option<u32>,
    version_before: Option<&'a str>,
    version_after: Option<&'a str>,
    dry_run: bool,
    partial_failure: Option<&'a str>,
    /// #291: a live version-skewed warm daemon was retired this run.
    daemon_retired: bool,
}

/// Shared body for both `plugin update` text surfaces (v3.2.18 · unifies the
/// former `render_plugin_update_text` + `render_plugin_update_animated_to`).
/// `force_static = true` suppresses the per-step braille spinner (the static
/// text-mode report rendered into a buffer); `false` animates each step (the
/// live stdout path). `step_delay` overrides the per-step dwell — `Some(ZERO)`
/// in unit tests so the animated branch runs without sleeping. The header,
/// step list, and verdict footer are byte-identical across both modes; only
/// the per-step spinner overlay differs.
fn render_plugin_update_inner<W: std::io::Write>(
    mut writer: W,
    f: &PluginUpdateFields<'_>,
    mode: &OutputMode,
    force_static: bool,
    step_delay: Option<std::time::Duration>,
) -> Result<()> {
    use crate::output::{
        framing_rule_n, is_color_text, write_framed_header, ProgressRenderer, Section, Step,
        StepStatus, RULE_WIDTH,
    };

    let color = is_color_text(mode);
    let rule_width = RULE_WIDTH;
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    // ── Header ────────────────────────────────────────────────────────
    write_framed_header(&mut writer, "🔄", "Plugin Update", color, rule_width)?;

    // ── Step list ─────────────────────────────────────────────────────
    // v3.2.13: when `partial_failure` is set, mark the launchd-plist step as
    // `Fail` so the per-step glyph matches the verdict (pre-3.2.13 it stayed
    // `Ok` and silently misrepresented the failed step as succeeded). The
    // plist re-registration is the only step that can populate it today.
    let partial_failed = f.partial_failure.is_some();
    let vault_detail =
        plugin_update_vault_detail(f.vault_synced, f.dry_run, f.version_before, f.version_after);
    let hooks_detail = format!("{} rewritten", f.hooks_rewritten);
    // `plists_count` (v3.2.13): `Some(N>0)` → N refreshed; `Some(0)` → vault
    // has no schedule entries (well-formed no-op); `None` → step didn't run
    // (dry-run, or a prior step failed).
    let plists_detail = if partial_failed {
        "failed".to_string()
    } else {
        match f.plists_count {
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
    let section_header = if f.dry_run {
        "Update plan (dry-run)"
    } else {
        "Update steps"
    };
    let section = Section::new(section_header, steps);
    let mut renderer = ProgressRenderer::with_writer(&mut writer, force_static, color);
    if let Some(d) = step_delay {
        renderer.set_step_delay(d);
    }
    renderer.render_section(&section)?;

    // ── Verdict footer ────────────────────────────────────────────────
    let verdict_status = if partial_failed {
        StepStatus::Fail
    } else {
        StepStatus::Ok
    };
    let verdict_glyph = verdict_status.glyph();
    let verdict_prefix = verdict_status.ansi_prefix(color);
    let verdict_text = plugin_update_verdict_text(
        partial_failed,
        f.dry_run,
        f.vault_synced,
        f.hooks_rewritten,
        f.plists_rewritten,
        f.version_before,
        f.version_after,
    );
    // Trailing count right-aligned to the rule width (matches doctor's layout).
    let total_str = "3 steps";
    let left_cols = 1 + 1 + 2 + verdict_text.chars().count(); // " ✓  " + text
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
    // Failure hint — indented `└` line; multi-line reasons flattened to ` · `
    // so the indented layout stays intact.
    if let Some(reason) = f.partial_failure {
        let one_line = reason.replace('\n', " · ");
        if color {
            writeln!(writer, " {dim}└ {one_line}{reset}")?;
        } else {
            writeln!(writer, " └ {one_line}")?;
        }
    }
    // R6: reload next-step fires on a real version change regardless of partial
    // failure — `version_after` is read post-sync, so the new version already
    // landed on disk and the running session still holds the old one.
    // Suppressing the hint on partial failure would lose it exactly when the
    // user most needs it.
    if let Some(hint) = plugin_update_reload_hint(f.dry_run, f.version_before, f.version_after) {
        if color {
            writeln!(writer, " {dim}↻ {hint}{reset}")?;
        } else {
            writeln!(writer, " ↻ {hint}")?;
        }
    }
    // #291: surface the retired warm daemon so the user knows the dark
    // dashboard has been refreshed. The respawned daemon comes up at our
    // version (`own_version`) on the next call. "warm daemon" matches the
    // `onebrain update` wording (`retired {n} warm daemon(s)`) for parity.
    if f.daemon_retired {
        let own = crate::commands::daemon_client::own_version();
        let msg = format!("retired warm daemon — respawns at v{own} on next use");
        if color {
            writeln!(writer, " {dim}↻ {msg}{reset}")?;
        } else {
            writeln!(writer, " ↻ {msg}")?;
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
    /// #291: `true` only when a live version-skewed warm daemon was retired.
    /// Skipped when `false` so the JSON envelope shape stays additive (a
    /// normal plugin update gains no new field).
    #[serde(skip_serializing_if = "is_false")]
    daemon_retired: bool,
}

/// serde `skip_serializing_if` predicate — omit a `false` bool from the JSON.
fn is_false(b: &bool) -> bool {
    !*b
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
        daemon_retired: report.daemon_retired,
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
fn render_plugin_update_text(
    env: &crate::output::Envelope<PluginUpdateData<'_>>,
    mode: &OutputMode,
) -> String {
    let d = match env.data.as_ref() {
        Some(d) => d,
        None => return String::new(),
    };
    let fields = PluginUpdateFields {
        vault_synced: d.vault_synced,
        hooks_rewritten: d.hooks_rewritten,
        plists_rewritten: d.plists_rewritten,
        plists_count: d.plists_count,
        version_before: d.version_before,
        version_after: d.version_after,
        dry_run: d.dry_run,
        partial_failure: d.partial_failure,
        daemon_retired: d.daemon_retired,
    };
    // Static text mode: `force_static = true` — no spinner for `plugin
    // update`; the operation is fast (single HTTP fetch + a few file writes)
    // and the user already sees doctor-level animation elsewhere. Render into
    // a buffer and hand the string back to `emit` — writes to a `Vec` never
    // fail, so the `io::Result` is safely discarded here.
    let mut buf: Vec<u8> = Vec::new();
    let _ = render_plugin_update_inner(&mut buf, &fields, mode, true, None);
    String::from_utf8(buf).unwrap_or_default()
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
            daemon_retired: false,
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
            daemon_retired: false,
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
    fn plugin_update_daemon_retired_renders_in_text_and_json() {
        // #291 (R2-#1): when step 5 retired a skewed warm daemon, BOTH surfaces
        // must show it — the framed text report gets the `↻ retired warm daemon`
        // glyph line naming our version, and the JSON envelope gains a
        // `daemon_retired: true` field. Mirrors the partial-failure surfacing.
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
            daemon_retired: true,
        };
        let own = crate::commands::daemon_client::own_version();

        // Text surface — the glyph line renders with our version.
        let mut text_buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut text_buf).unwrap();
        let text = String::from_utf8(text_buf).unwrap();
        assert!(
            text.contains("↻ retired warm daemon"),
            "retire glyph line missing:\n{text}"
        );
        assert!(
            text.contains(&format!("respawns at v{own}")),
            "retire line must name our version:\n{text}"
        );

        // JSON surface — the field appears and is true.
        let mut json_buf = Vec::new();
        let json_mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &json_mode, &mut json_buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&json_buf).unwrap();
        assert_eq!(
            env["data"]["daemon_retired"],
            serde_json::json!(true),
            "JSON envelope must carry daemon_retired: true:\n{env}"
        );
    }

    #[test]
    fn plugin_update_daemon_not_retired_omits_json_field() {
        // The additive-shape guard: with no retire, `daemon_retired` is skipped
        // from the JSON (via `skip_serializing_if`) so a normal plugin update's
        // envelope shape is unchanged, and the text report shows no retire line.
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
            daemon_retired: false,
        };
        let mut json_buf = Vec::new();
        let json_mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &json_mode, &mut json_buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&json_buf).unwrap();
        assert!(
            env["data"].get("daemon_retired").is_none(),
            "daemon_retired must be omitted when false:\n{env}"
        );

        let mut text_buf = Vec::new();
        emit_plugin_update_summary_to(&report, &text_mode_mono(), &mut text_buf).unwrap();
        let text = String::from_utf8(text_buf).unwrap();
        assert!(
            !text.contains("retired warm daemon"),
            "no retire line when nothing was retired:\n{text}"
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
        // returns `Ok(0)` from `register_schedule::run_embedded` — a well-formed
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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
            daemon_retired: false,
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

    // ─────────────────────────────────────────────────────────────────────
    // plugin_update_vault_detail — branch coverage for uncovered arms
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn vault_detail_dry_run_with_known_version_emits_current_skipped() {
        // dry_run=true + before=Some → "current vX · skipped"
        // This arm was the only uncovered branch in the dry_run block
        // (the None arm is hit by the dry-run integration test above).
        let s = plugin_update_vault_detail(false, true, Some("3.1.3"), None);
        assert_eq!(s, "current v3.1.3 · skipped");
    }

    #[test]
    fn vault_detail_same_version_shows_up_to_date() {
        // vault_synced=true, not dry-run, before==after → "vX · up-to-date"
        let s = plugin_update_vault_detail(true, false, Some("3.1.4"), Some("3.1.4"));
        assert_eq!(s, "v3.1.4 · up-to-date");
    }

    #[test]
    fn vault_detail_fresh_install_no_before_version() {
        // vault_synced=true, not dry-run, before=None, after=Some → "installed vY"
        let s = plugin_update_vault_detail(true, false, None, Some("3.1.4"));
        assert_eq!(s, "installed v3.1.4");
    }

    // ─────────────────────────────────────────────────────────────────────
    // plugin_update_verdict_text — branch coverage for uncovered arms
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn verdict_text_dry_run_with_known_version() {
        // dry_run=true + before=Some → "dry-run · current vX"
        // The None arm ("dry-run · no changes written") is hit by the
        // dry-run integration test; this arm was uncovered.
        let s = plugin_update_verdict_text(false, true, false, 0, false, Some("3.1.3"), None);
        assert_eq!(s, "dry-run · current v3.1.3");
    }

    #[test]
    fn verdict_text_update_complete_with_version_suffix() {
        // vault_synced=true, same versions (no bump) → any_change=true,
        // suffix=" · v3.1.4" → "update complete · v3.1.4"
        let s =
            plugin_update_verdict_text(false, false, true, 0, false, Some("3.1.4"), Some("3.1.4"));
        assert_eq!(s, "update complete · v3.1.4");
    }

    #[test]
    fn verdict_text_already_up_to_date_with_version_suffix() {
        // No changes at all, but version known → "already up-to-date · vX"
        // The no-suffix variant is covered by the text integration test
        // (version_before/after = None); this covers the suffix arm.
        let s =
            plugin_update_verdict_text(false, false, false, 0, false, Some("3.1.4"), Some("3.1.4"));
        assert_eq!(s, "already up-to-date · v3.1.4");
    }

    // ─────────────────────────────────────────────────────────────────────
    // JSON envelope — uncovered code paths
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn plugin_update_json_partial_failure_sets_ok_false_and_error_code() {
        // Exercises the `Envelope::partial` branch in emit_plugin_update_summary_to.
        // The happy-path test above only exercises `Envelope::ok`.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 2,
            plists_rewritten: false,
            plists_count: None,
            version_before: None,
            version_after: None,
            partial_failure: Some("launchctl exit 1".to_string()),
            warnings: Vec::new(),
            daemon_retired: false,
        };
        let mut buf = Vec::new();
        let mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &mode, &mut buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            env["ok"],
            serde_json::json!(false),
            "partial failure must set ok=false"
        );
        assert_eq!(
            env["error"]["code"],
            serde_json::json!("E_PLUGIN_UPDATE_PARTIAL"),
            "partial failure must carry canonical error code"
        );
        // data must be preserved so consumers can see which steps succeeded
        assert_eq!(
            env["data"]["vault_synced"],
            serde_json::json!(true),
            "partial failure must preserve data.vault_synced"
        );
    }

    #[test]
    fn plugin_update_json_version_fields_serialized_when_present() {
        // version_before and version_after are Option<String> with
        // skip_serializing_if; exercise the Some(_) path for both.
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 1,
            plists_rewritten: false,
            plists_count: Some(1),
            version_before: Some("3.1.3".to_string()),
            version_after: Some("3.1.4".to_string()),
            partial_failure: None,
            warnings: Vec::new(),
            daemon_retired: false,
        };
        let mut buf = Vec::new();
        let mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &mode, &mut buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(env["data"]["version_before"], serde_json::json!("3.1.3"));
        assert_eq!(env["data"]["version_after"], serde_json::json!("3.1.4"));
        assert_eq!(env["ok"], serde_json::json!(true));
    }

    #[test]
    fn plugin_update_json_dry_run_note_field_is_present() {
        // The `note` field in PluginUpdateData is Some("dry-run · no changes written")
        // when dry_run=true and None otherwise; None is omitted via
        // skip_serializing_if. Exercise the Some path through a dry-run JSON call.
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
            daemon_retired: false,
        };
        let mut buf = Vec::new();
        let mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &mode, &mut buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            env["data"]["note"],
            serde_json::json!("dry-run · no changes written"),
            "dry-run must include note field in JSON envelope"
        );
        assert_eq!(env["data"]["dry_run"], serde_json::json!(true));
    }

    #[test]
    fn plugin_update_json_warnings_plumbed_into_envelope() {
        // Exercises the `for w in &report.warnings { env.with_warning(...) }` loop.
        // The loop body is a distinct coverage line; all other tests pass an empty
        // warnings Vec and thus skip it entirely.
        use crate::v31::hook_rewriter::RewriteWarning;
        let report = plugin_update::PluginUpdateReport {
            dry_run: false,
            vault_synced: true,
            hooks_rewritten: 1,
            plists_rewritten: false,
            plists_count: Some(0),
            version_before: None,
            version_after: None,
            partial_failure: None,
            warnings: vec![RewriteWarning {
                code: "W_MALFORMED_HOOK_ENTRY".to_string(),
                message: "unexpected hook shape at index 2".to_string(),
            }],
            daemon_retired: false,
        };
        let mut buf = Vec::new();
        let mode = OutputMode::Json { pretty: false };
        emit_plugin_update_summary_to(&report, &mode, &mut buf).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(env["ok"], serde_json::json!(true));
        let warnings = env["warnings"]
            .as_array()
            .expect("envelope must contain a warnings array");
        assert_eq!(
            warnings.len(),
            1,
            "exactly one warning must appear in envelope"
        );
        assert_eq!(
            warnings[0]["code"],
            serde_json::json!("W_MALFORMED_HOOK_ENTRY")
        );
        assert!(
            warnings[0]["message"]
                .as_str()
                .unwrap_or("")
                .contains("unexpected hook shape"),
            "warning message must be forwarded verbatim"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // AlreadyReported Display impl
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn already_reported_display_formats_expected_message() {
        // The `fmt` impl (line 35) is not exercised by the downcast tests above
        // because `downcast_ref` doesn't call Display.
        let sentinel = AlreadyReported;
        assert_eq!(format!("{sentinel}"), "envelope already emitted to stdout");
    }
}
