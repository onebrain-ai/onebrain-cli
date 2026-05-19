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

    /// Write a checkpoint file from a Stop hook reason (Slice 4).
    Checkpoint {
        #[arg(long)]
        reason: String,
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

    /// Migrate vault structure to current version (Slice 9).
    Migrate,

    /// Initialize a new vault (Slice 10).
    Init {
        #[arg(long)]
        yes: bool,
    },

    /// Update OneBrain system files from GitHub (Slice 11).
    Update,

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
        Cmd::OrphanScan { .. } => todo!("Slice 2"),
        Cmd::QmdReindex => todo!("Slice 3"),
        Cmd::Checkpoint { .. } => todo!("Slice 4"),
        Cmd::Harness => todo!("Slice 5"),
        Cmd::Doctor { .. } => todo!("Slice 6"),
        Cmd::RegisterHooks => todo!("Slice 7"),
        Cmd::RegisterSchedule { .. } => todo!("Slice 8"),
        Cmd::Migrate => todo!("Slice 9"),
        Cmd::Init { .. } => todo!("Slice 10"),
        Cmd::Update => todo!("Slice 11"),
        Cmd::RunSkill { .. } => todo!("Slice 12"),
        Cmd::VaultSync => todo!("Slice 13"),
    }
}

fn classify_exit_code(e: &anyhow::Error) -> i32 {
    use onebrain_core::CoreError;
    if let Some(core_err) = e.downcast_ref::<CoreError>() {
        return match core_err {
            CoreError::VaultYamlMissing { .. } => 64, // EX_USAGE-ish
            CoreError::InvalidYaml(_) => 65,          // EX_DATAERR
            CoreError::NotAVault { .. } => 64,
        };
    }
    if e.downcast_ref::<onebrain_fs::FsError>().is_some() {
        return 66;
    }
    if e.downcast_ref::<onebrain_cache::CacheError>().is_some() {
        return 67;
    }
    1
}
