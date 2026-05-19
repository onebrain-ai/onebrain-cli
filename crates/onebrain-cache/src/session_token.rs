use crate::{CacheError, Result};
use chrono::Local;
use onebrain_core::SessionToken;
use std::path::Path;

/// Inputs that drive token resolution · injected for testability.
///
/// In production, populated from `std::env::var_os` and `std::process::parent_id`.
/// Tests construct one manually to bypass the global env.
#[derive(Debug, Default, Clone)]
pub struct ResolveInputs {
    pub wt_session: Option<String>,
    pub tmux_pane: Option<String>,
    pub term_session_id: Option<String>,
    pub ppid: Option<u32>,
    /// Override "today" for testing · None = chrono::Local::now()
    pub today_override: Option<String>,
}

impl ResolveInputs {
    /// Snapshot the real env + process state.
    pub fn from_env() -> Self {
        Self {
            wt_session: std::env::var("WT_SESSION").ok(),
            tmux_pane: std::env::var("TMUX_PANE").ok(),
            term_session_id: std::env::var("TERM_SESSION_ID").ok(),
            ppid: parent_pid(),
            today_override: None,
        }
    }
}

fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(std::os::unix::process::parent_id())
    }
    #[cfg(windows)]
    {
        // Best-effort; full PowerShell parent walking is Slice 7 territory.
        // Returning None falls through to random + cache.
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

pub fn resolve_session_token(cache_dir: &Path, inputs: &ResolveInputs) -> Result<SessionToken> {
    // 1. WT_SESSION
    if let Some(raw) = &inputs.wt_session {
        if let Some(t) = SessionToken::sanitize(raw) {
            return Ok(t);
        }
    }

    // 2. TMUX_PANE
    if let Some(raw) = &inputs.tmux_pane {
        if let Some(t) = SessionToken::sanitize(raw) {
            return Ok(t);
        }
    }

    // 3. TERM_SESSION_ID
    if let Some(raw) = &inputs.term_session_id {
        if let Some(t) = SessionToken::sanitize(raw) {
            return Ok(t);
        }
    }

    // 4. Day-scoped cache (read existing)
    let today = inputs
        .today_override
        .clone()
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());
    let cache_file = cache_dir.join(format!("session_token_{today}"));
    if cache_file.is_file() {
        let raw =
            std::fs::read_to_string(&cache_file).map_err(|source| CacheError::CacheDirIo {
                path: cache_file.clone(),
                source,
            })?;
        if let Some(t) = SessionToken::sanitize(raw.trim()) {
            return Ok(t);
        }
    }

    // 5. ppid (unix) / 6. PowerShell parent (windows — best-effort, None)
    if let Some(ppid) = inputs.ppid {
        let raw = ppid.to_string();
        if let Some(t) = SessionToken::sanitize(&raw) {
            write_cache(&cache_file, t.as_str())?;
            return Ok(t);
        }
    }

    // 7. Random + cache
    let random = random_alphanumeric(5);
    let token = SessionToken::from_clean(random);
    write_cache(&cache_file, token.as_str())?;
    Ok(token)
}

fn write_cache(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CacheError::CacheDirIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, token).map_err(|source| CacheError::CacheDirIo {
        path: path.to_path_buf(),
        source,
    })
}

fn random_alphanumeric(len: usize) -> String {
    // No `rand` dependency in Slice 1 — derive from system time nanos.
    // Sufficient entropy for cache-key purposes (same-process, day-scoped).
    use std::time::{SystemTime, UNIX_EPOCH};
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut seed = nanos as u64;
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(CHARS[(seed >> 33) as usize % CHARS.len()] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_cache() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let cache = dir.path().to_path_buf();
        (dir, cache)
    }

    #[test]
    fn wt_session_takes_precedence() {
        let (_dir, cache) = fresh_cache();
        let inputs = ResolveInputs {
            wt_session: Some("abc-123".to_string()),
            tmux_pane: Some("%12".to_string()),
            term_session_id: Some("xyz".to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&cache, &inputs).unwrap();
        assert_eq!(token.as_str(), "abc123");
    }

    #[test]
    fn tmux_pane_used_when_wt_session_absent() {
        let (_dir, cache) = fresh_cache();
        let inputs = ResolveInputs {
            tmux_pane: Some("%12".to_string()),
            term_session_id: Some("xyz".to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&cache, &inputs).unwrap();
        assert_eq!(token.as_str(), "12");
    }

    #[test]
    fn term_session_id_used_when_others_absent() {
        let (_dir, cache) = fresh_cache();
        let inputs = ResolveInputs {
            term_session_id: Some("iterm-7E2A".to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&cache, &inputs).unwrap();
        assert_eq!(token.as_str(), "iterm7E2A");
    }

    #[test]
    fn day_scoped_cache_hit_returns_cached_token() {
        let (_dir, cache) = fresh_cache();
        std::fs::create_dir_all(&cache).unwrap();
        let today = "2026-05-25";
        let path = cache.join(format!("session_token_{today}"));
        std::fs::write(&path, "cachedABC").unwrap();

        let inputs = ResolveInputs {
            today_override: Some(today.to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&cache, &inputs).unwrap();
        assert_eq!(token.as_str(), "cachedABC");
    }

    #[test]
    fn day_scoped_cache_miss_generates_random_and_writes() {
        let (_dir, cache) = fresh_cache();
        std::fs::create_dir_all(&cache).unwrap();
        let today = "2026-05-26";
        let inputs = ResolveInputs {
            today_override: Some(today.to_string()),
            ..Default::default()
        };
        let token1 = resolve_session_token(&cache, &inputs).unwrap();

        // Second call same day → same token (cache hit)
        let token2 = resolve_session_token(&cache, &inputs).unwrap();
        assert_eq!(token1, token2);

        // Cache file exists
        let path = cache.join(format!("session_token_{today}"));
        assert!(path.is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), token1.as_str());
    }

    #[test]
    fn ppid_used_when_no_env_and_no_cache() {
        let (_dir, cache) = fresh_cache();
        std::fs::create_dir_all(&cache).unwrap();
        let inputs = ResolveInputs {
            ppid: Some(12345),
            today_override: Some("2026-05-27".to_string()),
            ..Default::default()
        };
        let token = resolve_session_token(&cache, &inputs).unwrap();
        assert_eq!(token.as_str(), "12345");
    }
}
