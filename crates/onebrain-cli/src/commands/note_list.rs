//! `onebrain note list` — enumerate notes with metadata, sorted.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteListArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{list_notes, ListOptions, ListResult, ListSort};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteListArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let opts = ListOptions {
        folder: args.folder.clone(),
        limit: args.limit,
        sort: match args.sort.as_str() {
            "name" => ListSort::Name,
            "created" => ListSort::Created,
            _ => ListSort::Mtime,
        },
    };
    let data = list_notes(resolved.root.as_path(), &opts)?;

    let envelope = Envelope::ok("note.list", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<ListResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.notes.is_empty() {
        return "No notes.".to_string();
    }
    let mut out = String::new();
    for n in &d.notes {
        out.push_str(&format!(
            "{}  {}  ({})\n",
            n.modified.format("%Y-%m-%d"),
            n.path.display(),
            n.title
        ));
    }
    if d.notes.len() < d.total {
        out.push_str(&format!(
            "\n… {} of {} notes shown · raise --limit for more\n",
            d.notes.len(),
            d.total
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use onebrain_fs::note::NoteEntry;

    fn entry(path: &str, title: &str) -> NoteEntry {
        NoteEntry {
            path: PathBuf::from(path),
            title: title.into(),
            modified: Utc.with_ymd_and_hms(2026, 5, 26, 0, 0, 0).unwrap(),
            created: None,
            size_bytes: 10,
        }
    }

    #[test]
    fn text_lists_date_path_title() {
        let e = Envelope::ok(
            "note.list",
            None,
            ListResult {
                notes: vec![entry("a.md", "Alpha")],
                total: 1,
            },
        );
        let s = render_text(&e);
        assert!(s.contains("2026-05-26"));
        assert!(s.contains("a.md"));
        assert!(s.contains("(Alpha)"));
    }

    #[test]
    fn text_shows_truncation_footer() {
        let e = Envelope::ok(
            "note.list",
            None,
            ListResult {
                notes: vec![entry("a.md", "A")],
                total: 9,
            },
        );
        assert!(render_text(&e).contains("1 of 9 notes shown"));
    }

    #[test]
    fn text_handles_empty() {
        let e = Envelope::ok(
            "note.list",
            None,
            ListResult {
                notes: Vec::new(),
                total: 0,
            },
        );
        assert_eq!(render_text(&e), "No notes.");
    }
}
