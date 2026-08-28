//! Native macOS approval-dialog channel (Gateway PR 4, Task 4) — a SECOND
//! way to resolve a [`super::approval::PendingApproval`], alongside the
//! operator-facing `GET/POST /approvals` HTTP surface
//! ([`super::approval_routes`], Task 3). Where that surface waits for an
//! operator to open a URL, this one pops a native `display dialog` right on
//! the machine running `onebrain gateway run`, so a human sitting at the
//! console can just click Approve/Deny without a browser at all.
//!
//! ## Two channels, one registry, no coordination needed
//!
//! Both channels ultimately do the exact same thing: call
//! [`super::approval::Approvals::resolve`] with the same `id`. That method's
//! own doc comment already documents first-response-wins — resolving
//! removes the entry immediately, so whichever channel answers FIRST wins,
//! and the second answer (from either channel, racing either way) is simply
//! dropped as a `false` return with no observable effect. This module
//! deliberately adds no coordination on top of that: a native dialog
//! answered a moment after an operator already resolved the same request
//! over HTTP is harmless by construction, not because of anything special
//! here.
//!
//! No production caller wires this into `server.rs`'s `policy_gate` yet —
//! same "later task" situation `approval.rs`'s own `register`/`wait` doc
//! comments already name for the HTTP channel's underlying primitives.
//! [`is_available`] and [`prompt`] are exercised directly by this module's
//! own unit tests until then.
//!
//! ## Security: every interpolated value is attacker-influenceable
//!
//! The dialog text embeds `client_id` (a client picks its own `client_id`
//! at `/register` — see `oauth_routes::register_client_handler`), the tool
//! name, and the summary (built from model-supplied tool arguments, per
//! `audit::AuditEntry::args_summary`'s own doc comment). All three are
//! untrusted. This is the SAME bug class as PR 3's `/authorize` consent-page
//! XSS — a different language (AppleScript instead of HTML), the same
//! discipline: every one of those three values is run through
//! [`escape_applescript_string`] before it is interpolated into the
//! `-e` script text handed to `osascript`, exactly like every value
//! `oauth_routes.rs` interpolates into its consent HTML is run through
//! `html_escape` first (see that module's doc comment).
//!
//! [`escape_applescript_string`] escapes the two characters an AppleScript
//! double-quoted string literal treats specially — `\` and `"` — and
//! additionally neutralizes any control character (including a raw
//! embedded newline/CR/tab) to a plain space, so the resulting text is
//! always a single, well-formed line inside our surrounding `"..."`
//! literal. It runs in a single left-to-right pass over the ORIGINAL input
//! — never re-scanning already-emitted output — exactly like
//! `oauth_routes::html_escape`'s own doc comment explains for the same
//! reason: a backslash inserted while escaping one character can never be
//! mistaken for the start of a different escape sequence later in the same
//! pass.
//!
//! Why escaping `\` and `"` alone is sufficient: after escaping, scanning
//! the output left-to-right, every `\` character is always immediately
//! followed by another `\` or by a `"` — i.e. it is always "consumed" as
//! the first half of a two-character `\\` or `\"` sequence, never left
//! dangling. That means no suffix of the escaped text can end in a lone,
//! unescaped `\` immediately before the literal closing `"` this module
//! appends afterward — the one shape that would let attacker input "eat"
//! our closing quote and keep writing AppleScript source. And because every
//! `"` in the output only ever appears as the second character of an
//! escaped `\"` pair, the escaped text can never contain an unescaped quote
//! that would close our string literal early either. `tests::` below proves
//! this against a `" & do shell script "` payload (the exact class of
//! injection this reasoning rules out), a backslash-heavy variant, and a
//! variant containing raw newlines — asserting the raw payload never
//! appears in the escaped output, only its escaped form.
//!
//! ## No new crate dependency
//!
//! Shells out to `osascript` via `std::process::Command` — no `rfd`,
//! `native-dialog`, or any other dialog crate added (binding constraint).
//!
//! ## Availability + failure modes
//!
//! [`is_available`] is `cfg!(target_os = "macos")` **and** `osascript`
//! resolvable on `$PATH`; on every other target it is unconditionally
//! `false` and [`prompt`] is a no-op that cannot panic (it returns before
//! ever touching `std::process::Command` — see [`prompt`]'s own doc
//! comment). On macOS, [`prompt`] degrades gracefully — never panics — on
//! every failure mode: the user dismissing the dialog without picking a
//! button (a non-zero `osascript` exit), a missing/relocated `osascript`
//! binary (a spawn `Err`), or the child process being killed out from under
//! it. All of those collapse to "no decision was produced" and simply skip
//! calling [`super::approval::Approvals::resolve`] at all — exactly like a
//! silent operator on the HTTP channel, the pending request's own TTL
//! (`Approvals::wait`'s timeout) is what eventually resolves it, never this
//! module.
//!
//! ## The blocking boundary
//!
//! `osascript` is a blocking subprocess call — spawning it and waiting for
//! `.output()` can block for as long as it takes a human to click a button,
//! which could be indefinite. [`prompt`] therefore does the actual spawn +
//! wait entirely inside [`tokio::task::spawn_blocking`], and is itself a
//! plain (non-`async`) function that returns immediately after handing the
//! blocking work off — it never blocks the caller's own `.await` path, and
//! it holds no lock of any kind (this module has no mutable state of its
//! own; the only shared state it touches is `Approvals`, and only via its
//! own already-synchronous, lock-drop-guard-immediately
//! [`super::approval::Approvals::resolve`]). Calling [`prompt`] therefore
//! requires an active Tokio runtime in scope (`spawn_blocking` needs one) —
//! true of every real caller here (a gateway request handler already runs
//! inside one), and irrelevant to every test in this module, since the only
//! test that calls [`prompt`] at all does so on a target where
//! [`is_available`] is `false` and returns before `spawn_blocking` is ever
//! reached.

