use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct PluginFilesCheck;

const REQUIRED_PLUGIN_FILES: &[&str] = &["INSTRUCTIONS.md", ".claude-plugin/plugin.json"];
const REQUIRED_PLUGIN_DIRS: &[&str] = &["agents", "skills"];
const STALE_BASH_FILES: &[&str] = &[
    "session-init.sh",
    "orphan-scan.sh",
    "checkpoint-hook.sh",
    "vault-sync.sh",
    "pin-to-vault.sh",
    "qmd-reindex.sh",
    "backfill-recapped.sh",
];

impl Check for PluginFilesCheck {
    fn name(&self) -> &'static str {
        "plugin-files"
    }

    fn run(&self, vault_root: &Path, _config: &VaultConfig) -> DoctorResult {
        let plugin_base = vault_root.join(".claude").join("plugins").join("onebrain");

        let mut missing: Vec<String> = Vec::new();

        // Required files
        for rel in REQUIRED_PLUGIN_FILES {
            if !plugin_base.join(rel).is_file() {
                missing.push((*rel).to_string());
            }
        }

        // Required dirs (with non-empty .md check)
        for dir in REQUIRED_PLUGIN_DIRS {
            let p = plugin_base.join(dir);
            if !p.is_dir() {
                missing.push(format!("{}/", dir));
            } else if !has_any_md(&p) {
                missing.push(format!("{}/ (empty)", dir));
            }
        }

        let stale: Vec<String> = STALE_BASH_FILES
            .iter()
            .filter(|n| plugin_base.join(n).is_file())
            .map(|n| (*n).to_string())
            .collect();

        // Output
        if !missing.is_empty() {
            let msg = format!("missing: {}", missing.join(", "));
            let hint = "Run onebrain update to restore plugin files";
            let mut details: Vec<String> =
                missing.iter().map(|f| format!("missing: {}", f)).collect();
            details.push(hint.to_string());
            return DoctorResult::error("plugin-files", msg)
                .with_hint(hint)
                .with_details(details);
        }
        if !stale.is_empty() {
            let msg = format!("stale bash files: {}", stale.join(", "));
            let hint = "Run onebrain update to remove stale files";
            let mut details: Vec<String> = stale.iter().map(|f| format!("stale: {}", f)).collect();
            details.push(hint.to_string());
            return DoctorResult::warn("plugin-files", msg)
                .with_hint(hint)
                .with_details(details);
        }

        // Counts for ok detail
        let skills_dir = plugin_base.join("skills");
        let agents_dir = plugin_base.join("agents");
        let skill_count = count_skill_md(&skills_dir);
        let agent_count = count_agents_md(&agents_dir);
        let detail = format!(
            "{} skills · {} agents · INSTRUCTIONS.md ✓",
            skill_count, agent_count
        );

        DoctorResult::ok("plugin-files", "all required files present").with_details(vec![detail])
    }
}

fn has_any_md(dir: &Path) -> bool {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().is_file())
        .any(|e| e.file_name().to_string_lossy().ends_with(".md"))
}

/// Count files matching `*/SKILL.md` under skills/ — i.e. each direct subdir
/// that contains a SKILL.md.
fn count_skill_md(skills_dir: &Path) -> usize {
    if !skills_dir.is_dir() {
        return 0;
    }
    let mut count = 0usize;
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return 0;
    };
    for e in entries.flatten() {
        let p: PathBuf = e.path();
        if p.is_dir() && p.join("SKILL.md").is_file() {
            count += 1;
        }
    }
    count
}

