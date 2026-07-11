//! Lossy `multi_get` head-only cap (aggressive rung): caps a document to
//! `ctx.get_max_tokens` on a line boundary, same mechanics as `get_cap`,
//! but unconditionally lossy — `multi_get` has no per-doc continuation
//! cursor the way `get` does, so the cut content is genuinely gone from
//! this response (recoverable only via a plain `get` on that doc).

use super::{Payload, Signal, Transform, TransformCtx};
use crate::estimate::estimate_tokens;
use crate::level::OptLevel;

/// Default head size when the caller supplies no explicit cap — deliberately
/// small since `multi_get` fans out across many docs at once.
const DEFAULT_HEAD_TOKENS: u64 = 500;

pub struct HeadOnly;

impl Transform for HeadOnly {
    fn name(&self) -> &'static str {
        "head_only"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Aggressive
    }

    fn lossy(&self) -> bool {
        true
    }

    fn apply(&self, input: &Payload, ctx: &TransformCtx) -> Payload {
        let max_tokens = match ctx.get_max_tokens {
            Some(0) => return input.clone(),
            Some(n) => n as u64,
            None => DEFAULT_HEAD_TOKENS,
        };

        if estimate_tokens(&input.text, ctx.model) <= max_tokens {
            return input.clone();
        }

        let lines: Vec<&str> = input.text.lines().collect();
        let mut acc = String::new();
        for line in &lines {
            let candidate = if acc.is_empty() {
                (*line).to_string()
            } else {
                format!("{acc}\n{line}")
            };
            if estimate_tokens(&candidate, ctx.model) > max_tokens {
                break;
            }
            acc = candidate;
        }

        let mut signals = input.signals.clone();
        signals.push(Signal::Truncated {
            next: "body".to_string(),
        });
        Payload { text: acc, signals }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::ModelFamily;

    const INPUT: &str = include_str!("../../tests/fixtures/head_only/input.md");

    #[test]
    fn caps_to_head_and_signals_body_truncated() {
        let full_tokens = estimate_tokens(INPUT, ModelFamily::ClaudeGeneric);
        let cap = (full_tokens / 3).max(5) as u32;
        let ctx = TransformCtx {
            get_max_tokens: Some(cap),
            ..TransformCtx::default()
        };

        let out = HeadOnly.apply(&Payload::new(INPUT), &ctx);

        assert!(out.text.len() < INPUT.len());
        assert_eq!(
            out.signals,
            vec![Signal::Truncated {
                next: "body".to_string()
            }]
        );
    }

    #[test]
    fn short_doc_under_default_head_is_untouched() {
        let short = Payload::new("a short doc, well under the default head cap");
        let out = HeadOnly.apply(&short, &TransformCtx::default());
        assert_eq!(out, short);
    }

    #[test]
    fn unlimited_cap_is_a_no_op() {
        let ctx = TransformCtx {
            get_max_tokens: Some(0),
            ..TransformCtx::default()
        };
        let out = HeadOnly.apply(&Payload::new(INPUT), &ctx);
        assert_eq!(out.text, INPUT);
    }

    #[test]
    fn is_lossy_at_aggressive() {
        assert_eq!(HeadOnly.min_level(), OptLevel::Aggressive);
        assert!(HeadOnly.lossy());
    }
}
