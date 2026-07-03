//! Wraps `fastembed` to turn chunk texts into L2-normalized embedding vectors.
//!
//! Default model is `multilingual-e5-small` (384-dim, ~470MB, fast); `bge-m3`
//! (fp32, ~2.2GB — fastembed has no quantized bge-m3) is the accuracy upgrade
//! via `set-model`. Vectors returned by
//! [`Embedder::embed`] are always L2-normalized, even though several
//! underlying models already emit near-unit vectors: normalizing explicitly
//! lets the vector store assume unit-length vectors unconditionally.

use std::path::{Path, PathBuf};
#[cfg(feature = "semantic")]
use std::sync::Mutex;

#[cfg(feature = "semantic")]
use anyhow::bail;
use anyhow::Result;
#[cfg(feature = "semantic")]
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// An embedding backend: turns a batch of texts into one L2-normalized
/// vector each. [`Embedder`] is the real `fastembed`-backed implementation;
/// tests inject a deterministic in-memory fake behind this trait so the
/// engine's index/query/rebuild logic can be exercised without a multi-GB
/// model download (see `Engine::open_with_embedder`).
///
/// `Send + Sync`: [`Engine`](crate::engine::Engine) is held behind an
/// `Arc<Mutex<_>>` and driven from `tokio::task::spawn_blocking` by the
/// `onebrain mcp` server, so its boxed embedder must be safely shareable
/// across threads. Both real implementors (`Embedder`, backed by
/// `fastembed::TextEmbedding` behind a `Mutex`) and test fakes already
/// satisfy this.
pub trait Embed: Send + Sync {
    /// Embed a batch of texts, returning one vector per input text in the
    /// same order. Implementations must return L2-normalized vectors of
    /// length [`Embed::dims`].
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Embedding vector dimensionality (the vector store is opened at this
    /// width).
    fn dims(&self) -> usize;

    /// Embed DOCUMENT/passage texts. Default: same as [`Embed::embed`]
    /// (models that need no instruction prefix, and test fakes). The real
    /// [`Embedder`] prepends the model's `passage_prefix`.
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed(texts)
    }

    /// Embed a QUERY text. Default: same as [`Embed::embed`]. The real
    /// [`Embedder`] prepends the model's `query_prefix`.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed(&[text.to_string()])?.remove(0))
    }
}

