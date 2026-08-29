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
//! Wired into `server.rs`'s `policy_gate` by Gateway PR 4, Task 5 —
//! `server::await_approval` calls [`is_available`]/[`prompt`] right after
//! [`super::approval::Approvals::register`], the same "later task" this
//! module's earlier revisions named. [`is_available`] and [`prompt`] are
//! also exercised directly by this module's own unit tests.
//!
//! ## Security: every interpolated value is attacker-influenceable
//!
//! The dialog text embeds `client_id`, the tool name, and the summary
//! (built from model-supplied tool arguments, per
//! `audit::AuditEntry::args_summary`'s own doc comment). The summary is
//! flatly untrusted; the tool name comes from this crate's own `#[tool]`
//! router. `client_id` is SERVER-minted (`oauth_routes.rs`'s
//! `register_client_handler` assigns it via `mint_secret_32` — a client
//! never picks its own, which is the whole reason a grant keyed on it is
//! unforgeable), so it is charset-bounded in practice; it is escaped here
//! anyway, because a value being trustworthy today is not the property this
//! escaping depends on. This is the SAME bug class as PR 3's `/authorize` consent-page
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
//! resolvable on `$PATH` **and** the channel not explicitly disabled (see
//! "Disabling the channel from outside the process" below); on every other
//! target, or when disabled, it is unconditionally `false` and [`prompt`] is
//! a no-op that cannot panic (it returns before ever touching
//! `std::process::Command` — see [`prompt`]'s own doc comment). On macOS
//! with the channel enabled, [`prompt`] degrades gracefully — never panics —
//! on every failure mode: the user dismissing the dialog without picking a
//! button (a non-zero `osascript` exit), a missing/relocated `osascript`
//! binary (a spawn `Err`), or the child process being killed out from under
//! it. All of those collapse to "no decision was produced" and simply skip
//! calling [`super::approval::Approvals::resolve`] at all — exactly like a
//! silent operator on the HTTP channel, the pending request's own TTL
//! (`Approvals::wait`'s timeout) is what eventually resolves it, never this
//! module.
//!
//! ## Disabling the channel from outside the process
//!
//! `server::await_approval`'s own `cfg!(test)` guard keeps THIS crate's
//! `#[cfg(test)] mod tests` (this module's own, `approval.rs`'s,
//! `server.rs`'s, ...) from ever popping a real dialog — `cfg!(test)` is
//! baked in at COMPILE time, true only for a test-profile build of code
//! running inside the SAME test binary. It does nothing for a SEPARATELY
//! COMPILED, separately spawned `onebrain` binary — exactly what
//! `tests/gateway_approval_e2e.rs` (Gateway PR 4, Task 6) does: it spawns
//! the real release/debug `onebrain gateway run` as a subprocess and drives
//! `brain_capture` through a real `ask_once` policy over real HTTP. That
//! subprocess is a normal, non-test build (`cfg!(test)` is `false` in it),
//! so on a macOS CI runner — every macOS box ships `/usr/bin/osascript` —
//! [`is_available`] would otherwise be `true` and [`prompt`] would fire a
//! real, blocking, unattended GUI `display dialog`, hanging that test until
//! the approval TTL expires (or worse, blocking the CI runner itself — the
//! exact hazard this module's own docs warned about before any caller
//! reached this code at all).
//!
//! [`DISABLE_NATIVE_APPROVAL_ENV`] (`ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL`)
//! is the escape hatch: checked FIRST in [`is_available`], before the
//! platform/`osascript` probe, so setting it to any value unconditionally
//! forces this channel off regardless of platform. An env var (not a
//! `gateway.yml` config key) was the deliberate choice here: the e2e test
//! already spawns the gateway via `std::process::Command`, so setting one
//! more entry in that same `.env(...)` call is a one-line addition with no
//! new config schema, no new `gateway.yml` key for a real operator to ever
//! need to know about (this is a TEST-ONLY escape hatch, not a
//! product-facing policy switch — an operator who genuinely wants the
//! native channel off has the real, product-facing lever already:
//! `policy.mutating: auto`/`deny` never reaches `await_approval` at all, and
//! the `/approvals` HTTP channel alone still works regardless of this
//! variable), and it composes for free with the sandboxed-HOME/cache
//! pattern every gateway integration test already uses (`tests/
//! gateway_http.rs`, `tests/gateway_oauth_e2e.rs`) without touching
//! `gateway.yml` parsing at all. `server::capabilities`'s
//! `approval_channels.native` field (Gateway PR 4, Task 6) reads straight
//! off [`is_available`], so a disabled channel is reported truthfully too —
//! never silently, never differently from what `await_approval` itself
//! observes.
//!
//! ## The dialog is TIME-BOUNDED, and why that is load-bearing
//!
//! Every script [`build_dialog_script`] emits carries a `giving up after
//! <n>` clause, where `<n>` is the pending entry's OWN remaining TTL
//! ([`dialog_timeout_secs`]). Without it the dialog would live forever, and
//! two things follow that the rest of this design silently assumed away:
//!
//! - `super::approval::Approvals::wait` removes a timed-out entry, and
//!   `super::approval::Approvals::register` prunes expired ones before
//!   counting them against [`super::approval::MAX_PENDING_APPROVALS`]. That
//!   pruning is deliberate (it stops an abandoned call wedging a slot), but
//!   it means the freed slot lets the NEXT call open a FRESH dialog while an
//!   untimed old one is still on screen. The registry caps bound concurrent
//!   registry ENTRIES; only this clause bounds accumulated DIALOGS — and
//!   with each one pinning a [`tokio::task::spawn_blocking`] thread for as
//!   long as it is up, an unbounded accumulation eventually exhausts tokio's
//!   blocking pool and starves every other `spawn_blocking` in the gateway
//!   (`server::record_audit` included, which is awaited on every tool call).
//! - A human who answers a dialog whose entry already timed out gets no
//!   feedback: `super::approval::Approvals::resolve` returns `false` for the
//!   removed id and nothing happens. Self-dismissing at the same deadline the
//!   waiter uses removes that dead-button window rather than papering over it.
//!
//! The leak this closes was fail-SAFE, never fail-open: a stale dialog's
//! Approve granted nothing, because the entry (and with it every path to
//! `super::policy::Grants::record`) was already gone.
//!
//! [`dialog_timeout_secs`] clamps, and both bounds were established against a
//! real `osascript`, not from the language reference: `giving up after 0`
//! means **never give up** (an unbounded dialog — exactly the failure being
//! fixed, and reachable from a `policy.approval_wait_seconds: 0` typo), and
//! any value above `i32::MAX` makes `osascript` fail outright with
//! `Can't make … into type integer (-1700)` — no dialog shown at all, which
//! would silently take this whole channel offline for a large configured
//! wait.
//!
//! ## The blocking boundary
//!
//! `osascript` is a blocking subprocess call — spawning it and waiting for
//! `.output()` can block for as long as it takes a human to click a button,
//! bounded now only by the `giving up after` clause above. [`prompt`]
//! therefore does the actual spawn +
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
use super::auth::core::now_epoch_secs;

