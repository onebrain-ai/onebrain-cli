//! Cross-encoder reranking: re-scores a query/passage pair set for calibrated
//! relevance, sharpening the ranking that lex/vector retrieval produces.
//!
//! Default model is `bge-reranker-v2-m3-int8` (multilingual incl. Thai,
//! ~570MB). [`Rerank::rerank`] returns a calibrated 0–1 relevance score per
//! passage, in the same order as the input — callers combine this with the
//! retrieval score (or replace it outright) to re-order top-k results.

use anyhow::Result;
use std::collections::HashSet;

/// A reranking backend: scores a batch of passages against one query.
/// [`FakeReranker`] is a deterministic in-memory stand-in used by tests so
/// engine query/rerank logic can be exercised without a multi-GB model
/// download; a later task adds the real `fastembed`-backed cross-encoder
/// behind this same trait.
///
/// `Send + Sync`: mirrors [`crate::embed::Embed`] — the engine holds its
/// boxed reranker behind an `Arc<Mutex<_>>` shared across threads.
pub trait Rerank: Send + Sync {
    /// Calibrated 0–1 relevance per passage, SAME ORDER as `passages`.
    fn rerank(&self, query: &str, passages: &[String]) -> Result<Vec<f32>>;
}

/// One entry in the supported-reranker registry (see [`reranker_registry`]):
/// static metadata mirroring [`crate::embed::ModelInfo`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankerInfo {
    /// Config-facing name, e.g. `"bge-reranker-v2-m3-int8"`.
    pub name: &'static str,
    /// Approximate on-disk download size, human-readable.
    pub approx_size: &'static str,
    /// Approximate download size in bytes — the denominator for download
    /// progress (%) while the model dir fills up.
    pub approx_bytes: u64,
    /// Max input context length, in tokens.
    pub max_length: usize,
    /// Short human-readable guidance shown alongside the entry.
    pub note: &'static str,
    /// The Hugging Face repo id this model downloads from, e.g.
    /// `"onebrain-ai/bge-reranker-v2-m3-onnx-int8"`. Used to compute the
    /// on-disk cache subdirectory name — see [`RerankerInfo::cache_dir_name`].
    pub hf_repo: &'static str,
    /// The model file name inside the HF repo, e.g. `"model_int8.onnx"`.
    pub model_file: &'static str,
    /// SHA-256 checksum of the model file, for integrity verification after
    /// download.
    pub sha256: &'static str,
}

impl RerankerInfo {
    /// The `models--{org}--{repo}` subdirectory name this model's download
    /// uses under the collection cache dir. Same mapping as
    /// [`crate::embed::ModelInfo::cache_dir_name`]: every `/` in `hf_repo`
    /// becomes `--`.
    pub fn cache_dir_name(&self) -> String {
        format!("models--{}", self.hf_repo.replace('/', "--"))
    }
}

/// The full set of reranker models `onebrain search` supports, in display
/// order. Single source of truth for reranker names — [`is_supported_reranker`]
/// derives from it rather than duplicating the name list.
pub fn reranker_registry() -> &'static [RerankerInfo] {
    const REGISTRY: &[RerankerInfo] = &[RerankerInfo {
        name: "bge-reranker-v2-m3-int8",
        approx_size: "~570 MB",
        approx_bytes: 569_011_484,
        max_length: 512,
        note: "cross-encoder reranker · int8 · multilingual incl. Thai",
        hf_repo: "onebrain-ai/bge-reranker-v2-m3-onnx-int8",
        model_file: "model_int8.onnx",
        sha256: "dd7b26f4a233732aefbe857bef026050582dc7c1bdb8aeda909080bf15b2ad88",
    }];
    REGISTRY
}

/// `true` when `name` matches a reranker in [`reranker_registry`].
pub fn is_supported_reranker(name: &str) -> bool {
    reranker_registry().iter().any(|r| r.name == name)
}

/// Logistic sigmoid: maps a raw logit to a calibrated (0, 1) probability.
/// `sigmoid(0.0) == 0.5`; monotonically increasing; bounded strictly between
/// 0 and 1.
pub fn sigmoid(logit: f32) -> f32 {
    1.0 / (1.0 + (-logit).exp())
}

/// Lowercase whitespace tokenization into a set (duplicates collapse).
fn tokenize(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

/// Jaccard similarity between two token sets: `|intersection| / |union|`.
/// Two empty sets are defined as similarity `0.0` (no overlap to measure).
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    intersection as f32 / union as f32
}

/// Deterministic in-memory [`Rerank`] fake: no model, no download. Scores
/// each passage by lowercase-whitespace-token Jaccard overlap with the query,
/// mapped through [`sigmoid`] so scores land in a calibrated-looking (0, 1)
/// range: `sigmoid(4.0 * jaccard(query_tokens, passage_tokens) - 2.0)`. Used
/// by tests that exercise rerank-aware query/engine logic without a
/// multi-GB model download.
pub struct FakeReranker;

