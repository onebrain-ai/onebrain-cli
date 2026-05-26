//! `onebrain note move` — move/rename a note and rewrite incoming wikilinks.
//!
//! The most complex WRITE verb of the `note` group. The move + every link
//! rewrite is transactional: a mid-execution failure rolls the vault back to
//! its starting state (see [`onebrain_fs::note::move_note`]).
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteMoveArgs;
use crate::commands::note_common::{skipped_warning, W_NOTES_SKIPPED};
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{move_note, MoveResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteMoveArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = move_note(
        resolved.root.as_path(),
        &args.from,
        &args.to,
        !args.no_link_update,
        args.dry_run,
    )?;

    let warning = skipped_warning(&data.skipped);
    let mut envelope = Envelope::ok("note.move", Some(vault_info), data);
    if let Some(msg) = warning {
        envelope = envelope.with_warning(W_NOTES_SKIPPED, msg);
    }
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<MoveResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let move_line = format!("{} → {}", d.from, d.to);
    let links = format!(
        "{} link{} in {} file{}",
        d.links_rewritten,
        if d.links_rewritten == 1 { "" } else { "s" },
        d.files_updated,
        if d.files_updated == 1 { "" } else { "s" },
    );
    if d.dry_run {
        format!("DRY RUN — would move {move_line}; would rewrite {links}")
    } else {
        format!("moved {move_line}; rewrote {links}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(dry_run: bool, links: usize, files: usize) -> Envelope<MoveResult> {
        Envelope::ok(
            "note.move",
            None,
            MoveResult {
                from: "old.md".into(),
                to: "03-knowledge/new.md".into(),
                links_rewritten: links,
                files_updated: files,
                dry_run,
                updated_files: Vec::new(),
                skipped: Vec::new(),
            },
        )
    }

    #[test]
    fn text_renders_move_and_link_summary() {
        let e = env(false, 3, 2);
        assert_eq!(
            render_text(&e),
            "moved old.md → 03-knowledge/new.md; rewrote 3 links in 2 files"
        );
    }

    #[test]
    fn text_singular_link_and_file() {
        let e = env(false, 1, 1);
        assert_eq!(
            render_text(&e),
            "moved old.md → 03-knowledge/new.md; rewrote 1 link in 1 file"
        );
    }

    #[test]
    fn text_dry_run_prefixed() {
        let e = env(true, 3, 2);
        assert_eq!(
            render_text(&e),
            "DRY RUN — would move old.md → 03-knowledge/new.md; would rewrite 3 links in 2 files"
        );
    }
}
