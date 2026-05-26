use crate::legacy_output::{serialize_for_mode, SessionInitBlock, SessionInitOutput};
use crate::output::OutputMode;
use anyhow::{Context, Result};
use onebrain_cache::{
    clean_stale_state_file, query_unembedded_count, resolve_session_token, ResolveInputs,
};
use onebrain_core::{find_vault_root, load_vault_config, CoreError};
use std::env;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Result of `session init` — either the happy-path metadata or one of two
/// block variants. Kept private; rendering goes through `format_output`.
enum SessionInitResult {
    Ok(SessionInitOutput),
    Block(SessionInitBlock),
}

pub fn run(vault_dir: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    // Bun parity: `--vault-dir <path>` overrides the cwd-based auto-detect.
    let start = match vault_dir {
        Some(dir) => dir,
        None => env::current_dir().context("read current directory")?,
    };
    let line = build_output(&start, mode, query_unembedded_count)?;
    println!("{line}");
    Ok(())
}

/// `qmd_count` is injected so the unembedded figure is deterministic in tests
/// (production passes `query_unembedded_count`, which shells out to qmd). It
/// is only invoked on the happy path when the vault actually uses qmd.
fn build_output(cwd: &Path, mode: &OutputMode, qmd_count: impl Fn() -> usize) -> Result<String> {
    Ok(format_output(&compute_result(cwd, qmd_count)?, mode))
}

fn compute_result(cwd: &Path, qmd_count: impl Fn() -> usize) -> Result<SessionInitResult> {
    // Distinct block reasons for missing-vault vs malformed-yaml — the
    // SessionStart hook routes each to a different recovery path.
    let Some(vault_root) = find_vault_root(cwd) else {
        return Ok(SessionInitResult::Block(SessionInitBlock::init_required()));
    };

    let config = match load_vault_config(&vault_root) {
        Ok(c) => c,
        Err(err) => {
            let block = match &err {
                // YAML present but unparseable → distinct reason so the
                // SessionStart consumer routes to "fix your onebrain.yml".
                CoreError::InvalidYaml(_) => SessionInitBlock::vault_malformed(err.to_string()),
                // Anything else (file gone between walk-up + read, EACCES,
                // NotAVault) keeps the legacy reason for back-compat with
                // the SessionStart hook's current handling.
                _ => SessionInitBlock::init_required(),
            };
            return Ok(SessionInitResult::Block(block));
        }
    };

    // Approximate the process start time before any subprocess work.
    let process_start = SystemTime::now();

    let inputs = ResolveInputs::from_env();
    let token = resolve_session_token(&inputs).context("resolve session token")?;

    // Best-effort cleanup of an orphaned state file from a prior process —
    // mirrors Bun `cleanStaleStateFile`. Failures emit a stderr warning only;
    // they never block session-init.
    clean_stale_state_file(&token, &std::env::temp_dir(), process_start);

    // Unembedded-doc count · queried only when THIS vault actually uses qmd
    // (`qmd_collection` set). Vaults that don't use qmd report 0 rather than
    // leaking the global qmd index's pending count into an unrelated vault's
    // startup. The query itself still degrades to 0 on a missing binary /
    // timeout / unparseable output.
    let qmd_unembedded = if config.qmd_collection.is_some() {
        qmd_count()
    } else {
        0
    };

    let datetime = chrono::Local::now()
        .format("%a · %d %b %Y · %H:%M")
        .to_string();

    Ok(SessionInitResult::Ok(SessionInitOutput {
        datetime,
        session_token: token.to_string(),
        qmd_unembedded,
    }))
}

/// Render `result` for the requested output mode.
///
/// v3.1: text is the default. Machine consumers (Claude Code SessionStart
/// hook) must pass `--json` (or `--yaml` / `--output <fmt>`) explicitly to
/// get the structured envelope. The hook rewriter + init scaffold both add
/// `--json` so existing installs migrate automatically.
fn format_output(result: &SessionInitResult, mode: &OutputMode) -> String {
    if let OutputMode::Text { .. } = mode {
        return render_text(result);
    }
    match result {
        SessionInitResult::Ok(out) => serialize_for_mode(out, mode),
        SessionInitResult::Block(block) => serialize_for_mode(block, mode),
    }
}

