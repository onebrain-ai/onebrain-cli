//! `GET /api/vault/search` — vault search backed by the user's `qmd` index
//! (the SAME index the CLI + the qmd MCP server use).
//!
//! Two modes, both shelling out to the `qmd` binary and mapping its `--json`
//! output to a small ranked list the webui can open in Preview:
//!
//! ```text
//!   mode=lex     → qmd search <q> --json               BM25 keyword, no LLM, ~0.6s
//!   mode=hybrid  → qmd query "lex:<q>\nvec:<q>" --json  keyword + semantic, one
//!                  query-embedding (~1-2s), local rerank — NO LLM expansion
//! ```
//!
//! The webui runs `lex` live as-you-type and upgrades to `hybrid` on a short
//! pause (two-tier progressive), so this endpoint stays a thin, read-only
//! translator: it never reads a note itself — it returns vault-relative paths
//! the existing `GET /api/vault/file` (with its path-traversal guard) opens.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::api::{require_vault_root, ApiError};
use super::AppState;

/// Query string for `GET /api/vault/search`.
#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    /// The user's search text.
    q: String,
    /// `lex` (BM25 keyword, the default) or `hybrid` (keyword + semantic).
    #[serde(default)]
    mode: Option<String>,
}

/// Response body: a ranked hit list plus the mode actually run (so the client
/// can label which tier produced it).
#[derive(Debug, Serialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
    mode: &'static str,
}

/// One result row for the webui.
#[derive(Debug, Serialize, PartialEq)]
struct SearchHit {
    /// Vault-relative, slash-separated path (openable via `/api/vault/file`).
    path: String,
    /// qmd's relevance score, roughly 0..1 (higher = better).
    score: f64,
    /// Note title (qmd's — usually the H1 or filename).
    title: String,
    /// Short, cleaned one-line excerpt around the match (may be empty).
    snippet: String,
}

/// The qmd `--json` row shape — only the fields we use (`docid`/`context` are
/// ignored). Everything is `#[serde(default)]` so a future qmd that drops a
/// field degrades to an empty value rather than a hard parse error.
#[derive(Debug, Deserialize)]
struct QmdRow {
    #[serde(default)]
    score: f64,
    #[serde(default)]
    file: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    snippet: String,
}

/// Read-only: shell out to `qmd`, map its hits to vault-relative paths.
pub(crate) async fn get_vault_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let query = q.q.trim().to_string();
    let mode: &'static str = match q.mode.as_deref() {
        Some("hybrid") => "hybrid",
        _ => "lex",
    };

    // Empty query → empty result; don't pay a qmd spawn for nothing.
    if query.is_empty() {
        return Ok(Json(SearchResponse { hits: vec![], mode }).into_response());
    }

    // qmd is "enabled" for a vault only when it names its collection (onebrain.yml
    // `qmd_collection`). Absent → qmd is off for this vault: return 503 so the
    // client falls back to its own filename/path search, rather than running qmd
    // unscoped and leaking hits from other indexed collections.
    let collection = onebrain_core::load_vault_config_at(&root)
        .ok()
        .and_then(|c| c.qmd_collection)
        .ok_or_else(|| {
            ApiError::ServiceUnavailable(
                "search unavailable — qmd is not configured for this vault".to_string(),
            )
        })?;

    let hits = run_qmd(&root, &query, mode, &collection).await?;
    Ok(Json(SearchResponse { hits, mode }).into_response())
}

/// Build the qmd argv for a mode + query. Pure (no I/O) so it's unit-testable.
///
/// The query is ALWAYS a single argv element: `Command` execs qmd directly with
/// no shell, so a query full of metacharacters (`;`, `$()`, backticks, newlines)
/// is inert — it can never inject a second command.
fn qmd_args(mode: &str, query: &str) -> Vec<String> {
    match mode {
        // Structured query document with explicit lex + vec lines. Supplying the
        // typed lines skips qmd's LLM query-expansion, so the only slow step is a
        // single query embedding; the rerank is local.
        "hybrid" => vec![
            "query".to_string(),
            format!("lex:{query}\nvec:{query}"),
            "--json".to_string(),
        ],
        // BM25 keyword — no LLM, fast enough to run on every keystroke.
        _ => vec!["search".to_string(), query.to_string(), "--json".to_string()],
    }
}

/// Hard ceiling on a single qmd invocation — hybrid is ~1-5s, so this only trips
/// on a genuine hang (a stuck embedding call, a deadlocked node process).
const QMD_TIMEOUT: Duration = Duration::from_secs(20);

/// The resolved `qmd` binary, cached — a PATH walk on every keystroke would be
/// wasteful (and a cheap DoS amplifier). Resolved once; `None` if qmd isn't
/// installed (→ 503 → the client uses its own fallback search).
fn qmd_bin() -> Option<&'static PathBuf> {
    static QMD_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    QMD_BIN.get_or_init(|| which::which("qmd").ok()).as_ref()
}

