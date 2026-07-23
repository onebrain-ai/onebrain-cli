//! Pure helpers backing `onebrain run-skill` · prompt construction and
//! harness binary resolution. All side effects (process spawn, env var
//! reads, stderr writes) live in the CLI handler so this module stays
//! deterministic and trivially testable.

use onebrain_core::Harness;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Resolver-level errors. Spawn failures and exit-code translation live in
/// the CLI handler — they're tied to `std::process` types we don't want to
/// leak into a library API.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RunSkillError {
    #[error("skill name must not be empty (got \"/\" or \"\")")]
    EmptySkill,
}

/// Result of resolving a harness binary (`claude` / `gemini` / `codex`). `warning` is
/// non-empty when the binary's env override (`CLAUDE_BIN` / `GEMINI_BIN` /
/// `CODEX_BIN`) was
/// set but pointed to a missing path — the handler logs it to stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessBinResolution {
    pub path: PathBuf,
    pub warning: Option<String>,
}

/// Build the slash-command prompt for `claude -p`. Mirrors `buildPrompt`
/// from the Bun source:
///
/// - strip leading `/`
/// - empty → `EmptySkill`
/// - if the name already contains `:` (an explicit plugin namespace), keep
///   it as-is; otherwise prefix with `onebrain:`
/// - append ` k1=v1 k2=v2 ...` for each arg, preserving insertion order
///
/// Args are passed as a slice of `(key, value)` pairs (not a map) so the
/// CLI handler controls insertion order — `clap`'s `Vec<String>` of
/// `key=value` tokens is already ordered, so this works out cleanly.
pub fn build_prompt(skill: &str, args: &[(String, String)]) -> Result<String, RunSkillError> {
    build_prompt_for_harness(skill, args, Harness::Claude)
}

pub fn build_prompt_for_harness(
    skill: &str,
    args: &[(String, String)],
    harness: Harness,
) -> Result<String, RunSkillError> {
    let bare = skill.strip_prefix('/').unwrap_or(skill);
    if bare.is_empty() {
        return Err(RunSkillError::EmptySkill);
    }
    let namespaced = if bare.contains(':') {
        bare.to_string()
    } else {
        format!("onebrain:{bare}")
    };
    let invocation = match harness {
        Harness::Codex => format!("${namespaced}"),
        _ => format!("/{namespaced}"),
    };
    if args.is_empty() {
        return Ok(invocation);
    }
    let tokens: Vec<String> = args.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Ok(format!("{invocation} {}", tokens.join(" ")))
}

/// Resolve which `claude` binary to invoke. Thin wrapper over [`resolve_bin`]
/// — see it for the priority list. Kept as a named entry point for the claude
/// path (and its existing test coverage).
pub fn resolve_claude_bin(
    override_path: Option<&Path>,
    env_lookup: impl Fn(&str) -> Option<String>,
    path_exists: impl Fn(&Path) -> bool,
    home: Option<&str>,
) -> HarnessBinResolution {
    resolve_bin(
        "claude",
        "CLAUDE_BIN",
        override_path,
        env_lookup,
        path_exists,
        home,
    )
}

/// Resolve which `gemini` binary to invoke — mirror of [`resolve_claude_bin`]
/// with the `GEMINI_BIN` env override and `gemini` probe paths.
pub fn resolve_gemini_bin(
    override_path: Option<&Path>,
    env_lookup: impl Fn(&str) -> Option<String>,
    path_exists: impl Fn(&Path) -> bool,
    home: Option<&str>,
) -> HarnessBinResolution {
    resolve_bin(
        "gemini",
        "GEMINI_BIN",
        override_path,
        env_lookup,
        path_exists,
        home,
    )
}

pub fn resolve_codex_bin(
    override_path: Option<&Path>,
    env_lookup: impl Fn(&str) -> Option<String>,
    path_exists: impl Fn(&Path) -> bool,
    home: Option<&str>,
) -> HarnessBinResolution {
    resolve_bin(
        "codex",
        "CODEX_BIN",
        override_path,
        env_lookup,
        path_exists,
        home,
    )
}

