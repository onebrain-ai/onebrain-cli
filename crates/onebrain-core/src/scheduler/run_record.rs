//! One scheduled run, rendered as a vault-readable markdown record.
//!
//! PURE — no clock, no filesystem, no environment. Everything is injected, so
//! the rendering is testable on every platform without env mutation.

use chrono::{DateTime, Local};

pub const TAIL_MAX_BYTES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSource {
    Scheduled,
    Manual,
}

#[derive(Debug, Clone)]
pub struct RunRecord {
    pub started: DateTime<Local>,
    /// Skill name without its leading slash.
    pub entry_name: String,
    pub harness: Option<String>,
    /// Always a real code — `translate_exit` yields `128 + signal` for signals
    /// and `1` otherwise, so there is no "no exit code" case to model.
    pub exit_code: i32,
    pub duration_secs: u64,
    pub machine: String,
    pub source: RunSource,
    pub output_tail: String,
}

impl RunRecord {
    pub fn render(&self) -> String {
        let status = if self.exit_code == 0 {
            "✅ ok".to_string()
        } else {
            format!("❌ exit {}", self.exit_code)
        };
        let source = match self.source {
            RunSource::Scheduled => "scheduled",
            RunSource::Manual => "manual",
        };
        let harness = self
            .harness
            .as_deref()
            .map(|h| format!(" · {h}"))
            .unwrap_or_default();
        format!(
            "\n## {} · {}\n\n\
             - **status:** {status}\n\
             - **entry:** `{}`{harness}\n\
             - **source:** {source}\n\
             - **duration:** {}s\n\
             - **machine:** {}\n\n\
             ```text\n{}\n```\n",
            self.started.format("%H:%M"),
            self.entry_name,
            self.entry_name,
            self.duration_secs,
            self.machine,
            self.output_tail.trim_end(),
        )
    }
}

/// Last `max_bytes` of `output`, made safe to embed in a vault markdown note:
/// ANSI escapes stripped, fence-closing runs neutralised, never split mid-char.
pub fn safe_tail(output: &str, max_bytes: usize) -> String {
    let stripped = strip_ansi(output);
    let sliced = if stripped.len() <= max_bytes {
        stripped
    } else {
        let mut start = stripped.len() - max_bytes;
        while start < stripped.len() && !stripped.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &stripped[start..])
    };
    // U+200B between the backticks: renders invisibly, cannot close the fence.
    sliced.replace("```", "``\u{200b}`")
}

/// Remove ANSI CSI sequences (`ESC [ … final-byte`) and a bare trailing ESC.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Consume until the CSI final byte (0x40–0x7E).
            for f in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&f) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn rec() -> RunRecord {
        RunRecord {
            started: Local.with_ymd_and_hms(2026, 8, 3, 9, 0, 0).unwrap(),
            entry_name: "daily".into(),
            harness: Some("claude".into()),
            exit_code: 0,
            duration_secs: 42,
            machine: "keng-mbp".into(),
            source: RunSource::Scheduled,
            output_tail: "done".into(),
        }
    }

    #[test]
    fn renders_every_field_a_human_needs() {
        let out = rec().render();
        assert!(out.contains("09:00"), "start time: {out}");
        assert!(out.contains("claude"), "harness: {out}");
        assert!(out.contains("42s"), "duration: {out}");
        assert!(out.contains("keng-mbp"), "machine: {out}");
        assert!(out.contains("done"), "output tail: {out}");
    }

    #[test]
    fn a_failure_is_visually_distinct_from_a_success() {
        let ok = rec().render();
        let mut bad = rec();
        bad.exit_code = 78;
        let failed = bad.render();
        assert_ne!(ok, failed, "a failure must not render like a success");
        assert!(failed.contains("78"), "the exit code is named: {failed}");
    }

    #[test]
    fn a_manual_run_is_distinguishable_from_a_scheduled_one() {
        // doctor's staleness check counts ONLY scheduled runs. Without this,
        // one `onebrain skill run` by hand makes a week-dead cron job look alive.
        let mut manual = rec();
        manual.source = RunSource::Manual;
        assert_ne!(rec().render(), manual.render(), "must be distinguishable");
        assert!(manual.render().contains("manual"), "{}", manual.render());
    }

    #[test]
    fn safe_tail_keeps_the_end_not_the_beginning() {
        // A failure's cause is at the END of the output. Truncating the wrong
        // end keeps the banner and discards the error.
        let long = "x".repeat(50) + "THE_INTERESTING_PART";
        let t = safe_tail(&long, 25);
        assert!(t.contains("INTERESTING"), "kept the end: {t}");
    }

    #[test]
    fn safe_tail_does_not_split_a_multibyte_character() {
        // Thai: 3 bytes per char. A naive byte slice panics here.
        let thai = "ทดสอบภาษาไทยยาวมาก".repeat(20);
        let t = safe_tail(&thai, 40);
        assert!(t.len() <= 64, "bounded: {} bytes", t.len());
        // Reaching this line at all proves no panic on a char boundary.
    }

    #[test]
    fn safe_tail_strips_ansi_escapes() {
        let coloured = "\x1b[31mred error\x1b[0m";
        let t = safe_tail(coloured, TAIL_MAX_BYTES);
        assert!(
            !t.contains('\x1b'),
            "no escape bytes reach the vault: {t:?}"
        );
        assert!(t.contains("red error"), "the text survives: {t:?}");
    }

    #[test]
    fn safe_tail_neutralises_a_fence_close() {
        // The record wraps the tail in ```text. A ``` in the output would close
        // it early and turn the rest of the vault note into raw markdown.
        let evil = "before\n```\n# not a heading";
        let t = safe_tail(evil, TAIL_MAX_BYTES);
        assert!(!t.contains("```"), "fence neutralised: {t:?}");
        let rendered = RunRecord {
            output_tail: t,
            ..rec()
        }
        .render();
        assert_eq!(
            rendered.matches("```").count(),
            2,
            "exactly one fence pair: {rendered}"
        );
    }
}
