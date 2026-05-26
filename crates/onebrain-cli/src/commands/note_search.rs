//! `onebrain note search` — substring (default) / regex content search.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteSearchArgs;
use crate::commands::note_common::{skipped_warning, W_NOTES_SKIPPED};
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{search_notes, SearchMode, SearchOptions, SearchResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteSearchArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let opts = SearchOptions {
        pattern: args.pattern.clone(),
        folder: args.folder.clone(),
        limit: args.limit,
        mode: if args.mode == "regex" {
            SearchMode::Regex
        } else {
            SearchMode::Lex
        },
    };
    let data = search_notes(resolved.root.as_path(), &opts)?;

    let warning = skipped_warning(&data.skipped);
    let mut envelope = Envelope::ok("note.search", Some(vault_info), data);
    if let Some(msg) = warning {
        envelope = envelope.with_warning(W_NOTES_SKIPPED, msg);
    }
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<SearchResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.matches.is_empty() {
        return "No matches.".to_string();
    }
    let mut out = String::new();
    for m in &d.matches {
        out.push_str(&format!("{}:{}: {}\n", m.path.display(), m.line, m.context));
    }
    if d.truncated {
        out.push_str(&format!(
            "\n… {} of {} matches shown · raise --limit for more\n",
            d.matches.len(),
            d.total_found
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_fs::note::NoteMatch;

    fn env(matches: Vec<NoteMatch>, total: usize, truncated: bool) -> Envelope<SearchResult> {
        Envelope::ok(
            "note.search",
            None,
            SearchResult {
                matches,
                total_found: total,
                truncated,
                skipped: Vec::new(),
            },
        )
    }

    #[test]
    fn text_renders_grep_style_lines() {
        let e = env(
            vec![NoteMatch {
                path: PathBuf::from("a.md"),
                line: 2,
                context: "some TODO here".into(),
                title: Some("Alpha".into()),
            }],
            1,
            false,
        );
        let s = render_text(&e);
        assert!(s.contains("a.md:2: some TODO here"));
        assert!(!s.contains("matches shown"));
    }

    #[test]
    fn text_shows_truncation_footer() {
        let e = env(
            vec![NoteMatch {
                path: PathBuf::from("a.md"),
                line: 1,
                context: "x".into(),
                title: None,
            }],
            5,
            true,
        );
        let s = render_text(&e);
        assert!(s.contains("1 of 5 matches shown"));
    }

    #[test]
    fn text_handles_no_matches() {
        let e = env(Vec::new(), 0, false);
        assert_eq!(render_text(&e), "No matches.");
    }
}
