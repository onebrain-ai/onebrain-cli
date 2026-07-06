//! Cross-encoder reranking: re-scores a query/passage pair set for calibrated
//! relevance, sharpening the ranking that lex/vector retrieval produces.
//!
//! Default model is `onebrain-reranker-v1` (multilingual incl. Thai,
//! ~570MB). [`Rerank::rerank`] returns a calibrated 0–1 relevance score per
//! passage, in the same order as the input — callers combine this with the
//! retrieval score (or replace it outright) to re-order top-k results.

use crate::embed;
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
#[cfg(feature = "semantic")]
use std::sync::Mutex;

#[cfg(feature = "semantic")]
use fastembed::{
    OnnxSource, RerankInitOptionsUserDefined, TextRerank, TokenizerFiles, UserDefinedRerankingModel,
};

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
    /// Config-facing name, e.g. `"onebrain-reranker-v1"`.
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
    /// `"onebrain-ai/onebrain-reranker-v1"`. Used to compute the
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
///
/// The OneBrain reranker model line is versioned (`onebrain-reranker-v1`,
/// `-v2`, …): each version is a distinct registry entry pinned to a specific
/// model file by its `sha256`, so upgrading the underlying base model is an
/// explicit new entry, never a silent swap under an existing name.
pub fn reranker_registry() -> &'static [RerankerInfo] {
    const REGISTRY: &[RerankerInfo] = &[RerankerInfo {
        name: "onebrain-reranker-v1",
        approx_size: "~570 MB",
        approx_bytes: 569_011_484,
        max_length: 512,
        note: "OneBrain Reranker v1 — cross-encoder (bge-reranker-v2-m3 base, int8) · multilingual incl. Thai",
        hf_repo: "onebrain-ai/onebrain-reranker-v1",
        model_file: "model_int8.onnx",
        sha256: "dd7b26f4a233732aefbe857bef026050582dc7c1bdb8aeda909080bf15b2ad88",
    }];
    REGISTRY
}

/// `true` when `name` matches a reranker in [`reranker_registry`].
pub fn is_supported_reranker(name: &str) -> bool {
    reranker_registry().iter().any(|r| r.name == name)
}

/// The marker filename this crate writes next to a verified model file, e.g.
/// `model_int8.onnx.sha256-verified`. Its content is just the expected hex
/// digest at the time of verification, so a stale/mismatched marker (model
/// file replaced without updating the marker) is detected on the next
/// verify-once call rather than trusted blindly.
fn marker_path(model_path: &Path) -> std::path::PathBuf {
    let mut s = model_path.as_os_str().to_owned();
    s.push(".sha256-verified");
    std::path::PathBuf::from(s)
}

/// Verify `path`'s SHA-256 against `expected_hex` (case-insensitive), once.
///
/// On the happy path this only hashes the file the first time: if a marker
/// file (see [`marker_path`]) exists next to `path` and its content matches
/// `expected_hex`, hashing is skipped entirely. Otherwise the file is
/// streamed through SHA-256 in fixed-size chunks — this crate's model files
/// run ~570MB, so the whole file is never read into memory at once — and
/// compared against `expected_hex`. On success the marker is written so
/// later calls (e.g. every process start) skip the re-hash; a failed marker
/// write is logged-and-ignored (best effort) since it only costs a redundant
/// hash next time, never correctness. On mismatch, returns an error naming
/// both hashes and how to recover.
///
/// Pure filesystem + hashing, no `fastembed`/ONNX types involved, so this
/// stays compiled and tested in default (non-`semantic`) configurations too.
fn verify_sha256_once(path: &Path, expected_hex: &str) -> Result<()> {
    let marker = marker_path(path);
    if let Ok(existing) = std::fs::read_to_string(&marker) {
        if existing.trim().eq_ignore_ascii_case(expected_hex) {
            return Ok(());
        }
    }

    let mut file = File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open model file {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual_hex = format!("{:x}", hasher.finalize());

    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        bail!(
            "model file {} failed integrity check: expected sha256 {expected_hex}, got {actual_hex}. \
             The download is likely corrupt or tampered. Delete and re-fetch it: \
             `onebrain search model remove onebrain-reranker-v1`, then reindex.",
            path.display()
        );
    }

    // Best-effort: a failed marker write must not fail the load, it just
    // means the next load re-hashes instead of short-circuiting.
    let _ = std::fs::write(&marker, expected_hex);

    Ok(())
}

