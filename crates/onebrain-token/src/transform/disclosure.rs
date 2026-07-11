//! Lossy progressive disclosure: the aggressive-rung "snippet-less query"
//! technique — removes each hit's snippet field, leaving the agent to
//! explicitly request full content via `get`. Reuses `SnippetOmitted` since
//! a full omission is the same honesty claim as a partial one: the snippet is
//! no longer present.
//!
//! Query surfaces carry a JSON hit list, so the primary path is JSON-aware:
//! it removes the per-hit `text`/`snippet` field but **keeps every hit**
//! (doc_path/score preserved — plan §4.2, hit membership unchanged). It must
//! NOT empty the whole payload — doing so on a `get`/`multi_get` body was the
//! R1+R2 blocker; the surface gate now also keeps it off doc bodies entirely.
//! A raw-string fallback handles a non-JSON payload (defensive; query
//! surfaces are always JSON in production).

use serde_json::Value;

use super::{is_query_surface, Payload, Signal, Transform, TransformCtx};
use crate::gain::Surface;
use crate::level::OptLevel;

/// Hit fields that may carry a snippet, in preference order.
const SNIPPET_FIELDS: [&str; 2] = ["text", "snippet"];

pub struct Disclosure;

impl Transform for Disclosure {
    fn name(&self) -> &'static str {
        "disclosure"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Aggressive
    }

    fn applies_to(&self, surface: Surface) -> bool {
        is_query_surface(surface)
    }

    fn lossy(&self) -> bool {
        true
    }

    fn apply(&self, input: &Payload, _ctx: &TransformCtx) -> Payload {
        // JSON hit-list path: strip each hit's snippet field, keep the array.
        if let Ok(Value::Array(mut hits)) = serde_json::from_str::<Value>(&input.text) {
            let mut any_removed = false;
            for hit in &mut hits {
                let Some(obj) = hit.as_object_mut() else {
                    continue;
                };
                for key in SNIPPET_FIELDS {
                    if obj.remove(key).is_some() {
                        any_removed = true;
                    }
                }
            }
            if !any_removed {
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

        // Fallback: a bare snippet string → drop it entirely.
        if input.text.is_empty() {
            return input.clone();
        }
        let mut signals = input.signals.clone();
        signals.push(Signal::SnippetOmitted);
        Payload {
            text: String::new(),
            signals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/disclosure/input.txt");

    #[test]
    fn removes_per_hit_snippet_but_keeps_hit_membership() {
        let hits = r#"[{"doc_path":"a.md","score":0.9,"text":"a snippet"},{"doc_path":"b.md","score":0.8,"text":"another snippet"}]"#;
        let out = Disclosure.apply(&Payload::new(hits), &TransformCtx::default());

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out.text).unwrap();
        assert_eq!(
            parsed.len(),
            2,
            "hit membership must be preserved, not emptied"
        );
        for hit in &parsed {
            assert!(hit.get("text").is_none(), "snippet field must be removed");
            assert!(hit.get("doc_path").is_some(), "doc_path must survive");
            assert!(hit.get("score").is_some(), "score must survive");
        }
        assert_eq!(out.signals, vec![Signal::SnippetOmitted]);
    }

    #[test]
    fn json_hit_list_with_no_snippet_fields_is_a_no_op() {
        let hits = r#"[{"doc_path":"a.md","score":0.9},{"doc_path":"b.md","score":0.8}]"#;
        let out = Disclosure.apply(&Payload::new(hits), &TransformCtx::default());
        assert!(out.signals.is_empty());
    }

    #[test]
    fn drops_bare_snippet_string_entirely_and_signals_omission() {
        let out = Disclosure.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert_eq!(out.text, "");
        assert_eq!(out.signals, vec![Signal::SnippetOmitted]);
    }

    #[test]
    fn already_empty_snippet_is_a_no_op() {
        let empty = Payload::new("");
        let out = Disclosure.apply(&empty, &TransformCtx::default());
        assert_eq!(out, empty);
    }

    #[test]
    fn is_lossy_at_aggressive() {
        assert_eq!(Disclosure.min_level(), OptLevel::Aggressive);
        assert!(Disclosure.lossy());
    }

    #[test]
    fn applies_only_to_query_surfaces() {
        for s in [Surface::McpQuery, Surface::CliSearch, Surface::DaemonHttp] {
            assert!(Disclosure.applies_to(s), "{s:?} should apply");
        }
        for s in [Surface::McpGet, Surface::McpMultiGet, Surface::ReadHook] {
            assert!(!Disclosure.applies_to(s), "{s:?} must NOT apply");
        }
    }
}
