use anyhow::{Context, Result};
use onebrain_cache::{qmd_reindex, SpawnOs};
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn run() -> Result<()> {
    let cwd = env::current_dir().context("read current directory")?;
    perform(&cwd, SpawnOs::from_env())
}

/// Inner driver · accepts injected OS for cross-platform testing parity.
fn perform(vault_root: &Path, os: SpawnOs) -> Result<()> {
    qmd_reindex(vault_root, os, spawn_detached)?;
    Ok(())
}

/// Spawn the given command as a detached background process.
/// Returns `Ok(())` on successful spawn (does NOT wait for child).
fn spawn_detached(args: &[String]) -> std::io::Result<()> {
    if args.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "qmd-reindex: empty args",
        ));
    }
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    apply_detach_flags(&mut cmd);

    let _child = cmd.spawn()?;
    // Drop `_child` here · we do NOT wait. On Unix the child inherits to init;
    // on Windows the DETACHED_PROCESS flag means no console binding.
    Ok(())
}

#[cfg(unix)]
fn apply_detach_flags(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(windows)]
fn apply_detach_flags(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    cmd.creation_flags(DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn apply_detach_flags(_cmd: &mut Command) {
    // Other platforms (WASM, embedded) · no detach mechanism · best-effort spawn.
}
