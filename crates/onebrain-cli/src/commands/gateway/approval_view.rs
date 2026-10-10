//! Plain-words rendering of a [`PendingApproval`] for the humans who answer
//! it: the Telegram prompt and its post-resolution edits, the macOS dialog
//! text, and the `description` on `GET /approvals`.
//!
//! Everything here is built from the STRUCTURED fields on the pending entry
//! (tool, subject, vault, text length, client name/id) — never by parsing the
//! `summary` string, which stays a machine-ish audit line. Every
//! user-supplied value is neutralised with [`display_text`] (control and bidi
//! characters become visible `\u{..}` escapes), and the subject is capped, so
//! a hostile title can neither spoof the layout nor push a Telegram message
//! past its 4096-char limit. The note body is never an input here.

use serde::Serialize;

use super::approval::{client_id_short, escape_visible, Decision, PendingApproval, ResolvedVia};

/// Longest subject (title / path / query), in chars, kept on a pending
/// entry; longer ones get an ellipsis.
const CLIENT_NAME_MAX_CHARS: usize = 64;

const SUBJECT_MAX_CHARS: usize = 120;

/// The call-specific facts an approver should see, besides tool and vault.
/// Raw (but capped) — escaped only when rendered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApprovalSubject {
    /// Note title (`brain_capture`), file (`brain_get`) or query
    /// (`brain_search`). Never a note body.
    pub subject: Option<String>,
    /// Body length in chars (`brain_capture` only).
    pub text_chars: Option<usize>,
}

impl ApprovalSubject {
    pub fn new(subject: Option<&str>, text_chars: Option<usize>) -> Self {
        Self {
            subject: subject.map(cap_subject),
            text_chars,
        }
    }
}

