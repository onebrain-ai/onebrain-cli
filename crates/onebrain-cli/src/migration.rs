//! Migration notice — print v3.0→v3.1 rename guidance once per command.
//!
//! State file: `~/.cache/onebrain/migration-shown.txt` (one alias name per
//! line). On first invocation of each v3.0 alias we append the name and print
//! a stderr notice; subsequent invocations within or across processes find
//! the name in the file and stay silent.
//!
//! Power users can suppress entirely via `ONEBRAIN_QUIET_MIGRATION=1`.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

const STATE_FILENAME: &str = "migration-shown.txt";
const ENV_SUPPRESS: &str = "ONEBRAIN_QUIET_MIGRATION";

/// Side-effect-free check: should we emit a migration notice for `old`?
///
/// Pure function over the state-file directory so unit tests don't touch
/// `~/.cache`. Returns `false` when the env-var suppression is active OR the
/// state file already records `old`.
pub fn should_show(state_dir: &std::path::Path, old: &str, env_suppress: bool) -> bool {
    if env_suppress {
        return false;
    }
    let state_file = state_dir.join(STATE_FILENAME);
    match fs::read_to_string(&state_file) {
        Ok(s) => !s.lines().any(|line| line.trim() == old),
        Err(_) => true, // missing state file → first run
    }
}

/// Record `old` in the state file so future invocations stay silent. Best
/// effort — write failures are ignored (the notice just shows again, which
/// is harmless).
pub fn record(state_dir: &std::path::Path, old: &str) {
    let _ = fs::create_dir_all(state_dir);
    let state_file = state_dir.join(STATE_FILENAME);
    // Read-modify-write to dedupe even if a parallel process raced us.
    let mut seen: HashSet<String> = match fs::read_to_string(&state_file) {
        Ok(s) => s.lines().map(|l| l.trim().to_string()).collect(),
        Err(_) => HashSet::new(),
    };
    if seen.insert(old.to_string()) {
        // New entry — rewrite the file with all known aliases on one line each.
        let mut sorted: Vec<&str> = seen.iter().map(|s| s.as_str()).collect();
        sorted.sort();
        let body = sorted.join("\n");
        let _ = fs::write(&state_file, body);
    }
}

/// Default state directory: `~/.cache/onebrain/`.
pub fn default_state_dir() -> PathBuf {
    dirs::cache_dir()
        .map(|d| d.join("onebrain"))
        .unwrap_or_else(|| PathBuf::from("/tmp/onebrain"))
}

/// Print the user-facing notice. Stderr only — stdout stays clean for JSON
/// consumers. Pure formatter so callers can swap the writer in tests.
pub fn write_notice<W: Write>(mut w: W, old: &str, new_path: &str) -> std::io::Result<()> {
    writeln!(
        w,
        "⚠️  v3.1: `onebrain {}` has been renamed to `onebrain {}`.",
        old, new_path
    )?;
    writeln!(
        w,
        "   The old command still works, but please update hooks/scripts when convenient."
    )?;
    writeln!(
        w,
        "   (This notice shows once per command. Suppress with {}=1.)",
        ENV_SUPPRESS
    )?;
    Ok(())
}

/// One-shot helper: check state, print notice if needed, record. Wires the
/// three primitives together for the alias dispatcher in `main.rs`.
pub fn print_once(old: &str, new_path: &str) {
    let env_suppress = matches!(
        std::env::var(ENV_SUPPRESS).ok().as_deref(),
        Some("1") | Some("true")
    );
    let dir = default_state_dir();
    if should_show(&dir, old, env_suppress) {
        let _ = write_notice(std::io::stderr(), old, new_path);
        record(&dir, old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_run_shows_when_state_dir_missing() {
        let dir = tempdir().unwrap();
        // No state file exists yet.
        assert!(should_show(dir.path(), "session-init", false));
    }

    #[test]
    fn env_suppress_blocks_show() {
        let dir = tempdir().unwrap();
        assert!(!should_show(dir.path(), "session-init", true));
    }

    #[test]
    fn record_then_check_is_silent() {
        let dir = tempdir().unwrap();
        assert!(should_show(dir.path(), "orphan-scan", false));
        record(dir.path(), "orphan-scan");
        assert!(!should_show(dir.path(), "orphan-scan", false));
    }

    #[test]
    fn record_dedupes_within_file() {
        let dir = tempdir().unwrap();
        record(dir.path(), "x");
        record(dir.path(), "x");
        record(dir.path(), "y");
        let body = fs::read_to_string(dir.path().join(STATE_FILENAME)).unwrap();
        // Sorted, deduped.
        assert_eq!(body, "x\ny");
    }

    #[test]
    fn other_command_still_shows() {
        let dir = tempdir().unwrap();
        record(dir.path(), "session-init");
        // A different alias has not been recorded yet.
        assert!(should_show(dir.path(), "orphan-scan", false));
    }

    #[test]
    fn write_notice_includes_old_and_new() {
        let mut buf = Vec::new();
        write_notice(&mut buf, "session-init", "session init").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("session-init"));
        assert!(s.contains("session init"));
        assert!(s.contains("ONEBRAIN_QUIET_MIGRATION"));
    }
}
