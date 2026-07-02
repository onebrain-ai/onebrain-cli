//! `onebrain search get` — fetch a doc's full indexed text.
//!
//! Vault-required (exit 64 outside a vault). `Engine::get` never touches
//! the embedder, so opening the engine here never triggers a model
//! download (see engine.rs's lazy embedder).

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::SearchGetArgs;
use crate::commands::search_common::open_engine;
use crate::output::{emit, Envelope, OutputMode};

#[derive(Debug, Serialize)]
struct SearchGetData {
    doc_path: String,
    content: String,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &SearchGetArgs) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
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
    let content = engine.get(&doc_path).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n💡  paths are vault-relative (e.g. `00-inbox/note.md`); \
             if the doc is new, `onebrain search reindex` may not have indexed it yet"
        )
    })?;
    let data = SearchGetData { doc_path, content };

    let envelope = Envelope::ok("search.get", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
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
        let root = std::path::Path::new("/vault/ob-1");
        // Absolute, outside the vault → true.
        assert!(is_outside_vault("/elsewhere/x.md", root));
        assert!(is_outside_vault("/vault/other/x.md", root));
        // Absolute, under the vault → false (normalize handles it).
        assert!(!is_outside_vault("/vault/ob-1/00-inbox/note.md", root));
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
