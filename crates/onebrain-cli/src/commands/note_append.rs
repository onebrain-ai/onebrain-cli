//! `onebrain note append` — append content to a note (at EOF or under a
//! section). First WRITE verb of the `note` group.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteAppendArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{append_note, AppendResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteAppendArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = append_note(
        resolved.root.as_path(),
        &args.path,
        &args.content,
        args.section.as_deref(),
    )?;

    let envelope = Envelope::ok("note.append", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<AppendResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let path = &d.path;
    match &d.section {
        Some(section) if d.created_section => format!(
            "Appended {} bytes to {} (created section \"{}\").",
            d.bytes_appended, path, section
        ),
        Some(section) => format!(
            "Appended {} bytes to {} under section \"{}\".",
            d.bytes_appended, path, section
        ),
        None => format!(
            "Appended {} bytes to the end of {}.",
            d.bytes_appended, path
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        section: Option<&str>,
        bytes_appended: usize,
        created_section: bool,
    ) -> Envelope<AppendResult> {
        Envelope::ok(
            "note.append",
            None,
            AppendResult {
                path: "a.md".into(),
                section: section.map(str::to_string),
                bytes_appended,
                created_section,
            },
        )
    }

    #[test]
    fn text_no_section_reports_eof() {
        let e = env(None, 12, false);
        assert_eq!(render_text(&e), "Appended 12 bytes to the end of a.md.");
    }

    #[test]
    fn text_existing_section() {
        let e = env(Some("Alpha"), 5, false);
        assert_eq!(
            render_text(&e),
            "Appended 5 bytes to a.md under section \"Alpha\"."
        );
    }

    #[test]
    fn text_created_section() {
        let e = env(Some("Gamma"), 7, true);
        assert_eq!(
            render_text(&e),
            "Appended 7 bytes to a.md (created section \"Gamma\")."
        );
    }
}
