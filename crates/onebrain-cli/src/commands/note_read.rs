//! `onebrain note read` — surgical read of a single note (body / section /
//! frontmatter / tasks).
//!
//! Vault-required (exit 64 outside a vault). Emits the canonical `Envelope<T>`.

use crate::cli::NoteReadArgs;
use crate::output::{emit, Envelope, OutputMode};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_fs::note::{read_note, ReadOptions, ReadResult};
use std::path::PathBuf;

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &NoteReadArgs) -> Result<()> {
    let resolved = vault_ctx::require(vault_flag)?;
    let vault_info = vault_ctx::info_from(&resolved);

    let opts = ReadOptions {
        section: args.section.clone(),
        frontmatter_only: args.frontmatter_only,
        tasks_only: args.tasks_only,
        limit: args.limit,
    };
    let data = read_note(resolved.root.as_path(), &args.path, &opts)?;

    let envelope = Envelope::ok("note.read", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<ReadResult>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    match d.mode {
        "section" => match &d.content {
            Some(c) => c.clone(),
            None => "Section not found.".to_string(),
        },
        "frontmatter" => match &d.frontmatter {
            Some(v) => serde_yaml::to_string(v)
                .unwrap_or_else(|_| "<unserializable frontmatter>".to_string()),
            None => "No frontmatter.".to_string(),
        },
        "tasks" => match &d.tasks {
            Some(t) if !t.is_empty() => t.join("\n"),
            _ => "No tasks.".to_string(),
        },
        // "body" and any future mode fall through to raw content.
        _ => d.content.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        mode: &'static str,
        content: Option<&str>,
        frontmatter: Option<serde_yaml::Value>,
        tasks: Option<Vec<String>>,
    ) -> Envelope<ReadResult> {
        Envelope::ok(
            "note.read",
            None,
            ReadResult {
                path: PathBuf::from("a.md"),
                mode,
                content: content.map(str::to_string),
                frontmatter,
                tasks,
            },
        )
    }

    #[test]
    fn text_body_prints_content() {
        let e = env("body", Some("line1\nline2"), None, None);
        assert_eq!(render_text(&e), "line1\nline2");
    }

    #[test]
    fn text_section_prints_block() {
        let e = env("section", Some("## Alpha\na1"), None, None);
        assert_eq!(render_text(&e), "## Alpha\na1");
    }

    #[test]
    fn text_section_not_found() {
        let e = env("section", None, None, None);
        assert_eq!(render_text(&e), "Section not found.");
    }

    #[test]
    fn text_frontmatter_renders_yaml() {
        let v: serde_yaml::Value = serde_yaml::from_str("tags: [x]\ncreated: 2026-05-26").unwrap();
        let e = env("frontmatter", None, Some(v), None);
        let s = render_text(&e);
        assert!(s.contains("tags"));
        assert!(s.contains("2026-05-26"));
    }

    #[test]
    fn text_frontmatter_absent() {
        let e = env("frontmatter", None, None, None);
        assert_eq!(render_text(&e), "No frontmatter.");
    }

    #[test]
    fn text_tasks_joins_lines() {
        let e = env(
            "tasks",
            None,
            None,
            Some(vec!["- [ ] a".into(), "- [x] b".into()]),
        );
        assert_eq!(render_text(&e), "- [ ] a\n- [x] b");
    }

    #[test]
    fn text_tasks_empty() {
        let e = env("tasks", None, None, Some(Vec::new()));
        assert_eq!(render_text(&e), "No tasks.");
    }
}