/// Wraps a loaded `fastembed` text embedding model. Only compiled with the
/// `semantic` feature — lex-only builds have no ONNX runtime to back it.
#[cfg(feature = "semantic")]
pub struct Embedder {
    model: Mutex<TextEmbedding>,
    pub dims: usize,
    pub model_name: String,
    query_prefix: &'static str,
    passage_prefix: &'static str,
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
    /// Minimum cosine similarity for a vector hit to count as a real match
    /// (measured per model family; `None` = no floor). Below it, hits are
    /// noise — the nearest neighbors of a query about something the vault
    /// simply doesn't contain. e5-family: unrelated text clusters ≈0.84,
    /// related ≥0.87, so 0.85 splits them with margin.
    pub vec_floor: Option<f32>,
    /// Prefix prepended to QUERY text before embedding (e.g. `"query: "`
    /// for the e5 family, which was trained with instruction prefixes —
    /// omitting them measurably degrades retrieval). Empty = none.
    pub query_prefix: &'static str,
    /// Prefix prepended to DOCUMENT/passage text before embedding
    /// (`"passage: "` for e5). Only used at embed time — stored chunk text
    /// stays raw. Empty = none.
    pub passage_prefix: &'static str,
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
            vec_floor: Some(0.85),
            query_prefix: "query: ",
            passage_prefix: "passage: ",
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
            vec_floor: Some(0.85),
            query_prefix: "query: ",
            passage_prefix: "passage: ",
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
            vec_floor: Some(0.85),
            query_prefix: "query: ",
            passage_prefix: "passage: ",
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
            vec_floor: None,
            query_prefix: "",
            passage_prefix: "",
            hf_repo: "BAAI/bge-m3",
        },
        // NOTE: the two embeddinggemma variants share one HF repo (and thus
        // one `models--onnx-community--embeddinggemma-300m-ONNX` cache dir) —
        // `model_download_status` can't tell them apart, so their
        // downloaded/disk stats conflate: downloading either marks both as
        // downloaded, and the reported disk size is the dir total.
        ModelInfo {
            name: "embeddinggemma-300m-q",
            dims: 768,
            // Real download: model_quantized.onnx_data = 308.9 MB.
            approx_size: "~310 MB",
            approx_bytes: 310_000_000,
            context: 2048,
            thai_miracl: None,
            note: "small · int8 · Thai unverified",
            vec_floor: None,
            query_prefix: "task: search result | query: ",
            passage_prefix: "title: none | text: ",
            hf_repo: "onnx-community/embeddinggemma-300m-ONNX",
        },
        ModelInfo {
            name: "embeddinggemma-300m-q4",
            dims: 768,
            // Real download: model_q4.onnx_data = 196.7 MB.
            approx_size: "~200 MB",
            approx_bytes: 200_000_000,
            context: 2048,
            thai_miracl: None,
            note: "smallest · 4-bit · Thai unverified",
            vec_floor: None,
            query_prefix: "task: search result | query: ",
            passage_prefix: "title: none | text: ",
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

#[cfg(feature = "semantic")]
fn resolve_model(model_name: &str) -> Result<EmbeddingModel> {
    match model_name {
        "bge-m3" => Ok(EmbeddingModel::BGEM3),
        "multilingual-e5-large" => Ok(EmbeddingModel::MultilingualE5Large),
        "multilingual-e5-base" => Ok(EmbeddingModel::MultilingualE5Base),
        "multilingual-e5-small" => Ok(EmbeddingModel::MultilingualE5Small),
        "embeddinggemma-300m-q" => Ok(EmbeddingModel::EmbeddingGemma300MQ),
        "embeddinggemma-300m-q4" => Ok(EmbeddingModel::EmbeddingGemma300MQ4),
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
#[cfg(feature = "semantic")]
pub fn new(model_name: &str, cache_dir: &Path) -> Result<Embedder> {
    new_with_progress(model_name, cache_dir, true)
}

/// Same as [`new`] but SILENT: fastembed's stdout download bar is disabled.
/// Used by the interactive model TUI, which runs the terminal in raw mode and
/// draws its own in-table progress — a stray stdout print would corrupt it.
#[cfg(feature = "semantic")]
pub fn new_quiet(model_name: &str, cache_dir: &Path) -> Result<Embedder> {
    new_with_progress(model_name, cache_dir, false)
}

#[cfg(feature = "semantic")]
fn new_with_progress(model_name: &str, cache_dir: &Path, show_progress: bool) -> Result<Embedder> {
    let model = resolve_model(model_name)?;
    let dims = model_dims(model_name);

    let init = InitOptions::new(model)
        .with_cache_dir(cache_dir.to_path_buf())
        .with_show_download_progress(show_progress);
    let embedding = TextEmbedding::try_new(init)?;

    let info = model_registry()
        .iter()
        .find(|m| m.name == model_name)
        .expect("resolve_model already validated the name");
    Ok(Embedder {
        model: Mutex::new(embedding),
        dims,
        model_name: model_name.to_string(),
        query_prefix: info.query_prefix,
        passage_prefix: info.passage_prefix,
    })
}

/// L2-normalize a single vector in place. No-op on a zero vector (left
/// as-is to avoid dividing by zero).
#[cfg(feature = "semantic")]
fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(feature = "semantic")]
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

#[cfg(feature = "semantic")]
impl Embed for Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Embedder::embed(self, texts)
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if self.passage_prefix.is_empty() {
            return self.embed(texts);
        }
        let prefixed: Vec<String> = texts
            .iter()
            .map(|t| format!("{}{t}", self.passage_prefix))
            .collect();
        self.embed(&prefixed)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let q = if self.query_prefix.is_empty() {
            text.to_string()
        } else {
            format!("{}{text}", self.query_prefix)
        };
        Ok(self.embed(&[q])?.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e5_family_declares_instruction_prefixes() {
        for m in model_registry() {
            if m.name.starts_with("multilingual-e5") {
                assert_eq!(m.query_prefix, "query: ", "{}", m.name);
                assert_eq!(m.passage_prefix, "passage: ", "{}", m.name);
            }
        }
        // bge-m3 needs no prefixes.
        let bge = model_registry()
            .iter()
            .find(|m| m.name == "bge-m3")
            .unwrap();
        assert!(bge.query_prefix.is_empty() && bge.passage_prefix.is_empty());
    }

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
    fn registry_has_exactly_six_seeded_models() {
        let names: Vec<&str> = model_registry().iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            vec![
                "multilingual-e5-small",
                "multilingual-e5-base",
                "multilingual-e5-large",
                "bge-m3",
                "embeddinggemma-300m-q",
                "embeddinggemma-300m-q4",
            ]
        );
    }

    #[test]
    fn gemma_variants_share_one_hf_repo_and_cache_dir() {
        // Both quantizations download from the same repo, so their
        // downloaded/disk stats conflate (documented on the registry entries).
        let q = info("embeddinggemma-300m-q");
        let q4 = info("embeddinggemma-300m-q4");
        assert_eq!(q.hf_repo, q4.hf_repo);
        assert_eq!(q.cache_dir_name(), q4.cache_dir_name());
        // Same instruction prefixes across the family.
        assert_eq!(q.query_prefix, q4.query_prefix);
        assert_eq!(q.passage_prefix, q4.passage_prefix);
        // q4 is the smaller download.
        assert!(q4.approx_bytes < q.approx_bytes);
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

    #[cfg(feature = "semantic")]
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

    #[cfg(feature = "semantic")]
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

    #[cfg(feature = "semantic")]
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
