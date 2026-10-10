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

use super::approval::{
    client_id_short, escape_visible, Decision, PendingApproval, ResolvedVia, CLIENT_NAME_MAX_CHARS,
};

/// Longest subject (title / path / query), in chars, kept on a pending
/// entry; longer ones get an ellipsis.
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

/// What an Allowed line names: the decision only, never the result of the
/// call (the edit is sent before the call runs). A titled note is quoted;
/// everything else names the plain action that was allowed.
fn allowed_detail(p: &PendingApproval) -> String {
    match (p.tool.as_str(), &p.subject.subject) {
        ("brain_capture", Some(t)) => format!("\"{}\"", display_text(t)),
        ("brain_capture", None) => "a new note".to_string(),
        (tool, _) => plain_action(tool),
    }
}

/// The plain action for a tool that has no title to quote.
fn plain_action(tool: &str) -> String {
    match tool {
        "brain_get" => "read a note".to_string(),
        "brain_search" => "search your vault".to_string(),
        "brain_tasks" => "read your task list".to_string(),
        other => format!("run {}", display_text(other)),
    }
}

/// The asker's name. Self-registered and attacker-controlled, so it is
/// re-neutralised and capped HERE at render time, whatever path built `p`.
fn who(p: &PendingApproval) -> String {
    named(p).unwrap_or_else(|| "An unnamed app".to_string())
}

/// The rendered client name, or `None` when absent, blank or whitespace.
fn named(p: &PendingApproval) -> Option<String> {
    p.client_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(|n| display_text(&cap_name(n)))
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

/// A client-supplied vault name, escaped and capped like the client name
/// (it is checked against `gateway.yml` only after the approval gate).
fn vault_display(v: &str) -> String {
    display_text(&cap_name(v))
}

fn vault_text(p: &PendingApproval) -> String {
    p.vault
        .as_deref()
        .map_or_else(|| "default vault".to_string(), vault_display)
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
    if named(p).is_some() {
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

/// The `ask_once` consent-window line, or `None` when an Allow covers this
/// one call only.
fn grant_line(p: &PendingApproval) -> Option<String> {
    let m = p.grant_minutes?;
    let what = if p.tool == "brain_capture" {
        "save more notes"
    } else {
        "do this again"
    };
    let who = named(p).unwrap_or_else(|| "the app".to_string());
    Some(format!("Allow also lets {who} {what} for {m} min"))
}

/// Full pending Telegram prompt.
pub fn telegram_prompt(p: &PendingApproval, now: u64) -> String {
    let wait = telegram_wait_line(p.expires.saturating_sub(now));
    match grant_line(p) {
        Some(g) => format!("{}\n{g}\n{wait}", telegram_body(p)),
        None => format!("{}\n{wait}", telegram_body(p)),
    }
}

/// How an approval ended, as far as the Telegram edit is concerned.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Decided(Decision, ResolvedVia),
    TimedOut,
}

/// The line the Telegram message is edited to once resolved.
/// `until` formats an epoch second as the local `HH:MM` (see [`local_hhmm`]);
/// `now` is when the decision landed.
pub fn telegram_outcome(
    p: &PendingApproval,
    outcome: Outcome,
    now: u64,
    until: &dyn Fn(u64) -> String,
) -> String {
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
                (ResolvedVia::Revoked, _) => {
                    "\u{26d4} Access was revoked \u{b7} nothing was written".to_string()
                }
                (ResolvedVia::Telegram, true) => format!(
                    "\u{2705} Allowed \u{b7} {}{}",
                    allowed_detail(p),
                    grant_suffix(p, now, until)
                ),
                (ResolvedVia::Telegram, false) if p.tool == "brain_capture" => {
                    "\u{26d4} Denied \u{b7} nothing was written to the vault".to_string()
                }
                (ResolvedVia::Telegram, false) => {
                    format!("\u{26d4} Denied \u{b7} {}", plain_action(&p.tool))
                }
                (ResolvedVia::Native, a) => format!(
                    "\u{1f4bb} Answered on the Mac \u{b7} {}",
                    decision_word(p, a, now, until)
                ),
                (ResolvedVia::Http, a) => format!(
                    "\u{1f4bb} Answered on the approvals page \u{b7} {}",
                    decision_word(p, a, now, until)
                ),
            }
        }
    }
}

