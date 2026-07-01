//! Wraps `fastembed` to turn chunk texts into L2-normalized embedding vectors.
//!
//! Default model is `multilingual-e5-small` (384-dim, ~470MB, fast); `bge-m3`
//! (fp32, ~2.2GB — fastembed has no quantized bge-m3) is the accuracy upgrade
//! via `set-model`. Vectors returned by
//! [`Embedder::embed`] are always L2-normalized, even though several
//! underlying models already emit near-unit vectors: normalizing explicitly
//! lets the vector store assume unit-length vectors unconditionally.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// An embedding backend: turns a batch of texts into one L2-normalized
/// vector each. [`Embedder`] is the real `fastembed`-backed implementation;
/// tests inject a deterministic in-memory fake behind this trait so the
/// engine's index/query/rebuild logic can be exercised without a multi-GB
/// model download (see `Engine::open_with_embedder`).
pub trait Embed {
    /// Embed a batch of texts, returning one vector per input text in the
    /// same order. Implementations must return L2-normalized vectors of
    /// length [`Embed::dims`].
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Embedding vector dimensionality (the vector store is opened at this
    /// width).
    fn dims(&self) -> usize;
}

/// Wraps a loaded `fastembed` text embedding model.
pub struct Embedder {
    model: Mutex<TextEmbedding>,
    pub dims: usize,
    pub model_name: String,
}

/// One entry in the supported-model registry (see [`model_registry`]):
/// static metadata used both by the CLI's `search model list`/`set` verbs
/// and (later) an interactive picker. Thai MIRACL-th nDCG@10 scores are from
/// each model's public multilingual eval where available; `None` means the
/// figure hasn't been independently verified for Thai.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelInfo {
    /// Config-facing name (`search.embed_model` value), e.g.
    /// `"multilingual-e5-small"`.
    pub name: &'static str,
    /// Embedding vector dimensionality.
    pub dims: usize,
    /// Approximate on-disk download size, human-readable.
    pub approx_size: &'static str,
    /// Max input context length, in tokens.
    pub context: usize,
    /// Thai MIRACL-th nDCG@10, if verified for this model.
    pub thai_miracl: Option<f32>,
    /// Short human-readable guidance shown alongside the entry.
    pub note: &'static str,
}

/// The full set of embedding models `onebrain search` supports, in the
/// order they should be displayed (smallest/default first). This is the
/// single source of truth for model names — [`model_dims`] and
/// [`is_supported_model`] both derive from it rather than duplicating the
/// name list.
pub fn model_registry() -> &'static [ModelInfo] {
    const REGISTRY: &[ModelInfo] = &[
        ModelInfo {
            name: "multilingual-e5-small",
            dims: 384,
            approx_size: "~470 MB",
            context: 512,
            thai_miracl: Some(75.0),
            note: "default · small + fast",
        },
        ModelInfo {
            name: "multilingual-e5-base",
            dims: 768,
            approx_size: "~1.1 GB",
            context: 512,
            thai_miracl: Some(75.2),
            note: "larger · better recall",
        },
        ModelInfo {
            name: "multilingual-e5-large",
            dims: 1024,
            approx_size: "~2.1 GB",
            context: 512,
            thai_miracl: Some(80.2),
            note: "high accuracy",
        },
        ModelInfo {
            name: "bge-m3",
            dims: 1024,
            approx_size: "~2.2 GB",
            context: 8192,
            thai_miracl: Some(82.6),
            note: "best Thai/accuracy · fp32",
        },
        ModelInfo {
            name: "embeddinggemma-300m-q",
            dims: 768,
            approx_size: "~180 MB",
            context: 2048,
            thai_miracl: None,
            note: "smallest · Thai unverified",
        },
    ];
    REGISTRY
}

/// `true` when `name` matches a model in [`model_registry`].
pub fn is_supported_model(name: &str) -> bool {
    model_registry().iter().any(|m| m.name == name)
}

/// Pure lookup: embedding dimensionality for a supported model name.
/// Unknown names return `0` (callers needing strict validation should use
/// [`new`], which bails on unknown names). Derived from [`model_registry`]
/// so the dims are never out of sync with the registry entries.
pub fn model_dims(model_name: &str) -> usize {
    model_registry()
        .iter()
        .find(|m| m.name == model_name)
        .map(|m| m.dims)
        .unwrap_or(0)
}

