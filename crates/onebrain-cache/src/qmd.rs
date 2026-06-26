//! qmd MCP server query helpers.
//!
//! This module is the single source of truth for probing `qmd status` — the
//! spawn, the PATH resolution, the timeout, and the parse all live here so
//! every consumer (session-init's unembedded count, `onebrain qmd status`,
//! and `onebrain doctor`'s qmd-embeddings check in `onebrain-fs`) reports the
//! same thing. Before v3.4 these had drifted: this probe used a 2 s timeout +
//! a bare `qmd` spawn while doctor used 15 s + a PATH-robust lookup, so a slow
//! or PATH-hidden qmd produced a false "unavailable" / `0` here but not there.

use serde::Serialize;
use std::process::Command;
use std::time::Duration;

/// Hard deadline for the `qmd status` probe. Generous because a real index
/// (tens of MB) can take ~10 s to report; a tighter cap produced spurious
/// "qmd unavailable" / false-zero unembedded counts on healthy, well-populated
/// vaults. Reused by `onebrain doctor` so the two probes can't drift again
/// (that drift was the v3.4 bug — doctor had been bumped 3 s → 15 s in v3.2.4
/// but this probe was left at 2 s).
pub const QMD_STATUS_TIMEOUT_SECS: u64 = 15;

// Compile-time regression guard: a real index can take ~10 s for `qmd status`,
// so the cap must stay generous. The v3.2.4 doctor lesson (3 s → 15 s) was never
// carried to this shared probe — the old 2 s cap caused intermittent false
// zeros at session startup. Fails the build if anyone tightens it.
const _: () = assert!(
    QMD_STATUS_TIMEOUT_SECS >= 15,
    "qmd status timeout must stay >= 15s; a real index can take ~10s to report"
);

/// Tighter cap for the interactive session-startup probe ([`query_unembedded_count`]).
/// Session init blocks the greeting on this, so we'd rather report "unknown"
/// (`null`) after a few seconds than freeze startup for the full
/// [`QMD_STATUS_TIMEOUT_SECS`] on a slow/hung qmd. Safe because a probe timeout
/// now maps to `None`/`null` (not a false `0`): a shorter cap trades "exact
/// count on a cold index" for "snappy startup", never correctness. Explicit
/// status queries (`onebrain qmd status`) and `onebrain doctor` keep the
/// generous cap, since there the user is waiting *for* the figure.
pub const QMD_STARTUP_TIMEOUT_SECS: u64 = 5;

// The startup cap is the tighter of the two — it must never exceed the generous
// cap (else the "snappy startup" intent is lost).
const _: () = assert!(QMD_STARTUP_TIMEOUT_SECS <= QMD_STATUS_TIMEOUT_SECS);

/// Outcome of spawning `qmd status`. Distinguishes failure modes so consumers
/// can render them differently (doctor reports "timeout" vs "not found");
/// session-init / `qmd status` collapse every non-`Stdout` variant to
/// "unavailable". Lets callers unit-test all branches without spawning a real
/// `qmd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QmdProbe {
    /// `qmd` not found on PATH (or the common install dirs).
    NotFound,
    /// Spawned but exceeded [`QMD_STATUS_TIMEOUT_SECS`] (child was killed).
    Timeout,
    /// Spawned, exited, returned this stdout (may or may not be parseable).
    Stdout(String),
    /// Spawn or I/O error.
    Error,
}

/// Spawn `qmd status` with the generous [`QMD_STATUS_TIMEOUT_SECS`] deadline and
/// classify the result. Used by explicit status queries (`onebrain qmd status`)
/// and `onebrain doctor`, which want the real figure even on a cold index.
/// NEVER panics — a missing or hung qmd must never block the caller.
pub fn probe_qmd_status() -> QmdProbe {
    probe_qmd_status_with(Duration::from_secs(QMD_STATUS_TIMEOUT_SECS))
}

/// Core probe with an explicit deadline, so the interactive startup path can use
/// the tighter [`QMD_STARTUP_TIMEOUT_SECS`]. ALL probe logic — PATH resolution,
/// spawn, stdout drain, kill-on-timeout — lives here; the only thing that varies
/// by caller is the deadline (so the two consumers can't drift in behavior, only
/// in how long they're willing to wait).
fn probe_qmd_status_with(timeout: Duration) -> QmdProbe {
    use std::io::Read;
    use std::process::Stdio;
    use wait_timeout::ChildExt;

    let Some(mut command) = build_qmd_command() else {
        return QmdProbe::NotFound;
    };
    let mut child = match command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(_) => return QmdProbe::Error,
    };
    // Block on the child with a hard deadline. `qmd status` output is small
    // (well under the pipe buffer) so draining stdout after exit cannot
    // deadlock. On timeout, kill + reap.
    match child.wait_timeout(timeout) {
        Ok(Some(_status)) => {
            let mut out = String::new();
            if let Some(mut s) = child.stdout.take() {
                if s.read_to_string(&mut out).is_err() {
                    return QmdProbe::Error;
                }
            }
            QmdProbe::Stdout(out)
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            QmdProbe::Timeout
        }
        Err(_) => QmdProbe::Error,
    }
}

