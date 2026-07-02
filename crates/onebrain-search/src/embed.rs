//! Wraps `fastembed` to turn chunk texts into L2-normalized embedding vectors.
//!
//! Default model is `multilingual-e5-small` (384-dim, ~470MB, fast); `bge-m3`
//! (fp32, ~2.2GB — fastembed has no quantized bge-m3) is the accuracy upgrade
//! via `set-model`. Vectors returned by
//! [`Embedder::embed`] are always L2-normalized, even though several
//! underlying models already emit near-unit vectors: normalizing explicitly
//! lets the vector store assume unit-length vectors unconditionally.

use std::path::{Path, PathBuf};
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
    /// Approximate download size in bytes — the denominator for download
    /// progress (%) while the model dir fills up. Rough by design (matches
    /// `approx_size`); progress is capped below 100 until the download
    /// actually finishes.
    pub approx_bytes: u64,
    /// Max input context length, in tokens.
    pub context: usize,
    /// Thai MIRACL-th nDCG@10, if verified for this model.
    pub thai_miracl: Option<f32>,
    /// Short human-readable guidance shown alongside the entry.
    pub note: &'static str,
    /// The Hugging Face repo id fastembed downloads this model from, e.g.
    /// `"intfloat/multilingual-e5-small"`. Used to compute the on-disk cache
    /// subdirectory name — see [`ModelInfo::cache_dir_name`].
    pub hf_repo: &'static str,
}

impl ModelInfo {
    /// The `models--{org}--{repo}` subdirectory name fastembed (via `hf-hub`)
    /// uses for this model's download, under the collection cache dir.
    /// `hf-hub` maps a repo id `org/repo` to `models--org--repo` by replacing
    /// every `/` with `--`.
    pub fn cache_dir_name(&self) -> String {
        format!("models--{}", self.hf_repo.replace('/', "--"))
    }
}

/// Per-model download status computed from a collection's cache dir: whether
/// the model's `models--*` dir exists, its total on-disk size, and its path.
/// Pure `std::fs` — never downloads, never opens the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDownloadStatus {
    /// `true` when the model's `models--*` dir exists under the cache dir.
    pub downloaded: bool,
    /// Total byte size of all files under the model dir, `None` if not
    /// downloaded.
    pub disk_size: Option<u64>,
    /// Absolute path to the model's `models--*` dir (whether or not it
    /// exists — callers can show the expected location).
    pub path: PathBuf,
}

/// Compute the download status of `info` given the collection's `cache_dir`.
/// Pure filesystem: checks whether the model's `models--*` dir exists and
/// sums file sizes natively (no `du`, no subprocess, no model download).
pub fn model_download_status(info: &ModelInfo, cache_dir: &Path) -> ModelDownloadStatus {
    let path = cache_dir.join(info.cache_dir_name());
    if path.is_dir() {
        ModelDownloadStatus {
            downloaded: true,
            disk_size: Some(dir_size_bytes(&path)),
            path,
        }
    } else {
        ModelDownloadStatus {
            downloaded: false,
            disk_size: None,
            path,
        }
    }
}

