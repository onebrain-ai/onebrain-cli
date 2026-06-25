//! `onebrain note edit` — write content verbatim to a note (create or
//! overwrite). The web editor is the single source of formatting; no
//! frontmatter injection or template expansion is performed.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteEditArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{write_note, WriteResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteEditArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = write_note(resolved.root.as_path(), &args.path, &args.content)?;

    let envelope = Envelope::ok("note.edit", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<WriteResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let path = &d.path;
    if d.created {
        format!("Created {path}.")
    } else {
        format!("Wrote {path}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(created: bool) -> Envelope<WriteResult> {
        Envelope::ok(
            "note.edit",
            None,
            WriteResult {
                path: "a.md".into(),
                created,
            },
        )
    }

    #[test]
    fn text_created() {
        let e = env(true);
        assert_eq!(render_text(&e), "Created a.md.");
    }

    #[test]
    fn text_overwritten() {
        let e = env(false);
        assert_eq!(render_text(&e), "Wrote a.md.");
    }
}
