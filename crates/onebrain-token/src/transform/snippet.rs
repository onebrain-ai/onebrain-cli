//! Lossy snippet cap: caps each query hit's snippet to `ctx.snippet_max_chars`
//! (150 at balanced, 120 at aggressive per the ladder table) and signals
//! `SnippetOmitted` whenever it actually cuts something.
//!
//! Query surfaces carry a JSON hit list, so the primary path is JSON-aware:
//! it caps the per-hit `text`/`snippet` field in place and preserves hit
//! membership and structure (plan §4.2 — never drops or reorders hits). A
//! raw-string fallback handles a non-JSON payload (defensive; query surfaces
//! are always JSON in production).

use serde_json::Value;

use super::{is_query_surface, Payload, Signal, Transform, TransformCtx};
use crate::gain::Surface;
use crate::level::OptLevel;

/// Hit fields that may carry a snippet, in preference order.
const SNIPPET_FIELDS: [&str; 2] = ["text", "snippet"];

pub struct SnippetCap;

impl Transform for SnippetCap {
    fn name(&self) -> &'static str {
        "snippet"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Balanced
    }

    fn applies_to(&self, surface: Surface) -> bool {
        is_query_surface(surface)
    }

    fn lossy(&self) -> bool {
        true
    }

    fn apply(&self, input: &Payload, ctx: &TransformCtx) -> Payload {
        let max_chars = ctx.snippet_max_chars.unwrap_or(200) as usize;

        // JSON hit-list path: cap each hit's snippet field, keep the array.
        if let Ok(Value::Array(mut hits)) = serde_json::from_str::<Value>(&input.text) {
            let mut any_capped = false;
            for hit in &mut hits {
                let Some(obj) = hit.as_object_mut() else {
                    continue;
                };
                for key in SNIPPET_FIELDS {
                    let capped = match obj.get(key) {
                        Some(Value::String(s)) if s.chars().count() > max_chars => {
                            Some(s.chars().take(max_chars).collect::<String>())
                        }
                        _ => None,
                    };
                    if let Some(v) = capped {
                        obj.insert(key.to_string(), Value::String(v));
                        any_capped = true;
                    }
                }
            }
            if !any_capped {
                return input.clone();
            }
            let mut signals = input.signals.clone();
            signals.push(Signal::SnippetOmitted);
            return Payload {
                text: serde_json::to_string(&Value::Array(hits))
                    .unwrap_or_else(|_| input.text.clone()),
                signals,
            };
        }

        // Fallback: a bare snippet string.
        if input.text.chars().count() <= max_chars {
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

    #[test]
    fn caps_per_hit_snippet_in_a_json_hit_list_and_keeps_membership() {
        let long = "x".repeat(300);
        let hits = format!(
            r#"[{{"doc_path":"a.md","score":0.9,"text":"{long}"}},{{"doc_path":"b.md","score":0.8,"text":"short"}}]"#
        );
        let out = SnippetCap.apply(&Payload::new(hits), &ctx_with_max_chars(150));

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text).unwrap();
        assert_eq!(parsed.len(), 2, "hit membership must be preserved");
        assert_eq!(
            parsed[0]["text"].as_str().unwrap().chars().count(),
            150,
            "long hit snippet capped"
        );
        assert_eq!(
            parsed[0]["doc_path"].as_str().unwrap(),
            "a.md",
            "non-snippet fields untouched"
        );
        assert_eq!(
            parsed[1]["text"].as_str().unwrap(),
            "short",
            "short hit snippet untouched"
        );
        assert_eq!(out.signals, vec![Signal::SnippetOmitted]);
    }

    #[test]
    fn json_hit_list_all_short_is_a_no_op() {
        let hits = r#"[{"doc_path":"a.md","text":"one"},{"doc_path":"b.md","text":"two"}]"#;
        let out = SnippetCap.apply(&Payload::new(hits), &ctx_with_max_chars(150));
        assert!(out.signals.is_empty());
    }

    #[test]
    fn applies_only_to_query_surfaces() {
        use crate::gain::Surface;
        for s in [Surface::McpQuery, Surface::CliSearch, Surface::DaemonHttp] {
            assert!(SnippetCap.applies_to(s), "{s:?} should apply");
        }
        for s in [Surface::McpGet, Surface::McpMultiGet, Surface::ReadHook] {
            assert!(!SnippetCap.applies_to(s), "{s:?} must NOT apply");
        }
    }
}
