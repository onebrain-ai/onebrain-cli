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
    SessionInit,

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
    },

    /// Print harness detection result (internal).
    Harness,

    /// Run health checks against the current vault (Slice 6).
    Doctor {
        #[arg(long)]
        fix: bool,
    },

    /// Install Claude Code hooks for this vault (Slice 7).
    RegisterHooks,

    /// Install OS-level scheduler entries from vault.yml (Slice 8).
    RegisterSchedule {
        #[arg(long)]
        resume: Option<String>,
    },

    /// Run a one-shot vault migration (Slice 9).
    Migrate {
        /// Migration name (currently: `backfill-recapped`).
        name: String,
        /// Skip session logs whose ISO date prefix is strictly greater than this cutoff (inclusive lower bound).
        #[arg(long)]
        cutoff: Option<String>,
        /// Vault directory override (default: walk up from cwd).
        #[arg(long)]
        vault: Option<String>,
    },

    /// Initialize a new vault (Slice 10).
    Init {
        #[arg(long)]
        yes: bool,
    },

    /// Update OneBrain system files from GitHub (Slice 11).
    Update {
        /// Dry run · report what would change without installing.
        #[arg(long)]
        check: bool,
    },

    /// Run a OneBrain skill in headless mode (Slice 12).
    RunSkill {
        #[arg(long)]
        vault: String,
        #[arg(long)]
        skill: String,
        /// Pass-through arguments formatted as `key=value` · parsed by the skill runner.
        #[arg(long = "arg")]
        args: Vec<String>,
    },

    /// Sync vault between local and Cloud (Slice 13).
    VaultSync,
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
        Cmd::SessionInit => commands::session_init::run(),
        Cmd::OrphanScan {
            logs_folder,
            session_token,
        } => commands::orphan_scan::run(&logs_folder, &session_token),
        Cmd::QmdReindex => commands::qmd_reindex::run(),
        Cmd::Checkpoint { mode } => commands::checkpoint::run(&mode),
        Cmd::Harness => commands::harness::run(),
        Cmd::Doctor { fix } => std::process::exit(commands::doctor::run(fix)?),
        Cmd::RegisterHooks => todo!("Slice 7"),
        Cmd::RegisterSchedule { .. } => todo!("Slice 8"),
        Cmd::Migrate {
            name,
            cutoff,
            vault,
        } => commands::migrate::run(&name, cutoff.as_deref(), vault.as_deref()),
        Cmd::Init { .. } => todo!("Slice 10"),
        Cmd::Update { check } => std::process::exit(commands::update::run(check)?),
        Cmd::RunSkill { vault, skill, args } => {
            std::process::exit(commands::run_skill::run(&vault, &skill, &args)?)
        }
        Cmd::VaultSync => todo!("Slice 13"),
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
