//! `onebrain register-hooks` — idempotent .claude/settings.json wiring.
//!
//! Adds the Stop hook (canonical exec form), optional PostToolUse qmd hook
//! when `qmd_collection` is set in vault.yml, and the 14-entry OneBrain
//! permission set. `--dry-run` computes the diff without writing.
//! `--remove` is the uninstall path.

use anyhow::{Context, Result};
use onebrain_fs::register_hooks::{self, RegisterHooksOptions, RegisterHooksResult};
use std::io::Write;
use std::path::PathBuf;

pub fn run(vault: Option<PathBuf>, dry_run: bool, remove: bool) -> Result<i32> {
    let opts = RegisterHooksOptions {
        vault_dir: vault,
        dry_run,
        remove,
    };
    let result = register_hooks::run(opts).context("register-hooks failed")?;
    print_summary(&result, std::io::stdout())?;
    Ok(if result.ok { 0 } else { 1 })
}

fn print_summary<W: Write>(result: &RegisterHooksResult, mut w: W) -> Result<()> {
    let prefix = "register-hooks:";
    if result.direct_mode {
        writeln!(
            w,
            "{prefix} direct mode · no hooks to register (run `onebrain` from your shell directly)"
        )?;
        return Ok(());
    }
    if !result.claude_harness {
        writeln!(w, "{prefix} no claude harness detected · nothing to do")?;
        return Ok(());
    }
    if result.remove_mode {
        writeln!(
            w,
            "{prefix} removed · permissions removed = {}",
            result.permissions_removed.len()
        )?;
        writeln!(
            w,
            "{prefix} {}",
            if result.wrote {
                "done"
            } else {
                "dry-run · no changes written"
            }
        )?;
        return Ok(());
    }
    if let Some(s) = result.stop {
        writeln!(w, "{prefix} Stop {}", s.as_str())?;
    }
    if let Some(s) = result.qmd {
        writeln!(w, "{prefix} PostToolUse {}", s.as_str())?;
    }
    writeln!(
        w,
        "{prefix} permissions added = {}",
        result.permissions_added.len()
    )?;
    writeln!(
        w,
        "{prefix} {}",
        if result.wrote {
            "done"
        } else {
            "dry-run · no changes written"
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_fs::register_hooks::HookStatus;
    use std::path::PathBuf;

    fn fake_result() -> RegisterHooksResult {
        RegisterHooksResult {
            ok: true,
            stop: Some(HookStatus::Added),
            qmd: None,
            permissions_added: vec!["Read".into(), "Write".into()],
            permissions_removed: vec![],
            wrote: true,
            vault_dir: PathBuf::from("/tmp/v"),
            claude_harness: true,
            remove_mode: false,
            direct_mode: false,
        }
    }

    #[test]
    fn print_summary_direct_mode() {
        let mut r = fake_result();
        r.claude_harness = false;
        r.direct_mode = true;
        let mut buf = Vec::new();
        print_summary(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("direct mode"));
        assert!(s.contains("no hooks to register"));
    }

    #[test]
    fn print_summary_fresh_install() {
        let r = fake_result();
        let mut buf = Vec::new();
        print_summary(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Stop added"));
        assert!(s.contains("permissions added = 2"));
        assert!(s.contains("done"));
    }

    #[test]
    fn print_summary_dry_run() {
        let mut r = fake_result();
        r.wrote = false;
        let mut buf = Vec::new();
        print_summary(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("dry-run · no changes written"));
    }

    #[test]
    fn print_summary_no_claude_harness() {
        let mut r = fake_result();
        r.claude_harness = false;
        let mut buf = Vec::new();
        print_summary(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("no claude harness"));
    }

    #[test]
    fn print_summary_remove_mode() {
        let mut r = fake_result();
        r.remove_mode = true;
        r.stop = None;
        r.permissions_added.clear();
        r.permissions_removed = vec!["Read".into(); 14];
        let mut buf = Vec::new();
        print_summary(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("removed"));
        assert!(s.contains("permissions removed = 14"));
    }

    #[test]
    fn print_summary_with_qmd_status() {
        let mut r = fake_result();
        r.qmd = Some(HookStatus::Added);
        let mut buf = Vec::new();
        print_summary(&r, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("PostToolUse added"));
    }
}
