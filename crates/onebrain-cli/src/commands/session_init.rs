use crate::output::{SessionInitBlock, SessionInitOutput};
use anyhow::{Context, Result};
use onebrain_cache::{
    clean_stale_state_file, query_unembedded_count, resolve_session_token, ResolveInputs,
};
use onebrain_core::{find_vault_root, load_vault_config};
use std::env;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub fn run(vault_dir: Option<PathBuf>) -> Result<()> {
    // Bun parity: `--vault-dir <path>` overrides the cwd-based auto-detect.
    let start = match vault_dir {
        Some(dir) => dir,
        None => env::current_dir().context("read current directory")?,
    };
    let line = build_output(&start)?;
    println!("{line}");
    Ok(())
}

fn build_output(cwd: &Path) -> Result<String> {
    // Bun parity: block on EITHER missing vault.yml OR malformed vault.yml.
    let Some(vault_root) = find_vault_root(cwd) else {
        let block = SessionInitBlock::init_required();
        return Ok(serde_json::to_string(&block)?);
    };

    if load_vault_config(&vault_root).is_err() {
        let block = SessionInitBlock::init_required();
        return Ok(serde_json::to_string(&block)?);
    }

    // Approximate the process start time before any subprocess work.
    let process_start = SystemTime::now();

    let inputs = ResolveInputs::from_env();
    let token = resolve_session_token(&inputs).context("resolve session token")?;

    // Best-effort cleanup of an orphaned state file from a prior process —
    // mirrors Bun `cleanStaleStateFile`. Failures emit a stderr warning only;
    // they never block session-init.
    clean_stale_state_file(&token, &std::env::temp_dir(), process_start);

    // qmd query is unconditional · subprocess returns 0 on missing binary,
    // missing collection, timeout, or unparseable output — matches Bun.
    let qmd_unembedded = query_unembedded_count();

    let datetime = chrono::Local::now()
        .format("%a · %d %b %Y · %H:%M")
        .to_string();

    let output = SessionInitOutput {
        datetime,
        session_token: token.to_string(),
        qmd_unembedded,
    };
    Ok(serde_json::to_string(&output)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn happy_path_emits_required_fields() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert!(v.get("datetime").and_then(|d| d.as_str()).is_some());
        assert!(v.get("session_token").and_then(|s| s.as_str()).is_some());
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert!(
            v.get("decision").is_none(),
            "happy path must not include decision field"
        );
    }

    #[test]
    fn block_path_when_no_vault_yml_found() {
        let dir = tempdir().unwrap();
        // No vault.yml anywhere.
        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-init-required")
        );
        assert!(v.get("datetime").is_none());
        assert!(v.get("session_token").is_none());
    }

    #[test]
    fn block_path_when_vault_yml_malformed() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid\n").unwrap();

        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-init-required")
        );
    }

    #[test]
    fn happy_path_emits_qmd_unembedded_field_even_when_collection_absent() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "# no qmd_collection\n").unwrap();

        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        // qmd binary almost certainly not installed in test env → 0.
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
    }
}
