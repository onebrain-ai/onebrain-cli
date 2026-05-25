//! `onebrain migrate <name> [--cutoff YYYY-MM-DD] [--vault DIR]`
//!
//! Internal one-shot migrations. Currently supports `backfill-recapped`
//! (see `onebrain_fs::migrate`). Always exits 0 — failures are reported on
//! stderr but never produce a non-zero exit (matches Bun's internal-command
//! contract so callers like `/update` can chain unconditionally).

use anyhow::{Context, Result};
use onebrain_core::{find_vault_root, load_vault_config_at};
use onebrain_fs::run_backfill_recapped;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn run(name: &str, cutoff: Option<&str>, vault: Option<&str>) -> Result<()> {
    let vault_root = resolve_vault_root(vault).context("resolve vault root")?;
    let vault_root = match vault_root {
        Some(p) => p,
        None => {
            eprintln!("migrate: not inside a vault (no onebrain.yml or vault.yml found)");
            return Ok(());
        }
    };

    let config = match load_vault_config_at(&vault_root) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("migrate: config load failed: {err}");
            return Ok(());
        }
    };
    let logs_folder = vault_root.join(&config.folders.logs);

    let stdout = io::stdout();
    dispatch(name, cutoff, &logs_folder, &mut stdout.lock())
}

fn resolve_vault_root(vault: Option<&str>) -> Result<Option<PathBuf>> {
    if let Some(v) = vault {
        return Ok(Some(PathBuf::from(v)));
    }
    let cwd = env::current_dir().context("read current directory")?;
    Ok(find_vault_root(&cwd).map(|r| r.as_path().to_path_buf()))
}

fn dispatch<W: Write>(
    name: &str,
    cutoff: Option<&str>,
    logs_folder: &Path,
    out: &mut W,
) -> Result<()> {
    match name {
        "backfill-recapped" => {
            let today = chrono::Utc::now().date_naive().to_string();
            let result = run_backfill_recapped(logs_folder, cutoff, &today, |msg| {
                // Mirror Bun: each warning is its own stderr line.
                eprintln!("{msg}");
            });
            writeln!(
                out,
                "backfilled: {} files, skipped: {}",
                result.backfilled, result.skipped
            )
            .context("write stdout")?;
        }
        _ => {
            eprintln!("migrate: unknown migration '{name}'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn dispatch_unknown_migration_emits_no_stdout() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        fs::create_dir_all(&logs).unwrap();
        let mut buf = Vec::new();
        dispatch("not-a-real-migration", None, &logs, &mut buf).unwrap();
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn dispatch_backfill_empty_logs_emits_zero_summary() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        fs::create_dir_all(&logs).unwrap();
        let mut buf = Vec::new();
        dispatch("backfill-recapped", None, &logs, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "backfilled: 0 files, skipped: 0\n");
    }

    #[test]
    fn dispatch_backfill_writes_summary_with_counts() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let month = logs.join("session/2026/04");
        fs::create_dir_all(&month).unwrap();
        fs::write(
            month.join("2026-04-20-session-01.md"),
            "---\ntags: session-log\n---\n\nbody\n",
        )
        .unwrap();
        let mut buf = Vec::new();
        dispatch("backfill-recapped", None, &logs, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "backfilled: 1 files, skipped: 0\n");
    }

    #[test]
    fn dispatch_backfill_empty_snapshot() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        fs::create_dir_all(&logs).unwrap();
        let mut buf = Vec::new();
        dispatch("backfill-recapped", None, &logs, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        insta::assert_snapshot!("migrate_backfill_empty_logs", out);
    }
}
