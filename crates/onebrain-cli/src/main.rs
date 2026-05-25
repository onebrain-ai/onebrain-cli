//! `onebrain` binary entry point (v3.1 Consistency Standard).
//!
//! All command surface is declared in [`cli`]; dispatch lives in
//! [`v31::dispatch`]. Exit-code mapping is centralised in [`exit`].

mod banner;
mod cli;
mod commands;
mod exit;
mod legacy_output;
mod migration;
mod output;
mod safety;
mod tokio_helper;
mod v31;
mod vault_ctx;

use clap::Parser;
use cli::Cli;

fn main() {
    let cli = Cli::parse();
    let exit_code = match v31::dispatch::dispatch(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            exit::exit_code_for(&e)
        }
    };
    std::process::exit(exit_code);
}
