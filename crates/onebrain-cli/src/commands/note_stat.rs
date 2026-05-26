//! `onebrain note stat` — content + filesystem counts for a single note.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteStatArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{stat_note, NoteStatData};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteStatArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = stat_note(resolved.root.as_path(), &args.path)?;

    let envelope = Envelope::ok("note.stat", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<NoteStatData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let created = d
        .created
        .map(|c| c.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "—".to_string());
    format!(
        "{path}\n\
         lines     : {lines}\n\
         words     : {words}\n\
         chars     : {chars}\n\
         size      : {size} bytes\n\
         headings  : {headings}\n\
         wikilinks : {wikilinks}\n\
         tasks     : {total} ({done} done · {open} open)\n\
         created   : {created}\n\
         modified  : {modified}",
        path = d.path,
        lines = d.lines,
        words = d.words,
        chars = d.chars,
        size = d.size_bytes,
        headings = d.headings,
        wikilinks = d.wikilinks,
        total = d.tasks_total,
        done = d.tasks_done,
        open = d.tasks_open,
        created = created,
        modified = d.modified.format("%Y-%m-%d"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn env() -> Envelope<NoteStatData> {
        Envelope::ok(
            "note.stat",
            None,
            NoteStatData {
                path: "a.md".into(),
                lines: 10,
                words: 42,
                chars: 200,
                size_bytes: 256,
                headings: 3,
                wikilinks: 3,
                tasks_total: 3,
                tasks_done: 2,
                tasks_open: 1,
                created: Some(Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap()),
                modified: Utc.with_ymd_and_hms(2026, 5, 26, 0, 0, 0).unwrap(),
            },
        )
    }

    #[test]
    fn text_renders_all_counts() {
        let s = render_text(&env());
        assert!(s.contains("a.md"));
        assert!(s.contains("lines     : 10"));
        assert!(s.contains("words     : 42"));
        assert!(s.contains("chars     : 200"));
        assert!(s.contains("size      : 256 bytes"));
        assert!(s.contains("headings  : 3"));
        assert!(s.contains("wikilinks : 3"));
        assert!(s.contains("tasks     : 3 (2 done · 1 open)"));
        assert!(s.contains("created   : 2026-05-20"));
        assert!(s.contains("modified  : 2026-05-26"));
    }

    #[test]
    fn text_renders_dash_when_no_created() {
        let mut e = env();
        e.data.as_mut().unwrap().created = None;
        let s = render_text(&e);
        assert!(s.contains("created   : —"));
    }
}