/// Recursively sum the byte sizes of every regular file under `root`
/// (hand-rolled stack walk — no new crate dep). Unreadable dirs/files are
/// skipped, a missing `root` totals 0. Symlinks are not followed. Public so
/// the CLI's TUI can poll a model dir's growth for download progress.
pub fn dir_size_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(entry.path()),
                Ok(ft) if ft.is_file() => {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                    }
                }
                _ => {}
            }
        }
    }
    total
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
            approx_bytes: 470_000_000,
            context: 512,
            thai_miracl: Some(75.0),
            note: "default · small + fast",
            hf_repo: "intfloat/multilingual-e5-small",
        },
        ModelInfo {
            name: "multilingual-e5-base",
            dims: 768,
            approx_size: "~1.1 GB",
            approx_bytes: 1_100_000_000,
            context: 512,
            thai_miracl: Some(75.2),
            note: "larger · better recall",
            hf_repo: "intfloat/multilingual-e5-base",
        },
        ModelInfo {
            name: "multilingual-e5-large",
            dims: 1024,
            approx_size: "~2.1 GB",
            approx_bytes: 2_100_000_000,
            context: 512,
            thai_miracl: Some(80.2),
            note: "high accuracy",
            hf_repo: "Qdrant/multilingual-e5-large-onnx",
        },
        ModelInfo {
            name: "bge-m3",
            dims: 1024,
            approx_size: "~2.2 GB",
            approx_bytes: 2_200_000_000,
            context: 8192,
            thai_miracl: Some(82.6),
            note: "best Thai/accuracy · fp32",
            hf_repo: "BAAI/bge-m3",
        },
        ModelInfo {
            name: "embeddinggemma-300m-q",
            dims: 768,
            approx_size: "~180 MB",
            approx_bytes: 180_000_000,
            context: 2048,
            thai_miracl: None,
            note: "smallest · Thai unverified",
            hf_repo: "onnx-community/embeddinggemma-300m-ONNX",
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
/// under `cache_dir`. Prints fastembed's own download progress bar to stdout
/// on a first-time download.
pub fn new(model_name: &str, cache_dir: &Path) -> Result<Embedder> {
    new_with_progress(model_name, cache_dir, true)
}

/// Same as [`new`] but SILENT: fastembed's stdout download bar is disabled.
/// Used by the interactive model TUI, which runs the terminal in raw mode and
/// draws its own in-table progress — a stray stdout print would corrupt it.
pub fn new_quiet(model_name: &str, cache_dir: &Path) -> Result<Embedder> {
    new_with_progress(model_name, cache_dir, false)
}

fn new_with_progress(model_name: &str, cache_dir: &Path, show_progress: bool) -> Result<Embedder> {
    let model = resolve_model(model_name)?;
    let dims = model_dims(model_name);

    let init = InitOptions::new(model)
        .with_cache_dir(cache_dir.to_path_buf())
        .with_show_download_progress(show_progress);
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

    fn info(name: &str) -> &'static ModelInfo {
        model_registry().iter().find(|m| m.name == name).unwrap()
    }

    #[test]
    fn cache_dir_name_maps_slashes_to_double_dash() {
        assert_eq!(
            info("multilingual-e5-small").cache_dir_name(),
            "models--intfloat--multilingual-e5-small"
        );
        assert_eq!(info("bge-m3").cache_dir_name(), "models--BAAI--bge-m3");
        assert_eq!(
            info("embeddinggemma-300m-q").cache_dir_name(),
            "models--onnx-community--embeddinggemma-300m-ONNX"
        );
    }

    #[test]
    fn every_registry_entry_has_a_models_prefixed_cache_dir() {
        for m in model_registry() {
            let name = m.cache_dir_name();
            assert!(name.starts_with("models--"), "{name}");
            assert!(!name.contains('/'), "slash not mapped in {name}");
        }
    }

    #[test]
    fn download_status_not_downloaded_when_dir_absent() {
        let dir = tempfile::tempdir().unwrap();
        let st = model_download_status(info("bge-m3"), dir.path());
        assert!(!st.downloaded);
        assert_eq!(st.disk_size, None);
        assert!(st.path.ends_with("models--BAAI--bge-m3"));
    }

    #[test]
    fn download_status_sums_file_sizes_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let m = info("multilingual-e5-small");
        let model_dir = dir.path().join(m.cache_dir_name());
        std::fs::create_dir_all(model_dir.join("snapshots/abc")).unwrap();
        std::fs::write(model_dir.join("snapshots/abc/model.onnx"), vec![0u8; 2048]).unwrap();
        std::fs::write(model_dir.join("config.json"), vec![0u8; 100]).unwrap();
        let st = model_download_status(m, dir.path());
        assert!(st.downloaded);
        assert_eq!(st.disk_size, Some(2148));
        assert_eq!(st.path, model_dir);
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
