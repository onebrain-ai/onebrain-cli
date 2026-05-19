use crate::output::{SessionInitBlock, SessionInitOutput};
use anyhow::{Context, Result};
use onebrain_cache::{resolve_session_token, ResolveInputs};
use onebrain_core::{find_vault_root, load_vault_config};
use std::env;
use std::path::Path;

pub fn run() -> Result<()> {
    let cwd = env::current_dir().context("read current directory")?;
    let line = build_output(&cwd)?;
    println!("{line}");
    Ok(())
}

fn build_output(cwd: &Path) -> Result<String> {
    let Some(vault_root) = find_vault_root(cwd) else {
        let block = SessionInitBlock::init_required();
        return Ok(serde_json::to_string(&block)?);
    };

    let config = load_vault_config(&vault_root).context("parse vault.yml")?;

    let cache_dir = vault_root.join(".onebrain-cache");
    let inputs = ResolveInputs::from_env();
    let token = resolve_session_token(&cache_dir, &inputs).context("resolve session token")?;

    let qmd_unembedded = match &config.qmd_collection {
        Some(name) => {
            onebrain_fs::count_unembedded(&vault_root, name).context("count unembedded qmd docs")?
        }
        None => 0,
    };

    let datetime = chrono::Local::now()
        .format("%a · %d %b %Y · %H:%M")
        .to_string();

    let output = SessionInitOutput {
        datetime,
        session_token: token.as_str().to_string(),
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
    fn happy_path_omits_qmd_when_collection_absent() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "# no qmd_collection\n").unwrap();

        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
    }
}