async fn run_qmd(
    root: &Path,
    query: &str,
    mode: &str,
    collection: &str,
) -> Result<Vec<SearchHit>, ApiError> {
    let qmd = qmd_bin().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "search unavailable — the qmd binary is not installed".to_string(),
        )
    })?;

    // Async child with a hard timeout so a stuck qmd can't pin a runtime worker;
    // `kill_on_drop` reaps the child when the timeout drops the output future.
    // `--json` writes results to stdout (qmd self-caps at ~20 rows, so it stays
    // small); tips/warnings go to stderr, so stdout stays clean JSON.
    let mut cmd = Command::new(qmd);
    cmd.current_dir(root)
        .args(qmd_args(mode, query))
        .kill_on_drop(true);

    let out = match tokio::time::timeout(QMD_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "qmd invocation failed");
            return Err(ApiError::Internal("search failed".to_string()));
        }
        Err(_) => {
            tracing::warn!(timeout_s = QMD_TIMEOUT.as_secs(), "qmd search timed out");
            return Err(ApiError::Internal("search timed out".to_string()));
        }
    };

    if !out.status.success() {
        tracing::warn!(
            code = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "qmd exited non-zero"
        );
        return Err(ApiError::Internal("search failed".to_string()));
    }

    let rows: Vec<QmdRow> = serde_json::from_slice(&out.stdout).map_err(|e| {
        tracing::warn!(error = %e, "qmd --json parse failed");
        ApiError::Internal("search failed".to_string())
    })?;

    Ok(rows
        .into_iter()
        .filter_map(|r| map_row(r, Some(collection)))
        .collect())
}

/// Map a qmd `--json` row to a webui hit, or `None` if it isn't openable in this
/// vault. qmd `file` is `qmd://<collection>/<vault-relative-path>[:line]`; strip
/// the scheme, the collection, and any `:line` locator, and drop hits from a
/// different collection so the webui never offers a result it can't open.
fn map_row(row: QmdRow, collection: Option<&str>) -> Option<SearchHit> {
    let rest = row.file.strip_prefix("qmd://")?;
    let (col, path) = rest.split_once('/')?;
    if let Some(want) = collection {
        if col != want {
            return None;
        }
    }
    let path = strip_line_locator(path);
    if path.is_empty() {
        return None;
    }
    Some(SearchHit {
        path: path.to_string(),
        score: row.score,
        title: row.title,
        snippet: clean_snippet(&row.snippet),
    })
}

/// Drop a trailing `:<line>` locator (`a/b.md:42` → `a/b.md`). A colon that
/// isn't followed by an all-digit run (unusual in a vault path) is left intact.
fn strip_line_locator(path: &str) -> &str {
    match path.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => path,
    }
}

/// qmd snippets are diff hunks — a `@@ … @@ (n before, m after)` header line
/// then the matched text. Drop the header, collapse whitespace to single spaces,
/// and cap the length so the JSON response stays small.
fn clean_snippet(raw: &str) -> String {
    let body = if raw.starts_with("@@") {
        raw.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        raw
    };
    body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(file: &str) -> QmdRow {
        QmdRow {
            score: 0.9,
            file: file.to_string(),
            title: "T".to_string(),
            snippet: String::new(),
        }
    }

    #[test]
    fn maps_qmd_uri_to_vault_relative_path() {
        let h = map_row(row("qmd://ob-1/01-projects/oma/x.md"), Some("ob-1")).unwrap();
        assert_eq!(h.path, "01-projects/oma/x.md");
        assert_eq!(h.score, 0.9);
    }

    #[test]
    fn strips_trailing_line_locator() {
        let h = map_row(row("qmd://ob-1/a/b.md:42"), Some("ob-1")).unwrap();
        assert_eq!(h.path, "a/b.md");
    }

    #[test]
    fn colon_not_followed_by_digits_is_kept() {
        assert_eq!(strip_line_locator("a/weird:name.md"), "a/weird:name.md");
    }

    #[test]
    fn drops_hits_from_other_collections() {
        assert!(map_row(row("qmd://other-vault/x.md"), Some("ob-1")).is_none());
    }

    #[test]
    fn no_collection_filter_keeps_every_collection() {
        let h = map_row(row("qmd://anything/x.md"), None).unwrap();
        assert_eq!(h.path, "x.md");
    }

    #[test]
    fn rejects_non_qmd_uris() {
        assert!(map_row(row("file:///etc/passwd"), None).is_none());
        assert!(map_row(row("/etc/passwd"), None).is_none());
        assert!(map_row(row("qmd://no-slash-after-collection"), None).is_none());
    }

    #[test]
    fn hybrid_passes_query_as_one_argv_element() {
        let a = qmd_args("hybrid", "a; rm -rf ~");
        assert_eq!(a[0], "query");
        // The whole structured doc is ONE argv element → no shell, no injection.
        assert_eq!(a[1], "lex:a; rm -rf ~\nvec:a; rm -rf ~");
        assert_eq!(a[2], "--json");
    }

    #[test]
    fn lex_is_the_default_mode() {
        let a = qmd_args("lex", "foo");
        assert_eq!(a.iter().map(String::as_str).collect::<Vec<_>>(), ["search", "foo", "--json"]);
    }

    #[test]
    fn cleans_diff_hunk_snippet_to_one_line() {
        let s = clean_snippet("@@ -1,4 @@ (0 before, 191 after)\n---\ntags: [a, b]\n");
        assert_eq!(s, "--- tags: [a, b]");
    }

    #[test]
    fn plain_snippet_without_hunk_header_is_collapsed() {
        assert_eq!(clean_snippet("hello   world\n\nfoo"), "hello world foo");
    }
}
