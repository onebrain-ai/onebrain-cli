//! The JSON + SSE API mounted under `/api`.
//!
//! Every handler is a thin HTTP veneer over the CLI's existing vault primitives —
//! `onebrain_core::load_vault_config_at` for config, `onebrain_fs::note::*` for
//! note read/write/delete/move + folder create/delete, `onebrain_fs::task::scan_tasks`
//! for the task scan, a `walkdir` walk for the tree. No vault logic is
//! re-implemented here; the daemon owns only HTTP concerns — path-safety against
//! the attacker model, size caps, error→status mapping, and auth.
//!
//! | route                            | returns / does                              |
//! |----------------------------------|---------------------------------------------|
//! | `GET    /api/config`             | parsed `onebrain.yml` as JSON               |
//! | `GET    /api/vault/tree`         | `{ root, entries: [{path,name,kind}] }`     |
//! | `GET    /api/vault/file?path=`   | `{ path, content, rev }`                     |
//! | `POST   /api/vault/file?path=`   | create a note → `{ path, rev }`             |
//! | `PUT    /api/vault/file?path=`   | overwrite a note (conflict-checked on `rev`)|
//! | `DELETE /api/vault/file?path=`   | move a note to `.trash`                      |
//! | `GET    /api/vault/raw?path=`    | raw bytes + content-type guessed from ext   |
//! | `POST   /api/vault/upload?path=` | write raw uploaded bytes                     |
//! | `POST   /api/vault/move`         | rename a note + rewrite incoming wikilinks  |
//! | `POST   /api/vault/folder?path=` | create a folder                              |
//! | `DELETE /api/vault/folder?path=` | delete a folder                              |
//! | `GET    /api/vault/tasks`        | dated Obsidian-Tasks lines across the vault |
//! | `POST   /api/chat`               | SSE stream over a `claude -p` agent turn    |
//!
//! `rev` is a cheap revision tag (mtime in whole nanoseconds since the epoch)
//! for conflict detection on `PUT`.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::AppState;
/// Directory names pruned from the tree walk — the SAME set the `note` commands
/// use, so the API surface and the CLI agree on what counts as "the vault".
use onebrain_fs::note::{is_tooling_dir, to_slash, TOOLING_DIRS};

/// Build the `/api` sub-router. The auth layer AND the shared state are
/// attached by the caller (`build_router`), so this stays a pure route table:
/// it returns a `Router<Arc<AppState>>` (state still needed) and never calls
/// `.with_state` itself — `build_router` applies state once for the whole tree.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/config", get(get_config))
        .route("/vault/tree", get(get_vault_tree))
        .route(
            "/vault/file",
            get(get_vault_file)
                .post(post_vault_file)
                .put(put_vault_file)
                .delete(delete_vault_file),
        )
        // Raw bytes (images, PDFs) with a content-type guessed from the extension —
        // the editor fetches this for in-app preview of non-text files.
        .route("/vault/raw", get(get_vault_raw))
        .route("/vault/move", post(post_vault_move))
        .route(
            "/vault/folder",
            post(post_vault_folder).delete(delete_vault_folder),
        )
        // v3.3 — scan vault notes for Obsidian-Tasks lines (real Tasks panel).
        .route("/vault/tasks", get(get_vault_tasks))
        // v3.3 chat — SSE stream over a `claude -p` agent turn. Inherits the auth
        // middleware + body limit applied to this whole sub-router.
        .route("/chat", post(super::chat::post_chat))
        // Binary upload (chat attachments: images / files) — raw bytes → vault.
        .route("/vault/upload", post(post_vault_upload))
        // Body limit sized for the largest write (a binary upload, MAX_RAW_BYTES).
        // GET routes have no body so this is a no-op there; note writes stay well
        // under it.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_RAW_BYTES as usize))
}

// ─────────────────────────────────────────────────────────────────────────
// Error type — every handler returns `Result<Json<T>, ApiError>` and the
// `IntoResponse` impl turns the variant into the right HTTP status + a small
// JSON `{ "error": "<msg>" }` body.
// ─────────────────────────────────────────────────────────────────────────

/// API-layer error. Each variant maps to one HTTP status. Every message is a
/// CURATED, client-safe string — we never forward a raw `std::io::Error`
/// Display (which can leak host paths / errno detail) to the wire.
#[derive(Debug)]
pub(crate) enum ApiError {
    /// 400 — the request itself is malformed (e.g. a path that escapes the vault).
    BadRequest(String),
    /// 404 — the requested resource doesn't exist.
    NotFound(String),
    /// 413 — the resource exists but is too large to serve (size cap, fix F).
    PayloadTooLarge(String),
    /// 422 — the resource exists and the request is well-formed, but the content
    /// can't be processed as requested (e.g. a file that isn't valid UTF-8 text).
    Unprocessable(String),
    /// 503 — no vault is bound, so the vault endpoints have nothing to serve.
    /// Distinct from 404/500 so a client can tell "this daemon has no vault"
    /// apart from "this file is missing" / "the server broke".
    ServiceUnavailable(String),
    /// 500 — an unexpected server-side failure (I/O, parse). The message is a
    /// short curated label; the detailed error is logged server-side, not
    /// returned, so an errno/path never leaks to the client.
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m),
            ApiError::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            ApiError::ServiceUnavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

impl From<onebrain_fs::FsError> for ApiError {
    fn from(e: onebrain_fs::FsError) -> Self {
        match e {
            onebrain_fs::FsError::Core(c) => ApiError::BadRequest(c.to_string()),
            onebrain_fs::FsError::Io { .. } => ApiError::Internal("filesystem error".into()),
        }
    }
}

