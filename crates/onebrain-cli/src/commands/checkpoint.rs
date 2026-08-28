use anyhow::{Context, Result};
use onebrain_cache::{handle_reset, handle_stop, resolve_session_token, ResolveInputs};
use onebrain_core::SessionToken;
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(mode: &str, vault_dir: Option<PathBuf>, session_token: Option<&str>) -> Result<()> {
    // Bun parity: `--vault-dir <path>` overrides the cwd-based auto-detect.
    // `vault_root` is the directory `handle_stop` will walk up from to find
    // `vault.yml`; it equals the supplied override when present, else cwd.
    let vault_root = match vault_dir {
        Some(dir) => dir,
        None => env::current_dir().context("read current directory")?,
    };
    let cache_dir = std::env::temp_dir();
    // `--session-token` carries a token that was ALREADY resolved elsewhere
    // (the `onebrain hook` runner's `session init` output), so it is sanitized
    // and used verbatim — never re-hashed. Mirrors `session init`'s override.
    let token = match session_token {
        Some(raw) => SessionToken::sanitize(raw)
            .context("session token override must contain at least one alphanumeric character")?,
        None => {
            let inputs = ResolveInputs::from_env();
            resolve_session_token(&inputs).context("resolve session token")?
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match mode {
        "stop" => handle_stop(
            token.as_str(),
            &vault_root,
            now,
            &cache_dir,
            std::io::stdout(),
        ),
        "reset" => handle_reset(token.as_str(), now, &cache_dir),
        other => {
            let _ = writeln!(std::io::stderr(), "checkpoint: unknown mode '{other}'");
        }
    }
    Ok(())
}