/// Count `*.md` directly under agents/ (non-recursive).
fn count_agents_md(agents_dir: &Path) -> usize {
    if !agents_dir.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cfg() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
        }
    }

    fn build_clean_plugin(root: &Path) {
        let base = root.join(".claude/plugins/onebrain");
        std::fs::create_dir_all(base.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(base.join("agents")).unwrap();
        std::fs::create_dir_all(base.join("skills/foo")).unwrap();
        std::fs::create_dir_all(base.join("skills/bar")).unwrap();
        std::fs::write(base.join("INSTRUCTIONS.md"), "x").unwrap();
        std::fs::write(base.join(".claude-plugin/plugin.json"), "{}").unwrap();
        std::fs::write(base.join("agents/a.md"), "x").unwrap();
        std::fs::write(base.join("agents/b.md"), "x").unwrap();
        std::fs::write(base.join("agents/c.md"), "x").unwrap();
        std::fs::write(base.join("skills/foo/SKILL.md"), "x").unwrap();
        std::fs::write(base.join("skills/bar/SKILL.md"), "x").unwrap();
    }

    #[test]
    fn all_clean_reports_ok_with_counts() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "all required files present");
        assert!(r
            .details
            .iter()
            .any(|s| s.contains("2 skills") && s.contains("3 agents")));
    }

    #[test]
    fn missing_instructions_md_is_error() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        std::fs::remove_file(d.path().join(".claude/plugins/onebrain/INSTRUCTIONS.md")).unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert!(r.message.contains("INSTRUCTIONS.md"));
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain update to restore plugin files")
        );
    }

    #[test]
    fn missing_agents_dir_is_error() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        std::fs::remove_dir_all(d.path().join(".claude/plugins/onebrain/agents")).unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert!(r.message.contains("agents/"));
    }

    #[test]
    fn empty_skills_dir_is_error() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        std::fs::remove_dir_all(d.path().join(".claude/plugins/onebrain/skills/foo")).unwrap();
        std::fs::remove_dir_all(d.path().join(".claude/plugins/onebrain/skills/bar")).unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert!(r.message.contains("skills/ (empty)"));
    }

    #[test]
    fn stale_bash_file_is_warning() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        std::fs::write(
            d.path().join(".claude/plugins/onebrain/session-init.sh"),
            "x",
        )
        .unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert!(r.message.contains("stale bash files"));
        assert!(r.message.contains("session-init.sh"));
    }

    #[test]
    fn missing_takes_precedence_over_stale() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        std::fs::write(
            d.path().join(".claude/plugins/onebrain/session-init.sh"),
            "x",
        )
        .unwrap();
        std::fs::remove_file(d.path().join(".claude/plugins/onebrain/INSTRUCTIONS.md")).unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        // Should report missing, not stale
        assert!(r.message.contains("missing"));
        assert!(!r.message.contains("stale"));
    }

    /// `agents/` exists but contains only non-.md files → `has_any_md` returns
    /// false → error "agents/ (empty)". Covers the `.push(format!("{}/ (empty)",
    /// dir))` branch for the agents directory.
    #[test]
    fn empty_agents_dir_no_md_is_error() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        let base = d.path().join(".claude/plugins/onebrain/agents");
        std::fs::remove_file(base.join("a.md")).unwrap();
        std::fs::remove_file(base.join("b.md")).unwrap();
        std::fs::remove_file(base.join("c.md")).unwrap();
        std::fs::write(base.join("notes.txt"), "x").unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert!(r.message.contains("agents/ (empty)"), "msg: {}", r.message);
        assert_eq!(
            r.hint.as_deref(),
            Some("Run onebrain update to restore plugin files")
        );
    }

    /// `skills/` has a subdir WITHOUT a `SKILL.md` file: `count_skill_md` must
    /// not count it. Covers the `p.is_dir() && p.join("SKILL.md").is_file()`
    /// false branch inside `count_skill_md`.
    #[test]
    fn skill_subdir_without_skill_md_is_not_counted() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        // Add a skills subdir that has a .md file but no SKILL.md
        let orphan = d.path().join(".claude/plugins/onebrain/skills/baz");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("README.md"), "x").unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "all required files present");
        // baz/ has no SKILL.md → only foo and bar count
        assert!(
            r.details.iter().any(|s| s.contains("2 skills")),
            "details: {:?}",
            r.details
        );
    }

    /// A subdirectory inside `agents/` is not a file and must not be counted
    /// by `count_agents_md`. Covers the `e.file_type().map(|t| t.is_file())`
    /// false branch (subdirs filtered out).
    #[test]
    fn agents_subdir_is_not_counted_as_agent() {
        let d = tempdir().unwrap();
        build_clean_plugin(d.path());
        std::fs::create_dir_all(d.path().join(".claude/plugins/onebrain/agents/subdir")).unwrap();
        let r = PluginFilesCheck.run(d.path(), &cfg());
        assert_eq!(r.message, "all required files present");
        // subdir is not a file → filtered; the 3 original .md agent files remain
        assert!(
            r.details.iter().any(|s| s.contains("3 agents")),
            "details: {:?}",
            r.details
        );
    }
}
