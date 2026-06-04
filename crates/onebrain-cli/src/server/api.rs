//! The read-only JSON API mounted under `/api`.
//!
//! All three handlers are a thin HTTP veneer over the CLI's existing vault
//! primitives — `onebrain_core::load_vault_config_at` for config, a `walkdir`
//! walk for the tree, `std::fs` for a single file read. No vault logic is
//! re-implemented.
//!
//! | route                       | returns                                   |
//! |-----------------------------|-------------------------------------------|
//! | `GET /api/config`           | parsed `onebrain.yml` as JSON             |
//! | `GET /api/vault/tree`       | `{ root, entries: [{path,name,kind}] }`   |
//! | `GET /api/vault/file?path=` | `{ path, content, rev }`                   |
//!
//! `rev` is a cheap revision tag (mtime in whole nanoseconds since the epoch)
//! for future conflict detection on `PUT` (step 2b).
//
// step 2b: `PUT /api/vault/file` (single write path + re-read-before-write
//          conflict check against `rev`), `GET /api/search`, `GET /api/extensions`,
//          and `GET /api/chat/stream` (SSE) hook in here as sibling routes.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::AppState;

/// Directory names pruned from the tree walk. Mirrors the vault's own ignore
/// set (`onebrain_fs::note::walker::TOOLING_DIRS`) so the API surface and the
/// `note` commands agree on what counts as "the vault". Kept as a local const
/// (not imported) because that one is private to `onebrain-fs`.
const TOOLING_DIRS: &[&str] = &[".git", ".obsidian", ".claude", ".trash", "node_modules"];

/// Build the `/api` sub-router. The auth layer AND the shared state are
/// attached by the caller (`build_router`), so this stays a pure route table:
/// it returns a `Router<Arc<AppState>>` (state still needed) and never calls
/// `.with_state` itself — `build_router` applies state once for the whole tree.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/config", get(get_config))
        .route("/vault/tree", get(get_vault_tree))
        .route("/vault/file", get(get_vault_file))
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
enum ApiError {
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

/// Maximum size of a note we will read into memory and return. A vault note is
/// prose/markdown — kilobytes, occasionally a few hundred. 10 MB is a generous
/// cap that still refuses a pathological/accidental huge file before it can
/// balloon the daemon's memory. Over the cap → 413 (fix F).
const MAX_NOTE_BYTES: u64 = 10 * 1024 * 1024;

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

/// True if a walked entry is one of the pruned tooling directories.
fn is_tooling_dir(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map(|n| TOOLING_DIRS.contains(&n))
            .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────────────────
// GET /api/vault/file?path=<rel>
// ─────────────────────────────────────────────────────────────────────────

/// Query string for `GET /api/vault/file`.
#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
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

/// File mtime as whole nanoseconds since the Unix epoch (`None` if unavailable).
fn mtime_nanos(path: &Path) -> Option<u128> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

/// Render a relative path as a forward-slash string, regardless of OS
/// separator, so clients get stable identifiers.
fn to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
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
}
