//! `onebrain search get` — fetch a doc's full indexed text.
//!
//! Vault-required (exit 64 outside a vault). `Engine::get` never touches
//! the embedder, so opening the engine here never triggers a model
//! download (see engine.rs's lazy embedder).

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::SearchGetArgs;
use crate::commands::daemon_client::DaemonHandle;
use crate::commands::search_common::{map_daemon_error, open_engine, route_to_daemon};
use crate::output::{emit, Envelope, OutputMode};

#[derive(Debug, Serialize)]
struct SearchGetData {
    doc_path: String,
    content: String,
}

/// The hint appended when a doc isn't found in the index — shared by the direct
/// and daemon-routed paths so the message never drifts.
const NOT_INDEXED_HINT: &str = "💡  paths are vault-relative (e.g. `00-inbox/note.md`); \
     if the doc is new, `onebrain search reindex` may not have indexed it yet";

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &SearchGetArgs) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag.clone())?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    // An absolute path that isn't under the vault root can never be an index
    // key (keys are vault-relative), so `Engine::get` would just miss with a
    // generic "not indexed" hint. Catch it up front with a precise message.
    if is_outside_vault(&args.doc_path, resolved.root.as_path()) {
        anyhow::bail!(
            "❌  path is outside this vault: {}\n💡  `search get` takes vault-relative \
             paths of indexed `.md` notes (e.g. `00-inbox/note.md`)",
            args.doc_path
        );
    }

    // Index keys are vault-relative (forward-slash) paths; accept an
    // absolute path under the vault root and normalize it so
    // `search get /abs/vault/00-inbox/x.md` just works.
    let doc_path = normalize_doc_path(&args.doc_path, resolved.root.as_path());

    // Warm-daemon path: when a daemon already holds the engine, `Engine::get`
    // (a redb read) would clash with it → route the lookup through the daemon.
    // Only when it serves this exact vault; else fall through to a direct open.
    let content = if let Some(handle) = route_to_daemon(&resolved) {
        get_via_daemon(&handle, &doc_path)?
    } else {
        let (engine, _resolved) = open_engine(vault_flag)?;
        engine
            .get(&doc_path)
            .map_err(|e| anyhow::anyhow!("{e}\n{NOT_INDEXED_HINT}"))?
    };
    let data = SearchGetData { doc_path, content };

    let envelope = Envelope::ok("search.get", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Fetch a doc's indexed text through the warm daemon's `/api/internal/get`. A
/// daemon 404 (doc not indexed) maps to the SAME "not indexed yet" error the
/// direct path emits, so the two surfaces are indistinguishable to the user.
fn get_via_daemon(handle: &DaemonHandle, doc_path: &str) -> Result<String> {
    match handle.get(doc_path) {
        Ok(Some(content)) => Ok(content),
        Ok(None) => anyhow::bail!("doc not found: {doc_path}\n{NOT_INDEXED_HINT}"),
        Err(e) => Err(map_daemon_error(
            e,
            "route `search get` through warm daemon",
        )),
    }
}

/// `true` when `input` is an absolute path that is NOT under `vault_root`.
/// Such a path can't correspond to any index key (keys are vault-relative),
/// so the caller should reject it up front with a clear message rather than
/// let the lookup miss generically. Relative inputs are always `false` here
/// (they're resolved against the vault by `normalize_doc_path`).
fn is_outside_vault(input: &str, vault_root: &std::path::Path) -> bool {
    let p = std::path::Path::new(input);
    p.is_absolute() && p.strip_prefix(vault_root).is_err()
}

/// Normalize a user-supplied doc path to the index's key form: absolute
/// paths under the vault root are made vault-relative, `./` prefixes are
/// stripped, and platform back-slashes become forward slashes. Anything
/// else is passed through unchanged.
fn normalize_doc_path(input: &str, vault_root: &std::path::Path) -> String {
    let p = std::path::Path::new(input);
    let rel = p.strip_prefix(vault_root).unwrap_or(p);
    let s = rel.to_string_lossy().replace('\\', "/");
    s.trim_start_matches("./").to_string()
}

fn render_text(env: &Envelope<SearchGetData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    d.content.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_vault_root_and_dot_prefix() {
        let root = std::path::Path::new("/vault/ob-1");
        assert_eq!(
            normalize_doc_path("/vault/ob-1/00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        assert_eq!(
            normalize_doc_path("./00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        assert_eq!(
            normalize_doc_path("00-inbox/note.md", root),
            "00-inbox/note.md"
        );
        // Absolute path OUTSIDE the vault passes through unchanged.
        assert_eq!(
            normalize_doc_path("/elsewhere/x.md", root),
            "/elsewhere/x.md"
        );
    }

    #[test]
    fn is_outside_vault_detects_absolute_paths_not_under_root() {
        // `/…` counts as absolute only on unix; Windows needs a drive
        // prefix, so build platform-appropriate paths.
        #[cfg(unix)]
        let (root, outside_a, outside_b, inside) = (
            std::path::Path::new("/vault/ob-1"),
            "/elsewhere/x.md",
            "/vault/other/x.md",
            "/vault/ob-1/00-inbox/note.md",
        );
        #[cfg(windows)]
        let (root, outside_a, outside_b, inside) = (
            std::path::Path::new("C:\\vault\\ob-1"),
            "C:\\elsewhere\\x.md",
            "C:\\vault\\other\\x.md",
            "C:\\vault\\ob-1\\00-inbox\\note.md",
        );
        // Absolute, outside the vault → true.
        assert!(is_outside_vault(outside_a, root));
        assert!(is_outside_vault(outside_b, root));
        // Absolute, under the vault → false (normalize handles it).
        assert!(!is_outside_vault(inside, root));
        // Relative → always false here.
        assert!(!is_outside_vault("00-inbox/note.md", root));
        assert!(!is_outside_vault("./00-inbox/note.md", root));
    }

    #[test]
    fn text_prints_raw_content() {
        let e = Envelope::ok(
            "search.get",
            None,
            SearchGetData {
                doc_path: "a.md".into(),
                content: "line1\nline2".into(),
            },
        );
        assert_eq!(render_text(&e), "line1\nline2");
    }
}
