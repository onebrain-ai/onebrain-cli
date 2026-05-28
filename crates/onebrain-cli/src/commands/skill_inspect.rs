//! `onebrain skill info <NAME>` and `onebrain skill show <NAME>` — read a
//! skill's `SKILL.md` from the vault plugin (`.claude/plugins/onebrain/skills/
//! <name>/SKILL.md`), split it into frontmatter + body, and emit the relevant
//! piece. Used by skill-scripting workflows that introspect a skill before
//! invoking it.
//!
//! Naming: `show` (not `help`) intentionally — `--help` flags carry clap's CLI
//! usage screen, while `show` renders the skill's workflow markdown. Keeping
//! those concepts on different verbs avoids the "which help is this?"
//! confusion the old `skill help` verb caused in v3.2.10.
//!
//! Exit codes:
//!
//! - `64` (EX_USAGE)   — empty skill name
//! - `66` (EX_NOINPUT) — skill file not found at the expected path
//! - `78` (EX_CONFIG)  — vault has no `onebrain.yml`
//! - `65` (EX_DATAERR) — frontmatter present but unparseable YAML
//! - `0`               — success
//!
//! Text mode renders human-readable; `--json` (or `--yaml`) renders the
//! structured frontmatter (info) or `{ "body": "..." }` (show).

use crate::legacy_output::serialize_for_mode;
use crate::output::OutputMode;
use anyhow::Result;
use onebrain_core::find_config_file;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Frontmatter we recognise on a OneBrain skill's `SKILL.md`. All fields are
/// optional except `name` (which we derive from the directory if the
/// frontmatter is missing or sparse).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `true` if the skill can be scheduled via `onebrain schedule` without
    /// arguments.
    pub schedulable: Option<bool>,
    /// `true` if the skill can be scheduled but requires arguments (declared
    /// in `required_args`).
    pub schedulable_with_args: Option<bool>,
    pub required_args: Option<Vec<String>>,
}

/// Result of parsing a `SKILL.md` — the frontmatter + the body that follows.
#[derive(Debug, Clone)]
pub struct ParsedSkill {
    pub name: String,
    pub frontmatter: SkillFrontmatter,
    pub body: String,
}

/// Strip a leading `/` from a skill name (`/daily` and `daily` both reach the
/// same `skills/daily/SKILL.md`).
fn normalize_name(raw: &str) -> &str {
    raw.strip_prefix('/').unwrap_or(raw)
}

/// Build the path to a skill's `SKILL.md` under the vault's plugin tree.
fn skill_path(vault: &Path, name: &str) -> PathBuf {
    vault
        .join(".claude")
        .join("plugins")
        .join("onebrain")
        .join("skills")
        .join(name)
        .join("SKILL.md")
}

/// Parse a `SKILL.md` byte buffer into `(frontmatter, body)`. If the file
/// doesn't start with a `---\n` YAML frontmatter block, treat the entire
/// content as the body and return a default (empty) frontmatter — many skills
/// might not have frontmatter and a missing block is not an error.
pub(crate) fn split_frontmatter(text: &str) -> Result<(SkillFrontmatter, String)> {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate BOM
    if let Some(rest) = trimmed.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let yaml = &rest[..end];
            let body = &rest[end + "\n---\n".len()..];
            let front: SkillFrontmatter = serde_yaml::from_str(yaml)
                .map_err(|e| anyhow::anyhow!("SKILL.md frontmatter is not valid YAML: {e}"))?;
            return Ok((front, body.to_string()));
        }
    }
    Ok((SkillFrontmatter::default(), trimmed.to_string()))
}

/// Read + parse the skill at `<vault>/.claude/plugins/onebrain/skills/<name>/SKILL.md`.
/// Returns `Ok(Err(exit_code))` for the "expected" failure modes (missing
/// skill, malformed yaml) so the dispatcher can surface a sysexits code
/// rather than a generic Err. Hard IO errors still bubble up as anyhow.
pub(crate) fn load_skill(vault: &Path, name: &str) -> Result<Result<ParsedSkill, i32>> {
    let name = normalize_name(name);
    if name.is_empty() {
        eprintln!("skill: name must not be empty");
        return Ok(Err(64));
    }
    if find_config_file(vault).is_none() {
        eprintln!(
            "Vault not found at {} (no onebrain.yml present)",
            vault.display()
        );
        return Ok(Err(78));
    }
    let path = skill_path(vault, name);
    let bytes = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skill: no SKILL.md at {}", path.display());
            return Ok(Err(66));
        }
        Err(e) => {
            eprintln!("skill: reading {}: {e}", path.display());
            return Ok(Err(66));
        }
    };
    let (mut frontmatter, body) = match split_frontmatter(&bytes) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("skill {name}: {e}");
            return Ok(Err(65));
        }
    };
    // Frontmatter `name:` is convention but we trust the directory name as
    // the source of truth when the YAML omits it.
    if frontmatter.name.is_none() {
        frontmatter.name = Some(name.to_string());
    }
    Ok(Ok(ParsedSkill {
        name: name.to_string(),
        frontmatter,
        body,
    }))
}

// ─────────────────────────────────────────────────────────────────────────
// skill info — emit the frontmatter
// ─────────────────────────────────────────────────────────────────────────

pub fn info_run(vault_root: &Path, name: &str, mode: &OutputMode) -> Result<i32> {
    let skill = match load_skill(vault_root, name)? {
        Ok(s) => s,
        Err(code) => return Ok(code),
    };
    let output = SkillInfoOutput {
        name: skill
            .frontmatter
            .name
            .clone()
            .unwrap_or_else(|| skill.name.clone()),
        description: skill.frontmatter.description.clone(),
        schedulable: skill.frontmatter.schedulable,
        schedulable_with_args: skill.frontmatter.schedulable_with_args,
        required_args: skill.frontmatter.required_args.clone(),
    };
    println!("{}", format_info(&output, mode));
    Ok(0)
}

