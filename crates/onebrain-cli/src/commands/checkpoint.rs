use anyhow::{Context, Result};
use onebrain_cache::{handle_reset, handle_stop, resolve_session_token, ResolveInputs};
use std::env;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(mode: &str) -> Result<()> {
    let cwd = env::current_dir().context("read current directory")?;
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
