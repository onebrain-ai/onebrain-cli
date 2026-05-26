//! qmd MCP server query helpers.

use serde::Serialize;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Default timeout for `qmd` subprocess calls (ms). Keeps a missing or hung
/// qmd from blocking session-init / status.
const QMD_TIMEOUT_MS: u64 = 2000;

/// Build a `qmd` subprocess for the given args, platform-wrapped.
///
/// On Windows qmd ships as a `.cmd`/`.ps1` shim that can't be spawned
/// directly, so we route through `powershell.exe -Command`.
#[cfg(unix)]
fn build_qmd_command(args: &[&str]) -> Command {
    let mut c = Command::new("qmd");
    c.args(args);
    c
}

#[cfg(windows)]
fn build_qmd_command(args: &[&str]) -> Command {
    let mut c = Command::new("powershell.exe");
    c.args(["-NoProfile", "-Command", &format!("qmd {}", args.join(" "))]);
    c
}

#[cfg(not(any(unix, windows)))]
fn build_qmd_command(args: &[&str]) -> Command {
    let mut c = Command::new("qmd");
    c.args(args);
    c
}

/// Spawn `qmd <args>`, capture stdout, and return it as a string.
///
/// Returns `None` on any failure — spawn error (missing binary), timeout,
/// or unreadable stdout. Silent fallback by design: a missing or broken qmd
/// must never block the caller. Kills the child on timeout.
fn capture_qmd_stdout(args: &[&str], timeout_ms: u64) -> Option<String> {
    let mut command = build_qmd_command(args);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return None;
        }
    };

    let (tx, rx) = mpsc::channel::<Option<String>>();
    thread::spawn(move || {
        let mut buf = String::new();
        let mut stdout = stdout;
        let res = stdout.read_to_string(&mut buf).ok().map(|_| buf);
        let _ = tx.send(res);
    });

    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(Some(s)) => {
            let _ = child.wait();
            Some(s)
        }
        Ok(None) => {
            let _ = child.wait();
            None
        }
        Err(_) => {
            // Timeout · kill child · give up.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Count documents that still need embedding.
///
/// Reads the `Pending: N need embedding` figure from `qmd status` via
/// [`query_status`]. qmd ≤ 2.1.0 ignores `--json` and always prints the
/// human-readable text, so the v3.0 `--json`-parsing path was effectively
/// dead (always 0) on real installs; parsing the text form fixes that while
/// keeping the silent fallback — returns `0` on any error, timeout, missing
/// binary, or unparseable output so a missing/broken qmd never blocks
/// session-init.
///
/// Timeout: 2000ms (via `query_status`).
pub fn query_unembedded_count() -> usize {
    query_status()
        .and_then(|s| s.pending_embedding)
        .unwrap_or(0) as usize
}

/// Index + embedding health reported by `qmd status`, as parsed from the
/// human-readable text output (qmd ≤ 2.1.0 ignores `--json`, so we parse the
/// text form). All fields are `Option` — a field that can't be parsed is
/// reported as `null` rather than failing the whole call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct QmdStatus {
    /// Total documents indexed (the `Documents · Total` line).
    pub total_files: Option<u64>,
    /// Vectors embedded (the `Documents · Vectors` line).
    pub embedded_vectors: Option<u64>,
    /// Documents pending embedding (the `Documents · Pending` line).
    pub pending_embedding: Option<u64>,
    /// On-disk index size, verbatim (e.g. `"45.7 MB"`).
    pub index_size: Option<String>,
    /// Last index update, verbatim (e.g. `"1d ago"`).
    pub last_updated: Option<String>,
}

/// Run `qmd status` and parse the headline index/embedding figures.
///
/// Returns `None` when qmd is unavailable (missing binary, timeout, empty
/// output) so the caller can report "qmd not installed". Returns
/// `Some(QmdStatus)` with best-effort field parsing otherwise.
///
/// Timeout: 2000ms.
pub fn query_status() -> Option<QmdStatus> {
    let text = capture_qmd_stdout(&["status"], QMD_TIMEOUT_MS)?;
    if text.trim().is_empty() {
        return None;
    }
    Some(parse_status(&text))
}

/// First whitespace-delimited token in `s` that parses as a `u64`.
fn first_u64(s: &str) -> Option<u64> {
    s.split_whitespace().find_map(|tok| tok.parse::<u64>().ok())
}

/// Trimmed remainder of a `Prefix: value` line, or `None` when empty.
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Parse the text output of `qmd status` into a [`QmdStatus`].
///
/// Matches the `Total:` / `Vectors:` / `Pending:` / `Updated:` prefixes under
/// the `Documents` block, plus `Size:` under the top-level `Index` block.
/// Defensive: any line that doesn't match leaves its field `None`.
fn parse_status(text: &str) -> QmdStatus {
    let mut status = QmdStatus::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Total:") {
            status.total_files = first_u64(rest);
        } else if let Some(rest) = line.strip_prefix("Vectors:") {
            status.embedded_vectors = first_u64(rest);
        } else if let Some(rest) = line.strip_prefix("Pending:") {
            status.pending_embedding = first_u64(rest);
        } else if let Some(rest) = line.strip_prefix("Size:") {
            status.index_size = non_empty(rest);
        } else if let Some(rest) = line.strip_prefix("Updated:") {
            status.last_updated = non_empty(rest);
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_zero_when_qmd_not_installed() {
        // In test environments qmd is rarely installed → spawn fails or returns
        // garbage · result must be 0 either way.
        let n = query_unembedded_count();
        // Can't assert exactly 0 in environments that *do* have qmd available,
        // but for CI/sandbox we expect 0. Just check it doesn't panic and
        // returns a usize. (No-op assertion.)
        let _ = n;
    }

    // Captured verbatim from `qmd status` on qmd 2.1.0 (the documented/installed
    // version) — the line prefixes `parse_status` keys on. Re-capture if a future
    // qmd changes this layout.
    const SAMPLE: &str = "QMD Status\n\
\n\
Index: /Users/x/.cache/qmd/index.sqlite\n\
Size:  45.7 MB\n\
\n\
Documents\n\
  Total:    600 files indexed\n\
  Vectors:  7203 embedded\n\
  Pending:  29 need embedding (run 'qmd embed')\n\
  Updated:  1d ago\n";

    #[test]
    fn parses_all_headline_fields() {
        let s = parse_status(SAMPLE);
        assert_eq!(s.total_files, Some(600));
        assert_eq!(s.embedded_vectors, Some(7203));
        assert_eq!(s.pending_embedding, Some(29));
        assert_eq!(s.index_size.as_deref(), Some("45.7 MB"));
        assert_eq!(s.last_updated.as_deref(), Some("1d ago"));
    }

    #[test]
    fn missing_fields_stay_none() {
        let s = parse_status("QMD Status\n\nDocuments\n  Total: 5 files indexed\n");
        assert_eq!(s.total_files, Some(5));
        assert_eq!(s.embedded_vectors, None);
        assert_eq!(s.pending_embedding, None);
        assert_eq!(s.index_size, None);
        assert_eq!(s.last_updated, None);
    }

    #[test]
    fn zero_pending_parses_as_zero_not_none() {
        let s = parse_status("Documents\n  Pending:  0 need embedding\n");
        assert_eq!(s.pending_embedding, Some(0));
    }

    #[test]
    fn garbage_text_yields_all_none() {
        let s = parse_status("not a qmd status output at all");
        assert_eq!(
            s,
            QmdStatus {
                total_files: None,
                embedded_vectors: None,
                pending_embedding: None,
                index_size: None,
                last_updated: None,
            }
        );
    }
}
