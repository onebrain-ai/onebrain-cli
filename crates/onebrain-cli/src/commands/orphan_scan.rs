use crate::legacy_output::serialize_for_mode;
use crate::output::OutputMode;
use anyhow::{Context, Result};
use onebrain_fs::scan_orphans;
use std::env;
use std::path::Path;

pub fn run(logs_folder: &str, session_token: &str, mode: &OutputMode) -> Result<()> {
    let cwd = env::current_dir().context("read current directory")?;
    let line = build_output(Path::new(logs_folder), session_token, &cwd, mode)?;
    println!("{line}");
    Ok(())
}

fn build_output(
    logs_folder: &Path,
    session_token: &str,
    vault_root: &Path,
    mode: &OutputMode,
) -> Result<String> {
    let result = scan_orphans(logs_folder, session_token, chrono::Local::now(), vault_root)
        .context("scan orphan checkpoint files")?;
    Ok(serialize_for_mode(&result, mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn json_mode() -> OutputMode {
        OutputMode::Json { pretty: false }
    }

    #[test]
    fn empty_logs_emits_zero_count() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("checkpoint")).unwrap();
        let line = build_output(dir.path(), "abc12345", dir.path(), &json_mode()).unwrap();
        assert_eq!(line, "{\"orphan_count\":0}");
    }

    #[test]
    fn output_is_valid_json_with_orphan_count_field() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("checkpoint")).unwrap();
        let line = build_output(dir.path(), "abc12345", dir.path(), &json_mode()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("orphan_count").is_some());
    }

    #[test]
    fn yaml_mode_emits_yaml() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("checkpoint")).unwrap();
        let line = build_output(dir.path(), "abc12345", dir.path(), &OutputMode::Yaml).unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
        assert_eq!(v.get("orphan_count").and_then(|n| n.as_u64()), Some(0));
        assert!(
            !line.trim_start().starts_with('{'),
            "expected YAML, got JSON-shaped output: {line}"
        );
    }
}
