//! `onebrain doctor` — run all vault health checks and emit a plain-text
//! report. Mirrors Bun's `printNonTtyOutput` byte-for-byte (icons, layout,
//! summary line) so cross-implementation parity tests stay green.
//!
//! `--fix` attempts targeted auto-repair recipes for each fixable warning,
//! then re-runs the checks so the user sees the result in a single
//! invocation. The recipes are deliberately narrow (one well-understood
//! action per check); anything ambiguous is reported as "untouched · run
//! the listed command yourself".

use anyhow::{anyhow, Context, Result};
use onebrain_core::{find_vault_root, load_vault_config, DoctorResult, DoctorStatus};
use onebrain_fs::doctor::run_all_checks;
use std::env;
use std::io::Write;
use std::path::Path;

/// Outcome of a single fix attempt — printed as part of the `--fix`
/// summary so the user can see what changed (or didn't).
enum FixOutcome {
    /// Fix ran cleanly and the underlying issue should be resolved.
    Fixed(String),
    /// Fix was attempted but did not finish (subprocess failed, timed out,
    /// etc.). The message explains why.
    Failed(String),
    /// No automated recipe exists for this warning — the user must take
    /// the action manually. Message is the suggested command.
    Manual(String),
}

/// Entry point — returns `Ok(0)` on no errors, `Ok(1)` when any check
/// produced `DoctorStatus::Error`. With `--fix`, the run is two-pass:
/// initial check, fix attempts on each warning, then a final re-check
/// whose result drives the exit code.
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

    let mut results = run_all_checks(vault_root.as_path(), &config);
    print_report(&results, std::io::stdout())?;

    if fix {
        let warnings: Vec<&DoctorResult> = results
            .iter()
            .filter(|r| r.status == DoctorStatus::Warn)
            .collect();
        if warnings.is_empty() {
            println!("\nFix: nothing to do — no warnings.");
        } else {
            println!(
                "\nFix: attempting recipes for {} warning(s)...",
                warnings.len()
            );
            let outcomes: Vec<(String, FixOutcome)> = warnings
                .iter()
                .map(|r| (r.check.clone(), attempt_fix(r, vault_root.as_path())))
                .collect();
            print_fix_summary(&outcomes);
            // Re-run checks so the final exit code + report reflects the
            // post-fix state. Users see two reports back-to-back, but the
            // second one is the source of truth.
            println!("\nDoctor (post-fix):");
            results = run_all_checks(vault_root.as_path(), &config);
            print_report(&results, std::io::stdout())?;
        }
    }

    let error_count = results
        .iter()
        .filter(|r| r.status == DoctorStatus::Error)
        .count();
    Ok(if error_count == 0 { 0 } else { 1 })
}

/// Dispatch the warning to the matching fix recipe. Unmapped checks fall
/// through to `FixOutcome::Manual` so the user knows what to do.
fn attempt_fix(result: &DoctorResult, _vault_root: &Path) -> FixOutcome {
    match result.check.as_str() {
        "qmd-embeddings" => fix_qmd_embeddings(),
        other => FixOutcome::Manual(format!(
            "no automated recipe for `{other}` · {}",
            result
                .hint
                .as_deref()
                .unwrap_or("see check details for next step")
        )),
    }
}

/// Recipe — `qmd-embeddings` warning means N files need embedding. Spawn
/// `qmd embed` and wait for it to finish; report success/failure based on
/// the exit code. The user sees full `qmd embed` output streamed to their
/// terminal in real time (we inherit stdio rather than capturing it).
fn fix_qmd_embeddings() -> FixOutcome {
    use std::process::Command;
    let qmd = match which::which("qmd") {
        Ok(p) => p,
        Err(_) => {
            return FixOutcome::Failed(
                "qmd binary not on PATH · install qmd then re-run".to_string(),
            )
        }
    };
    println!("  ▸ running: qmd embed");
    let status = Command::new(qmd).arg("embed").status();
    match status {
        Ok(s) if s.success() => FixOutcome::Fixed("qmd embed completed".to_string()),
        Ok(s) => FixOutcome::Failed(format!(
            "qmd embed exited with code {}",
            s.code().unwrap_or(-1)
        )),
        Err(e) => FixOutcome::Failed(format!("spawn qmd embed: {e}")),
    }
}

fn print_fix_summary(outcomes: &[(String, FixOutcome)]) {
    let mut fixed = 0;
    let mut failed = 0;
    let mut manual = 0;
    for (check, outcome) in outcomes {
        match outcome {
            FixOutcome::Fixed(msg) => {
                fixed += 1;
                println!("  ✓ {check}: {msg}");
            }
            FixOutcome::Failed(msg) => {
                failed += 1;
                println!("  ✗ {check}: {msg}");
            }
            FixOutcome::Manual(msg) => {
                manual += 1;
                println!("  · {check}: {msg}");
            }
        }
    }
    println!("\nFix summary: {fixed} fixed · {failed} failed · {manual} manual",);
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