impl Rerank for FakeReranker {
    fn rerank(&self, query: &str, passages: &[String]) -> Result<Vec<f32>> {
        let query_tokens = tokenize(query);
        Ok(passages
            .iter()
            .map(|p| {
                let passage_tokens = tokenize(p);
                let overlap = jaccard(&query_tokens, &passage_tokens);
                sigmoid(4.0 * overlap - 2.0)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_exactly_one_seeded_reranker() {
        let names: Vec<&str> = reranker_registry().iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["bge-reranker-v2-m3-int8"]);
    }

    #[test]
    fn registry_entry_matches_seeded_metadata() {
        let r = &reranker_registry()[0];
        assert_eq!(r.hf_repo, "onebrain-ai/bge-reranker-v2-m3-onnx-int8");
        assert_eq!(r.model_file, "model_int8.onnx");
        assert_eq!(
            r.sha256,
            "dd7b26f4a233732aefbe857bef026050582dc7c1bdb8aeda909080bf15b2ad88"
        );
        assert_eq!(r.approx_bytes, 569_011_484);
        assert_eq!(r.approx_size, "~570 MB");
        assert_eq!(r.max_length, 512);
        assert_eq!(
            r.note,
            "cross-encoder reranker · int8 · multilingual incl. Thai"
        );
    }

    #[test]
    fn cache_dir_name_maps_slashes_to_double_dash() {
        let r = &reranker_registry()[0];
        assert_eq!(
            r.cache_dir_name(),
            "models--onebrain-ai--bge-reranker-v2-m3-onnx-int8"
        );
    }

    #[test]
    fn is_supported_reranker_true_for_registry_entries() {
        for r in reranker_registry() {
            assert!(
                is_supported_reranker(r.name),
                "{} should be supported",
                r.name
            );
        }
    }

    #[test]
    fn is_supported_reranker_false_for_unknown_name() {
        assert!(!is_supported_reranker("not-a-real-reranker"));
    }

    #[test]
    fn sigmoid_midpoint_is_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sigmoid_is_monotonic() {
        let xs = [-5.0, -2.0, -1.0, 0.0, 1.0, 2.0, 5.0];
        for pair in xs.windows(2) {
            assert!(
                sigmoid(pair[0]) < sigmoid(pair[1]),
                "sigmoid({}) should be < sigmoid({})",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn sigmoid_is_bounded_between_zero_and_one() {
        // Bounds chosen to stay within f32 precision — very large-magnitude
        // logits round to exactly 0.0/1.0 in f32, so extreme values aren't
        // useful here. The strict (0, 1) bound matters for realistic
        // reranker logits, which stay well within this range.
        for logit in [-10.0, -5.0, -1.0, 0.0, 1.0, 5.0, 10.0] {
            let s = sigmoid(logit);
            assert!(s > 0.0, "sigmoid({logit}) = {s} should be > 0");
            assert!(s < 1.0, "sigmoid({logit}) = {s} should be < 1");
        }
    }

    #[test]
    fn fake_reranker_returns_input_order_length() {
        let fake = FakeReranker;
        let passages = vec![
            "the cat sat on the mat".to_string(),
            "dogs are great pets".to_string(),
            "the cat sat on the rug".to_string(),
        ];
        let scores = fake.rerank("cat sat mat", &passages).unwrap();
        assert_eq!(scores.len(), passages.len());
    }

    #[test]
    fn fake_reranker_is_deterministic() {
        let fake = FakeReranker;
        let passages = vec!["the cat sat on the mat".to_string()];
        let a = fake.rerank("cat mat", &passages).unwrap();
        let b = fake.rerank("cat mat", &passages).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn fake_reranker_higher_overlap_scores_higher() {
        let fake = FakeReranker;
        let passages = vec![
            "the cat sat on the mat".to_string(), // high overlap with query
            "dogs are great pets".to_string(),    // no overlap
        ];
        let scores = fake.rerank("the cat sat on the mat", &passages).unwrap();
        assert!(
            scores[0] > scores[1],
            "higher-overlap passage should score higher: {:?}",
            scores
        );
    }

    #[test]
    fn fake_reranker_scores_are_bounded() {
        let fake = FakeReranker;
        let passages = vec![
            "completely unrelated text here".to_string(),
            "the cat sat on the mat".to_string(),
        ];
        let scores = fake.rerank("the cat sat on the mat", &passages).unwrap();
        for s in scores {
            assert!((0.0..1.0).contains(&s), "score {s} out of bounds");
        }
    }
}
