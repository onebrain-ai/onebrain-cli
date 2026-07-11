//! Lossless JSON compaction: re-serializes JSON text with no incidental
//! whitespace. Non-JSON input passes through untouched — this transform
//! only ever touches agent-facing `--json`/MCP payload text.

use super::{is_query_surface, Payload, Transform, TransformCtx};
use crate::gain::Surface;
use crate::level::OptLevel;

pub struct JsonCompact;

impl Transform for JsonCompact {
    fn name(&self) -> &'static str {
        "json_compact"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Conservative
    }

    fn applies_to(&self, surface: Surface) -> bool {
        is_query_surface(surface)
    }

    fn lossy(&self) -> bool {
        false
    }

    fn apply(&self, input: &Payload, _ctx: &TransformCtx) -> Payload {
        match serde_json::from_str::<serde_json::Value>(&input.text) {
            Ok(value) => {
                let compact = serde_json::to_string(&value).unwrap_or_else(|_| input.text.clone());
                Payload {
                    text: compact,
                    signals: input.signals.clone(),
                }
            }
            Err(_) => input.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/json_compact/input.json");

    #[test]
    fn shrinks_pretty_json_and_preserves_data() {
        let out = JsonCompact.apply(&Payload::new(INPUT), &TransformCtx::default());

        assert!(
            out.text.len() < INPUT.len(),
            "compacted text ({}) should be shorter than pretty input ({})",
            out.text.len(),
            INPUT.len()
        );

        let original: serde_json::Value = serde_json::from_str(INPUT).unwrap();
        let compacted: serde_json::Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(original, compacted, "compaction must be lossless");
    }

    #[test]
    fn is_lossless_at_conservative_and_leaves_no_new_signal() {
        assert_eq!(JsonCompact.min_level(), OptLevel::Conservative);
        assert!(!JsonCompact.lossy());

        let out = JsonCompact.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert!(out.signals.is_empty());
    }

    #[test]
    fn non_json_text_passes_through_unchanged() {
        let plain = Payload::new("just some prose, not json");
        let out = JsonCompact.apply(&plain, &TransformCtx::default());
        assert_eq!(out, plain);
    }
}
