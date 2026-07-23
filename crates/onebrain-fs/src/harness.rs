//! Harness detection · which AI runtime(s) does this vault target?

use onebrain_core::Harness;
use std::io::Write;
use std::path::Path;

/// Detect which AI runtime harness(es) are in use for the given vault.
///
/// Priority:
/// 1. `ONEBRAIN_HARNESS` env var override (single value · returned as a 1-element Vec)
///    - `claude` or `claude-code` → `[Claude]`
///    - `gemini` → `[Gemini]`
///    - `codex` → `[Codex]`
///    - `direct` → `[Direct]`
///    - Any other value → stderr warning + fall through to dir detection
/// 2. `.claude/`, `.gemini/`, and `.codex/` directory presence (independent)
/// 3. Fallback `[Direct]`
///
/// Always returns at least one harness.
pub fn detect_harnesses(vault_root: &Path) -> Vec<Harness> {
    detect_harnesses_with_env(vault_root, std::env::var("ONEBRAIN_HARNESS").ok())
}

/// Inner driver · accepts env override directly. Tests use this to inject values
/// without mutating the global process env.
pub(crate) fn detect_harnesses_with_env(
    vault_root: &Path,
    env_override: Option<String>,
) -> Vec<Harness> {
    if let Some(value) = env_override {
        match value.as_str() {
            "claude" | "claude-code" => return vec![Harness::Claude],
            "gemini" => return vec![Harness::Gemini],
            "codex" => return vec![Harness::Codex],
            "direct" => return vec![Harness::Direct],
            other => {
                let _ = writeln!(
                    std::io::stderr(),
                    "harness: unknown ONEBRAIN_HARNESS value \"{other}\" — ignoring, falling back to directory detection"
                );
            }
        }
    }

    let mut detected = Vec::new();
    if vault_root.join(".claude").exists() {
        detected.push(Harness::Claude);
    }
    if vault_root.join(".gemini").exists() {
        detected.push(Harness::Gemini);
    }
    if vault_root.join(".codex").exists() {
        detected.push(Harness::Codex);
    }

    if detected.is_empty() {
        return vec![Harness::Direct];
    }
    detected
}

/// Backward-compat shim · returns first detected harness (defaults to Direct).
pub fn detect_harness(vault_root: &Path) -> Harness {
    detect_harnesses(vault_root)
        .into_iter()
        .next()
        .unwrap_or(Harness::Direct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn mk_dir(parent: &Path, name: &str) {
        fs::create_dir(parent.join(name)).unwrap();
    }

    // --- detect_harnesses · directory detection ---

    #[test]
    fn both_dirs_present_returns_claude_then_gemini() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".claude");
        mk_dir(dir.path(), ".gemini");
        let result = detect_harnesses_with_env(dir.path(), None);
        assert_eq!(result, vec![Harness::Claude, Harness::Gemini]);
    }

    #[test]
    fn only_claude_dir_returns_claude() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".claude");
        assert_eq!(
            detect_harnesses_with_env(dir.path(), None),
            vec![Harness::Claude]
        );
    }

    #[test]
    fn only_gemini_dir_returns_gemini() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".gemini");
        assert_eq!(
            detect_harnesses_with_env(dir.path(), None),
            vec![Harness::Gemini]
        );
    }

    #[test]
    fn only_codex_dir_returns_codex() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".codex");
        assert_eq!(
            detect_harnesses_with_env(dir.path(), None),
            vec![Harness::Codex]
        );
    }

    #[test]
    fn neither_dir_returns_direct() {
        let dir = tempdir().unwrap();
        assert_eq!(
            detect_harnesses_with_env(dir.path(), None),
            vec![Harness::Direct]
        );
    }

    // --- detect_harnesses · env override ---

    #[test]
    fn env_claude_overrides_dir_detection() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".gemini"); // would otherwise return Gemini
        let result = detect_harnesses_with_env(dir.path(), Some("claude".to_string()));
        assert_eq!(result, vec![Harness::Claude]);
    }

    #[test]
    fn env_claude_code_alias_returns_claude() {
        let dir = tempdir().unwrap();
        let result = detect_harnesses_with_env(dir.path(), Some("claude-code".to_string()));
        assert_eq!(result, vec![Harness::Claude]);
    }

    #[test]
    fn env_gemini_returns_gemini() {
        let dir = tempdir().unwrap();
        let result = detect_harnesses_with_env(dir.path(), Some("gemini".to_string()));
        assert_eq!(result, vec![Harness::Gemini]);
    }

    #[test]
    fn env_codex_returns_codex() {
        let dir = tempdir().unwrap();
        let result = detect_harnesses_with_env(dir.path(), Some("codex".to_string()));
        assert_eq!(result, vec![Harness::Codex]);
    }

    #[test]
    fn env_direct_returns_direct() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".claude"); // would otherwise return Claude
        let result = detect_harnesses_with_env(dir.path(), Some("direct".to_string()));
        assert_eq!(result, vec![Harness::Direct]);
    }

    #[test]
    fn env_garbage_falls_back_to_dir_detection() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".claude");
        let result = detect_harnesses_with_env(dir.path(), Some("garbage".to_string()));
        assert_eq!(result, vec![Harness::Claude]);
    }

    // --- detect_harness (singular shim) ---

    #[test]
    fn detect_harness_returns_first_when_both_dirs_present() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".claude");
        mk_dir(dir.path(), ".gemini");
        assert_eq!(detect_harness(dir.path()), Harness::Claude);
    }

    #[test]
    fn detect_harness_returns_direct_when_neither_dir_present() {
        let dir = tempdir().unwrap();
        assert_eq!(detect_harness(dir.path()), Harness::Direct);
    }

    #[test]
    fn detect_harness_only_gemini_returns_gemini() {
        let dir = tempdir().unwrap();
        mk_dir(dir.path(), ".gemini");
        assert_eq!(detect_harness(dir.path()), Harness::Gemini);
    }
}
