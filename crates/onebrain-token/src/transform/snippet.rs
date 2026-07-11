//! Lossy snippet cap: truncates a snippet to `ctx.snippet_max_chars`
//! (150 at balanced, 120 at aggressive per the ladder table) and signals
//! `SnippetOmitted` whenever it actually cuts something.

use super::{Payload, Signal, Transform, TransformCtx};
use crate::level::OptLevel;

pub struct SnippetCap;

impl Transform for SnippetCap {
    fn name(&self) -> &'static str {
        "snippet"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Balanced
    }

    fn lossy(&self) -> bool {
        true
    }

    fn apply(&self, input: &Payload, ctx: &TransformCtx) -> Payload {
        let max_chars = ctx.snippet_max_chars.unwrap_or(200) as usize;
        let char_count = input.text.chars().count();
        if char_count <= max_chars {
            return input.clone();
        }

        let truncated: String = input.text.chars().take(max_chars).collect();
        let mut signals = input.signals.clone();
        signals.push(Signal::SnippetOmitted);
        Payload {
            text: truncated,
            signals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/snippet/input.txt");

    fn ctx_with_max_chars(n: u32) -> TransformCtx {
        TransformCtx {
            snippet_max_chars: Some(n),
            ..TransformCtx::default()
        }
    }

    #[test]
    fn truncates_to_150_chars_at_balanced_default() {
        let out = SnippetCap.apply(&Payload::new(INPUT.trim()), &ctx_with_max_chars(150));
        assert_eq!(out.text.chars().count(), 150);
        assert_eq!(out.signals, vec![Signal::SnippetOmitted]);
    }

    #[test]
    fn truncates_tighter_to_120_chars_at_aggressive() {
        let out = SnippetCap.apply(&Payload::new(INPUT.trim()), &ctx_with_max_chars(120));
        assert_eq!(out.text.chars().count(), 120);
    }

    #[test]
    fn short_text_under_cap_is_untouched() {
        let short = Payload::new("short snippet");
        let out = SnippetCap.apply(&short, &ctx_with_max_chars(150));
        assert_eq!(out, short);
    }

    #[test]
    fn is_lossy_at_balanced() {
        assert_eq!(SnippetCap.min_level(), OptLevel::Balanced);
        assert!(SnippetCap.lossy());
    }
}
