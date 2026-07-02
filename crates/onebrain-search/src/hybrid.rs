//! Reciprocal Rank Fusion (RRF) — combines lexical (BM25) and vector search
//! result lists into a single ranking.
//!
//! RRF only uses each result's *rank* (0-based position) within its own list,
//! not its raw score. This sidesteps the fact that BM25 scores and cosine
//! similarities live on different, incomparable scales.

use std::collections::HashMap;

/// Reciprocal Rank Fusion of two ranked result lists. Each list contributes
/// `1.0 / (k + rank)` (rank is 0-based position within its own list) to each
/// chunk_id's score; scores are summed across lists. Returns the top_k
/// `(chunk_id, fused_score)` pairs sorted by fused score descending.
pub fn rrf_fuse(
    lex: &[(String, f32)],
    vec: &[(String, f32)],
    k: f64,
    top_k: usize,
) -> Vec<(String, f64)> {
    let mut scores: HashMap<String, f64> = HashMap::new();

    for (rank, (chunk_id, _score)) in lex.iter().enumerate() {
        *scores.entry(chunk_id.clone()).or_insert(0.0) += 1.0 / (k + rank as f64);
    }
    for (rank, (chunk_id, _score)) in vec.iter().enumerate() {
        *scores.entry(chunk_id.clone()).or_insert(0.0) += 1.0 / (k + rank as f64);
    }

    let mut fused: Vec<(String, f64)> = scores.into_iter().collect();
    // Sort by score descending; break ties by chunk_id ascending for a
    // deterministic, stable output regardless of HashMap iteration order.
    // `total_cmp` (a total order over f64) avoids the latent NaN panic of
    // `partial_cmp().unwrap()` — matching `vector.rs`'s sort.
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    fused.truncate(top_k);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fuses_and_ranks() {
        let lex = vec![("a".to_string(), 9.0), ("b".to_string(), 8.0)];
        let vecr = vec![("b".to_string(), 0.9), ("c".to_string(), 0.8)];
        let out = rrf_fuse(&lex, &vecr, 60.0, 3);
        assert_eq!(out[0].0, "b"); // in both lists -> ranks first
        assert_eq!(out.len(), 3);
    }
    #[test]
    fn respects_top_k() {
        let lex = vec![
            ("a".to_string(), 1.0),
            ("b".to_string(), 1.0),
            ("c".to_string(), 1.0),
        ];
        let out = rrf_fuse(&lex, &[], 60.0, 2);
        assert_eq!(out.len(), 2);
    }
    #[test]
    fn empty_inputs() {
        assert!(rrf_fuse(&[], &[], 60.0, 5).is_empty());
    }
}