/// Error mapping for the delete endpoints. The handlers pre-check existence (404)
/// and type (400) before calling core, so a `CoreError::InvalidTarget` reaching
/// here means the target VANISHED in the TOCTOU window between the check and the
/// unlink — that reads as 404 Not Found, not a 400 Bad Request.
fn deletion_error(e: onebrain_fs::FsError) -> ApiError {
    match e {
        onebrain_fs::FsError::Core(onebrain_core::CoreError::InvalidTarget(_)) => {
            ApiError::NotFound("target no longer exists".into())
        }
        other => other.into(),
    }
}

/// Maximum size of a note we will read into memory and return. A vault note is
/// prose/markdown — kilobytes, occasionally a few hundred. 10 MB is a generous
/// cap that still refuses a pathological/accidental huge file before it can
/// balloon the daemon's memory. Over the cap → 413 (fix F).
const MAX_NOTE_BYTES: u64 = 10 * 1024 * 1024;

/// Reject writes whose path touches a tooling directory the tree hides
/// (`.git`, `.obsidian`, `.claude`, `.trash`, `node_modules`) at ANY level.
/// Checks every `Normal` component (so a leading `./` or nesting can't bypass)
/// and compares case-insensitively (case-insensitive filesystems resolve
/// `.GIT` to `.git`). `..`/absolute components are already rejected downstream.
fn reject_tooling_path(rel: &str) -> Result<(), ApiError> {
    for comp in std::path::Path::new(rel).components() {
        if let std::path::Component::Normal(name) = comp {
            if let Some(s) = name.to_str() {
                if TOOLING_DIRS.iter().any(|t| t.eq_ignore_ascii_case(s)) {
                    return Err(ApiError::BadRequest(format!(
                        "cannot write to a tooling directory: {s}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Pull the bound vault root out of the shared state, or return 503 when no
/// vault is bound. Centralises the guard the three vault handlers share so the
/// "never serve `/` as a fallback vault" rule lives in exactly one place.
fn require_vault_root(state: &AppState) -> Result<&Path, ApiError> {
    state.vault_root.as_deref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "no vault bound — set ONEBRAIN_VAULT to a OneBrain vault".to_string(),
        )
    })
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/config
// ─────────────────────────────────────────────────────────────────────────

/// Return the parsed `onebrain.yml` (or legacy `vault.yml`) as JSON.
///
/// Reuses `load_vault_config_at`, which applies the same dual-read + default
/// semantics the rest of the CLI uses — so the API never drifts from the
/// canonical config shape.
///
/// Error mapping mirrors `session_init.rs`'s distinction (fix D): a MISSING
/// config is a clean 404, MALFORMED YAML is a 400, anything else is a 500. We
/// never forward the raw `CoreError` Display to the client.
async fn get_config(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?;
    let cfg = onebrain_core::load_vault_config_at(root).map_err(|e| match e {
        // Config file absent → the vault has no config to return.
        onebrain_core::CoreError::VaultYamlMissing { .. } => {
            ApiError::NotFound("vault config not found".to_string())
        }
        // YAML present but unparseable → the request is fine, the file is bad.
        onebrain_core::CoreError::InvalidYaml(_) => {
            ApiError::BadRequest("vault config is not valid YAML".to_string())
        }
        // Anything else (EACCES, etc.) is a genuine server-side failure. Log the
        // detail; return only a generic label so no path/errno leaks.
        other => {
            tracing::warn!(error = %other, "load vault config failed");
            ApiError::Internal("could not load vault config".to_string())
        }
    })?;
    // `VaultConfig` derives `Serialize`, so it round-trips straight to JSON.
    Ok(Json(cfg).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/tree
// ─────────────────────────────────────────────────────────────────────────

/// One entry in the tree listing. `kind` is `"file"` or `"dir"`; `path` is
/// RELATIVE to the vault root, slash-separated (stable across OSes).
#[derive(Debug, Serialize)]
struct TreeEntry {
    path: String,
    name: String,
    kind: &'static str,
}

/// `GET /api/vault/tree` response body.
#[derive(Debug, Serialize)]
struct TreeResponse {
    /// Absolute vault root (display string) for the client's reference.
    root: String,
    entries: Vec<TreeEntry>,
}

/// Recursively list the vault's folders + files, skipping the tooling dirs.
///
/// Paths are returned relative to the vault root so the client never sees the
/// host's absolute layout. Entries are sorted (dirs and files interleaved by
/// path) for a stable, testable order.
///
/// The `walkdir` traversal is synchronous filesystem work, so it runs inside
/// `tokio::task::spawn_blocking` (fix C) — keeping it off the async worker
/// threads where it would stall other requests. The closure owns a cloned
/// `PathBuf` so it needs no borrow across the `.await`.
//
// step 2b: the tree is currently UNBOUNDED — a huge vault produces one giant
//          flat `Vec<TreeEntry>`. Step 2b should paginate / lazy-load children
//          and reshape the DTO into a nested (vs. flat) tree the SPA can expand
//          on demand. Deferred here to keep step 2 a thin, faithful listing.
async fn get_vault_tree(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();

    // Off-runtime blocking walk. `spawn_blocking` returns a `JoinError` only if
    // the closure panics; we map that to a 500 (the walk itself never panics).
    let response = tokio::task::spawn_blocking(move || walk_tree(&root))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "tree walk task failed");
            ApiError::Internal("could not list vault".to_string())
        })?;

    Ok(Json(response).into_response())
}

/// Pure, synchronous tree walk. Lives outside the async handler so it can run
/// under `spawn_blocking` (and be reasoned about / tested in isolation).
fn walk_tree(root: &Path) -> TreeResponse {
    let mut entries: Vec<TreeEntry> = Vec::new();

    let walker = walkdir::WalkDir::new(root)
        .min_depth(1) // skip the root itself
        .into_iter()
        .filter_entry(|e| !is_tooling_dir(e));

    for entry in walker {
        // Best-effort: a transient stat/permission error on one entry skips it
        // rather than failing the whole listing.
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue, // can't happen under a WalkDir rooted at `root`
        };
        let kind = if entry.file_type().is_dir() {
            "dir"
        } else {
            "file"
        };
        entries.push(TreeEntry {
            path: to_slash(rel),
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));

    TreeResponse {
        root: root.display().to_string(),
        entries,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/tasks — real Obsidian-Tasks scan
// ─────────────────────────────────────────────────────────────────────────

/// `{ "tasks": [...] }` envelope. Each task is an [`onebrain_fs::task::TaskHit`]
/// (file / line / text / done / due) — the shared core scan, so the daemon and a
/// future `onebrain task list` CLI verb agree on one regex + walk.
#[derive(Debug, Serialize)]
struct TasksResponse {
    tasks: Vec<onebrain_fs::task::TaskHit>,
}

/// `GET /api/vault/tasks` — walk the vault's `.md` notes and return every dated
/// Obsidian-Tasks checkbox line via the shared [`onebrain_fs::task::scan_tasks`]
/// core. Read-only; the blocking walk runs off the async worker threads.
async fn get_vault_tasks(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let tasks = tokio::task::spawn_blocking(move || {
        // Scan only the configured project + area folders (real, actionable
        // todos) — not docs/READMEs, inbox, knowledge, etc. Read the folder
        // names from the vault config so a customised layout still works; fall
        // back to the default allowlist (01-projects/ + 02-areas/) if config
        // can't be read.
        let opts = match onebrain_core::load_vault_config_at(&root) {
            Ok(cfg) => onebrain_fs::task::TaskScanOptions {
                // trim_end_matches('/') so a config value written with a trailing
                // slash (e.g. `projects: 01-projects/`) doesn't become a `//`
                // prefix that matches no note → silently zero tasks.
                include_prefixes: vec![
                    format!("{}/", cfg.folders.projects.trim_end_matches('/')),
                    format!("{}/", cfg.folders.areas.trim_end_matches('/')),
                ],
                ..Default::default()
            },
            Err(_) => onebrain_fs::task::TaskScanOptions::default(),
        };
        onebrain_fs::task::scan_tasks(&root, &opts)
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "task scan task failed");
        ApiError::Internal("could not scan tasks".to_string())
    })?;
    Ok(Json(TasksResponse { tasks }).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/file?path=<rel>
// ─────────────────────────────────────────────────────────────────────────

/// Query string for `GET /api/vault/file` and `/api/vault/raw`.
#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
    /// When present (`&download=1`), `/api/vault/raw` adds a `Content-Disposition:
    /// attachment` carrying the file's real name, so a save keeps the original
    /// filename + extension instead of the blob-URL id some webviews fall back to.
    #[serde(default)]
    download: Option<String>,
}

/// `GET /api/vault/file` response body.
#[derive(Debug, Serialize)]
struct FileResponse {
    /// Vault-relative, slash-separated path (echoes the validated request).
    path: String,
    content: String,
    /// Cheap revision tag for future conflict detection: file mtime in whole
    /// nanoseconds since the Unix epoch. Stringified so a JS client doesn't
    /// lose precision on a value that overflows `Number.MAX_SAFE_INTEGER`.
    rev: String,
}

/// Read one note's content + revision tag.
///
/// **Security (must-have):** the requested `path` is validated to stay inside
/// the vault before any read. [`resolve_in_vault`] canonicalises both the vault
/// root and the target and rejects anything that escapes (`..` traversal,
/// absolute paths, symlinks pointing out). A bad/escaping path → 400; a
/// well-formed but missing path → 404.
///
/// The path-traversal guard, the size-cap stat, and the file read are ALL
/// performed inside a single `spawn_blocking` closure (fix C). Keeping the
/// security check and the read in one blocking unit means the canonicalised
/// path is never split across an `.await` — there's no window where the checked
/// path and the read could diverge, and no blocking syscall runs on an async
/// worker thread.
async fn get_vault_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let requested = q.path;

    let response = tokio::task::spawn_blocking(move || read_vault_file(&root, &requested))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "file read task failed");
            ApiError::Internal("could not read file".to_string())
        })??; // outer `?` = JoinError → 500; inner `?` = the handler's ApiError.

    Ok(Json(response).into_response())
}

/// Synchronous "validate → stat (size cap) → read" core for `get_vault_file`.
/// Runs under `spawn_blocking`. Returns the typed `ApiError` so the async
/// handler can hand it straight back.
fn read_vault_file(vault_root: &Path, requested: &str) -> Result<FileResponse, ApiError> {
    // Reject an empty path up front (fix D): without this `resolve_in_vault`
    // would join `""` and canonicalise to the vault ROOT (a directory), which we
    // also reject below — but a dedicated 400 here is clearer.
    if requested.is_empty() {
        return Err(ApiError::BadRequest("empty path".to_string()));
    }

    let safe = resolve_in_vault(vault_root, requested)?;

    // The target must be a regular file, not a directory (fix D). `resolve_in_vault`
    // already canonicalised it (so it exists); a directory here → 400.
    let meta = std::fs::metadata(&safe).map_err(|e| {
        // The path canonicalised a moment ago, so a metadata failure is a real
        // server-side I/O fault, not a missing file. Log detail, return generic.
        tracing::warn!(error = %e, "stat resolved vault file failed");
        ApiError::Internal("could not stat file".to_string())
    })?;
    if meta.is_dir() {
        return Err(ApiError::BadRequest("not a file".to_string()));
    }

    // Size cap (fix F): refuse a pathologically large file BEFORE reading it
    // into memory.
    if meta.len() > MAX_NOTE_BYTES {
        return Err(ApiError::PayloadTooLarge("file too large".to_string()));
    }

    // Read content. A missing file is a clean 404 (it could have been removed
    // between canonicalize and read — a benign race). A non-UTF8 file surfaces
    // as `InvalidData` from `read_to_string` → 422 (the file exists and the
    // request is fine; we just can't return it as text). Any other I/O error is
    // a 500 with a generic message (no raw errno to the client).
    let content = match std::fs::read_to_string(&safe) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::NotFound("no such file".to_string()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(ApiError::Unprocessable(
                "file is not valid UTF-8 text".to_string(),
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "read vault file failed");
            return Err(ApiError::Internal("could not read file".to_string()));
        }
    };

    let rev = mtime_nanos(&safe)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".to_string());

    // Echo the normalised, slash-separated relative path (not the host abs path).
    let rel = safe
        .strip_prefix(vault_root)
        .map(to_slash)
        .unwrap_or_else(|_| requested.to_string());

    Ok(FileResponse {
        path: rel,
        content,
        rev,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/raw?path=<rel> — raw bytes for binary preview (images, PDFs)
// ─────────────────────────────────────────────────────────────────────────

/// Larger cap than notes: images / PDFs are bigger than prose. Still bounded so a
/// pathological file can't balloon the daemon's memory.
const MAX_RAW_BYTES: u64 = 50 * 1024 * 1024;

/// Guess a `Content-Type` from the file extension (preview-relevant types only;
/// everything else is served as a download-y octet-stream).
fn content_type_for(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        // audio / video — played by the web UI's native <audio>/<video> element
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/mp4",
        "oga" | "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

/// Build a `Content-Disposition: attachment` value that survives spaces and
/// non-ASCII (e.g. Thai) filenames: a quoted ASCII fallback plus an RFC 5987
/// `filename*` carrying the UTF-8 name percent-encoded. Browsers prefer
/// `filename*`, so the saved file keeps its real name + extension.
fn content_disposition_attachment(path: &str) -> String {
    let raw = path.rsplit('/').next().unwrap_or("");
    let name = if raw.is_empty() { "download" } else { raw };
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' && !c.is_control() {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut enc = String::new();
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            enc.push(b as char);
        } else {
            enc.push('%');
            enc.push_str(&format!("{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

/// Serve a vault file's RAW bytes (same path-traversal guard + size cap as the
/// text read, but no UTF-8 requirement) with a content-type for the browser to
/// render. Used by the editor's image/PDF preview and, with `&download=1`, its
/// download button.
async fn get_vault_raw(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let requested = q.path;
    let ctype = content_type_for(&requested);
    // Build the attachment header before `requested` is moved into the read task.
    let disposition = q
        .download
        .as_deref()
        .filter(|v| !v.is_empty() && *v != "0")
        .map(|_| content_disposition_attachment(&requested));
    let bytes = tokio::task::spawn_blocking(move || read_vault_raw(&root, &requested))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "raw read task failed");
            ApiError::Internal("could not read file".to_string())
        })??;
    let total = bytes.len() as u64;
    use axum::http::header::{ACCEPT_RANGES, CONTENT_RANGE, CONTENT_TYPE, RANGE};
    let ct = axum::http::HeaderValue::from_static(ctype);

    // Honour a Range request for inline media — audio/video seeking, and Safari,
    // which refuses to play a 200 full-body. Never for an explicit download.
    if disposition.is_none() {
        if let Some((start, end)) = headers
            .get(RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|r| parse_byte_range(r, total))
        {
            let slice = bytes[start as usize..=end as usize].to_vec();
            let mut resp = Response::new(axum::body::Body::from(slice));
            *resp.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
            let h = resp.headers_mut();
            h.insert(CONTENT_TYPE, ct);
            h.insert(ACCEPT_RANGES, axum::http::HeaderValue::from_static("bytes"));
            if let Ok(v) =
                axum::http::HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
            {
                h.insert(CONTENT_RANGE, v);
            }
            return Ok(resp);
        }
    }

    let mut resp = Response::new(axum::body::Body::from(bytes));
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, ct);
    // advertise range support so the browser knows it can seek media
    h.insert(ACCEPT_RANGES, axum::http::HeaderValue::from_static("bytes"));
    if let Some(cd) = disposition {
        if let Ok(v) = axum::http::HeaderValue::from_str(&cd) {
            h.insert(axum::http::header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(resp)
}

/// Parse a single-range `Range: bytes=start-end` header into inclusive `[start, end]`
/// byte offsets clamped to `total`. Returns `None` for malformed, multi-range, or
/// unsatisfiable requests — the caller then serves the full `200` body.
fn parse_byte_range(header: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range not supported
    }
    let (s, e) = spec.split_once('-')?;
    let (start, end) = match (s.trim(), e.trim()) {
        ("", "") => return None,
        ("", suffix) => {
            // `bytes=-N` → final N bytes
            let n: u64 = suffix.parse().ok()?;
            if n == 0 {
                return None;
            }
            (total.saturating_sub(n), total - 1)
        }
        (start, "") => (start.parse().ok()?, total - 1),
        (start, end) => (start.parse().ok()?, end.parse::<u64>().ok()?.min(total - 1)),
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

/// Validate → stat (size cap) → read raw bytes. Runs under `spawn_blocking`.
fn read_vault_raw(vault_root: &Path, requested: &str) -> Result<Vec<u8>, ApiError> {
    if requested.is_empty() {
        return Err(ApiError::BadRequest("empty path".to_string()));
    }
    let safe = resolve_in_vault(vault_root, requested)?;
    let meta = std::fs::metadata(&safe).map_err(|e| {
        tracing::warn!(error = %e, "stat raw vault file failed");
        ApiError::Internal("could not stat file".to_string())
    })?;
    if meta.is_dir() {
        return Err(ApiError::BadRequest("not a file".to_string()));
    }
    if meta.len() > MAX_RAW_BYTES {
        return Err(ApiError::PayloadTooLarge("file too large".to_string()));
    }
    std::fs::read(&safe).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound("no such file".to_string()),
        _ => {
            tracing::warn!(error = %e, "read raw vault file failed");
            ApiError::Internal("could not read file".to_string())
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/upload?path=<rel> — write raw bytes (chat attachment)
// ─────────────────────────────────────────────────────────────────────────

/// Write raw bytes (an image/file attached in the chat) to a vault path,
/// creating parent directories. Returns 201 `{ path }`. Same path-traversal +
/// tooling-dir guards as the note writes; size-capped at MAX_RAW_BYTES.
async fn post_vault_upload(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    reject_tooling_path(&q.path)?;
    let root = require_vault_root(&state)?.to_path_buf();
    let safe = resolve_in_vault_create(&root, &q.path)?;
    if body.len() as u64 > MAX_RAW_BYTES {
        return Err(ApiError::PayloadTooLarge("file too large".to_string()));
    }
    let rel = q.path.clone();
    let bytes = body.to_vec();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(parent) = safe.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&safe, &bytes)
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "upload task failed");
        ApiError::Internal("upload task failed".to_string())
    })?
    .map_err(|e| {
        tracing::warn!(error = %e, "write upload failed");
        ApiError::Internal("could not write file".to_string())
    })?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "path": rel })),
    )
        .into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/file?path=<rel>
// ─────────────────────────────────────────────────────────────────────────

/// `POST /api/vault/file` response body.
#[derive(Debug, Serialize)]
struct WriteResponse {
    path: String,
    rev: String,
}

/// Create a new note at the given vault-relative path with the raw markdown body.
///
/// Returns 201 on success. Returns 409 if the file already exists — callers
/// must read the current content and supply a `rev` before overwriting (that
/// path is a future `PUT`). The `String` body extractor must be the last
/// parameter so axum doesn't try to pull it from the state/query extractors.
async fn post_vault_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
    body: String,
) -> Result<Response, ApiError> {
    reject_tooling_path(&q.path)?;
    let root = require_vault_root(&state)?.to_path_buf();
    let safe = resolve_in_vault_create(&root, &q.path)?;
    // This existence check races a concurrent POST to the same path (TOCTOU):
    // `write_note` is an atomic temp+rename but not create-exclusive, so two
    // simultaneous creates could both pass here and one would overwrite the
    // other. Accepted for the single-tenant localhost daemon (one user, one
    // token); a multi-writer model would need an O_EXCL create path in core.
    if safe.exists() {
        return Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "file already exists" })),
        )
            .into_response());
    }
    let rel = safe
        .strip_prefix(&root)
        .unwrap_or(Path::new(&q.path))
        .to_path_buf();
    let res =
        tokio::task::spawn_blocking(move || onebrain_fs::note::write_note(&root, &rel, &body))
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "write task failed");
                ApiError::Internal("write task failed".into())
            })??;
    let rev = mtime_nanos(&safe)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".into());
    Ok((
        StatusCode::CREATED,
        Json(WriteResponse {
            path: res.path,
            rev,
        }),
    )
        .into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// PUT /api/vault/file?path=<rel>
// ─────────────────────────────────────────────────────────────────────────

/// Update an existing note at the given vault-relative path.
///
/// Requires the file to EXIST — returns 404 via [`resolve_in_vault`] if
/// missing. The client must supply `If-Match: <rev>` (the mtime-nanos string
/// returned by the GET) or `If-Match: *` (unconditional overwrite). A stale
/// rev returns 409 with the current rev so the client can merge and retry.
/// On success returns 200 with the new rev.
async fn put_vault_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    reject_tooling_path(&q.path)?;
    let root = require_vault_root(&state)?.to_path_buf();
    // resolve_in_vault canonicalises + 404s on missing — exactly what PUT needs.
    let safe = resolve_in_vault(&root, &q.path)?;

    let current = mtime_nanos(&safe)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".into());

    let if_match = headers
        .get("If-Match")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if let Some(expected) = &if_match {
        if expected != "*" && expected != &current {
            // This 409 deliberately carries a `rev` field beyond the standard
            // `{ error }` envelope — the client (webui autosaver) needs the
            // server's CURRENT rev to offer reload/overwrite and retry the save.
            return Ok((
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "rev mismatch", "rev": current })),
            )
                .into_response());
        }
    }

    let rel = safe
        .strip_prefix(&root)
        .unwrap_or(Path::new(&q.path))
        .to_path_buf();
    let root2 = root.clone();
    tokio::task::spawn_blocking(move || onebrain_fs::note::write_note(&root2, &rel, &body))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "write task failed");
            ApiError::Internal("write task failed".into())
        })??;

    let rev = mtime_nanos(&safe)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".into());
    let path = safe
        .strip_prefix(&root)
        .map(to_slash)
        .unwrap_or_else(|_| q.path.clone());

    Ok((StatusCode::OK, Json(WriteResponse { path, rev })).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// DELETE /api/vault/file?path=<rel>
// ─────────────────────────────────────────────────────────────────────────

/// `DELETE /api/vault/file` response body.
#[derive(Debug, Serialize)]
struct TrashResponse {
    path: String,
    trashed_to: String,
}

/// Move an existing note into `<vault>/.trash/`, preserving its relative path.
///
/// Requires the file to exist — returns 404 via [`resolve_in_vault`] if
/// missing. No request body. On success returns 200 with `{ path, trashed_to }`.
async fn delete_vault_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    reject_tooling_path(&q.path)?;
    let root = require_vault_root(&state)?.to_path_buf();
    let safe = resolve_in_vault(&root, &q.path)?; // 404 if missing
                                                  // FIX 3: a directory must not be trashed via the FILE endpoint.
    if safe.is_dir() {
        return Err(ApiError::BadRequest(
            "not a file; use the folder endpoint".into(),
        ));
    }
    let rel = Path::new(&q.path).to_path_buf();
    let res = tokio::task::spawn_blocking(move || onebrain_fs::note::delete_note(&root, &rel))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "delete task failed");
            ApiError::Internal("delete task failed".into())
        })?
        .map_err(deletion_error)?;
    Ok(Json(TrashResponse {
        path: res.path,
        trashed_to: res.trashed_to,
    })
    .into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/move
// ─────────────────────────────────────────────────────────────────────────

/// `POST /api/vault/move` request body.
#[derive(Debug, Deserialize)]
struct MoveRequest {
    from: String,
    to: String,
}

/// Rename/move a note and rewrite every incoming wikilink that referenced the
/// old basename.
///
/// - `from` must exist (validated via [`resolve_in_vault`] → 404 if absent).
/// - `to` must be a safe, vault-relative path that does not yet exist (→ 409
///   if it does).
/// - On success returns 200 with the [`onebrain_fs::note::MoveResult`] JSON,
///   which includes `from`, `to`, `links_rewritten`, `files_updated`, and
///   `updated_files`.
async fn post_vault_move(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MoveRequest>,
) -> Result<Response, ApiError> {
    reject_tooling_path(&req.from)?;
    reject_tooling_path(&req.to)?;
    let root = require_vault_root(&state)?.to_path_buf();
    let _from_safe = resolve_in_vault(&root, &req.from)?; // 404 if missing
    let to_safe = resolve_in_vault_create(&root, &req.to)?; // safe, may not exist
                                                            // Pre-check covers the common case (409). A concurrent create of `to` in the
                                                            // window before `move_note` is an accepted TOCTOU edge on the single-tenant
                                                            // localhost daemon — `move_note` then surfaces it via the default mapping.
    if to_safe.exists() {
        return Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "destination exists" })),
        )
            .into_response());
    }
    let from = Path::new(&req.from).to_path_buf();
    let to = Path::new(&req.to).to_path_buf();
    let res = tokio::task::spawn_blocking(move || {
        onebrain_fs::note::move_note(&root, &from, &to, true, false)
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "move task failed");
        ApiError::Internal("move task failed".into())
    })??;
    Ok(Json(res).into_response()) // MoveResult is Serialize
}

