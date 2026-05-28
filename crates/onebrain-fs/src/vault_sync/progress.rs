//! Progress UI for vault-sync.
//!
//! Two implementations behind a `Progress` trait:
//!   - `TtyProgress`   — `indicatif` spinner (cargo feature off the architecture list)
//!   - `PlainProgress` — `vault-sync: <label>\n` lines (Bun's non-TTY format)
//!
//! `embedded` mode is a thin variant of `TtyProgress` that prefixes each step
//! result line with `▸ ` to mirror Bun's `cli-ui` bar formatter without pulling
//! `cli-ui.ts` into this slice.

use indicatif::{ProgressBar, ProgressStyle};
use std::io::Write;
use std::time::Duration;

/// Step-level UI used by the orchestrator. Two methods per step:
///   - `start(emoji, label)` — show "working on it"
///   - `stop(result, details)` — replace with the outcome line
///
/// The `intro` / `outro` methods bracket the whole run; on non-TTY they are
/// no-ops because Bun's non-TTY output prefixes every line with `vault-sync:`
/// without intro/outro.
pub trait Progress: Send {
    fn intro(&mut self, title: &str);
    fn start(&mut self, emoji: &str, label: &str);
    fn stop(&mut self, result: &str);
    fn outro(&mut self, msg: &str);
}

pub struct PlainProgress<W: Write + Send> {
    out: W,
    current_label: Option<String>,
    /// When true the start-line and outro-line `vault-sync: …` writes are
    /// suppressed — parity with [`TtyProgress::embedded`]. Set by
    /// `build_progress` when the orchestrator runs as a sub-step of a wrapper
    /// command (e.g. `onebrain plugin update`) that renders its own report
    /// and would otherwise see this progress leak through.
    embedded: bool,
}

impl<W: Write + Send> PlainProgress<W> {
    pub fn new(out: W) -> Self {
        Self::with_embedded(out, false)
    }

    /// Construct a `PlainProgress` with the embedded flag set explicitly.
    /// `embedded = true` mirrors `TtyProgress::embedded` and silences the
    /// per-step `vault-sync: <label>` lines and the `vault-sync: done` outro
    /// so the parent command can frame the run with its own report. v3.2.13:
    /// `onebrain plugin update` is the first caller — non-TTY runs (CI,
    /// scheduler, piped stdout) would otherwise still see the bare
    /// `vault-sync: …` chatter through plugin update's framed UI.
    pub fn with_embedded(out: W, embedded: bool) -> Self {
        Self {
            out,
            current_label: None,
            embedded,
        }
    }
}

impl<W: Write + Send> Progress for PlainProgress<W> {
    fn intro(&mut self, _title: &str) {
        // Non-TTY: no intro frame (Bun parity).
    }

    fn start(&mut self, _emoji: &str, label: &str) {
        // Bun emits one line per step on non-TTY: `vault-sync: <label>\n`.
        // v3.2.13: embedded callers suppress to keep parent reports clean.
        if !self.embedded {
            let _ = writeln!(self.out, "vault-sync: {label}");
        }
        self.current_label = Some(label.to_string());
    }

    fn stop(&mut self, _result: &str) {
        // Bun's non-TTY path doesn't print a separate stop line — the start
        // line is the only output. Keep that behavior so stdout stays byte-
        // identical with Bun for parity diffs.
        self.current_label = None;
    }

    fn outro(&mut self, _msg: &str) {
        if !self.embedded {
            let _ = writeln!(self.out, "vault-sync: done");
        }
    }
}

pub struct TtyProgress {
    pb: Option<ProgressBar>,
    embedded: bool,
}

impl TtyProgress {
    pub fn new(embedded: bool) -> Self {
        Self { pb: None, embedded }
    }
}

impl Progress for TtyProgress {
    fn intro(&mut self, title: &str) {
        if !self.embedded {
            // Plain styled banner instead of @clack/prompts.
            println!("\n  {title}\n");
        }
    }

    fn start(&mut self, emoji: &str, label: &str) {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        let prefix = if self.embedded { "▸ " } else { "" };
        pb.set_message(format!("{prefix}{emoji} {label}"));
        pb.enable_steady_tick(Duration::from_millis(80));
        self.pb = Some(pb);
    }

    fn stop(&mut self, result: &str) {
        if let Some(pb) = self.pb.take() {
            let prefix = if self.embedded { "▸ " } else { "✓ " };
            pb.finish_with_message(format!("{prefix}{result}"));
        }
    }

    fn outro(&mut self, msg: &str) {
        if !self.embedded {
            println!("\n  {msg}\n");
        }
    }
}

/// Pick the right progress implementation based on TTY + embedded flags.
/// `stdout` writer is only used by `PlainProgress`. The `embedded` flag flows
/// into both backends (since v3.2.13: previously the `PlainProgress` path
/// ignored it and leaked `vault-sync: …` lines through embedded callers like
/// `onebrain plugin update`).
pub fn build_progress(is_tty: bool, embedded: bool) -> Box<dyn Progress> {
    if is_tty {
        Box::new(TtyProgress::new(embedded))
    } else {
        Box::new(PlainProgress::with_embedded(std::io::stdout(), embedded))
    }
}

/// Build a `PlainProgress` writing to an arbitrary sink — for snapshot tests
/// that need to capture the output without touching stdout.
pub fn plain_progress_to<W: Write + Send + 'static>(out: W) -> Box<dyn Progress> {
    Box::new(PlainProgress::new(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_emits_one_line_per_start() {
        let mut buf = Vec::new();
        {
            let mut p = PlainProgress::new(&mut buf);
            p.intro("OneBrain Vault Sync");
            p.start("📥", "Downloading");
            p.stop("done");
            p.start("📂", "Syncing files");
            p.stop("done");
            p.outro("Done — v1.11.0 synced");
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("vault-sync: Downloading"));
        assert!(s.contains("vault-sync: Syncing files"));
        assert!(s.contains("vault-sync: done"));
        // No intro frame in non-TTY mode.
        assert!(!s.contains("OneBrain Vault Sync"));
    }

    #[test]
    fn plain_progress_to_returns_boxed_impl() {
        let buf: Vec<u8> = Vec::new();
        let p = plain_progress_to(buf);
        drop(p); // Smoke test — exists and can be boxed.
    }
}