/// Build the platform-appropriate `qmd status` command, resolving the binary
/// robustly and running it with a PATH that can also find its interpreter.
/// Returns `None` (⟹ [`QmdProbe::NotFound`]) when `qmd` can't be located on
/// Unix; Windows defers to `powershell.exe` so the user's profile PATH is
/// consulted.
#[cfg(unix)]
fn build_qmd_command() -> Option<Command> {
    let search = qmd_search_path();
    // Resolve on the normal PATH first, then fall back to the augmented one.
    let qmd = which::which("qmd")
        .ok()
        .or_else(|| which::which_in("qmd", Some(&search), std::env::current_dir().ok()?).ok())?;
    let mut c = Command::new(qmd);
    c.arg("status");
    // Run qmd with the augmented PATH so its own interpreter (node/bun) resolves
    // even under a restricted launcher PATH — otherwise a located-but-interpreted
    // qmd (e.g. the node shim in `/opt/homebrew/bin`) fails its `#!/usr/bin/env
    // node` shebang when node isn't on PATH, which looks like an unavailable qmd.
    c.env("PATH", &search);
    Some(c)
}

/// PATH for locating and running `qmd`: the existing PATH (kept at priority)
/// plus the bun-global dir (`bun install -g qmd`) appended as a fallback,
/// mirroring the resolution `onebrain doctor` has used since v3.1. This covers
/// a restricted launcher PATH (Claude Code's SessionStart hook, launchd, the
/// Obsidian terminal) that omits the bun dir, and — crucially — also lets a
/// located-but-interpreted qmd find its own interpreter (node/bun) when it
/// runs. HOME-relative on purpose: it can't pick up a system-wide qmd, so it
/// doesn't defeat hermetic tests that scrub PATH to simulate an absent qmd.
#[cfg(unix)]
fn qmd_search_path() -> std::ffi::OsString {
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        if !path.is_empty() {
            path.push(":");
        }
        path.push(&home);
        path.push("/.bun/bin");
    }
    path
}

#[cfg(windows)]
fn build_qmd_command() -> Option<Command> {
    // Delegate to powershell so the user's profile PATH is consulted.
    let mut c = Command::new("powershell.exe");
    c.args(["-NoProfile", "-Command", "qmd status"]);
    Some(c)
}

#[cfg(not(any(unix, windows)))]
fn build_qmd_command() -> Option<Command> {
    let mut c = Command::new("qmd");
    c.arg("status");
    Some(c)
}

/// Count documents that still need embedding, or `None` when that can't be
/// determined (qmd missing, timed out, errored, or output unparseable).
///
/// Returning `Option` — not `0` on failure — is deliberate: a false `0` is
/// indistinguishable from "all embedded" and silently hides pending work at
/// session startup. `None` lets the caller surface "unknown" instead;
/// `Some(0)` is a genuine zero. qmd ≤ 2.1.0 ignores `--json` and prints the
/// human-readable text, so this parses the text form (see [`parse_status`]).
///
/// Timeout: [`QMD_STARTUP_TIMEOUT_SECS`] — tighter than the status/doctor cap
/// because interactive session startup blocks on this; a timeout degrades to
/// `None` (unknown), never a false `0`, so a slow qmd can't freeze the greeting.
pub fn query_unembedded_count() -> Option<usize> {
    unembedded_from_probe(probe_qmd_status_with(Duration::from_secs(
        QMD_STARTUP_TIMEOUT_SECS,
    )))
}

/// Pure mapping from a probe outcome to an unembedded count. Split out so the
/// false-zero-prevention logic is unit-testable without spawning a real qmd.
fn unembedded_from_probe(probe: QmdProbe) -> Option<usize> {
    status_from_probe(probe)
        .and_then(|s| s.pending_embedding)
        .map(|n| n as usize)
}

