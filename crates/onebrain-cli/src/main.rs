mod commands;
mod output;
mod tokio_helper;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "onebrain",
    version,
    about = "OneBrain CLI — personal AI OS for Obsidian"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print session metadata as JSON (internal · used by Claude Code SessionStart hook).
    SessionInit {
        /// Vault root directory · defaults to auto-detect from cwd (Bun v2.3.3 parity).
        #[arg(long = "vault-dir")]
        vault_dir: Option<std::path::PathBuf>,
    },

    /// Scan for orphan checkpoint files in 07-logs/checkpoint/ (Slice 2).
    OrphanScan {
        logs_folder: String,
        session_token: String,
    },

    /// Rebuild the qmd search index (Slice 3).
    QmdReindex,

    /// Handle checkpoint lifecycle (stop | reset) · called by Claude Code Stop hook.
    Checkpoint {
        /// Mode · `stop` or `reset`.
        mode: String,
        /// Vault root directory · defaults to auto-detect from cwd (Bun v2.3.3 parity).
        #[arg(long = "vault-dir")]
        vault_dir: Option<std::path::PathBuf>,
    },

    /// Print harness detection result (internal).
    Harness,

    /// Run health checks against the current vault (Slice 6).
    Doctor {
        #[arg(long)]
        fix: bool,
    },

    /// Install Claude Code hooks for this vault (Slice 7).
    RegisterHooks {
        /// Vault root · defaults to current working directory · accepts `--vault-dir` (Bun v2.3.3 parity).
        #[arg(long, visible_alias = "vault-dir")]
        vault: Option<std::path::PathBuf>,
        /// Compute changes but do not write settings.json.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Uninstall OneBrain hooks + permission entries from settings.json.
        #[arg(long)]
        remove: bool,
    },

    /// Install OS-level scheduler entries from vault.yml (Slice 8).
    RegisterSchedule {
        /// Vault root directory · defaults to current working directory · accepts `--vault-dir` (Bun v2.3.3 parity).
        #[arg(long, visible_alias = "vault-dir")]
        vault: Option<std::path::PathBuf>,
        /// Print the plists that would be written without touching disk.
        #[arg(long)]
        dry_run: bool,
        /// Remove all plists for entries currently in vault.yml.
        #[arg(long)]
        remove: bool,
        /// Re-emit plists with the current vault path (logs a notice).
        #[arg(long)]
        refresh: bool,
        /// Clear the .paused marker for the given skill.
        #[arg(long)]
        resume: Option<String>,
        /// Print a status report (which entries are installed).
        #[arg(long)]
        status: bool,
        /// Fire a scheduled skill once for testing (deferred to Slice 12).
        #[arg(long)]
        test: Option<String>,
    },

    /// Run a one-shot vault migration (Slice 9).
    Migrate {
        /// Migration name (currently: `backfill-recapped`).
        name: String,
        /// ISO date cutoff (YYYY-MM-DD) · Bun v2.3.3 positional form · conflicts with `--cutoff`.
        cutoff_date: Option<String>,
        /// ISO date cutoff (YYYY-MM-DD) · Rust-form alternative · conflicts with the positional argument.
        #[arg(long, conflicts_with = "cutoff_date")]
        cutoff: Option<String>,
        /// Vault directory override (default: walk up from cwd) · accepts `--vault-dir` (Bun v2.3.3 parity).
        #[arg(long, visible_alias = "vault-dir")]
        vault: Option<String>,
    },

    /// Initialize a new vault (Slice 10).
    Init {
        /// Skip all prompts and install the Essentials schedule preset (non-interactive · CI-friendly).
        #[arg(long)]
        yes: bool,
        /// Vault root directory · defaults to cwd (Bun v2.3.3 parity).
        #[arg(long = "vault-dir")]
        vault_dir: Option<std::path::PathBuf>,
        /// Overwrite an existing vault.yml without prompting (Bun v2.3.3 parity).
        #[arg(long)]
        force: bool,
        /// Skip the embedded vault-sync step (offline init · scaffold only · re-run `onebrain vault-sync` later to install plugin files).
        #[arg(long = "no-sync")]
        no_sync: bool,
    },

    /// Update OneBrain system files from GitHub (Slice 11).
    Update {
        /// Dry run · report what would change without installing.
        #[arg(long)]
        check: bool,
    },

    /// Run a OneBrain skill in headless mode (Slice 12).
    RunSkill {
        /// Vault root directory · accepts `--vault-dir` (Bun v2.3.3 parity).
        #[arg(long, visible_alias = "vault-dir")]
        vault: String,
        #[arg(long)]
        skill: String,
        /// Pass-through arguments formatted as `key=value` · parsed by the skill runner.
        #[arg(long = "arg")]
        args: Vec<String>,
    },

    /// Sync vault between local and Cloud (Slice 13).
    VaultSync {
        /// Optional vault root · defaults to walk-up from cwd (Bun v2.3.3 parity).
        vault_root: Option<std::path::PathBuf>,
        /// Override branch resolved from vault.yml::update_channel (e.g. `main` or `next`).
        #[arg(long)]
        branch: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let exit_code = match dispatch(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            classify_exit_code(&e)
        }
    };
    std::process::exit(exit_code);
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::SessionInit { vault_dir } => commands::session_init::run(vault_dir),
        Cmd::OrphanScan {
            logs_folder,
            session_token,
        } => commands::orphan_scan::run(&logs_folder, &session_token),
        Cmd::QmdReindex => commands::qmd_reindex::run(),
        Cmd::Checkpoint { mode, vault_dir } => commands::checkpoint::run(&mode, vault_dir),
        Cmd::Harness => commands::harness::run(),
        Cmd::Doctor { fix } => std::process::exit(commands::doctor::run(fix)?),
        Cmd::RegisterHooks {
            vault,
            dry_run,
            remove,
        } => std::process::exit(commands::register_hooks::run(vault, dry_run, remove)?),
        Cmd::RegisterSchedule {
            vault,
            dry_run,
            remove,
            refresh,
            resume,
            status,
            test,
        } => {
            commands::register_schedule::run(vault, dry_run, remove, refresh, resume, status, test)
        }
        Cmd::Migrate {
            name,
            cutoff_date,
            cutoff,
            vault,
        } => {
            // clap `conflicts_with = "cutoff_date"` enforces that at most one of
            // them is set, so `.or(...)` is just selecting whichever the user
            // chose. No silent precedence ambiguity.
            let resolved = cutoff_date.or(cutoff);
            commands::migrate::run(&name, resolved.as_deref(), vault.as_deref())
        }
        Cmd::Init {
            yes,
            vault_dir,
            force,
            no_sync,
        } => std::process::exit(commands::init::run(yes, vault_dir, force, no_sync)?),
        Cmd::Update { check } => std::process::exit(commands::update::run(check)?),
        Cmd::RunSkill { vault, skill, args } => {
            std::process::exit(commands::run_skill::run(&vault, &skill, &args)?)
        }
        Cmd::VaultSync { vault_root, branch } => {
            std::process::exit(commands::vault_sync::run(vault_root, branch)?)
        }
    }
}

fn classify_exit_code(e: &anyhow::Error) -> i32 {
    use onebrain_core::CoreError;
    // Walk the full anyhow chain so CoreError wrapped inside FsError /
    // CacheError still yields its specific exit code (round-1 finding).
    for cause in e.chain() {
        if let Some(core_err) = cause.downcast_ref::<CoreError>() {
            return match core_err {
                CoreError::VaultYamlMissing { .. } => 64, // EX_USAGE-ish
                CoreError::InvalidYaml(_) => 65,          // EX_DATAERR
                CoreError::NotAVault { .. } => 64,
            };
        }
    }
    // No CoreError anywhere in the chain — classify by the root wrapper.
    if e.downcast_ref::<onebrain_fs::FsError>().is_some() {
        return 66;
    }
    if e.downcast_ref::<onebrain_cache::CacheError>().is_some() {
        return 67;
    }
    1
}