/// Floor for the dialog's `giving up after` clause. `0` is NOT a valid
/// substitute for "expire immediately": a real `osascript` treats `giving up
/// after 0` as **never give up**, verified directly rather than read off the
/// language reference. So a degenerate `policy.approval_wait_seconds: 0`
/// (which `super::policy::PolicyConfig::startup_warnings` already flags)
/// would otherwise produce the exact unbounded dialog this clause exists to
/// prevent. One second of overhang past an already-timed-out waiter is
/// bounded and harmless; forever is not.
const MIN_DIALOG_TIMEOUT_SECS: u64 = 1;

/// Ceiling for the same clause: `osascript` coerces the operand to an
/// AppleScript `integer`, and anything above `i32::MAX` fails the coercion
/// (`Can't make 2.147483648E+9 into type integer. (-1700)`) — a non-zero exit
/// with no dialog ever shown. Clamping keeps a large configured
/// `approval_wait_seconds` from silently taking this channel offline.
const MAX_DIALOG_TIMEOUT_SECS: u64 = i32::MAX as u64;

/// Env var that, when set to any NON-EMPTY value, unconditionally disables
/// this channel — checked first in [`is_available`], before the
/// platform/`osascript` probe. A set-but-EMPTY value counts as unset, and
/// `=0`/`=false` count as SET (it is a presence switch, not a boolean
/// parser): both follow `super::env_switch_on`, which is this crate's
/// existing convention for `ONEBRAIN_NO_DAEMON` and `$ONEBRAIN_BIND`. See
/// the module docs' "Disabling the channel
/// from outside the process" section for why this exists and why an env var
/// (not a `gateway.yml` key) is the right shape for it. `pub` so
/// `server::capabilities`'s own doc comment can reference the exact name by
/// path, and so this module is the single source of truth for it — nothing
/// else in this crate defines this string independently. The e2e test that
/// actually sets it (`tests/gateway_approval_e2e.rs`) cannot import this
/// constant (this crate ships no library target for a separately-compiled
/// integration-test binary to depend on — see that file's own doc comment,
/// which copies this exact literal deliberately, right next to a comment
/// pointing back here).
pub const DISABLE_NATIVE_APPROVAL_ENV: &str = "ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL";

