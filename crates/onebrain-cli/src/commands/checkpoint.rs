use anyhow::{Context, Result};
use onebrain_cache::{handle_reset, handle_stop, resolve_session_token, ResolveInputs};
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(mode: &str, vault_dir: Option<PathBuf>) -> Result<()> {
    // Bun parity: `--vault-dir <path>` overrides the cwd-based auto-detect.
    let cwd = match vault_dir {
        Some(dir) => dir,
        None => env::current_dir().context("read current directory")?,
    };
    let cache_dir = std::env::temp_dir();
    let inputs = ResolveInputs::from_env();
    let token = resolve_session_token(&inputs).context("resolve session token")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match mode {
        "stop" => handle_stop(token.as_str(), &cwd, now, &cache_dir, std::io::stdout()),
        "reset" => handle_reset(token.as_str(), now, &cache_dir),
        other => {
            let _ = writeln!(std::io::stderr(), "checkpoint: unknown mode '{other}'");
        }
    }
    Ok(())
}
