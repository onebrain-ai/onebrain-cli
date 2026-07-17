//! [`HintedError`] — typed carrier for the output-style contract's ✗/💡
//! dressing on the TOP-LEVEL error path (bail sites whose error reaches
//! `main::render_error`), without leaking that dressing into structured
//! output.
//!
//! The problem it solves (#279 review, CRITICAL 1): `main.rs` builds the
//! JSON error envelope's `message` from the top-level `anyhow` Display, so a
//! bail string with glyphs and an embedded `\n💡 …` line would appear
//! verbatim in `--output json` `error.message`. `HintedError` splits the two
//! renderings:
//!
//! - `Display` (→ `e.to_string()` → the JSON `error.message`) is `plain`
//!   ONLY: single-line, glyph-free "what happened — why" text.
//! - Text mode (`main::render_error`) downcasts and prints
//!   `✗ {plain}` + newline + `💡 {hint}` — the contract dressing lives
//!   exclusively there.
//!
//! Attach it with `.context(HintedError::new(..))` when there IS an
//! underlying error (preserves the chain, so `exit::exit_code_for`'s
//! downcast walk — e.g. `io::Error::PermissionDenied` → exit 66 — still
//! works; CRITICAL 2), or `anyhow::Error::new(HintedError::new(..))` for a
//! fresh bail with no source.
//!
//! **Not for `eprintln!` sites.** The bulk of the #279 sweep prints directly
//! to stderr and never reaches the top-level envelope — those keep their
//! literal ✗/⚠/💡 strings.

use std::fmt;

/// See the module docs. `plain` must be single-line and glyph-free; `hint`
/// carries no 💡 glyph (the text-mode renderer adds it).
#[derive(Debug)]
pub struct HintedError {
    pub plain: String,
    pub hint: String,
}

impl HintedError {
    pub fn new(plain: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            plain: plain.into(),
            hint: hint.into(),
        }
    }
}

impl fmt::Display for HintedError {
    /// `plain` only — this is what structured-mode consumers see.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.plain)
    }
}

impl std::error::Error for HintedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_plain_only() {
        let h = HintedError::new("what happened — why", "do the thing");
        assert_eq!(h.to_string(), "what happened — why");
        assert!(!h.to_string().contains('💡'));
    }

    #[test]
    fn anyhow_to_string_is_plain_when_hinted_is_top() {
        // The JSON envelope path uses `e.to_string()` (top-level Display) —
        // with HintedError on top the message is glyph-free and single-line.
        let e = anyhow::Error::new(HintedError::new("plain text", "a hint"));
        assert_eq!(e.to_string(), "plain text");
    }

    #[test]
    fn context_attached_hinted_is_downcastable_and_keeps_chain() {
        // `.context(HintedError)` — the text renderer must find it via
        // downcast_ref, Display must be plain, and the ORIGINAL error must
        // stay in the chain (exit-code mapping depends on it).
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let e = anyhow::Error::from(io).context(HintedError::new("p", "h"));
        let hinted = e.downcast_ref::<HintedError>().expect("downcast");
        assert_eq!(hinted.plain, "p");
        assert_eq!(hinted.hint, "h");
        assert_eq!(e.to_string(), "p");
        assert!(
            e.chain()
                .any(|c| c.downcast_ref::<std::io::Error>().is_some()),
            "source io::Error must survive in the chain"
        );
    }
}