/// Compute the download status of `info` given the collection's `cache_dir`.
/// Pure filesystem check — reuses [`embed::model_download_status`]'s logic
/// (same `models--*` cache dir layout, since both fastembed and this
/// `hf-hub`-backed reranker download share the HF hub cache convention), just
/// adapted to [`RerankerInfo`]'s field names instead of [`embed::ModelInfo`].
pub fn reranker_download_status(
    info: &RerankerInfo,
    cache_dir: &Path,
) -> embed::ModelDownloadStatus {
    let path = cache_dir.join(info.cache_dir_name());
    if path.is_dir() {
        embed::ModelDownloadStatus {
            downloaded: true,
            disk_size: Some(embed::dir_size_bytes(&path)),
            path,
        }
    } else {
        embed::ModelDownloadStatus {
            downloaded: false,
            disk_size: None,
            path,
        }
    }
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

/// Wraps a loaded `fastembed` cross-encoder reranking model. Only compiled
/// with the `semantic` feature — lex-only builds have no ONNX runtime to
/// back it. Mirrors [`embed::Embedder`]'s shape: model behind a `Mutex`
/// (`TextRerank::rerank` takes `&mut self`), constructed once, shared across
/// threads via `Rerank: Send + Sync`.
#[cfg(feature = "semantic")]
#[derive(Debug)]
pub struct Reranker {
    model: Mutex<TextRerank>,
    pub model_name: String,
}

/// Load a `fastembed` cross-encoder reranking model via `hf-hub`, caching
/// downloaded files under `cache_dir` using the same `models--org--repo`
/// layout [`RerankerInfo::cache_dir_name`] expects.
#[cfg(feature = "semantic")]
pub fn new(model_name: &str, cache_dir: &Path) -> Result<Reranker> {
    let info = reranker_registry()
        .iter()
        .find(|r| r.name == model_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unsupported reranker model '{model_name}': supported names are {}",
                reranker_registry()
                    .iter()
                    .map(|r| r.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let api = hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(cache_dir.to_path_buf())
        .build()?;
    let repo = api.model(info.hf_repo.to_string());

    let model_path = repo.get(info.model_file)?;
    verify_sha256_once(&model_path, info.sha256)?;

    let tokenizer_files = TokenizerFiles {
        tokenizer_file: std::fs::read(repo.get("tokenizer.json")?)?,
        config_file: std::fs::read(repo.get("config.json")?)?,
        special_tokens_map_file: std::fs::read(repo.get("special_tokens_map.json")?)?,
        tokenizer_config_file: std::fs::read(repo.get("tokenizer_config.json")?)?,
    };

    let model = TextRerank::try_new_from_user_defined(
        UserDefinedRerankingModel::new(OnnxSource::File(model_path), tokenizer_files),
        RerankInitOptionsUserDefined::default().with_max_length(info.max_length),
    )?;

    Ok(Reranker {
        model: Mutex::new(model),
        model_name: model_name.to_string(),
    })
}

#[cfg(feature = "semantic")]
impl Rerank for Reranker {
    fn rerank(&self, query: &str, passages: &[String]) -> Result<Vec<f32>> {
        // Short-circuit on empty input before touching the mutex/model at
        // all — don't rely on fastembed's own behavior for a zero-length
        // batch (untested edge case upstream, and pointless lock contention
        // for a result we already know: no passages, no scores).
        if passages.is_empty() {
            return Ok(Vec::new());
        }
        let mut model = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("reranker mutex poisoned"))?;
        let results = model.rerank::<String>(query.to_string(), passages, false, None)?;
        // rerank() returns results sorted by score DESC; restore input order
        // via each result's `index`.
        let mut out = vec![0.0f32; passages.len()];
        for r in results {
            out[r.index] = sigmoid(r.score);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_exactly_one_seeded_reranker() {
        let names: Vec<&str> = reranker_registry().iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["onebrain-reranker-v1"]);
    }

    #[test]
    fn registry_entry_matches_seeded_metadata() {
        let r = &reranker_registry()[0];
        assert_eq!(r.hf_repo, "onebrain-ai/onebrain-reranker-v1");
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
            "OneBrain Reranker v1 — cross-encoder (bge-reranker-v2-m3 base, int8) · multilingual incl. Thai"
        );
    }

    #[test]
    fn cache_dir_name_maps_slashes_to_double_dash() {
        let r = &reranker_registry()[0];
        assert_eq!(
            r.cache_dir_name(),
            "models--onebrain-ai--onebrain-reranker-v1"
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
            assert!(s > 0.0 && s < 1.0, "score {s} out of bounds");
        }
    }

    #[test]
    fn fake_reranker_empty_passages_returns_empty() {
        // Documents the `Rerank::rerank` length-preservation contract at
        // n=0: an empty input yields an empty output, not an error.
        let fake = FakeReranker;
        let scores = fake.rerank("query", &[]).unwrap();
        assert!(scores.is_empty());
    }

    #[test]
    fn reranker_download_status_not_downloaded_when_dir_absent() {
        let dir = tempfile::tempdir().unwrap();
        let info = &reranker_registry()[0];
        let st = reranker_download_status(info, dir.path());
        assert!(!st.downloaded);
        assert_eq!(st.disk_size, None);
        assert!(st
            .path
            .ends_with("models--onebrain-ai--onebrain-reranker-v1"));
    }

    #[test]
    fn reranker_download_status_sums_file_sizes_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let info = &reranker_registry()[0];
        let model_dir = dir.path().join(info.cache_dir_name());
        std::fs::create_dir_all(model_dir.join("snapshots/abc")).unwrap();
        std::fs::write(
            model_dir.join("snapshots/abc/model_int8.onnx"),
            vec![0u8; 4096],
        )
        .unwrap();
        std::fs::write(model_dir.join("config.json"), vec![0u8; 200]).unwrap();
        let st = reranker_download_status(info, dir.path());
        assert!(st.downloaded);
        assert_eq!(st.disk_size, Some(4296));
        assert_eq!(st.path, model_dir);
    }

    #[test]
    fn verify_sha256_once_passes_and_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.onnx");
        std::fs::write(&path, b"hello reranker").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"hello reranker"));

        verify_sha256_once(&path, &expected).unwrap();

        let marker = marker_path(&path);
        assert!(marker.exists(), "marker file should be written on success");
        assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), expected);
    }

    #[test]
    fn verify_sha256_once_bails_on_mismatch_with_both_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.onnx");
        std::fs::write(&path, b"hello reranker").unwrap();
        let wrong = "0".repeat(64);

        let err = verify_sha256_once(&path, &wrong).unwrap_err();
        let msg = err.to_string();
        let actual = format!("{:x}", Sha256::digest(b"hello reranker"));
        assert!(msg.contains(&wrong), "{msg}");
        assert!(msg.contains(&actual), "{msg}");
        assert!(!marker_path(&path).exists(), "no marker on failure");
    }

    #[test]
    fn verify_sha256_once_short_circuits_via_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.onnx");
        std::fs::write(&path, b"hello reranker").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"hello reranker"));

        verify_sha256_once(&path, &expected).unwrap();
        assert!(marker_path(&path).exists());

        // Corrupt the file after the marker was written: a second call must
        // still succeed because the marker short-circuits the re-hash.
        std::fs::write(&path, b"corrupted contents").unwrap();
        verify_sha256_once(&path, &expected).unwrap();
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn new_rejects_unknown_reranker_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = new("not-a-real-reranker", dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported reranker model"), "{msg}");
        assert!(msg.contains("onebrain-reranker-v1"), "{msg}");
    }

    /// Gated live test: downloads the real `onebrain-reranker-v1` model from
    /// HF and exercises a real cross-encoder pass. Skipped by default — the
    /// model repo doesn't exist on HF yet at the time this test was written;
    /// exercised later once the model is published. Set
    /// `ONEBRAIN_TEST_RERANK=1` to run it.
    #[cfg(feature = "semantic")]
    #[test]
    fn reranker_scores_relevant_passage_higher() {
        if std::env::var("ONEBRAIN_TEST_RERANK").is_err() {
            return; // gated: downloads a model
        }
        let dir = tempfile::tempdir().unwrap();
        let r = new("onebrain-reranker-v1", dir.path()).unwrap();
        let passages = vec![
            "The cat sat on the mat.".to_string(),
            "Quantum entanglement links particle states.".to_string(),
        ];
        let scores = r.rerank("Where did the cat sit?", &passages).unwrap();
        assert_eq!(scores.len(), 2);
        for s in &scores {
            assert!(*s > 0.0 && *s < 1.0, "score {s} out of (0,1)");
        }
        assert!(
            scores[0] > scores[1],
            "relevant passage should outscore irrelevant: {:?}",
            scores
        );
    }
}