use std::process::Command;
use std::sync::Arc;

use super::approval::{Approvals, Decision, PendingApproval};

/// `true` iff a native `display dialog` prompt can plausibly be shown on
/// this machine: this build targets macOS, and `osascript` resolves to a
/// real file somewhere on `$PATH`. Pure and synchronous — no subprocess is
/// spawned to answer this question, only `$PATH` is inspected (via
/// [`osascript_on_path`]).
///
/// This is a NECESSARY, not sufficient, condition for a dialog to actually
/// appear — e.g. a headless macOS process with no GUI session could still
/// pass this check and then have `osascript` itself fail at prompt-time
/// (handled by [`prompt`]'s own graceful degradation, not here).
///
/// No production caller yet — same "later task" situation `approval.rs`'s
/// own `register`/`wait` document (see this module's own doc comment).
/// Exercised directly by this module's own unit tests until then.
#[allow(dead_code)]
pub fn is_available() -> bool {
    cfg!(target_os = "macos") && osascript_on_path()
}

/// `true` iff some directory on `$PATH` contains a file named `osascript`.
/// Deliberately just an existence check (no executable-bit probe, no
/// `Command::new("osascript").arg("-e").arg("0")` smoke-test spawn) — this
/// runs on every [`is_available`] call, including from a synchronous,
/// non-Tokio context, so it must stay a plain filesystem scan, never a
/// subprocess spawn.
fn osascript_on_path() -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join("osascript").is_file())
}

/// Escape `input` for safe interpolation into an AppleScript
/// double-quoted string literal. See the module docs' "Security" section
/// for the full correctness argument and why escaping exactly `\` and `"`
/// (plus neutralizing control characters) is sufficient.
fn escape_applescript_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Build the `-e` script text handed to `osascript`: a `display dialog`
/// with an "Approve"/"Deny" button pair (default button "Approve", so a
/// stray Return key press never silently denies), embedding `p`'s
/// `client_id`/`tool`/`summary` — every one of them run through
/// [`escape_applescript_string`] first (see module docs' "Security"
/// section). Pure and fully unit-testable without ever invoking
/// `osascript`.
fn build_dialog_script(p: &PendingApproval) -> String {
    let client_id = escape_applescript_string(&p.client_id);
    let tool = escape_applescript_string(&p.tool);
    let summary = escape_applescript_string(&p.summary);
    format!(
        "display dialog \"OneBrain gateway approval request\\n\\nClient: {client_id}\\nTool: {tool}\\nSummary: {summary}\" with title \"OneBrain Gateway\" buttons {{\"Deny\", \"Approve\"}} default button \"Approve\" with icon caution"
    )
}

/// Parse `osascript`'s stdout from a completed `display dialog` call (text
/// form: `button returned:Approve`, optionally followed by more
/// comma-separated fields we don't use) into a [`Decision`]. Returns `None`
/// for anything that doesn't cleanly parse as one of our two known button
/// labels — an unexpected shape here is treated exactly like every other
/// failure mode [`prompt`] degrades on: no decision, no resolve, let the
/// TTL handle it.
fn decision_from_button_output(stdout: &str) -> Option<Decision> {
    let first_field = stdout.trim().split(',').next()?.trim();
    match first_field.strip_prefix("button returned:")?.trim() {
        "Approve" => Some(Decision::Approve),
        "Deny" => Some(Decision::Deny),
        _ => None,
    }
}