fn resolve_model(model_name: &str) -> Result<EmbeddingModel> {
    match model_name {
        "bge-m3" => Ok(EmbeddingModel::BGEM3),
        "multilingual-e5-large" => Ok(EmbeddingModel::MultilingualE5Large),
        "multilingual-e5-base" => Ok(EmbeddingModel::MultilingualE5Base),
        "multilingual-e5-small" => Ok(EmbeddingModel::MultilingualE5Small),
        "embeddinggemma-300m-q" => Ok(EmbeddingModel::EmbeddingGemma300MQ),
        other if is_supported_model(other) => bail!(
            "'{other}' is listed in the model registry but has no fastembed \
             mapping yet — this is a bug, please report it"
        ),
        other => bail!(
            "unsupported embedding model '{other}': supported names are {}",
            model_registry()
                .iter()
                .map(|m| m.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Load a `fastembed` text embedding model, caching downloaded model files
/// under `cache_dir`.
pub fn new(model_name: &str, cache_dir: &Path) -> Result<Embedder> {
    let model = resolve_model(model_name)?;
    let dims = model_dims(model_name);

    let init = InitOptions::new(model)
        .with_cache_dir(cache_dir.to_path_buf())
        .with_show_download_progress(true);
    let embedding = TextEmbedding::try_new(init)?;

    Ok(Embedder {
        model: Mutex::new(embedding),
        dims,
        model_name: model_name.to_string(),
    })
}

/// L2-normalize a single vector in place. No-op on a zero vector (left
/// as-is to avoid dividing by zero).
fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

impl Embedder {
    /// Embed a batch of texts, returning one L2-normalized vector per input
    /// text (same order as `texts`).
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut model = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("embedder mutex poisoned"))?;
        let mut vectors = model.embed(texts, None)?;
        for v in vectors.iter_mut() {
            l2_normalize(v);
        }
        Ok(vectors)
    }
}

impl Embed for Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Embedder::embed(self, texts)
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_lookup() {
        assert_eq!(model_dims("bge-m3"), 1024);
        assert_eq!(model_dims("multilingual-e5-small"), 384);
    }

    #[test]
    fn dims_lookup_unknown_returns_zero() {
        assert_eq!(model_dims("not-a-real-model"), 0);
    }

    #[test]
    fn registry_has_exactly_five_seeded_models() {
        let names: Vec<&str> = model_registry().iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            vec![
                "multilingual-e5-small",
                "multilingual-e5-base",
                "multilingual-e5-large",
                "bge-m3",
                "embeddinggemma-300m-q",
            ]
        );
    }

    #[test]
    fn registry_dims_match_model_dims_lookup() {
        for m in model_registry() {
            assert_eq!(
                model_dims(m.name),
                m.dims,
                "model_dims must stay in sync with the registry for {}",
                m.name
            );
        }
    }

    #[test]
    fn is_supported_model_true_for_registry_entries() {
        for m in model_registry() {
            assert!(is_supported_model(m.name), "{} should be supported", m.name);
        }
    }

    #[test]
    fn is_supported_model_false_for_unknown_name() {
        assert!(!is_supported_model("not-a-real-model"));
    }

    #[test]
    fn resolve_model_supports_every_registry_entry() {
        for m in model_registry() {
            assert!(
                resolve_model(m.name).is_ok(),
                "resolve_model must map every registry entry, missing: {}",
                m.name
            );
        }
    }

    #[test]
    fn resolve_model_rejects_unknown_name() {
        let err = resolve_model("not-a-real-model").unwrap_err();
        assert!(err.to_string().contains("unsupported embedding model"));
    }

    #[test]
    fn embed_normalizes() {
        if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
            return; // gated: downloads a model
        }
        let dir = tempfile::tempdir().unwrap();
        let e = new("multilingual-e5-small", dir.path()).unwrap(); // smallest, for test speed
        let v = e.embed(&["hello".to_string()]).unwrap();
        assert_eq!(v[0].len(), 384);
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "L2-normalized");
    }
}
