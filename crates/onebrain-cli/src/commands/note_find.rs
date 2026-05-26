//! `onebrain note find` — locate files/folders by glob (+ optional mtime).
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteFindArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_core::CoreError;
use onebrain_fs::note::{find_notes, FindOptions, FindResult, FindType};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteFindArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    // The clap `value_parser` whitelist already rejects anything outside
    // {file, folder}. The explicit error arm is defense-in-depth so a future
    // whitelist value without a matching arm fails loudly rather than silently
    // defaulting to File.
    let type_filter = match args.r#type.as_deref() {
        None => None,
        Some("file") => Some(FindType::File),
        Some("folder") => Some(FindType::Folder),
        Some(other) => {
            return Err(CoreError::InvalidTarget(format!("unknown find type: {other}")).into())
        }
    };
    let opts = FindOptions {
        glob: args.glob.clone(),
        r#type: type_filter,
        mtime: args.mtime,
        limit: args.limit,
    };
    let data = find_notes(resolved.root.as_path(), &opts)?;

    let envelope = Envelope::ok("note.find", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<FindResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.entries.is_empty() {
        return "No matches.".to_string();
    }
    let mut out = String::new();
    for e in &d.entries {
        let slash = if e.is_dir { "/" } else { "" };
        out.push_str(&format!("{}{}\n", e.path.display(), slash));
    }
    if d.truncated {
        out.push_str(&format!(
            "\n… {} of {} matches shown · raise --limit for more\n",
            d.entries.len(),
            d.total
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_fs::note::FindEntry;

    fn env(entries: Vec<FindEntry>, total: usize, truncated: bool) -> Envelope<FindResult> {
        Envelope::ok(
            "note.find",
            None,
            FindResult {
                entries,
                total,
                truncated,
            },
        )
    }

    #[test]
    fn text_marks_dirs_with_slash() {
        let e = env(
            vec![
                FindEntry {
                    path: PathBuf::from("07-logs/x.md"),
                    is_dir: false,
                },
                FindEntry {
                    path: PathBuf::from("03-knowledge"),
                    is_dir: true,
                },
            ],
            2,
            false,
        );
        let s = render_text(&e);
        assert!(s.contains("07-logs/x.md\n"));
        assert!(s.contains("03-knowledge/\n"));
    }

    #[test]
    fn text_shows_truncation_footer() {
        let e = env(
            vec![FindEntry {
                path: PathBuf::from("a.md"),
                is_dir: false,
            }],
            42,
            true,
        );
        assert!(render_text(&e).contains("1 of 42 matches shown"));
    }

    #[test]
    fn text_handles_no_matches() {
        assert_eq!(render_text(&env(Vec::new(), 0, false)), "No matches.");
    }
}