/// `true` iff a native `display dialog` prompt can plausibly be shown on
/// this machine: [`DISABLE_NATIVE_APPROVAL_ENV`] is not set to a non-empty
/// value, this build
/// targets macOS, and `osascript` resolves to a real file somewhere on
/// `$PATH`. Pure and synchronous — no subprocess is spawned to answer this
/// question, only the environment and `$PATH` are inspected (via
/// [`osascript_on_path`]).
///
/// This is a NECESSARY, not sufficient, condition for a dialog to actually
/// appear — e.g. a headless macOS process with no GUI session could still
/// pass this check and then have `osascript` itself fail at prompt-time
/// (handled by [`prompt`]'s own graceful degradation, not here).
///
/// First given a production caller by Gateway PR 4, Task 5 —
/// `server::await_approval` calls this to decide whether to fire
/// [`prompt`] alongside the `/approvals` HTTP channel. Gateway PR 4, Task 6
/// gives it a second: `server::capabilities`'s `approval_channels.native`
/// field reads this directly, so a caller is told the truth about whether
/// this channel can actually deliver a prompt right now — including when
/// it's been disabled via [`DISABLE_NATIVE_APPROVAL_ENV`].
pub fn is_available() -> bool {
    if super::env_switch_on(DISABLE_NATIVE_APPROVAL_ENV) {
        return false;
    }
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

/// How long `p`'s dialog may stay on screen, in seconds: the entry's OWN
/// remaining TTL (`expires - now`), clamped into
/// `MIN_DIALOG_TIMEOUT_SECS..=MAX_DIALOG_TIMEOUT_SECS` for the two
/// empirically-established `osascript` reasons documented on those constants.
///
/// Deriving it from `expires` rather than taking
/// `super::policy::PolicyConfig::approval_wait_seconds` as a parameter is
/// deliberate: the dialog then dies at (as near as makes no difference) the
/// same instant `super::approval::Approvals::wait` gives up on the same id,
/// with no second copy of that deadline to drift out of sync. `now` is a
/// parameter, not read inside, so this stays pure and directly testable.
fn dialog_timeout_secs(p: &PendingApproval, now: u64) -> u64 {
    p.expires
        .saturating_sub(now)
        .clamp(MIN_DIALOG_TIMEOUT_SECS, MAX_DIALOG_TIMEOUT_SECS)
}

/// Build the `-e` script text handed to `osascript`: a `display dialog`
/// with an "Approve"/"Deny" button pair (default button "Approve", so a
/// stray Return key press never silently denies), embedding `p`'s
/// `client_id`/`tool`/`summary` — every one of them run through
/// [`escape_applescript_string`] first (see module docs' "Security"
/// section) — and a `giving up after {giving_up_after}` clause so the
/// dialog can never outlive the pending entry it belongs to (see the module
/// docs' "The dialog is TIME-BOUNDED" section; [`dialog_timeout_secs`]
/// produces the value). Pure and fully unit-testable without ever invoking
/// `osascript`.
fn build_dialog_script(p: &PendingApproval, giving_up_after: u64) -> String {
    let client_id = escape_applescript_string(&p.client_id);
    let tool = escape_applescript_string(&p.tool);
    let summary = escape_applescript_string(&p.summary);
    format!(
        "display dialog \"OneBrain gateway approval request\\n\\nClient: {client_id}\\nTool: {tool}\\nSummary: {summary}\" with title \"OneBrain Gateway\" buttons {{\"Deny\", \"Approve\"}} default button \"Approve\" with icon caution giving up after {giving_up_after}"
    )
}

/// Parse `osascript`'s stdout from a completed `display dialog` call (text
/// form: `button returned:Approve`, optionally followed by more
/// comma-separated fields we don't use) into a [`Decision`]. Returns `None`
/// for anything that doesn't cleanly parse as one of our two known button
/// labels — an unexpected shape here is treated exactly like every other
/// failure mode [`prompt`] degrades on: no decision, no resolve, let the
/// TTL handle it.
///
/// That `None` is what makes the `giving up after` clause safe to add: a
/// dialog that gives up exits ZERO (so [`run_dialog`]'s `status.success()`
/// check does not catch it) and prints `button returned:, gave up:true` — an
/// EMPTY button label, which falls through to `None` here, so a self-
/// dismissed dialog can never be mistaken for a human answer. Captured
/// byte-for-byte from a real `osascript` and pinned by
/// `tests::a_gave_up_dialog_yields_no_decision`, not inferred.
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
/// handing that work off, never blocking its caller's `.await` path. That
/// blocking task is guaranteed to end: the script carries a `giving up
/// after` clause sized to `p`'s own remaining TTL
/// ([`dialog_timeout_secs`]), so the dialog — and the pool thread it pins —
/// is reclaimed no later than the waiter gives up on the same id. Every
/// failure past that point (dismissed dialog, missing binary, killed
/// process, unparseable output) is absorbed by [`run_dialog`] returning
/// `None`, in which case `approvals.resolve` is simply never called — a
/// late or absent dialog answer is harmless by construction, per
/// [`Approvals::resolve`]'s own first-response-wins doc comment.
///
/// First given a production caller by Gateway PR 4, Task 5 —
/// `server::await_approval` calls this right after registering a
/// [`PendingApproval`], handing it `state.approvals.clone()` (an
/// `Arc<Approvals>` — see [`super::server::GatewayState`]'s own doc comment
/// for why that field is an `Arc` in the first place: this function's
/// `spawn_blocking` closure below needs an owned, `'static` handle, and
/// `Arc<Approvals>` is the minimal thing that provides one without widening
/// this call to need the whole `GatewayState`).
pub fn prompt(p: &PendingApproval, approvals: Arc<Approvals>) {
    if !is_available() {
        return;
    }
    let id = p.id.clone();
    let script = build_dialog_script(p, dialog_timeout_secs(p, now_epoch_secs()));
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
        let now = crate::commands::gateway::auth::core::now_epoch_secs();
        PendingApproval {
            id: "a1".to_string(),
            client_id: "client-1".to_string(),
            tool: "brain_capture".to_string(),
            vault: Some("t1".to_string()),
            summary: "note: Quarterly Plan".to_string(),
            created: now,
            expires: now + 300,
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
        let script = build_dialog_script(&p, 300);
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
        let script = build_dialog_script(&sample(), 300);
        assert!(script.contains("buttons {\"Deny\", \"Approve\"}"));
        assert!(script.contains("default button \"Approve\""));
        assert!(script.contains("Client: client-1"));
        assert!(script.contains("Tool: brain_capture"));
    }

    // ── The dialog is time-bounded (round-2 finding A) ───────────────────

    /// The whole point: EVERY script this module emits carries a `giving up
    /// after` clause, so no dialog can outlive the pending entry that
    /// opened it. Asserted as a suffix, because AppleScript takes the clause
    /// as a trailing modifier on the `display dialog` command — anywhere
    /// else and it would not parse.
    #[test]
    fn build_dialog_script_always_time_bounds_the_dialog() {
        let script = build_dialog_script(&sample(), 300);
        assert!(
            script.ends_with(" giving up after 300"),
            "the script must end with the giving-up clause: {script}"
        );
    }

    /// The clause tracks the entry's OWN remaining TTL, so the dialog and
    /// `Approvals::wait` give up on the same id at the same moment.
    #[test]
    fn dialog_timeout_tracks_the_entrys_remaining_ttl() {
        let mut p = sample();
        p.expires = 1_000;
        assert_eq!(dialog_timeout_secs(&p, 700), 300);
        assert_eq!(
            dialog_timeout_secs(&p, 995),
            5,
            "a nearly-elapsed entry must open a correspondingly short dialog"
        );
    }

    /// `giving up after 0` means NEVER GIVE UP in real AppleScript (verified
    /// against `/usr/bin/osascript`, not read off the language reference) —
    /// the exact unbounded dialog this clause exists to prevent, and
    /// reachable from a `policy.approval_wait_seconds: 0` typo or from an
    /// entry whose TTL has already elapsed. The floor must never let a `0`
    /// reach the script.
    #[test]
    fn dialog_timeout_never_emits_zero_which_applescript_reads_as_never_expire() {
        let mut p = sample();
        p.expires = 1_000;
        assert_eq!(dialog_timeout_secs(&p, 1_000), MIN_DIALOG_TIMEOUT_SECS);
        assert_eq!(
            dialog_timeout_secs(&p, 9_999),
            MIN_DIALOG_TIMEOUT_SECS,
            "an already-expired entry must still produce a POSITIVE timeout"
        );
        const { assert!(MIN_DIALOG_TIMEOUT_SECS > 0) };
        assert!(build_dialog_script(&p, dialog_timeout_secs(&p, 9_999))
            .ends_with(&format!(" giving up after {MIN_DIALOG_TIMEOUT_SECS}")));
    }

    /// Above `i32::MAX`, `osascript` refuses the coercion to an AppleScript
    /// `integer` and exits non-zero WITHOUT showing a dialog at all — which
    /// would take this channel silently offline for a large configured
    /// `approval_wait_seconds`. The ceiling keeps the operand coercible.
    #[test]
    fn dialog_timeout_is_capped_at_what_applescript_can_coerce_to_an_integer() {
        let mut p = sample();
        p.expires = u64::MAX;
        assert_eq!(dialog_timeout_secs(&p, 0), MAX_DIALOG_TIMEOUT_SECS);
        assert_eq!(MAX_DIALOG_TIMEOUT_SECS, i32::MAX as u64);
        assert!(i32::try_from(dialog_timeout_secs(&p, 0)).is_ok());
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

    /// The load-bearing precondition for the `giving up after` clause: a
    /// dialog that self-dismisses must NOT be readable as a human decision.
    ///
    /// The literal below is the exact stdout a real `/usr/bin/osascript`
    /// produced for this module's own script with the clause appended —
    /// captured byte-for-byte, including the empty button label and the
    /// trailing newline. Note it exits ZERO, so [`run_dialog`]'s
    /// `status.success()` guard does not filter it out; this parse is the
    /// only thing that does.
    #[test]
    fn a_gave_up_dialog_yields_no_decision() {
        assert_eq!(
            decision_from_button_output("button returned:, gave up:true\n"),
            None,
            "a self-dismissed dialog must never resolve a pending approval"
        );
    }

    // ── Step 2: is_available on this platform ────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn is_available_is_true_on_macos_with_osascript_on_path() {
        // Holds the crate-wide `test_env` lock for the read window (an
        // empty pair list — nothing is actually set) purely to serialize
        // against `is_available_is_false_when_explicitly_disabled_via_env_var`
        // below, which DOES mutate `DISABLE_NATIVE_APPROVAL_ENV` under the
        // same lock: without this, cargo's default parallel test runner
        // could interleave the two, observing the disable var mid-set and
        // flaking this assertion for a reason that has nothing to do with
        // this test's own subject.
        let _env = crate::test_env::set_vars(&[]);
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

    /// The BINDING requirement this whole env var exists for (see the module
    /// docs' "Disabling the channel from outside the process" section):
    /// setting [`DISABLE_NATIVE_APPROVAL_ENV`] must force [`is_available`]
    /// to `false` — unconditionally, regardless of platform (this test runs
    /// on every CI target, not just macOS, since the override must win
    /// there too, even though `is_available` would already be `false` off
    /// macOS for an unrelated reason). This is what lets
    /// `tests/gateway_approval_e2e.rs`'s spawned gateway subprocess prove it
    /// never reaches a real `osascript` call under `ask_once` — see that
    /// file's own doc comment for the full picture (it cannot import this
    /// constant directly — no library target — so it duplicates the exact
    /// string literal next to a comment pointing back here).
    #[test]
    fn is_available_is_false_when_explicitly_disabled_via_env_var() {
        let _env = crate::test_env::set_var(DISABLE_NATIVE_APPROVAL_ENV, "1");
        assert!(
            !is_available(),
            "the disable env var must win over even a fully-available macOS + osascript machine"
        );
    }

    /// The switch follows this crate's existing env-switch convention
    /// (`super::env_switch_on`, the same shape as `ONEBRAIN_NO_DAEMON`):
    /// a set-but-EMPTY value counts as UNSET, so blanking the key in a
    /// hook-managed env block neutralizes it without having to remove it.
    /// Off macOS `is_available()` is `false` for an unrelated reason, so the
    /// empty-value arm can only be asserted where it would otherwise be
    /// `true`.
    #[cfg(target_os = "macos")]
    #[test]
    fn is_available_treats_a_set_but_empty_disable_var_as_unset() {
        let _env = crate::test_env::set_var(DISABLE_NATIVE_APPROVAL_ENV, "");
        assert!(
            is_available(),
            "an EMPTY value must count as unset, matching ONEBRAIN_NO_DAEMON's convention"
        );
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
        let _rx = approvals.register(p.clone()).unwrap();

        prompt(&p, approvals.clone());

        assert_eq!(
            approvals.list().len(),
            1,
            "a no-op prompt must not resolve (or otherwise touch) the pending approval"
        );
    }
}