/// Resolve a harness binary by name, using a per-binary env override and the
/// shared probe order:
///
/// 1. explicit caller `override_path` (test seam) — used as-is
/// 2. `{env_var}` if set AND the path exists
/// 3. `{env_var}` set but path missing → emit `warning` and fall through
/// 4. `$HOME/.local/bin/{bin}` if exists
/// 5. `/opt/homebrew/bin/{bin}` if exists
/// 6. `/usr/local/bin/{bin}` if exists
/// 7. bare `{bin}` (rely on PATH lookup at spawn time)
///
/// Closures (`env_lookup`, `path_exists`, `home`) keep this deterministic and
/// avoid global env mutation in tests.
pub fn resolve_bin(
    bin: &str,
    env_var: &str,
    override_path: Option<&Path>,
    env_lookup: impl Fn(&str) -> Option<String>,
    path_exists: impl Fn(&Path) -> bool,
    home: Option<&str>,
) -> HarnessBinResolution {
    if let Some(p) = override_path {
        return HarnessBinResolution {
            path: p.to_path_buf(),
            warning: None,
        };
    }

    let mut warning: Option<String> = None;
    if let Some(from_env) = env_lookup(env_var) {
        let candidate = PathBuf::from(&from_env);
        if path_exists(&candidate) {
            return HarnessBinResolution {
                path: candidate,
                warning: None,
            };
        }
        // Keep this string stable (parameterized only by env_var) so users
        // grepping logs find the same message Bun emitted.
        warning = Some(format!(
            "{env_var} points to a missing file: {from_env} — ignoring and probing defaults"
        ));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        candidates.push(PathBuf::from(h).join(format!(".local/bin/{bin}")));
    }
    candidates.push(PathBuf::from(format!("/opt/homebrew/bin/{bin}")));
    candidates.push(PathBuf::from(format!("/usr/local/bin/{bin}")));

    for c in candidates {
        if path_exists(&c) {
            return HarnessBinResolution { path: c, warning };
        }
    }

    HarnessBinResolution {
        path: PathBuf::from(bin),
        warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    // ---- build_prompt ----

    #[test]
    fn build_prompt_namespaces_bare_with_leading_slash() {
        assert_eq!(build_prompt("/daily", &[]).unwrap(), "/onebrain:daily");
    }

    #[test]
    fn build_prompt_namespaces_when_slash_omitted() {
        assert_eq!(build_prompt("daily", &[]).unwrap(), "/onebrain:daily");
    }

    #[test]
    fn build_prompt_preserves_explicit_namespace_with_slash() {
        assert_eq!(
            build_prompt("/other-plugin:foo", &[]).unwrap(),
            "/other-plugin:foo"
        );
    }

    #[test]
    fn build_prompt_preserves_explicit_namespace_without_slash() {
        assert_eq!(
            build_prompt("onebrain:weekly", &[]).unwrap(),
            "/onebrain:weekly"
        );
    }

    #[test]
    fn build_prompt_appends_args_as_key_value() {
        let args = vec![pair("topic", "this-week")];
        assert_eq!(
            build_prompt("/distill", &args).unwrap(),
            "/onebrain:distill topic=this-week"
        );
    }

    #[test]
    fn build_prompt_preserves_arg_insertion_order() {
        let args = vec![pair("first", "1"), pair("second", "2"), pair("third", "3")];
        assert_eq!(
            build_prompt("/echo", &args).unwrap(),
            "/onebrain:echo first=1 second=2 third=3"
        );
    }

    #[test]
    fn build_prompt_empty_args_returns_bare_slash_command() {
        assert_eq!(build_prompt("/daily", &[]).unwrap(), "/onebrain:daily");
    }

    #[test]
    fn build_prompt_rejects_slash_only() {
        assert_eq!(build_prompt("/", &[]), Err(RunSkillError::EmptySkill));
    }

    #[test]
    fn build_prompt_rejects_empty_string() {
        assert_eq!(build_prompt("", &[]), Err(RunSkillError::EmptySkill));
    }

    #[test]
    fn build_codex_prompt_uses_dollar_skill_syntax() {
        let args = vec![pair("topic", "this-week")];
        assert_eq!(
            build_prompt_for_harness("daily", &args, onebrain_core::Harness::Codex).unwrap(),
            "$onebrain:daily topic=this-week"
        );
    }

    // ---- resolve_claude_bin ----

    #[test]
    fn resolve_codex_bin_honors_codex_bin() {
        let result = resolve_codex_bin(
            None,
            |k| (k == "CODEX_BIN").then(|| "/custom/codex".to_string()),
            |p| p == Path::new("/custom/codex"),
            None,
        );
        assert_eq!(result.path, PathBuf::from("/custom/codex"));
    }

    #[test]
    fn resolve_claude_bin_uses_explicit_override_unconditionally() {
        let result = resolve_claude_bin(
            Some(Path::new("/some/test/claude")),
            |_| panic!("env should not be consulted when override is set"),
            |_| panic!("path_exists should not be called when override is set"),
            None,
        );
        assert_eq!(result.path, PathBuf::from("/some/test/claude"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_claude_bin_honors_env_when_path_exists() {
        let result = resolve_claude_bin(
            None,
            |k| {
                if k == "CLAUDE_BIN" {
                    Some("/bin/sh".to_string())
                } else {
                    None
                }
            },
            |p| p == Path::new("/bin/sh"),
            Some("/Users/example"),
        );
        assert_eq!(result.path, PathBuf::from("/bin/sh"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_claude_bin_warns_on_missing_env_and_falls_through_to_home() {
        let result = resolve_claude_bin(
            None,
            |k| {
                if k == "CLAUDE_BIN" {
                    Some("/definitely/missing".to_string())
                } else {
                    None
                }
            },
            |p| p == Path::new("/Users/example/.local/bin/claude"),
            Some("/Users/example"),
        );
        assert_eq!(
            result.path,
            PathBuf::from("/Users/example/.local/bin/claude")
        );
        let warning = result.warning.expect("expected warning for missing env");
        assert!(
            warning.contains("CLAUDE_BIN points to a missing file"),
            "warning was: {warning}"
        );
        assert!(warning.contains("/definitely/missing"));
    }

    #[test]
    fn resolve_claude_bin_falls_back_to_homebrew_when_home_missing() {
        let result = resolve_claude_bin(
            None,
            |_| None,
            |p| p == Path::new("/opt/homebrew/bin/claude"),
            None,
        );
        assert_eq!(result.path, PathBuf::from("/opt/homebrew/bin/claude"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_claude_bin_falls_back_to_usr_local_when_homebrew_missing() {
        let result = resolve_claude_bin(
            None,
            |_| None,
            |p| p == Path::new("/usr/local/bin/claude"),
            Some("/Users/example"),
        );
        assert_eq!(result.path, PathBuf::from("/usr/local/bin/claude"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_claude_bin_returns_bare_claude_when_nothing_exists() {
        let result = resolve_claude_bin(None, |_| None, |_| false, Some("/Users/example"));
        assert_eq!(result.path, PathBuf::from("claude"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_claude_bin_probes_home_first() {
        // Both home and homebrew exist · home wins (Bun probe order).
        let result = resolve_claude_bin(
            None,
            |_| None,
            |_| true, // every path "exists"
            Some("/Users/example"),
        );
        assert_eq!(
            result.path,
            PathBuf::from("/Users/example/.local/bin/claude")
        );
    }

    // ---- resolve_gemini_bin ----

    #[test]
    fn resolve_gemini_bin_honors_env_when_path_exists() {
        let result = resolve_gemini_bin(
            None,
            |k| (k == "GEMINI_BIN").then(|| "/bin/sh".to_string()),
            |p| p == Path::new("/bin/sh"),
            Some("/Users/example"),
        );
        assert_eq!(result.path, PathBuf::from("/bin/sh"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_gemini_bin_warns_on_missing_env_and_probes_homebrew() {
        // Real machine: gemini lives at /opt/homebrew/bin/gemini (no ~/.local one).
        let result = resolve_gemini_bin(
            None,
            |k| (k == "GEMINI_BIN").then(|| "/definitely/missing".to_string()),
            |p| p == Path::new("/opt/homebrew/bin/gemini"),
            Some("/Users/example"),
        );
        assert_eq!(result.path, PathBuf::from("/opt/homebrew/bin/gemini"));
        let warning = result.warning.expect("expected warning for missing env");
        assert!(
            warning.contains("GEMINI_BIN points to a missing file"),
            "was: {warning}"
        );
        assert!(warning.contains("/definitely/missing"));
    }

    #[test]
    fn resolve_gemini_bin_returns_bare_gemini_when_nothing_exists() {
        let result = resolve_gemini_bin(None, |_| None, |_| false, Some("/Users/example"));
        assert_eq!(result.path, PathBuf::from("gemini"));
        assert!(result.warning.is_none());
    }

    #[test]
    fn resolve_gemini_bin_uses_explicit_override_unconditionally() {
        let result = resolve_gemini_bin(
            Some(Path::new("/some/test/gemini")),
            |_| panic!("env should not be consulted when override is set"),
            |_| panic!("path_exists should not be called when override is set"),
            None,
        );
        assert_eq!(result.path, PathBuf::from("/some/test/gemini"));
        assert!(result.warning.is_none());
    }
}
