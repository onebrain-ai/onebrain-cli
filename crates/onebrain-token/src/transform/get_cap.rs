//! `get`-style continuation cap: truncates to `ctx.get_max_tokens` on a
//! line boundary and emits a truthful continuation cursor (the line index
//! to resume from) rather than silently dropping the tail. Categorized
//! lossless (design plan step 1.4) because nothing is discarded — the rest
//! is always one more call away via the cursor.

use super::{Payload, Signal, Transform, TransformCtx};
use crate::estimate::estimate_tokens;
use crate::gain::Surface;
use crate::level::OptLevel;

pub struct GetCap;

impl Transform for GetCap {
    fn name(&self) -> &'static str {
        "get_cap"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Conservative
    }

    fn applies_to(&self, surface: Surface) -> bool {
        // `get` only — it has a real `path:N` continuation cursor.
        matches!(surface, Surface::McpGet)
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
                if acc.is_empty() {
                    // The very first line alone exceeds the cap: include it
                    // anyway (small overrun) so we never deliver an empty
                    // payload with a cursor that points at what we just
                    // failed to deliver. The cursor advances past it (i + 1),
                    // guaranteeing forward progress on resume.
                    acc = candidate;
                    cut_at = i + 1;
                } else {
                    cut_at = i;
                }
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
            ..TransformCtx::default()
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

    #[test]
    fn single_line_over_cap_still_delivers_non_empty_and_advances_cursor() {
        // One line, no newlines, longer than the cap. The pre-fix loop broke
        // at i==0 with acc empty → empty text + cursor "0" (non-advancing:
        // a caller resuming at fromLine=0 loops forever). Now it includes the
        // line and advances the cursor past it.
        let one_long_line = "z".repeat(400);
        let ctx = ctx_with_cap(5); // far below the line's token cost
        assert!(estimate_tokens(&one_long_line, ctx.model) > 5);

        let out = GetCap.apply(&Payload::new(one_long_line.clone()), &ctx);

        assert!(
            !out.text.is_empty(),
            "must never deliver empty text for non-empty input"
        );
        let Signal::Truncated { next } = &out.signals[0] else {
            panic!("expected a Truncated signal, got {:?}", out.signals)
        };
        let cursor: usize = next.parse().expect("cursor must be a line index");
        assert!(
            cursor >= 1,
            "cursor must advance past the delivered line (got {cursor})"
        );
        // Resuming at the cursor consumes the whole 1-line input — no infinite loop.
        assert_eq!(one_long_line.lines().skip(cursor).count(), 0);
    }
}
