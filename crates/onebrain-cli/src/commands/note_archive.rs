//! `onebrain note archive` — move a note into the dated archive bucket.
//!
//! Third WRITE verb of the `note` group. Destination is
//! `<archive_root>/YYYY/MM/<filename>` (current UTC date). Wikilinks resolve
//! automatically because the basename is unchanged.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteArchiveArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{archive_note, ArchiveResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteArchiveArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = archive_note(resolved.root.as_path(), &args.path, &args.archive_root)?;

    let envelope = Envelope::ok("note.archive", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<ArchiveResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    format!("archived {} → {}", d.from.display(), d.to.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_renders_from_to_arrow() {
        let e = Envelope::ok(
            "note.archive",
            None,
            ArchiveResult {
                from: PathBuf::from("01-projects/Old Note.md"),
                to: PathBuf::from("06-archive/2026/05/Old Note.md"),
            },
        );
        assert_eq!(
            render_text(&e),
            "archived 01-projects/Old Note.md → 06-archive/2026/05/Old Note.md"
        );
    }
}
