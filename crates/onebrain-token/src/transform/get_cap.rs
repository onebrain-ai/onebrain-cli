//! `get`-style continuation cap: truncates to `ctx.get_max_tokens` on a
//! line boundary and emits a truthful continuation cursor (the line index
//! to resume from) rather than silently dropping the tail. Categorized
//! lossless (design plan step 1.4) because nothing is discarded — the rest
//! is always one more call away via the cursor.

use super::{Payload, Signal, Transform, TransformCtx};
use crate::estimate::estimate_tokens;
use crate::level::OptLevel;

pub struct GetCap;

impl Transform for GetCap {
    fn name(&self) -> &'static str {
        "get_cap"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Conservative
    }

    fn lossy(&self) -> bool {
        false
    }

    fn apply(&self, input: &Payload, ctx: &TransformCtx) -> Payload {
        let max_tokens = match ctx.get_max_tokens {
            Some(0) | None => return input.clone(),
            Some(n) => n as u64,
        };

        if estimate_tokens(&input.text, ctx.model) <= max_tokens {
            return input.clone();
        }

        let lines: Vec<&str> = input.text.lines().collect();
        let mut acc = String::new();
        let mut cut_at = lines.len();
        for (i, line) in lines.iter().enumerate() {
            let candidate = if acc.is_empty() {
                (*line).to_string()
            } else {
                format!("{acc}\n{line}")
            };
            if estimate_tokens(&candidate, ctx.model) > max_tokens {
                cut_at = i;
                break;
            }
            acc = candidate;
        }

        let mut signals = input.signals.clone();
        signals.push(Signal::Truncated {
            next: cut_at.to_string(),
        });
        Payload { text: acc, signals }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::ModelFamily;

    const INPUT: &str = include_str!("../../tests/fixtures/get_cap/input.md");

    fn ctx_with_cap(tokens: u32) -> TransformCtx {
        TransformCtx {
            model: ModelFamily::ClaudeGeneric,
            get_max_tokens: Some(tokens),
            snippet_max_chars: None,
            doc_path: None,
        }
    }

    #[test]
    fn passes_through_untouched_when_under_cap() {
        let ctx = ctx_with_cap(100_000);
        let out = GetCap.apply(&Payload::new(INPUT), &ctx);
        assert_eq!(out.text, INPUT);
        assert!(out.signals.is_empty());
    }

    #[test]
    fn truncates_on_a_line_boundary_with_a_truthful_cursor() {
        let full_tokens = estimate_tokens(INPUT, ModelFamily::ClaudeGeneric);
        let cap = (full_tokens / 3).max(5) as u32;
        let ctx = ctx_with_cap(cap);

        let out = GetCap.apply(&Payload::new(INPUT), &ctx);

        assert!(out.text.len() < INPUT.len());
        assert!(estimate_tokens(&out.text, ctx.model) <= cap as u64);
        assert!(
            matches!(out.signals.as_slice(), [Signal::Truncated { .. }]),
            "expected exactly one Truncated signal, got {:?}",
            out.signals
        );

        let Signal::Truncated { next } = &out.signals[0] else {
            unreachable!()
        };
        let cursor: usize = next.parse().expect("cursor must be a line index");
        let remaining_lines: Vec<&str> = INPUT.lines().skip(cursor).collect();
        assert!(
            !remaining_lines.is_empty(),
            "cursor must point at real remaining content"
        );
    }

    #[test]
    fn unlimited_cap_is_a_no_op() {
        let ctx = ctx_with_cap(0);
        let out = GetCap.apply(&Payload::new(INPUT), &ctx);
        assert_eq!(out.text, INPUT);
        assert!(out.signals.is_empty());
    }

    #[test]
    fn is_categorized_lossless_at_conservative() {
        assert_eq!(GetCap.min_level(), OptLevel::Conservative);
        assert!(!GetCap.lossy());
    }
}
