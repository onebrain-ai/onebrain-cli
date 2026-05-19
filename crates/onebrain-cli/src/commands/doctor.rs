//! `onebrain doctor` — run all vault health checks and emit a plain-text
//! report. Mirrors Bun's `printNonTtyOutput` byte-for-byte (icons, layout,
//! summary line) so cross-implementation parity tests stay green.
//!
//! `--fix` is parsed (wired through `Cmd::Doctor`) but auto-repair lands in
//! v3.0.1 — for now we emit a deferral stub on stderr.

use anyhow::{anyhow, Context, Result};
use onebrain_core::{find_vault_root, load_vault_config, DoctorResult, DoctorStatus};
use onebrain_fs::doctor::run_all_checks;
use std::env;
use std::io::Write;

/// Entry point — returns `Ok(0)` on no errors, `Ok(1)` when any check
/// produced `DoctorStatus::Error`. `--fix` is reserved for v3.0.1.
pub fn run(fix: bool) -> Result<i32> {
    let cwd = env::current_dir().context("read current directory")?;
    let vault_root =
        find_vault_root(&cwd).ok_or_else(|| anyhow!("not inside a vault (no vault.yml found)"))?;

    // Best-effort config load — on error, fall back to defaults so doctor can
    // still report what it sees (matches Bun behavior: stderr warning + defaults).
    let config = load_vault_config(&vault_root).unwrap_or_else(|err| {
        eprintln!("doctor: vault.yml load warning: {err}");
        onebrain_core::VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    });

    let results = run_all_checks(vault_root.as_path(), &config);

    print_report(&results, std::io::stdout())?;

    let error_count = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    let exit_code = if error_count == 0 { 0 } else { 1 };

    if fix {
        eprintln!("doctor: --fix not yet implemented in v3.0 · deferred to v3.0.1");
    }

    Ok(exit_code)
}

fn print_report<W: Write>(results: &[DoctorResult], mut w: W) -> Result<()> {
    writeln!(w, "OneBrain Doctor")?;
    writeln!(w)?;
    for r in results {
        let icon = match r.status {
            DoctorStatus::Ok => "[\u{2713}]",
            DoctorStatus::Warn => "[!]",
            DoctorStatus::Error => "[\u{2717}]",
        };
        writeln!(w, "  {} {:<20} {}", icon, r.check, r.message)?;
        if let Some(hint) = &r.hint {
            writeln!(w, "        \u{2192} {hint}")?;
        }
        for d in &r.details {
            writeln!(w, "        \u{00b7} {d}")?;
        }
    }
    writeln!(w)?;
    let total = results.len();
    let errors = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    let warnings = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Warn)
        .count();
    if errors > 0 {
        writeln!(
            w,
            "Summary: {total} checks · {errors} error(s) · {warnings} warning(s) — fix before using"
        )?;
    } else if warnings > 0 {
        writeln!(
            w,
            "Summary: {total} checks · {warnings} warning(s) — ok to run"
        )?;
    } else {
        writeln!(w, "Summary: {total} checks — all passed")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorResult;

    #[test]
    fn print_report_all_ok() {
        let results = vec![DoctorResult::ok("vault.yml", "valid")];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[\u{2713}]"));
        assert!(s.contains("all passed"));
    }

    #[test]
    fn print_report_summarizes_errors_and_warnings() {
        let results = vec![
            DoctorResult::error("a", "broken"),
            DoctorResult::warn("b", "iffy"),
            DoctorResult::ok("c", "good"),
        ];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("1 error(s)"));
        assert!(s.contains("1 warning(s)"));
        assert!(s.contains("fix before using"));
    }

    #[test]
    fn print_report_snapshot_mixed_statuses() {
        let results = vec![
            DoctorResult::ok("vault.yml", "valid").with_details(vec!["qmd: ob-1".into()]),
            DoctorResult::warn("settings-hooks", "2 issue(s)")
                .with_hint("Run onebrain doctor --fix to repair hooks")
                .with_details(vec!["Stop hook missing".into()]),
            DoctorResult::error("folders", "7/8 present")
                .with_hint("Missing: 01-projects")
                .with_details(vec!["missing: 01-projects".into()]),
        ];
        let mut buf = Vec::new();
        print_report(&results, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        insta::assert_snapshot!(output);
    }
}
