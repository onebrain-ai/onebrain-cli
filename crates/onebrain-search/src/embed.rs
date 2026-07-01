//! Wraps `fastembed` to turn chunk texts into L2-normalized embedding vectors.
//!
//! Default model is `bge-m3` (multilingual, 1024-dim). Vectors returned by
//! [`Embedder::embed`] are always L2-normalized, even though several
//! underlying models already emit near-unit vectors: normalizing explicitly
//! lets the vector store assume unit-length vectors unconditionally.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Wraps a loaded `fastembed` text embedding model.
pub struct Embedder {
    model: Mutex<TextEmbedding>,
    pub dims: usize,
    pub model_name: String,
}

/// Pure lookup: embedding dimensionality for a supported model name.
/// Unknown names return `0` (callers needing strict validation should use
/// [`new`], which bails on unknown names).
pub fn model_dims(model_name: &str) -> usize {
    match model_name {
        "bge-m3" => 1024,
        "multilingual-e5-large" => 1024,
        "multilingual-e5-base" => 768,
        "multilingual-e5-small" => 384,
        _ => 0,
    }
}

fn resolve_model(model_name: &str) -> Result<EmbeddingModel> {
    match model_name {
        "bge-m3" => Ok(EmbeddingModel::BGEM3),
        "multilingual-e5-large" => Ok(EmbeddingModel::MultilingualE5Large),
        "multilingual-e5-base" => Ok(EmbeddingModel::MultilingualE5Base),
        "multilingual-e5-small" => Ok(EmbeddingModel::MultilingualE5Small),
        other => bail!(
            "unsupported embedding model '{other}': supported names are \
             bge-m3, multilingual-e5-large, multilingual-e5-base, multilingual-e5-small"
        ),
    }
}

/// Load a `fastembed` text embedding model, caching downloaded model files
/// under `cache_dir`.
pub fn new(model_name: &str, cache_dir: &Path) -> Result<Embedder> {
    let model = resolve_model(model_name)?;
    let dims = model_dims(model_name);

    let init = InitOptions::new(model).with_cache_dir(cache_dir.to_path_buf());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_lookup() {
        assert_eq!(model_dims("bge-m3"), 1024);
        assert_eq!(model_dims("multilingual-e5-small"), 384);
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