// ─────────────────────────────────────────────────────────────────────────
// POST /api/vault/folder?path=<rel>
// DELETE /api/vault/folder?path=<rel>
// ─────────────────────────────────────────────────────────────────────────

/// Create a new folder at the given vault-relative path.
///
/// Returns 201 on success with `{ path }`. Returns 409 if the folder already
/// exists so the caller can treat create-on-existing-folder as an explicit
/// conflict rather than a silent no-op.
async fn post_vault_folder(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    reject_tooling_path(&q.path)?;
    let root = require_vault_root(&state)?.to_path_buf();
    let safe = resolve_in_vault_create(&root, &q.path)?;
    // Pre-check covers the common case (409). A concurrent create of the same
    // folder in the window before `create_folder` is an accepted TOCTOU edge on
    // the single-tenant localhost daemon (same tradeoff as the file create).
    if safe.exists() {
        return Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "folder already exists" })),
        )
            .into_response());
    }
    let rel = Path::new(&q.path).to_path_buf();
    let res = tokio::task::spawn_blocking(move || onebrain_fs::note::create_folder(&root, &rel))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "folder task failed");
            ApiError::Internal("folder task failed".into())
        })??;
    Ok((StatusCode::CREATED, Json(res)).into_response())
}

/// Move a folder into `<vault>/.trash/`, preserving its relative path.
///
/// Requires the folder to exist — returns 404 via [`resolve_in_vault`] if
/// missing. No request body. On success returns 200 with `{ path, trashed_to }`.
async fn delete_vault_folder(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    reject_tooling_path(&q.path)?;
    let root = require_vault_root(&state)?.to_path_buf();
    let safe = resolve_in_vault(&root, &q.path)?; // 404 if missing
                                                  // A file must not be trashed via the FOLDER endpoint (mirror of the file
                                                  // handler's guard) — so a later InvalidTarget means "vanished" → 404.
    if !safe.is_dir() {
        return Err(ApiError::BadRequest(
            "not a folder; use the file endpoint".into(),
        ));
    }
    let rel = Path::new(&q.path).to_path_buf();
    let res = tokio::task::spawn_blocking(move || onebrain_fs::note::delete_folder(&root, &rel))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "folder task failed");
            ApiError::Internal("folder task failed".into())
        })?
        .map_err(deletion_error)?;
    Ok(Json(res).into_response())
}

