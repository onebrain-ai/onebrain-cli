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

    let content = engine.get(&args.doc_path)?;
    let data = SearchGetData {
        doc_path: args.doc_path.clone(),
        content,
    };

    let envelope = Envelope::ok("search.get", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<SearchGetData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    d.content.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

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
