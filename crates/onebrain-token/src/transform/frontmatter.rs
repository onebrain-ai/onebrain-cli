//! Lossy YAML frontmatter strip: drops the leading `---`-delimited block
//! (tags/created/status metadata an agent rarely needs) and signals what
//! was cut so a caller that does need it knows to re-fetch the full doc.

use super::{Payload, Signal, Transform, TransformCtx};
use crate::level::OptLevel;

pub struct FrontmatterStrip;

impl Transform for FrontmatterStrip {
    fn name(&self) -> &'static str {
        "frontmatter"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Balanced
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
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let after = &rest[end + "\n---".len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    Some(after.to_string())
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
}
