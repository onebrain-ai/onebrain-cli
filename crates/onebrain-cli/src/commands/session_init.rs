use crate::legacy_output::{SessionInitBlock, SessionInitOutput};
use anyhow::{Context, Result};
use onebrain_cache::{
    clean_stale_state_file, query_unembedded_count, resolve_session_token, ResolveInputs,
};
use onebrain_core::{find_vault_root, load_vault_config, CoreError};
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
    // R1 C2: distinct block reasons for missing-vault vs malformed-yaml.
    let Some(vault_root) = find_vault_root(cwd) else {
        let block = SessionInitBlock::init_required();
        return Ok(serde_json::to_string(&block)?);
    };

    if let Err(err) = load_vault_config(&vault_root) {
        let block = match &err {
            // YAML present but unparseable → distinct reason so the
            // SessionStart consumer routes to "fix your vault.yml".
            CoreError::InvalidYaml(_) => SessionInitBlock::vault_malformed(err.to_string()),
            // Anything else (file gone between walk-up + read, EACCES,
            // NotAVault) keeps the legacy reason for back-compat with
            // the SessionStart hook's current handling.
            _ => SessionInitBlock::init_required(),
        };
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
        // R1 C2: malformed YAML now reports a distinct `onebrain-vault-malformed`
        // reason so SessionStart consumers can route to "fix your vault.yml"
        // instead of "/onboarding".
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid\n").unwrap();

        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-malformed")
        );
        // error_detail surfaces the parse-error message.
        assert!(
            v.get("error_detail")
                .and_then(|d| d.as_str())
                .is_some_and(|s| !s.is_empty()),
            "expected non-empty error_detail; got {v:?}"
        );
    }

    #[test]
    fn block_path_when_no_vault_yml_omits_error_detail() {
        // Counterpart to the previous test: missing vault.yml keeps the
        // legacy `init-required` reason and skips the `error_detail` field.
        let dir = tempdir().unwrap();
        let line = build_output(dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-init-required")
        );
        assert!(
            v.get("error_detail").is_none(),
            "init-required block must not carry error_detail"
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