fn cap_subject(s: &str) -> String {
    if s.chars().count() <= SUBJECT_MAX_CHARS {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(SUBJECT_MAX_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

/// Escape control and bidi characters visibly; everything else (quotes,
/// Thai, emoji) is left as written. For prose lines, unlike
/// `approval::summary_value`'s quoted form.
pub fn display_text(s: &str) -> String {
    escape_visible(s)
}

/// The shared "what is it asking" phrase.
pub fn action_phrase(tool: &str) -> String {
    match tool {
        "brain_capture" => "wants to save a new note".to_string(),
        "brain_get" => "wants to read a note".to_string(),
        "brain_search" => "wants to search your vault".to_string(),
        "brain_tasks" => "wants to read your task list".to_string(),
        other => format!("wants to run {}", display_text(other)),
    }
}

/// Past-tense completion for an allowed call.
fn done_phrase(p: &PendingApproval) -> String {
    match p.tool.as_str() {
        "brain_capture" => match &p.subject.subject {
            Some(t) => format!("saved \"{}\" to the vault", display_text(t)),
            None => "saved the note to the vault".to_string(),
        },
        "brain_get" => "read the note".to_string(),
        "brain_search" => "searched the vault".to_string(),
        "brain_tasks" => "read the task list".to_string(),
        other => format!("ran {}", display_text(other)),
    }
}

/// The asker's name. Self-registered and attacker-controlled, so it is
/// re-neutralised and capped HERE at render time, whatever path built `p`.
fn who(p: &PendingApproval) -> String {
    match p.client_name.as_deref() {
        Some(n) if !n.trim().is_empty() => display_text(&cap_name(n)),
        _ => "An unnamed app".to_string(),
    }
}

fn cap_name(s: &str) -> String {
    if s.chars().count() <= CLIENT_NAME_MAX_CHARS {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(CLIENT_NAME_MAX_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn chars_phrase(n: usize) -> String {
    if n == 1 {
        "1 character".to_string()
    } else {
        format!("{n} characters")
    }
}

/// `within 4 min` / `in under a minute`, from whole seconds remaining
/// (rounded UP to minutes).
fn wait_phrase(remaining_secs: u64) -> String {
    if remaining_secs < 60 {
        "in under a minute".to_string()
    } else {
        format!("within {} min", remaining_secs.div_ceil(60))
    }
}

fn vault_text(p: &PendingApproval) -> String {
    p.vault
        .as_deref()
        .map_or_else(|| "default vault".to_string(), display_text)
}

/// Telegram message body: headline, detail rows and the "asked by" line —
/// everything except the countdown, so it can be reused under an outcome.
pub fn telegram_body(p: &PendingApproval) -> String {
    let mut out = format!("\u{1f510} {} {}\n", who(p), action_phrase(&p.tool));
    let mut rows = Vec::new();
    match p.tool.as_str() {
        "brain_capture" => {
            if let Some(t) = &p.subject.subject {
                rows.push(format!("\u{1f4dd} {}", display_text(t)));
            }
            rows.push(format!("\u{1f4c2} {}", vault_text(p)));
            if let Some(n) = p.subject.text_chars {
                rows.push(format!("\u{270d}\u{fe0f} {}", chars_phrase(n)));
            }
        }
        "brain_get" => {
            if let Some(f) = &p.subject.subject {
                rows.push(format!("\u{1f4c4} {}", display_text(f)));
            }
            rows.push(format!("\u{1f4c2} {}", vault_text(p)));
        }
        "brain_search" => {
            if let Some(q) = &p.subject.subject {
                rows.push(format!("\u{1f50e} {}", display_text(q)));
            }
            rows.push(format!("\u{1f4c2} {}", vault_text(p)));
        }
        _ => rows.push(format!("\u{1f4c2} {}", vault_text(p))),
    }
    out.push('\n');
    out.push_str(&rows.join("\n"));
    out.push_str("\n\n");
    let id = client_id_short(&p.client_id);
    if p.client_name.is_some() {
        out.push_str(&format!(
            "Asked by {} \u{b7} self-declared name \u{b7} id {id}",
            who(p)
        ));
    } else {
        out.push_str(&format!("Asked by an unnamed app \u{b7} id {id}"));
    }
    out
}

/// The countdown line appended to the pending prompt.
pub fn telegram_wait_line(remaining_secs: u64) -> String {
    if remaining_secs < 60 {
        "\u{23f3} Answer in under a minute, or it's denied automatically".to_string()
    } else {
        format!(
            "\u{23f3} Answer {}, or it's denied automatically",
            wait_phrase(remaining_secs)
        )
    }
}

/// Full pending Telegram prompt.
pub fn telegram_prompt(p: &PendingApproval, now: u64) -> String {
    format!(
        "{}\n{}",
        telegram_body(p),
        telegram_wait_line(p.expires.saturating_sub(now))
    )
}

/// How an approval ended, as far as the Telegram edit is concerned.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Decided(Decision, ResolvedVia),
    TimedOut,
}

/// The line the Telegram message is edited to once resolved.
pub fn telegram_outcome(p: &PendingApproval, outcome: Outcome) -> String {
    match outcome {
        Outcome::TimedOut => {
            "\u{231b} Timed out \u{b7} denied automatically, nothing written".to_string()
        }
        Outcome::Decided(d, via) => {
            let allowed = d == Decision::Approve;
            match (via, allowed) {
                (ResolvedVia::Shutdown, _) => {
                    "\u{23f9} Gateway stopped \u{b7} denied, nothing written".to_string()
                }
                (ResolvedVia::Disconnect, _) => {
                    "\u{26d4} Denied \u{b7} the app disconnected, nothing written".to_string()
                }
                (ResolvedVia::Telegram, true) => {
                    format!("\u{2705} Allowed \u{b7} {}", done_phrase(p))
                }
                (ResolvedVia::Telegram, false) => {
                    "\u{26d4} Denied \u{b7} nothing was written to the vault".to_string()
                }
                (ResolvedVia::Native, a) => format!(
                    "\u{1f4bb} Answered on the Mac \u{b7} {}",
                    if a { "allowed" } else { "denied" }
                ),
                (ResolvedVia::Http, a) => format!(
                    "\u{1f4bb} Answered on the approvals page \u{b7} {}",
                    if a { "allowed" } else { "denied" }
                ),
            }
        }
    }
}

/// Unescaped dialog lines (the caller escapes each for AppleScript and joins
/// them). `remaining_secs` is the dialog's own timeout.
pub fn dialog_lines(p: &PendingApproval, remaining_secs: u64) -> Vec<String> {
    let suffix = if p.tool == "brain_capture" {
        " to OneBrain"
    } else {
        ""
    };
    let mut lines = vec![
        format!("{} {}{suffix}", who(p), action_phrase(&p.tool)),
        String::new(),
    ];
    let row = |label: &str, value: String| format!("{label:<13}{value}");
    let vault = p
        .vault
        .as_deref()
        .map_or_else(|| "default".to_string(), display_text);
    match p.tool.as_str() {
        "brain_capture" => {
            if let Some(t) = &p.subject.subject {
                lines.push(row("Note title:", display_text(t)));
            }
            lines.push(row("Vault:", vault));
            if let Some(n) = p.subject.text_chars {
                lines.push(row("Length:", chars_phrase(n)));
            }
        }
        "brain_get" => {
            if let Some(f) = &p.subject.subject {
                lines.push(row("Note:", display_text(f)));
            }
            lines.push(row("Vault:", vault));
        }
        "brain_search" => {
            if let Some(q) = &p.subject.subject {
                lines.push(row("Search for:", display_text(q)));
            }
            lines.push(row("Vault:", vault));
        }
        _ => lines.push(row("Vault:", vault)),
    }
    lines.push(String::new());
    let id = client_id_short(&p.client_id);
    let asked = if p.client_name.is_some() {
        format!("Asked by {} (self-declared name \u{b7} id {id})", who(p))
    } else {
        format!("Asked by an unnamed app (id {id})")
    };
    lines.push(format!(
        "{asked} \u{b7} answer {}",
        wait_phrase(remaining_secs)
    ));
    lines
}

/// One plain-words line for `GET /approvals`, e.g.
/// `Claude wants to save a new note: "Plan" (vault: default vault, 25 characters) · 4 min left`.
pub fn describe(p: &PendingApproval, now: u64) -> String {
    let mut out = format!("{} {}", who(p), action_phrase(&p.tool));
    if let Some(t) = &p.subject.subject {
        out.push_str(&format!(": \"{}\"", display_text(t)));
    }
    let mut extra = vec![format!("vault: {}", vault_text(p))];
    if let Some(n) = p.subject.text_chars {
        extra.push(chars_phrase(n));
    }
    out.push_str(&format!(" ({})", extra.join(", ")));
    out.push_str(&format!(
        " \u{b7} {}",
        match p.expires.saturating_sub(now) {
            s if s < 60 => "under a minute left".to_string(),
            s => format!("{} min left", s.div_ceil(60)),
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::gateway::policy::RiskClass;

    const ID: &str = "rKkfrep1IPUdbsIDkOsWPZoYfdIc6cx-qL8Vj1CGY58";

    fn capture() -> PendingApproval {
        PendingApproval {
            id: "a1".to_string(),
            client_id: ID.to_string(),
            tool: "brain_capture".to_string(),
            vault: None,
            summary: "capture: ...".to_string(),
            created: 1000,
            expires: 1240,
            class: RiskClass::Mutating,
            client_name: Some("Claude".to_string()),
            subject: ApprovalSubject::new(Some("ทดสอบ approve จากมือถือ 1"), Some(25)),
        }
    }

    #[test]
    fn golden_telegram_prompt_for_a_capture() {
        assert_eq!(
            telegram_prompt(&capture(), 1000),
            "\u{1f510} Claude wants to save a new note\n\
             \n\
             \u{1f4dd} ทดสอบ approve จากมือถือ 1\n\
             \u{1f4c2} default vault\n\
             \u{270d}\u{fe0f} 25 characters\n\
             \n\
             Asked by Claude \u{b7} self-declared name \u{b7} id rKkfrep1\u{2026}Y58\n\
             \u{23f3} Answer within 4 min, or it's denied automatically"
        );
    }

    #[test]
    fn named_vault_and_unnamed_client() {
        let mut p = capture();
        p.vault = Some("work".to_string());
        p.client_name = None;
        let t = telegram_body(&p);
        assert!(
            t.starts_with("\u{1f510} An unnamed app wants to save a new note\n"),
            "{t}"
        );
        assert!(t.contains("\n\u{1f4c2} work\n"), "{t}");
        assert!(
            t.ends_with("Asked by an unnamed app \u{b7} id rKkfrep1\u{2026}Y58"),
            "{t}"
        );
    }

    #[test]
    fn countdown_rounds_up_and_says_under_a_minute() {
        let p = capture();
        assert!(telegram_prompt(&p, 1000).contains("within 4 min"));
        assert!(
            telegram_prompt(&p, 1001).contains("within 4 min"),
            "239s -> 4"
        );
        assert!(
            telegram_prompt(&p, 1180).contains("within 1 min"),
            "exactly 60s"
        );
        assert!(telegram_prompt(&p, 1185)
            .ends_with("\u{23f3} Answer in under a minute, or it's denied automatically"));
        assert!(
            telegram_prompt(&p, 5000).contains("under a minute"),
            "expired never panics"
        );
    }

    #[test]
    fn golden_outcomes() {
        use Decision::{Approve, Deny};
        let p = capture();
        let o = |o| telegram_outcome(&p, o);
        assert_eq!(
            o(Outcome::Decided(Approve, ResolvedVia::Telegram)),
            "\u{2705} Allowed \u{b7} saved \"ทดสอบ approve จากมือถือ 1\" to the vault"
        );
        assert_eq!(
            o(Outcome::Decided(Deny, ResolvedVia::Telegram)),
            "\u{26d4} Denied \u{b7} nothing was written to the vault"
        );
        assert_eq!(
            o(Outcome::TimedOut),
            "\u{231b} Timed out \u{b7} denied automatically, nothing written"
        );
        assert_eq!(
            o(Outcome::Decided(Approve, ResolvedVia::Native)),
            "\u{1f4bb} Answered on the Mac \u{b7} allowed"
        );
        assert_eq!(
            o(Outcome::Decided(Deny, ResolvedVia::Native)),
            "\u{1f4bb} Answered on the Mac \u{b7} denied"
        );
        assert_eq!(
            o(Outcome::Decided(Approve, ResolvedVia::Http)),
            "\u{1f4bb} Answered on the approvals page \u{b7} allowed"
        );
        assert_eq!(
            o(Outcome::Decided(Deny, ResolvedVia::Shutdown)),
            "\u{23f9} Gateway stopped \u{b7} denied, nothing written"
        );
        assert_eq!(
            o(Outcome::Decided(Deny, ResolvedVia::Disconnect)),
            "\u{26d4} Denied \u{b7} the app disconnected, nothing written"
        );
    }

    #[test]
    fn allowed_outcome_for_other_tools_is_generic() {
        let mut p = capture();
        p.tool = "brain_get".to_string();
        assert_eq!(
            telegram_outcome(
                &p,
                Outcome::Decided(Decision::Approve, ResolvedVia::Telegram)
            ),
            "\u{2705} Allowed \u{b7} read the note"
        );
        p.tool = "brain_zap".to_string();
        assert_eq!(
            telegram_outcome(
                &p,
                Outcome::Decided(Decision::Approve, ResolvedVia::Telegram)
            ),
            "\u{2705} Allowed \u{b7} ran brain_zap"
        );
    }

    #[test]
    fn action_map_and_unknown_tool_fallback() {
        assert_eq!(action_phrase("brain_capture"), "wants to save a new note");
        assert_eq!(action_phrase("brain_get"), "wants to read a note");
        assert_eq!(action_phrase("brain_search"), "wants to search your vault");
        assert_eq!(action_phrase("brain_tasks"), "wants to read your task list");
        assert_eq!(
            action_phrase("zap\u{202e}\n"),
            "wants to run zap\\u{202e}\\u{a}"
        );
    }

    #[test]
    fn other_tools_show_their_safe_fields_only() {
        let mut p = capture();
        p.tool = "brain_get".to_string();
        p.subject = ApprovalSubject::new(Some("01-projects/x.md"), None);
        let t = telegram_body(&p);
        assert!(
            t.contains("\n\u{1f4c4} 01-projects/x.md\n\u{1f4c2} default vault\n"),
            "{t}"
        );
        p.tool = "brain_search".to_string();
        p.subject = ApprovalSubject::new(Some("budget 2026"), None);
        assert!(telegram_body(&p).contains("\n\u{1f50e} budget 2026\n"));
        p.tool = "brain_tasks".to_string();
        p.subject = ApprovalSubject::default();
        assert!(telegram_body(&p)
            .contains("wants to read your task list\n\n\u{1f4c2} default vault\n\n"));
    }

    #[test]
    fn hostile_title_and_name_are_neutralised_and_capped() {
        let mut p = capture();
        let title = format!("a\u{202e}b\nc{}", "x".repeat(500));
        p.subject = ApprovalSubject::new(Some(&title), Some(1));
        p.vault = Some("v\u{2066}\n".to_string());
        let t = telegram_prompt(&p, 1000);
        assert!(!t.contains('\u{202e}') && !t.contains('\u{2066}'), "{t}");
        assert!(t.contains("a\\u{202e}b\\u{a}c"), "{t}");
        assert!(t.contains("\u{2026}"), "title capped");
        assert!(t.chars().count() < 800, "{} chars", t.chars().count());
        assert!(t.contains("1 character\n"), "singular");
        assert_eq!(t.lines().count(), 8, "no injected lines: {t}");
        let d = dialog_lines(&p, 240).join("\n");
        assert!(!d.contains('\u{202e}'), "{d}");
    }

    #[test]
    fn who_neutralises_and_caps_a_raw_hostile_name_in_every_output() {
        let mut p = capture();
        // Deliberately NOT pre-sanitised: PendingApproval may be built by
        // any path.
        p.client_name = Some(format!("Cla\u{202e}ude\n{}", "z".repeat(500)));
        let w = who(&p);
        assert!(w.starts_with("Cla\\u{202e}ude\\u{a}zzz"), "{w}");
        assert!(w.chars().count() <= 64 + 1 + "\\u{202e}\\u{a}".len(), "{w}");
        for out in [
            telegram_prompt(&p, 1000),
            telegram_outcome(&p, Outcome::TimedOut),
            dialog_lines(&p, 240).join("\n"),
            describe(&p, 1000),
        ] {
            assert!(!out.contains('\u{202e}'), "{out}");
            assert!(out.chars().count() < 1000, "{} chars", out.chars().count());
        }
        let t = telegram_prompt(&p, 1000);
        assert_eq!(t.lines().count(), 8, "no injected lines: {t}");
        assert_eq!(dialog_lines(&p, 240).len(), 7);
    }

    #[test]
    fn describe_is_one_plain_line() {
        assert_eq!(
            describe(&capture(), 1000),
            "Claude wants to save a new note: \"ทดสอบ approve จากมือถือ 1\" (vault: default vault, 25 characters) \u{b7} 4 min left"
        );
    }
}