/// Run `osascript -e script` to completion and translate its outcome into a
/// [`Decision`], or `None` on any failure (spawn error, non-zero exit, or
/// unparseable stdout) — see the module docs' "Availability + failure
/// modes" section. Blocking; only ever called from inside
/// [`tokio::task::spawn_blocking`] by [`prompt`], never directly from an
/// async context.
fn run_dialog(script: &str) -> Option<Decision> {
    let output = Command::new("osascript").arg("-e").arg(script).output();
    let output = output.ok()?;
    if !output.status.success() {
        // Covers a dismissed dialog (AppleScript raises a user-cancelled
        // error, non-zero exit) and any other `osascript`-side failure
        // alike: no decision, no resolve.
        return None;
    }
    decision_from_button_output(&String::from_utf8_lossy(&output.stdout))
}

/// Show a native macOS approval dialog for `p` and, on an explicit
/// Approve/Deny click, resolve it via `approvals.resolve` — the second
/// channel this module adds (see module docs). A no-op, returning
/// immediately, whenever [`is_available`] is `false` (any non-macOS target,
/// or macOS without a resolvable `osascript`) — the one and only guard that
/// keeps this function panic-free on every platform: the entire
/// `std::process::Command`/`tokio::task::spawn_blocking` path below it is
/// simply never reached in that case, so there is nothing there left that
/// could panic.
///
/// On macOS, the actual spawn + wait for a human click runs inside
/// [`tokio::task::spawn_blocking`] (see the module docs' "The blocking
/// boundary" section) — this function itself returns immediately after
/// handing that work off, never blocking its caller's `.await` path. Every
/// failure past that point (dismissed dialog, missing binary, killed
/// process, unparseable output) is absorbed by [`run_dialog`] returning
/// `None`, in which case `approvals.resolve` is simply never called — a
/// late or absent dialog answer is harmless by construction, per
/// [`Approvals::resolve`]'s own first-response-wins doc comment.
///
/// No production caller yet — same "later task" situation `approval.rs`'s
/// own `register`/`wait` document (see this module's own doc comment).
/// Exercised directly by this module's own unit tests until then.
#[allow(dead_code)]
pub fn prompt(p: &PendingApproval, approvals: Arc<Approvals>) {
    if !is_available() {
        return;
    }
    let id = p.id.clone();
    let script = build_dialog_script(p);
    tokio::task::spawn_blocking(move || {
        if let Some(decision) = run_dialog(&script) {
            approvals.resolve(&id, decision);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PendingApproval {
        PendingApproval {
            id: "a1".to_string(),
            client_id: "client-1".to_string(),
            tool: "brain_capture".to_string(),
            summary: "note: Quarterly Plan".to_string(),
            created: 1_700_000_000,
            expires: 1_700_000_300,
            class: crate::commands::gateway::policy::RiskClass::Mutating,
        }
    }

    // ── Step 1: escape_applescript_string ───────────────────────────────

    #[test]
    fn escapes_double_quotes() {
        assert_eq!(escape_applescript_string(r#"say "hi""#), r#"say \"hi\""#);
    }

    #[test]
    fn escapes_backslashes() {
        assert_eq!(
            escape_applescript_string(r"C:\path\to\file"),
            r"C:\\path\\to\\file"
        );
    }

    #[test]
    fn neutralizes_newlines_and_carriage_returns_to_spaces() {
        assert_eq!(
            escape_applescript_string("line1\nline2\rline3\ttab"),
            "line1 line2 line3 tab"
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(
            escape_applescript_string("Claude Desktop 1.0"),
            "Claude Desktop 1.0"
        );
    }

    /// The adversarial payload: a `client_id` shaped to break out of the
    /// surrounding AppleScript string literal and chain a `do shell
    /// script` call. Escaping must leave the payload appearing ONLY in its
    /// escaped form — the raw, unescaped payload substring must never
    /// appear anywhere in the output.
    #[test]
    fn escapes_the_do_shell_script_injection_payload() {
        let payload = r#"" & do shell script "id"#;
        let escaped = escape_applescript_string(payload);
        assert_eq!(escaped, r#"\" & do shell script \"id"#);
        assert!(
            !escaped.contains(payload),
            "the raw, unescaped injection payload must not appear in the escaped output: {escaped}"
        );
        // Every `"` in the output is the second half of an escaped `\"`
        // pair — never a lone, unescaped quote that could close our
        // surrounding string literal early.
        for (i, c) in escaped.char_indices() {
            if c == '"' {
                assert!(
                    i > 0 && escaped.as_bytes()[i - 1] == b'\\',
                    "found an unescaped quote at byte {i} in {escaped:?}"
                );
            }
        }
    }

    /// A backslash-heavy variant, including one that ends in an odd run of
    /// backslashes immediately followed by a quote — the shape that could,
    /// if backslash-escaping were skipped or ordered wrongly, let the
    /// attacker's own escaping "consume" our surrounding closing quote.
    #[test]
    fn escapes_a_backslash_heavy_payload_ending_in_a_dangling_backslash_and_quote() {
        let payload = r#"C:\evil\path\\\"#; // ends in `\\\` — an odd run of backslashes
        let full_payload = format!("{payload}\" & do shell script \"whoami");
        let escaped = escape_applescript_string(&full_payload);
        assert!(
            !escaped.contains(&full_payload),
            "the raw payload must not survive unescaped: {escaped}"
        );
        // Same invariant as above: scanning left-to-right, every `\` is
        // immediately followed by another `\` or a `"` (i.e. it is always
        // consumed as half of a two-character escape), and every `"` is
        // always the second half of a `\"` pair.
        let bytes = escaped.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    assert!(
                        i + 1 < bytes.len() && (bytes[i + 1] == b'\\' || bytes[i + 1] == b'"'),
                        "found a dangling, unpaired backslash at byte {i} in {escaped:?}"
                    );
                    i += 2;
                }
                b'"' => panic!("found an unescaped quote at byte {i} in {escaped:?}"),
                _ => i += 1,
            }
        }
    }

    /// A payload combining raw embedded newlines with the injection
    /// attempt, proving the newline-neutralization and quote/backslash
    /// escaping compose correctly.
    #[test]
    fn escapes_a_payload_with_embedded_newlines() {
        let payload = "line one\n\" & do shell script \"whoami\r\nline two";
        let escaped = escape_applescript_string(payload);
        assert!(!escaped.contains('\n') && !escaped.contains('\r'));
        assert!(
            !escaped.contains("\" & do shell script \"whoami"),
            "the raw injection fragment must not survive unescaped: {escaped}"
        );
        assert_eq!(
            escaped,
            "line one \\\" & do shell script \\\"whoami  line two",
            "each of \\r and \\n is neutralized to its own space, so the \\r\\n run becomes two spaces"
        );
    }

    // ── build_dialog_script: the payload appears only escaped ───────────

    #[test]
    fn build_dialog_script_embeds_only_the_escaped_form_of_a_malicious_client_id() {
        let mut p = sample();
        p.client_id = r#"" & do shell script "id"#.to_string();
        let script = build_dialog_script(&p);
        assert!(
            !script.contains(r#"" & do shell script "id"#),
            "raw payload leaked unescaped into the script: {script}"
        );
        assert!(
            script.contains(r#"\" & do shell script \"id"#),
            "escaped payload must be present: {script}"
        );
    }

    #[test]
    fn build_dialog_script_has_the_expected_buttons_and_default() {
        let script = build_dialog_script(&sample());
        assert!(script.contains("buttons {\"Deny\", \"Approve\"}"));
        assert!(script.contains("default button \"Approve\""));
        assert!(script.contains("Client: client-1"));
        assert!(script.contains("Tool: brain_capture"));
    }

    // ── decision_from_button_output ──────────────────────────────────────

    #[test]
    fn parses_approve_and_deny() {
        assert_eq!(
            decision_from_button_output("button returned:Approve"),
            Some(Decision::Approve)
        );
        assert_eq!(
            decision_from_button_output("button returned:Deny\n"),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn parses_extra_trailing_fields() {
        assert_eq!(
            decision_from_button_output("button returned:Approve, gave up:false"),
            Some(Decision::Approve)
        );
    }

    #[test]
    fn unparseable_output_yields_none() {
        assert_eq!(decision_from_button_output(""), None);
        assert_eq!(decision_from_button_output("garbage"), None);
        assert_eq!(decision_from_button_output("button returned:Maybe"), None);
    }

    // ── Step 2: is_available on this platform ────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn is_available_is_true_on_macos_with_osascript_on_path() {
        assert!(
            is_available(),
            "osascript ships at /usr/bin/osascript on every macOS system, including CI runners"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn is_available_is_false_off_macos() {
        assert!(!is_available());
    }

    // ── Step 2: prompt is a non-panicking no-op off macOS ────────────────

    /// Runs ONLY on non-macOS targets (the ubuntu-latest/windows-latest legs
    /// of the CI test matrix) — never on this development machine, which is
    /// macOS. This is deliberate: on macOS, `is_available()` is true, so
    /// calling `prompt` for real would reach `tokio::task::spawn_blocking`
    /// and actually invoke `osascript -e 'display dialog ...'`, popping a
    /// real GUI dialog that blocks waiting for a human click — exactly the
    /// kind of test this task's brief says not to write. Off macOS,
    /// `is_available()` is unconditionally false, so `prompt` returns
    /// before ever touching `Command` or `spawn_blocking` — safe to call
    /// directly, synchronously, with no Tokio runtime in scope at all.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn prompt_is_a_noop_off_macos_and_does_not_panic() {
        let approvals = Arc::new(Approvals::new());
        let p = sample();
        let _rx = approvals.register(p.clone());

        prompt(&p, approvals.clone());

        assert_eq!(
            approvals.list().len(),
            1,
            "a no-op prompt must not resolve (or otherwise touch) the pending approval"
        );
    }
}
