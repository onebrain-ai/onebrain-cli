//! `onebrain note backlinks` — list every note that links TO a target note.
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteBacklinksArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{backlinks, BacklinksData};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteBacklinksArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let data = backlinks(resolved.root.as_path(), &args.path, args.include_archive)?;

    let envelope = Envelope::ok("note.backlinks", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<BacklinksData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.backlinks.is_empty() {
        return format!("No backlinks to {}.", d.target.display());
    }
    let mut out = String::new();
    for b in &d.backlinks {
        out.push_str(&format!(
            "{}:{}: {}\n",
            b.source.display(),
            b.line,
            b.context
        ));
    }
    out.push_str(&format!(
        "\n{} backlink(s) to {}\n",
        d.count,
        d.target.display()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_fs::note::BacklinkEntry;

    fn env(backlinks: Vec<BacklinkEntry>) -> Envelope<BacklinksData> {
        let count = backlinks.len();
        Envelope::ok(
            "note.backlinks",
            None,
            BacklinksData {
                target: PathBuf::from("MEMORY-INDEX.md"),
                backlinks,
                count,
            },
        )
    }

    #[test]
    fn text_renders_source_line_context_and_count() {
        let e = env(vec![
            BacklinkEntry {
                source: PathBuf::from("a.md"),
                line: 2,
                context: "see [[MEMORY-INDEX]]".into(),
            },
            BacklinkEntry {
                source: PathBuf::from("b.md"),
                line: 5,
                context: "also [[MEMORY-INDEX|x]]".into(),
            },
        ]);
        let s = render_text(&e);
        assert!(s.contains("a.md:2: see [[MEMORY-INDEX]]"));
        assert!(s.contains("b.md:5: also [[MEMORY-INDEX|x]]"));
        assert!(s.contains("2 backlink(s) to MEMORY-INDEX.md"));
    }

    #[test]
    fn text_handles_no_backlinks() {
        let e = env(Vec::new());
        assert_eq!(render_text(&e), "No backlinks to MEMORY-INDEX.md.");
    }
}