/// `allowed` / `denied`; an allow also names the `ask_once` window.
fn decision_word(
    p: &PendingApproval,
    allowed: bool,
    now: u64,
    until: &dyn Fn(u64) -> String,
) -> String {
    if allowed {
        format!("allowed{}", grant_suffix(p, now, until))
    } else {
        "denied".to_string()
    }
}

/// ` · more notes allowed until HH:MM` when this approval opens an
/// `ask_once` window, however the Allow arrived; empty otherwise.
fn grant_suffix(p: &PendingApproval, now: u64, until: &dyn Fn(u64) -> String) -> String {
    let Some(m) = p.grant_minutes else {
        return String::new();
    };
    let more = if p.tool == "brain_capture" {
        "more notes allowed"
    } else {
        "repeat allowed"
    };
    format!(
        " \u{b7} {more} until {}",
        until(now.saturating_add(m.saturating_mul(60)))
    )
}

/// Epoch seconds as the gateway machine's local `HH:MM`.
pub fn local_hhmm(epoch: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(i64::try_from(epoch).unwrap_or(i64::MAX), 0)
        .single()
        .map_or_else(|| "?".to_string(), |t| t.format("%H:%M").to_string())
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
    let row = |label: &str, value: String| format!("{label} {value}");
    let vault = p
        .vault
        .as_deref()
        .map_or_else(|| "default".to_string(), vault_display);
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
    if let Some(g) = grant_line(p) {
        lines.push(g);
    }
    let id = client_id_short(&p.client_id);
    let asked = if named(p).is_some() {
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
    if let Some(g) = grant_line(p) {
        out.push_str(&format!(" \u{b7} {g}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::gateway::policy::RiskClass;

    fn tout(p: &PendingApproval, o: Outcome) -> String {
        telegram_outcome(p, o, 1000, &|e| format!("t{e}"))
    }

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
            grant_minutes: None,
            family: "fam-1".to_string(),
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
        let o = |o| tout(&p, o);
        assert_eq!(
            o(Outcome::Decided(Approve, ResolvedVia::Telegram)),
            "\u{2705} Allowed \u{b7} \"ทดสอบ approve จากมือถือ 1\""
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
        assert_eq!(
            o(Outcome::Decided(Deny, ResolvedVia::Revoked)),
            "\u{26d4} Access was revoked \u{b7} nothing was written"
        );
    }

    #[test]
    fn allowed_outcome_states_the_decision_only() {
        let allow = Outcome::Decided(Decision::Approve, ResolvedVia::Telegram);
        let mut p = capture();
        for (tool, want) in [
            ("brain_get", "\u{2705} Allowed \u{b7} read a note"),
            ("brain_search", "\u{2705} Allowed \u{b7} search your vault"),
            ("brain_tasks", "\u{2705} Allowed \u{b7} read your task list"),
            ("brain_zap", "\u{2705} Allowed \u{b7} run brain_zap"),
        ] {
            p.tool = tool.to_string();
            assert_eq!(tout(&p, allow), want);
        }
        p.tool = "brain_capture".to_string();
        p.subject = ApprovalSubject::default();
        assert_eq!(tout(&p, allow), "\u{2705} Allowed \u{b7} a new note");
        for w in ["saved", "written", "done"] {
            assert!(!tout(&capture(), allow).contains(w));
        }
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
        p.grant_minutes = Some(30);
        let t = telegram_prompt(&p, 1000);
        assert!(!t.contains('\u{202e}') && !t.contains('\u{2066}'), "{t}");
        assert!(t.contains("a\\u{202e}b\\u{a}c"), "{t}");
        assert!(t.contains("\u{2026}"), "title capped");
        assert!(t.chars().count() < 800, "{} chars", t.chars().count());
        assert!(t.contains("1 character\n"), "singular");
        assert_eq!(t.lines().count(), 9, "no injected lines: {t}");
        assert!(t.contains("\nAllow also lets Claude save more notes for 30 min\n"));
        let d = dialog_lines(&p, 240).join("\n");
        assert!(!d.contains('\u{202e}'), "{d}");
    }

    #[test]
    fn who_neutralises_and_caps_a_raw_hostile_name_in_every_output() {
        let mut p = capture();
        // Deliberately NOT pre-sanitised: PendingApproval may be built by
        // any path.
        p.client_name = Some(format!("Cla\u{202e}ude\n{}", "z".repeat(500)));
        p.grant_minutes = Some(30);
        let w = who(&p);
        assert!(w.starts_with("Cla\\u{202e}ude\\u{a}zzz"), "{w}");
        assert!(w.chars().count() <= 64 + 1 + "\\u{202e}\\u{a}".len(), "{w}");
        for out in [
            telegram_prompt(&p, 1000),
            tout(&p, Outcome::TimedOut),
            dialog_lines(&p, 240).join("\n"),
            describe(&p, 1000),
        ] {
            assert!(!out.contains('\u{202e}'), "{out}");
            assert!(out.chars().count() < 1100, "{} chars", out.chars().count());
        }
        let t = telegram_prompt(&p, 1000);
        assert_eq!(t.lines().count(), 9, "no injected lines: {t}");
        assert!(t.contains("\nAllow also lets Cla\\u{202e}ude"), "{t}");
        assert_eq!(dialog_lines(&p, 240).len(), 8);
    }

    #[test]
    fn everything_hostile_at_once_stays_under_telegram_4096_in_prompt_and_outcome() {
        let mut p = capture();
        let big = format!("a\u{202e}\n{}", "x".repeat(5000));
        p.client_name = Some(big.clone());
        p.vault = Some(big.clone());
        p.subject = ApprovalSubject::new(Some(&big), Some(usize::MAX));
        p.grant_minutes = Some(u64::MAX);
        let allow = Outcome::Decided(Decision::Approve, ResolvedVia::Telegram);
        assert!(telegram_prompt(&p, 0).chars().count() < 4096);
        assert!(
            format!("{}\n\n{}", tout(&p, allow), telegram_body(&p))
                .chars()
                .count()
                < 4096
        );
    }

    #[test]
    fn blank_client_name_falls_back_like_none() {
        let mut p = capture();
        p.grant_minutes = Some(30);
        let none_t = {
            p.client_name = None;
            (
                telegram_prompt(&p, 1000),
                dialog_lines(&p, 240),
                describe(&p, 1000),
            )
        };
        for blank in ["", "   ", "\t"] {
            p.client_name = Some(blank.to_string());
            assert_eq!(
                (
                    telegram_prompt(&p, 1000),
                    dialog_lines(&p, 240),
                    describe(&p, 1000)
                ),
                none_t
            );
        }
        assert!(none_t
            .0
            .contains("Allow also lets the app save more notes for 30 min"));
    }

    #[test]
    fn hostile_long_vault_is_capped_in_telegram_and_dialog() {
        let mut p = capture();
        p.vault = Some(format!("v\u{202e}\n{}", "x".repeat(5000)));
        let t = telegram_prompt(&p, 1000);
        assert!(t.chars().count() < 600, "{} chars", t.chars().count());
        assert!(!t.contains('\u{202e}'), "{t}");
        assert_eq!(t.lines().count(), 8, "{t}");
        let vault_line = dialog_lines(&p, 240)
            .into_iter()
            .find(|l| l.starts_with("Vault:"))
            .unwrap();
        assert!(vault_line.chars().count() < 13 + 64 + 20, "{vault_line}");
        assert!(vault_line.ends_with('\u{2026}'), "{vault_line}");
        assert!(describe(&p, 1000).chars().count() < 400);
    }

    #[test]
    fn denied_read_tools_name_the_action_not_a_write() {
        let deny = Outcome::Decided(Decision::Deny, ResolvedVia::Telegram);
        let mut p = capture();
        assert_eq!(
            tout(&p, deny),
            "\u{26d4} Denied \u{b7} nothing was written to the vault"
        );
        p.tool = "brain_get".to_string();
        assert_eq!(tout(&p, deny), "\u{26d4} Denied \u{b7} read a note");
        p.tool = "brain_search".to_string();
        assert_eq!(tout(&p, deny), "\u{26d4} Denied \u{b7} search your vault");
    }

    #[test]
    fn client_names_keep_plain_quotes() {
        let mut p = capture();
        p.client_name = crate::commands::gateway::approval::sanitize_client_name("My \"App\"");
        assert!(telegram_body(&p).starts_with("\u{1f510} My \"App\" wants to"));
    }

    #[test]
    fn ask_once_shows_the_grant_window_in_prompt_dialog_and_outcome() {
        let mut p = capture();
        // ask_always / auto: no line anywhere.
        assert!(!telegram_prompt(&p, 1000).contains("Allow also"));
        assert!(!dialog_lines(&p, 240).join("\n").contains("Allow also"));
        p.grant_minutes = Some(30);
        let t = telegram_prompt(&p, 1000);
        assert!(
            t.ends_with(
                "Allow also lets Claude save more notes for 30 min\n\
                 \u{23f3} Answer within 4 min, or it's denied automatically"
            ),
            "{t}"
        );
        let d = dialog_lines(&p, 240);
        assert_eq!(
            d[d.len() - 2],
            "Allow also lets Claude save more notes for 30 min"
        );
        assert_eq!(
            tout(&p, Outcome::Decided(Decision::Approve, ResolvedVia::Telegram)),
            "\u{2705} Allowed \u{b7} \"ทดสอบ approve จากมือถือ 1\" \u{b7} more notes allowed until t2800"
        );
        assert_eq!(
            tout(&p, Outcome::Decided(Decision::Approve, ResolvedVia::Native)),
            "\u{1f4bb} Answered on the Mac \u{b7} allowed \u{b7} more notes allowed until t2800"
        );
        assert_eq!(
            tout(&p, Outcome::Decided(Decision::Approve, ResolvedVia::Http)),
            "\u{1f4bb} Answered on the approvals page \u{b7} allowed \u{b7} more notes allowed until t2800"
        );
        assert_eq!(
            tout(&p, Outcome::Decided(Decision::Deny, ResolvedVia::Native)),
            "\u{1f4bb} Answered on the Mac \u{b7} denied"
        );
        // Not shown on a deny.
        assert!(
            !tout(&p, Outcome::Decided(Decision::Deny, ResolvedVia::Telegram)).contains("until")
        );
        p.tool = "brain_get".to_string();
        p.client_name = None;
        assert!(
            telegram_prompt(&p, 1000).contains("Allow also lets the app do this again for 30 min")
        );
        assert!(tout(
            &p,
            Outcome::Decided(Decision::Approve, ResolvedVia::Telegram)
        )
        .ends_with("read a note \u{b7} repeat allowed until t2800"));
        assert!(
            describe(&p, 1000).ends_with("\u{b7} Allow also lets the app do this again for 30 min")
        );
    }

    #[test]
    fn describe_is_one_plain_line() {
        assert_eq!(
            describe(&capture(), 1000),
            "Claude wants to save a new note: \"ทดสอบ approve จากมือถือ 1\" (vault: default vault, 25 characters) \u{b7} 4 min left"
        );
    }
}