// ─────────────────────────────────────────────────────────────────────────
// Path-traversal guard — the security-critical core, kept pure + tested.
// ─────────────────────────────────────────────────────────────────────────

/// Validate that `rel` resolves to a path INSIDE `vault_root`, returning the
/// canonical absolute path on success.
///
/// Defence in depth, in order:
/// 1. Reject an absolute requested path outright (`/etc/passwd`) — the API only
///    ever serves vault-relative paths.
/// 2. Reject any `..` component lexically, BEFORE touching the filesystem, so a
///    traversal attempt can't even reach a `canonicalize` syscall.
/// 3. Canonicalize the vault root, then canonicalize the joined target and
///    confirm it is still prefixed by the canonical root. This step catches a
///    SYMLINK inside the vault that points outward (the lexical check in (2)
///    can't see through symlinks).
///
/// A missing target file makes `canonicalize` fail with `NotFound`; we map that
/// to a 404 here (not a 400) so a legitimate-but-absent path reads as "missing"
/// rather than "malformed".
fn resolve_in_vault(vault_root: &Path, rel: &str) -> Result<PathBuf, ApiError> {
    // (0) An interior NUL byte can never appear in a real filesystem path; a
    //     request carrying one is malformed/hostile. Reject it lexically (fix D)
    //     so it never reaches `canonicalize` (which would error with a confusing
    //     `InvalidInput` we'd otherwise have to map). This must come first,
    //     before `Path::new(rel)` is used to build any syscall argument.
    if rel.contains('\0') {
        return Err(ApiError::BadRequest(
            "path contains an interior NUL byte".to_string(),
        ));
    }

    let rel_path = Path::new(rel);

    // (1) Absolute paths are never valid here.
    if rel_path.is_absolute() {
        return Err(ApiError::BadRequest(format!(
            "path must be relative to the vault root: {rel}"
        )));
    }

    // (2) Lexical `..` / root-dir rejection — fail before any syscall.
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir => {
                return Err(ApiError::BadRequest(format!(
                    "path escapes the vault (`..` not allowed): {rel}"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadRequest(format!(
                    "path must be relative to the vault root: {rel}"
                )));
            }
            // Normal / CurDir are fine.
            _ => {}
        }
    }

    // (3) Canonical-prefix check — catches symlinks that escape.
    let canonical_root = vault_root.canonicalize().map_err(|e| {
        // The vault root failing to canonicalise is a server-side fault (it
        // existed when the daemon bound it). Log detail; return a generic label.
        tracing::warn!(error = %e, "canonicalize vault root failed");
        ApiError::Internal("could not resolve vault root".to_string())
    })?;
    let joined = canonical_root.join(rel_path);
    let canonical_target = match joined.canonicalize() {
        Ok(p) => p,
        // The file doesn't exist (yet) — well-formed path, just absent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::NotFound("no such file".to_string()));
        }
        Err(e) => {
            tracing::warn!(error = %e, "canonicalize target failed");
            return Err(ApiError::Internal("could not resolve path".to_string()));
        }
    };

    if !canonical_target.starts_with(&canonical_root) {
        return Err(ApiError::BadRequest("path escapes the vault".to_string()));
    }

    Ok(canonical_target)
}

