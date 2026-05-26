//! `onebrain note orphans` — list notes with zero incoming wikilinks.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteOrphansArgs;
use crate::commands::note_common::{skipped_warning, W_NOTES_SKIPPED};
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{orphans, OrphansData};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteOrphansArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = orphans(resolved.root.as_path(), args.folder.as_deref(), args.limit)?;

    let warning = skipped_warning(&data.skipped);
    let mut envelope = Envelope::ok("note.orphans", Some(vault_info), data);
    if let Some(msg) = warning {
        envelope = envelope.with_warning(W_NOTES_SKIPPED, msg);
    }
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<OrphansData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.orphans.is_empty() {
        return "No orphans.".to_string();
    }
    let mut out = String::new();
    for p in &d.orphans {
        out.push_str(&format!("{}\n", p.display()));
    }
    if d.truncated {
        out.push_str(&format!(
            "\n… {} of {} orphans shown · raise --limit for more\n",
            d.orphans.len(),
            d.total
        ));
    } else {
        out.push_str(&format!("\n{} orphan(s)\n", d.total));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(orphans: Vec<&str>, total: usize, truncated: bool) -> Envelope<OrphansData> {
        Envelope::ok(
            "note.orphans",
            None,
            OrphansData {
                orphans: orphans.into_iter().map(PathBuf::from).collect(),
                total,
                truncated,
                skipped: Vec::new(),
            },
        )
    }

    #[test]
    fn text_lists_paths_with_count() {
        let e = env(vec!["a.md", "b.md"], 2, false);
        let s = render_text(&e);
        assert!(s.contains("a.md\n"));
        assert!(s.contains("b.md\n"));
        assert!(s.contains("2 orphan(s)"));
    }

    #[test]
    fn text_shows_truncation_footer() {
        let e = env(vec!["a.md"], 42, true);
        let s = render_text(&e);
        assert!(s.contains("1 of 42 orphans shown"));
    }

    #[test]
    fn text_handles_no_orphans() {
        assert_eq!(render_text(&env(Vec::new(), 0, false)), "No orphans.");
    }
}
