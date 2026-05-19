//! Pure helpers backing `onebrain run-skill` · prompt construction and
//! `claude` binary resolution. All side effects (process spawn, env var
//! reads, stderr writes) live in the CLI handler so this module stays
//! deterministic and trivially testable.

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

/// Result of resolving the `claude` binary. `warning` is non-empty when
/// `CLAUDE_BIN` was set but pointed to a missing path — the handler logs it
/// to stderr (matching Bun's `pc.yellow(...)` warning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeBinResolution {
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
    let bare = skill.strip_prefix('/').unwrap_or(skill);
    if bare.is_empty() {
        return Err(RunSkillError::EmptySkill);
    }
    let namespaced = if bare.contains(':') {
        bare.to_string()
    } else {
        format!("onebrain:{bare}")
    };
    let slash = format!("/{namespaced}");
    if args.is_empty() {
        return Ok(slash);
    }
    let tokens: Vec<String> = args.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Ok(format!("{slash} {}", tokens.join(" ")))
}

/// Resolve which `claude` binary to invoke. Matches Bun's `resolveClaudeBin`
/// priority list:
///
/// 1. explicit caller `override_path` (test seam) — used as-is
/// 2. `CLAUDE_BIN` env var if set AND path exists
/// 3. `CLAUDE_BIN` set but path missing → emit `warning` and fall through
/// 4. `$HOME/.local/bin/claude` if exists
/// 5. `/opt/homebrew/bin/claude` if exists
/// 6. `/usr/local/bin/claude` if exists
/// 7. bare `claude` (rely on PATH lookup at spawn time)
///
/// Closures (`env_lookup`, `path_exists`, `home`) keep this function
/// deterministic and avoid global env mutation in tests.
pub fn resolve_claude_bin(
    override_path: Option<&Path>,
    env_lookup: impl Fn(&str) -> Option<String>,
    path_exists: impl Fn(&Path) -> bool,
    home: Option<&str>,
) -> ClaudeBinResolution {
    if let Some(p) = override_path {
        return ClaudeBinResolution {
            path: p.to_path_buf(),
            warning: None,
        };
    }

    let mut warning: Option<String> = None;
    if let Some(from_env) = env_lookup("CLAUDE_BIN") {
        let candidate = PathBuf::from(&from_env);
        if path_exists(&candidate) {
            return ClaudeBinResolution {
                path: candidate,
                warning: None,
            };
        }
        // Mirror Bun's warning exactly so users grepping logs find the same string.
        warning = Some(format!(
            "CLAUDE_BIN points to a missing file: {from_env} — ignoring and probing defaults"
        ));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        candidates.push(PathBuf::from(h).join(".local/bin/claude"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
    candidates.push(PathBuf::from("/usr/local/bin/claude"));

    for c in candidates {
        if path_exists(&c) {
            return ClaudeBinResolution { path: c, warning };
        }
    }

    ClaudeBinResolution {
        path: PathBuf::from("claude"),
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

    // ---- resolve_claude_bin ----

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
}