/// Like [`resolve_in_vault`] but for paths that may not exist yet (create /
/// move destination). Performs the lexical guards (NUL, absolute, `..`) and
/// joins onto the canonical vault root. Symlink-escape on a *non-existent*
/// target is out of scope for the single-user self-hosted model — existing
/// paths still go through the canonicalizing `resolve_in_vault`.
pub(crate) fn resolve_in_vault_create(vault_root: &Path, rel: &str) -> Result<PathBuf, ApiError> {
    if rel.contains('\0') {
        return Err(ApiError::BadRequest(
            "path contains an interior NUL byte".into(),
        ));
    }
    if rel.is_empty() {
        return Err(ApiError::BadRequest("empty path".into()));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ApiError::BadRequest(format!(
            "path must be relative to the vault root: {rel}"
        )));
    }
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir => {
                return Err(ApiError::BadRequest(format!(
                    "path escapes the vault (`..` not allowed): {rel}"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadRequest(format!(
                    "path must be relative to the vault root: {rel}"
                )));
            }
            _ => {}
        }
    }
    let canonical_root = vault_root.canonicalize().map_err(|e| {
        tracing::warn!(error = %e, "canonicalize vault root failed");
        ApiError::Internal("could not resolve vault root".into())
    })?;
    let joined = canonical_root.join(rel_path);

    // Symlink-safe create: the target may not exist yet, so we cannot
    // canonicalize it (that's the whole reason this is a separate fn from
    // `resolve_in_vault`). Instead canonicalize the DEEPEST EXISTING ANCESTOR
    // and verify it is still inside the canonical root — this catches a parent
    // component that is a symlink escaping the vault (a write through it would
    // otherwise land outside). The non-existing tail carries no `..` (rejected
    // above) and is created as real dirs/files, so it cannot climb out.
    let mut ancestor = joined.as_path();
    let canonical_existing = loop {
        match ancestor.canonicalize() {
            Ok(p) => break p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match ancestor.parent() {
                Some(parent) => ancestor = parent,
                // We always reach `canonical_root` (it canonicalised above), so
                // a None here means we climbed past it — treat as a server fault.
                None => return Err(ApiError::Internal("could not resolve path".into())),
            },
            Err(e) => {
                tracing::warn!(error = %e, "canonicalize ancestor failed");
                return Err(ApiError::Internal("could not resolve path".into()));
            }
        }
    };
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(ApiError::BadRequest("path escapes the vault".into()));
    }
    Ok(joined)
}

