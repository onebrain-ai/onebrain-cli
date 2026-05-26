//! `onebrain note new` — create a new note, optionally from a template, with
//! inline frontmatter. Second WRITE verb of the `note` group.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteNewArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{new_note, NewResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteNewArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = new_note(
        resolved.root.as_path(),
        &args.path,
        args.template.as_deref(),
        &args.frontmatter,
        args.force,
    )?;

    let envelope = Envelope::ok("note.new", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<NewResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let path = &d.path;
    let verb = if d.overwritten {
        "Overwrote"
    } else {
        "Created"
    };
    match &d.template {
        Some(template) => format!("{verb} {path} from template \"{template}\"."),
        None => format!("{verb} {path}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(overwritten: bool, template: Option<&str>) -> Envelope<NewResult> {
        Envelope::ok(
            "note.new",
            None,
            NewResult {
                path: "03-knowledge/Note.md".into(),
                created: true,
                overwritten,
                template: template.map(str::to_string),
            },
        )
    }

    #[test]
    fn text_created_plain() {
        let e = env(false, None);
        assert_eq!(render_text(&e), "Created 03-knowledge/Note.md.");
    }

    #[test]
    fn text_created_from_template() {
        let e = env(false, Some("concept"));
        assert_eq!(
            render_text(&e),
            "Created 03-knowledge/Note.md from template \"concept\"."
        );
    }

    #[test]
    fn text_overwritten() {
        let e = env(true, None);
        assert_eq!(render_text(&e), "Overwrote 03-knowledge/Note.md.");
    }
}