/// Pure mapping from a probe outcome to a parsed [`QmdStatus`]. Any non-stdout
/// outcome, or empty stdout, is "unavailable" (`None`). Unit-testable.
fn status_from_probe(probe: QmdProbe) -> Option<QmdStatus> {
    match probe {
        QmdProbe::Stdout(text) if !text.trim().is_empty() => Some(parse_status(&text)),
        _ => None,
    }
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

impl QmdStatus {
    /// Parse the text output of `qmd status` into a [`QmdStatus`]. Public so
    /// other crates (e.g. `onebrain doctor`) parse identically rather than
    /// re-implementing prefix matching.
    pub fn parse(text: &str) -> Self {
        parse_status(text)
    }
}

/// Run `qmd status` and parse the headline index/embedding figures.
///
/// Returns `None` when qmd is unavailable (missing binary, timeout, error,
/// empty output) so the caller can report "qmd not installed"; `Some` with
/// best-effort field parsing otherwise.
///
/// Timeout: [`QMD_STATUS_TIMEOUT_SECS`].
pub fn query_status() -> Option<QmdStatus> {
    status_from_probe(probe_qmd_status())
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
    fn query_unembedded_count_does_not_panic() {
        // Smoke test: whether or not a real qmd is installed, the public probe
        // must return without panicking. The value is environment-dependent
        // (`None` when qmd is absent, `Some(n)` when present), so we don't pin
        // it here — the probe→count mapping is pinned by the tests above.
        let _ = query_unembedded_count();
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

    // ── Probe → status mapping ────────────────────────────────────────────
    // A failed probe must NEVER degrade to a false zero: it maps to `None`
    // ("unknown") so callers can surface "qmd unavailable" instead of
    // "0 pending". This is the core regression these changes guard against.

    #[test]
    fn unavailable_probe_maps_to_none_not_zero() {
        assert_eq!(status_from_probe(QmdProbe::NotFound), None);
        assert_eq!(status_from_probe(QmdProbe::Timeout), None);
        assert_eq!(status_from_probe(QmdProbe::Error), None);
        assert_eq!(status_from_probe(QmdProbe::Stdout(String::new())), None);
        assert_eq!(status_from_probe(QmdProbe::Stdout("   \n".into())), None);
    }

    #[test]
    fn stdout_probe_parses_into_status() {
        let s = status_from_probe(QmdProbe::Stdout(SAMPLE.into())).expect("some status");
        assert_eq!(s.pending_embedding, Some(29));
        assert_eq!(s.total_files, Some(600));
    }

    #[test]
    fn unembedded_count_is_none_when_probe_unavailable() {
        // The false-zero regression guard: a missing/timed-out/erroring qmd
        // must report "unknown" (None), not a misleading 0.
        assert_eq!(unembedded_from_probe(QmdProbe::NotFound), None);
        assert_eq!(unembedded_from_probe(QmdProbe::Timeout), None);
        assert_eq!(unembedded_from_probe(QmdProbe::Error), None);
    }

    #[test]
    fn unembedded_count_reads_real_pending() {
        assert_eq!(
            unembedded_from_probe(QmdProbe::Stdout(SAMPLE.into())),
            Some(29)
        );
    }

    #[test]
    fn unembedded_count_zero_pending_is_some_zero_not_none() {
        // A genuine "0 pending" stays Some(0) — distinct from None (unknown).
        let out = "Documents\n  Total: 5 files indexed\n  Pending:  0 need embedding\n";
        assert_eq!(unembedded_from_probe(QmdProbe::Stdout(out.into())), Some(0));
    }

    #[test]
    fn unembedded_count_is_none_when_stdout_has_no_pending_line() {
        // qmd responded, but the `Pending:` line is absent/unparseable → the
        // count is unknown (None), never a false 0. Session-init surfaces this
        // as `null`, not "0 unembedded".
        let out = "Documents\n  Total: 5 files indexed\n";
        assert_eq!(unembedded_from_probe(QmdProbe::Stdout(out.into())), None);
    }

    // The generous-timeout invariant is enforced at compile time by the
    // `const _: () = assert!(...)` guard next to the constant definition above.

    #[cfg(unix)]
    #[test]
    fn qmd_search_path_appends_bun_global_dir() {
        // The augmented PATH must include the bun-global install dir so a
        // bun-installed qmd (and its interpreter) resolves under a restricted
        // launcher PATH. HOME-relative so hermetic tests that scrub PATH don't
        // accidentally pick up a system qmd.
        let p = qmd_search_path();
        let s = p.to_string_lossy();
        assert!(s.contains(".bun/bin"), "missing bun-global dir: {s}");
    }

    // Captured verbatim from `qmd status` on qmd 2.1.0 with a real, populated
    // index — includes the AST Chunking / Collections / Examples blocks that
    // follow Documents. Guards against cross-block prefix collisions skewing
    // the headline figures (and thus text == --json equivalence, since both
    // render from this single parse).
    const REAL_FULL_SAMPLE: &str = "QMD Status\n\
\n\
Index: /Users/keng/.cache/qmd/index.sqlite\n\
Size:  51.1 MB\n\
\n\
Documents\n\
  Total:    726 files indexed\n\
  Vectors:  9062 embedded\n\
  Pending:  85 need embedding (run 'qmd embed')\n\
  Updated:  4m ago\n\
\n\
AST Chunking\n\
  Status:   active\n\
  Languages: typescript, tsx, javascript, python, go, rust\n\
\n\
Collections\n\
  ob-1-441565 (qmd://ob-1-441565/)\n\
    Pattern:  **/*.md\n\
    Files:    726 (updated 4m ago)\n\
    Contexts: 1\n\
\n\
Examples\n\
  # List files in a collection\n\
  qmd ls ob-1-441565\n";

    #[test]
    fn parse_status_ignores_unrelated_blocks_in_real_output() {
        let s = parse_status(REAL_FULL_SAMPLE);
        assert_eq!(s.total_files, Some(726));
        assert_eq!(s.embedded_vectors, Some(9062));
        assert_eq!(s.pending_embedding, Some(85));
        assert_eq!(s.index_size.as_deref(), Some("51.1 MB"));
        assert_eq!(s.last_updated.as_deref(), Some("4m ago"));
    }
}
