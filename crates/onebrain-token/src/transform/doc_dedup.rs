//! Lossless cross-hit doc dedup: collapses byte-identical `(doc_path, text)`
//! chunk pairs (the same window returned twice by fusion/rerank) while
//! leaving genuinely different chunks — even from the same doc — untouched.
//! Removing an exact duplicate loses no information, so this is lossless;
//! it never compares by similarity, only by exact identity.

use std::collections::HashSet;

use super::{is_query_surface, Payload, Signal, Transform, TransformCtx};
use crate::gain::Surface;
use crate::level::OptLevel;

pub struct DocDedup;

impl Transform for DocDedup {
    fn name(&self) -> &'static str {
        "doc_dedup"
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
        let Ok(serde_json::Value::Array(hits)) =
            serde_json::from_str::<serde_json::Value>(&input.text)
        else {
            return input.clone();
        };

        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut collapsed: u32 = 0;
        let mut out = Vec::with_capacity(hits.len());

        for hit in hits {
            let doc_path = hit
                .get("doc_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = hit
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if seen.insert((doc_path, text)) {
                out.push(hit);
            } else {
                collapsed += 1;
            }
        }

        if collapsed == 0 {
            return input.clone();
        }

        let mut signals = input.signals.clone();
        signals.push(Signal::ChunksCollapsed { count: collapsed });
        Payload {
            text: serde_json::to_string(&out).unwrap_or_else(|_| input.text.clone()),
            signals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/doc_dedup/input.json");

    #[test]
    fn collapses_exact_duplicate_chunk_and_signals_count() {
        let out = DocDedup.apply(&Payload::new(INPUT), &TransformCtx::default());

        let hits: Vec<serde_json::Value> = serde_json::from_str(&out.text).unwrap();
        assert_eq!(hits.len(), 3, "one exact-duplicate chunk should collapse");
        assert_eq!(out.signals, vec![Signal::ChunksCollapsed { count: 1 }]);
    }

    #[test]
    fn never_collapses_genuinely_different_high_rerank_chunks_from_same_doc() {
        let out = DocDedup.apply(&Payload::new(INPUT), &TransformCtx::default());
        let hits: Vec<serde_json::Value> = serde_json::from_str(&out.text).unwrap();

        let same_doc_texts: Vec<&str> = hits
            .iter()
            .filter(|h| {
                h.get("doc_path").and_then(|v| v.as_str())
                    == Some("01-projects/onebrain/cli/2026-07-11-v3.4.10-design.md")
            })
            .map(|h| h.get("text").and_then(|v| v.as_str()).unwrap())
            .collect();

        // Both the deduped chunk AND the genuinely-different high-rerank
        // chunk from the same doc must both survive.
        assert_eq!(same_doc_texts.len(), 2);
        assert!(same_doc_texts
            .iter()
            .any(|t| t.contains("no I/O in transform functions")));
        assert!(same_doc_texts
            .iter()
            .any(|t| t.contains("Ledger activates at balanced")));
    }

    #[test]
    fn no_duplicates_is_a_no_op() {
        let hits = r#"[{"doc_path":"a.md","text":"one"},{"doc_path":"b.md","text":"two"}]"#;
        let out = DocDedup.apply(&Payload::new(hits), &TransformCtx::default());
        assert!(out.signals.is_empty());
    }

    #[test]
    fn is_lossless_at_conservative() {
        assert_eq!(DocDedup.min_level(), OptLevel::Conservative);
        assert!(!DocDedup.lossy());
    }
}