#[derive(Debug, Serialize)]
struct SkillInfoOutput {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedulable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedulable_with_args: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_args: Option<Vec<String>>,
}

fn format_info(out: &SkillInfoOutput, mode: &OutputMode) -> String {
    if let OutputMode::Text { .. } = mode {
        let mut lines = vec![format!("name:        {}", out.name)];
        if let Some(d) = &out.description {
            lines.push(format!("description: {d}"));
        }
        if let Some(s) = out.schedulable {
            lines.push(format!("schedulable: {s}"));
        }
        if let Some(s) = out.schedulable_with_args {
            lines.push(format!("schedulable_with_args: {s}"));
        }
        if let Some(args) = &out.required_args {
            lines.push(format!("required_args: {}", args.join(", ")));
        }
        return lines.join("\n");
    }
    serialize_for_mode(out, mode)
}

// ─────────────────────────────────────────────────────────────────────────
// skill show — emit the SKILL.md body (the human-readable workflow)
// ─────────────────────────────────────────────────────────────────────────

pub fn show_run(vault_root: &Path, name: &str, mode: &OutputMode) -> Result<i32> {
    let skill = match load_skill(vault_root, name)? {
        Ok(s) => s,
        Err(code) => return Ok(code),
    };
    let body = skill.body.trim_start_matches('\n').to_string();
    let output = SkillShowOutput {
        name: skill
            .frontmatter
            .name
            .clone()
            .unwrap_or_else(|| skill.name.clone()),
        body,
    };
    println!("{}", format_show(&output, mode));
    Ok(0)
}

#[derive(Debug, Serialize)]
struct SkillShowOutput {
    name: String,
    body: String,
}

fn format_show(out: &SkillShowOutput, mode: &OutputMode) -> String {
    if let OutputMode::Text { .. } = mode {
        // Text mode: just dump the markdown body — it's already
        // human-readable. Don't decorate; callers piping into pagers shouldn't
        // get a header glued on.
        return out.body.clone();
    }
    serialize_for_mode(out, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_vault_with_skill(name: &str, skill_md: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("onebrain.yml"), "qmd_collection: ob\n").unwrap();
        let skill_dir = root
            .join(".claude")
            .join("plugins")
            .join("onebrain")
            .join("skills")
            .join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        (dir, root)
    }

    fn text_mode() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: false,
        }
    }

    fn json_mode() -> OutputMode {
        OutputMode::Json { pretty: false }
    }

    #[test]
    fn split_frontmatter_parses_a_typical_skill() {
        let text =
            "---\nname: daily\ndescription: A briefing\nschedulable: true\n---\n# Daily\n\nBody.";
        let (front, body) = split_frontmatter(text).unwrap();
        assert_eq!(front.name.as_deref(), Some("daily"));
        assert_eq!(front.description.as_deref(), Some("A briefing"));
        assert_eq!(front.schedulable, Some(true));
        assert!(body.starts_with("# Daily"));
    }

    #[test]
    fn split_frontmatter_treats_skill_without_frontmatter_as_pure_body() {
        let text = "# Just a body\n\nNo frontmatter here.";
        let (front, body) = split_frontmatter(text).unwrap();
        assert!(front.name.is_none());
        assert_eq!(body, text);
    }

    #[test]
    fn info_run_emits_frontmatter_text() {
        let (_dir, vault) = make_vault_with_skill(
            "daily",
            "---\nname: daily\ndescription: D\nschedulable: true\n---\nbody",
        );
        let code = info_run(&vault, "daily", &text_mode()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn info_run_emits_json() {
        let (_dir, vault) = make_vault_with_skill(
            "daily",
            "---\nname: daily\ndescription: D\nschedulable: true\n---\nbody",
        );
        let code = info_run(&vault, "daily", &json_mode()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn show_run_emits_body() {
        let (_dir, vault) = make_vault_with_skill(
            "daily",
            "---\nname: daily\n---\n# Daily Briefing\n\nDoing the daily.",
        );
        let code = show_run(&vault, "daily", &text_mode()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn missing_skill_returns_66() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "qmd_collection: ob\n").unwrap();
        let code = info_run(dir.path(), "nonexistent", &text_mode()).unwrap();
        assert_eq!(code, 66);
    }

    #[test]
    fn missing_vault_returns_78() {
        let dir = tempdir().unwrap();
        let code = info_run(dir.path(), "daily", &text_mode()).unwrap();
        assert_eq!(code, 78);
    }

    #[test]
    fn empty_name_returns_64() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("onebrain.yml"), "qmd_collection: ob\n").unwrap();
        let code = info_run(dir.path(), "/", &text_mode()).unwrap();
        assert_eq!(code, 64);
    }

    #[test]
    fn malformed_yaml_returns_65() {
        let (_dir, vault) = make_vault_with_skill("broken", "---\nname: : : invalid\n---\nbody");
        let code = info_run(&vault, "broken", &text_mode()).unwrap();
        assert_eq!(code, 65);
    }

    #[test]
    fn name_with_leading_slash_works() {
        let (_dir, vault) = make_vault_with_skill("daily", "---\nname: daily\n---\n# body");
        let code = info_run(&vault, "/daily", &text_mode()).unwrap();
        assert_eq!(code, 0);
    }
}