/// File mtime as whole nanoseconds since the Unix epoch (`None` if unavailable).
fn mtime_nanos(path: &Path) -> Option<u128> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn resolve_accepts_a_normal_nested_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("01-projects")).unwrap();
        fs::write(root.join("01-projects/a.md"), "x").unwrap();
        let got = resolve_in_vault(root, "01-projects/a.md").unwrap();
        assert!(got.ends_with("a.md"));
        assert!(got.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn resolve_rejects_parent_traversal() {
        let dir = tempdir().unwrap();
        let err = resolve_in_vault(dir.path(), "../../etc/passwd").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn resolve_rejects_absolute_path() {
        let dir = tempdir().unwrap();
        let err = resolve_in_vault(dir.path(), "/etc/passwd").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn resolve_missing_file_is_not_found() {
        let dir = tempdir().unwrap();
        let err = resolve_in_vault(dir.path(), "nope.md").unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escaping_the_vault() {
        // A symlink INSIDE the vault that points to a file OUTSIDE must be
        // rejected by the canonical-prefix check — the lexical `..` guard
        // can't see through it.
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        let vault = tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            vault.path().join("leak.md"),
        )
        .unwrap();

        let err = resolve_in_vault(vault.path(), "leak.md").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn resolve_rejects_interior_nul_byte() {
        // A NUL byte can't appear in a real path — reject lexically (fix D),
        // before it can reach a canonicalize syscall.
        let dir = tempdir().unwrap();
        let err = resolve_in_vault(dir.path(), "a\0b.md").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    // ── read_vault_file: status mapping for the tricky cases (fix D + F) ──

    #[test]
    fn read_vault_file_empty_path_is_bad_request() {
        let dir = tempdir().unwrap();
        let err = read_vault_file(dir.path(), "").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn read_vault_file_directory_is_bad_request() {
        // A path that resolves to a DIRECTORY is a 400 "not a file" (fix D).
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("01-projects")).unwrap();
        let err = read_vault_file(dir.path(), "01-projects").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn read_vault_file_missing_is_not_found() {
        let dir = tempdir().unwrap();
        let err = read_vault_file(dir.path(), "nope.md").unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn read_vault_file_non_utf8_is_unprocessable() {
        // A file with invalid UTF-8 bytes can't be returned as text → 422 (fix D).
        let dir = tempdir().unwrap();
        // 0xFF / 0xFE are never valid UTF-8 lead bytes.
        fs::write(dir.path().join("binary.md"), [0xFFu8, 0xFE, 0x00, 0x01]).unwrap();
        let err = read_vault_file(dir.path(), "binary.md").unwrap_err();
        assert!(matches!(err, ApiError::Unprocessable(_)), "got {err:?}");
    }

    #[test]
    fn read_vault_file_over_cap_is_payload_too_large() {
        // A file larger than MAX_NOTE_BYTES → 413, refused before the read (fix F).
        let dir = tempdir().unwrap();
        let big = dir.path().join("big.md");
        // Write just past the cap. `set_len` produces a sparse file on most
        // filesystems, so this is cheap even at 10 MB + 1.
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_NOTE_BYTES + 1).unwrap();
        let err = read_vault_file(dir.path(), "big.md").unwrap_err();
        assert!(matches!(err, ApiError::PayloadTooLarge(_)), "got {err:?}");
    }

    #[test]
    fn read_vault_file_at_cap_is_ok() {
        // A file exactly AT the cap (not over) is served — boundary check.
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.md");
        // An empty file is trivially under the cap and valid UTF-8.
        fs::write(&path, "# fine\n").unwrap();
        let resp = read_vault_file(dir.path(), "ok.md").unwrap();
        assert_eq!(resp.path, "ok.md");
        assert!(resp.content.contains("# fine"));
    }

    #[test]
    fn to_slash_normalises_separators() {
        assert_eq!(to_slash(Path::new("a/b/c.md")), "a/b/c.md");
    }

    #[test]
    fn content_disposition_keeps_name_and_extension() {
        // ASCII + spaces: real name in BOTH the quoted fallback and filename*.
        let cd = content_disposition_attachment("00-inbox/Workstream 1b - Kickoff.pptx");
        assert!(
            cd.contains(r#"filename="Workstream 1b - Kickoff.pptx""#),
            "{cd}"
        );
        assert!(
            cd.contains("filename*=UTF-8''Workstream%201b%20-%20Kickoff.pptx"),
            "{cd}"
        );
        // Non-ASCII (Thai): ASCII fallback sanitised to '_', filename* carries it.
        let th = content_disposition_attachment("งานวิจัย.pdf");
        assert!(th.starts_with("attachment;"), "{th}");
        assert!(th.contains("filename*=UTF-8''"), "{th}");
        assert!(th.ends_with(".pdf"), "{th}");
        // Trailing-slash / empty basename falls back to a safe name.
        assert!(content_disposition_attachment("a/b/").contains("download"));
    }

    #[test]
    fn parse_byte_range_handles_common_forms() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_byte_range("bytes=-200", 1000), Some((800, 999)));
        assert_eq!(parse_byte_range("bytes=0-99999", 1000), Some((0, 999))); // end clamped
                                                                             // malformed / unsatisfiable → None (caller serves the full 200 body)
        assert_eq!(parse_byte_range("bytes=abc", 1000), None);
        assert_eq!(parse_byte_range("bytes=500-100", 1000), None); // start > end
        assert_eq!(parse_byte_range("bytes=2000-3000", 1000), None); // start >= total
        assert_eq!(parse_byte_range("bytes=0-1,2-3", 1000), None); // multi-range
        assert_eq!(parse_byte_range("bytes=0-99", 0), None); // empty file
    }
}
