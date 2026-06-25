//! `onebrain note delete` — soft-delete a note by moving it into `.trash/`.
//! Recoverable; mirrors Obsidian's local trash behaviour.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteDeleteArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{delete_note, DeleteResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteDeleteArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = delete_note(resolved.root.as_path(), &args.path)?;

    let envelope = Envelope::ok("note.delete", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<DeleteResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    format!("Deleted {} → {}.", d.path, d.trashed_to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Envelope<DeleteResult> {
        Envelope::ok(
            "note.delete",
            None,
            DeleteResult {
                path: "a.md".into(),
                trashed_to: ".trash/a.md".into(),
            },
        )
    }

    #[test]
    fn text_reports_trash_destination() {
        let e = env();
        assert_eq!(render_text(&e), "Deleted a.md → .trash/a.md.");
    }
}