fn render_text(result: &SessionInitResult) -> String {
    match result {
        SessionInitResult::Ok(out) => {
            // Single-line happy path → keep tight; multi-line metadata risks
            // pushing useful info offscreen on narrow terminals.
            format!(
                "Session ready · token={token} · datetime={datetime}\nqmd index: {qmd} unembedded",
                token = out.session_token,
                datetime = out.datetime,
                qmd = out.qmd_unembedded,
            )
        }
        SessionInitResult::Block(block) => match block.reason {
            "onebrain-vault-not-found" => {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<cwd>".to_string());
                format!("⚠ No OneBrain vault found at {cwd}\n→ Run `onebrain init` to create one")
            }
            "onebrain-vault-malformed" => {
                let detail = block.error_detail.as_deref().unwrap_or("(no detail)");
                format!(
                    "⚠ OneBrain vault config is malformed: {detail}\n→ Run `onebrain doctor --fix` to attempt auto-repair, or edit onebrain.yml manually"
                )
            }
            other => format!("⚠ Session init blocked: {other}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Explicit JSON mode — what hook consumers see when they pass `--json`
    /// (or v3.0 callers via the auto-migrated settings.json).
    fn json_mode() -> OutputMode {
        OutputMode::Json { pretty: false }
    }

    fn text_mode() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: false,
        }
    }

    #[test]
    fn happy_path_emits_required_fields() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), &json_mode(), || 0).unwrap();
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
        let line = build_output(dir.path(), &json_mode(), || 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found")
        );
        assert!(v.get("datetime").is_none());
        assert!(v.get("session_token").is_none());
    }

    #[test]
    fn block_path_when_vault_yml_malformed() {
        // Malformed YAML reports `onebrain-vault-malformed` so SessionStart
        // consumers route to "fix your onebrain.yml" instead of "/onboarding".
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid\n").unwrap();

        let line = build_output(dir.path(), &json_mode(), || 0).unwrap();
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
        // Counterpart to the previous test: missing vault.yml emits the
        // `onebrain-vault-not-found` reason (renamed from `init-required`
        // in v3.1) and skips the `error_detail` field.
        let dir = tempdir().unwrap();
        let line = build_output(dir.path(), &json_mode(), || 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found")
        );
        assert!(
            v.get("error_detail").is_none(),
            "vault-not-found block must not carry error_detail"
        );
    }

    #[test]
    fn collection_absent_reports_zero_without_querying_qmd() {
        // Gating guard: a vault with no `qmd_collection` reports 0 and must NOT
        // leak the global qmd index's pending count. The injected closure
        // returns 99, yet the field stays 0 because the query is skipped.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "# no qmd_collection\n").unwrap();

        let line = build_output(dir.path(), &json_mode(), || 99).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("qmd_unembedded").and_then(|n| n.as_u64()),
            Some(0),
            "collection-absent vault must not query qmd"
        );
    }

    #[test]
    fn collection_set_surfaces_the_queried_count() {
        // When the vault uses qmd, the queried unembedded count flows through
        // verbatim. Injected so the test is deterministic regardless of whether
        // a real qmd is installed on the dev machine.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), &json_mode(), || 7).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("qmd_unembedded").and_then(|n| n.as_u64()),
            Some(7),
            "collection-set vault must surface the queried count"
        );
    }

    #[test]
    fn block_path_emits_yaml_when_mode_is_yaml() {
        // v3.1: --yaml / --output yaml flips the hook-protocol block to
        // YAML. Default stays JSON (verified above).
        let dir = tempdir().unwrap();
        let line = build_output(dir.path(), &OutputMode::Yaml, || 0).unwrap();
        // Parse the YAML to assert structure rather than string-matching
        // (serde_yaml's emitter formatting is implementation-defined).
        let v: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
        assert_eq!(
            v.get("decision").and_then(|d| d.as_str()),
            Some("block"),
            "yaml block missing decision; got: {line}"
        );
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found"),
            "yaml block missing reason; got: {line}"
        );
        // Defensive: must NOT look like JSON (no leading `{`).
        assert!(
            !line.trim_start().starts_with('{'),
            "expected YAML, got JSON-shaped output: {line}"
        );
    }

    #[test]
    fn happy_path_emits_yaml_when_mode_is_yaml() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();
        let line = build_output(dir.path(), &OutputMode::Yaml, || 0).unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
        assert!(v.get("datetime").and_then(|d| d.as_str()).is_some());
        assert!(v.get("session_token").and_then(|s| s.as_str()).is_some());
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert!(
            !line.trim_start().starts_with('{'),
            "expected YAML, got JSON-shaped output: {line}"
        );
    }

    // ── v3.1: text is the new default ────────────────────────────────────

    #[test]
    fn default_outside_vault_emits_text_not_json() {
        let dir = tempdir().unwrap();
        let line = build_output(dir.path(), &text_mode(), || 0).unwrap();
        assert!(
            !line.trim_start().starts_with('{'),
            "default mode must NOT emit JSON braces; got: {line}"
        );
        assert!(
            line.contains("No OneBrain vault found"),
            "expected human-readable not-found marker; got: {line}"
        );
        assert!(
            line.contains("onebrain init"),
            "expected init suggestion; got: {line}"
        );
    }

    #[test]
    fn default_inside_vault_emits_text_success() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();
        let line = build_output(dir.path(), &text_mode(), || 0).unwrap();
        assert!(
            !line.trim_start().starts_with('{'),
            "default mode must NOT emit JSON braces; got: {line}"
        );
        assert!(
            line.contains("Session ready"),
            "expected `Session ready` marker; got: {line}"
        );
        assert!(line.contains("token="), "expected token field; got: {line}");
    }

    #[test]
    fn default_on_malformed_vault_emits_text() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid\n").unwrap();
        let line = build_output(dir.path(), &text_mode(), || 0).unwrap();
        assert!(!line.trim_start().starts_with('{'), "got: {line}");
        assert!(line.contains("malformed"), "got: {line}");
        assert!(line.contains("onebrain doctor"), "got: {line}");
    }

    #[test]
    fn json_pretty_emits_indented_multiline() {
        let dir = tempdir().unwrap();
        // Block path is simplest — no volatile fields to assert against.
        let line = build_output(dir.path(), &OutputMode::Json { pretty: true }, || 0).unwrap();
        // Pretty JSON contains newlines + 2-space indent.
        assert!(
            line.contains('\n'),
            "expected multi-line indented JSON; got: {line}"
        );
        assert!(
            line.contains("  \"decision\""),
            "expected 2-space indent on `decision`; got: {line}"
        );
        // Still parseable.
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["reason"], "onebrain-vault-not-found");
    }
}
