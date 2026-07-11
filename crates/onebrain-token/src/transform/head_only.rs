//! Lossy `multi_get` head-only cap (aggressive rung): caps a document to
//! `ctx.get_max_tokens` on a line boundary, same mechanics as `get_cap`,
//! but unconditionally lossy — `multi_get` has no per-doc continuation
//! cursor the way `get` does, so the cut content is genuinely gone from
//! this response (recoverable only via a plain `get` on that doc).

use super::{Payload, Signal, Transform, TransformCtx};
use crate::estimate::estimate_tokens;
use crate::gain::Surface;
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

    fn applies_to(&self, surface: Surface) -> bool {
        // `multi_get` only — fan-out with no per-doc continuation cursor.
        matches!(surface, Surface::McpMultiGet)
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

        // Preserve the doc's line-ending style — `.lines()` strips `\r`, so
        // rejoining a CRLF doc with `\n` would silently normalize it to LF.
        let eol = super::whitespace::detect_eol(&input.text);
        let lines: Vec<&str> = input.text.lines().collect();
        let mut acc = String::new();
        for line in &lines {
            let candidate = if acc.is_empty() {
                (*line).to_string()
            } else {
                format!("{acc}{eol}{line}")
            };
            if estimate_tokens(&candidate, ctx.model) > max_tokens {
                if acc.is_empty() {
                    // First line alone exceeds the cap: include it anyway so
                    // a non-empty input never yields an empty head (the
                    // recovery for multi_get is a full `get`, so a small
                    // overrun on the head is fine).
                    acc = candidate;
                }
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

    #[test]
    fn single_line_over_cap_still_delivers_non_empty() {
        // Pre-fix, a single line longer than the cap left acc empty → empty
        // head. Now the first line is included even if it overruns the cap.
        let one_long_line = "z".repeat(400);
        let ctx = TransformCtx {
            get_max_tokens: Some(5),
            ..TransformCtx::default()
        };
        assert!(estimate_tokens(&one_long_line, ctx.model) > 5);

        let out = HeadOnly.apply(&Payload::new(one_long_line), &ctx);
        assert!(
            !out.text.is_empty(),
            "multi_get head must never be empty for non-empty input"
        );
    }

    #[test]
    fn preserves_crlf_when_capping_a_crlf_doc() {
        let doc = "line one here\r\nline two here\r\nline three here\r\nline four here\r\n";
        let full = estimate_tokens(doc, ModelFamily::ClaudeGeneric);
        let ctx = TransformCtx {
            get_max_tokens: Some((full / 2).max(3) as u32),
            ..TransformCtx::default()
        };

        let out = HeadOnly.apply(&Payload::new(doc), &ctx);
        assert!(out.text.len() < doc.len(), "should have capped");
        assert!(
            out.text.contains("\r\n"),
            "CRLF must be preserved in the head, got {:?}",
            out.text
        );
    }
}
