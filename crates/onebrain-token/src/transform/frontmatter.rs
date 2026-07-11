//! Lossy YAML frontmatter strip: drops the leading `---`-delimited block
//! (tags/created/status metadata an agent rarely needs) and signals what
//! was cut so a caller that does need it knows to re-fetch the full doc.

use super::{is_doc_surface, Payload, Signal, Transform, TransformCtx};
use crate::gain::Surface;
use crate::level::OptLevel;

pub struct FrontmatterStrip;

impl Transform for FrontmatterStrip {
    fn name(&self) -> &'static str {
        "frontmatter"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Balanced
    }

    fn applies_to(&self, surface: Surface) -> bool {
        is_doc_surface(surface)
    }

    fn lossy(&self) -> bool {
        true
    }

    fn apply(&self, input: &Payload, _ctx: &TransformCtx) -> Payload {
        match strip_frontmatter(&input.text) {
            Some(stripped) => {
                let mut signals = input.signals.clone();
                signals.push(Signal::Truncated {
                    next: "frontmatter".to_string(),
                });
                Payload {
                    text: stripped,
                    signals,
                }
            }
            None => input.clone(),
        }
    }
}

fn strip_frontmatter(text: &str) -> Option<String> {
    // CRLF-safe: a Windows vault checks notes out with `\r\n`, so the opener,
    // the closing fence, and the newline after it can each be `\n` or `\r\n`.
    // The body itself is returned with its original line endings intact — a
    // `get` on a CRLF doc must stay CRLF; only the frontmatter block is cut.
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;

    // Closing fence: `\n---` or `\r\n---`. Find the earliest one so a stray
    // `---` later in the body can't be mistaken for the closer. `rest[end]` is
    // the `\n` of the closer; a preceding `\r` belongs to the fence line, not
    // the body, so nothing to trim there (it's before `end`).
    let end = rest.find("\n---")?;
    let after_fence = &rest[end + "\n---".len()..];

    // Consume the rest of the closing `---` line up to and including its EOL
    // (`\r\n` or `\n`), so the returned body starts on the next line.
    let body = after_fence
        .strip_prefix("\r\n")
        .or_else(|| after_fence.strip_prefix('\n'))
        .unwrap_or(after_fence);

    Some(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/frontmatter/input.md");

    #[test]
    fn strips_frontmatter_block_and_keeps_body() {
        let out = FrontmatterStrip.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert!(!out.text.contains("tags: [onebrain, cli]"));
        assert!(!out.text.starts_with("---"));
        assert!(out.text.contains("# Body heading"));
        assert!(out
            .text
            .contains("This is the body content that must survive frontmatter stripping."));
    }

    #[test]
    fn signals_truncation_with_a_frontmatter_marker() {
        let out = FrontmatterStrip.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert_eq!(
            out.signals,
            vec![Signal::Truncated {
                next: "frontmatter".to_string()
            }]
        );
    }

    #[test]
    fn no_frontmatter_is_a_no_op() {
        let plain = Payload::new("# Just a heading\n\nNo frontmatter here.\n");
        let out = FrontmatterStrip.apply(&plain, &TransformCtx::default());
        assert_eq!(out, plain);
    }

    #[test]
    fn is_lossy_at_balanced() {
        assert_eq!(FrontmatterStrip.min_level(), OptLevel::Balanced);
        assert!(FrontmatterStrip.lossy());
    }

    /// CRLF regression (Windows CI red): a note with `\r\n` line endings must
    /// still have its frontmatter stripped. Built from an explicit `\r\n`
    /// literal so it does NOT depend on how git checks out the fixture — this
    /// reproduces the real-user bug (a CRLF vault) on every platform, not just
    /// Windows.
    #[test]
    fn strips_crlf_frontmatter_and_keeps_crlf_body() {
        let input =
            "---\r\ntags: [x]\r\ncreated: 2026-07-11\r\n---\r\n# Body\r\n\r\nBody line.\r\n";
        let out = FrontmatterStrip.apply(&Payload::new(input), &TransformCtx::default());

        assert!(
            !out.text.contains("tags: [x]"),
            "CRLF frontmatter must be stripped, got:\n{:?}",
            out.text
        );
        assert!(!out.text.starts_with("---"));
        assert!(out.text.contains("# Body"), "body heading must survive");
        assert!(out.text.contains("Body line."), "body must survive");
        // Body line endings preserved verbatim — a CRLF doc stays CRLF.
        assert!(
            out.text.contains("\r\n"),
            "body CRLF line endings must be preserved, got:\n{:?}",
            out.text
        );
        assert_eq!(
            out.signals,
            vec![Signal::Truncated {
                next: "frontmatter".to_string()
            }]
        );
    }
}
