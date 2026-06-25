//! `onebrain note mkdir` — create a folder (and any missing parents) inside
//! the vault. Errors if the path already exists.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteMkdirArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{create_folder, FolderResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteMkdirArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = create_folder(resolved.root.as_path(), &args.path)?;

    let envelope = Envelope::ok("note.mkdir", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<FolderResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    format!("Created folder {}.", d.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Envelope<FolderResult> {
        Envelope::ok(
            "note.mkdir",
            None,
            FolderResult {
                path: "01-projects/new".into(),
            },
        )
    }

    #[test]
    fn text_reports_created_folder() {
        let e = env();
        assert_eq!(render_text(&e), "Created folder 01-projects/new.");
    }
}
