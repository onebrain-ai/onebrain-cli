//! Name the process holding a search collection's lock (#426).
//!
//! When `Engine::open` loses the collection-lock race (`EngineBusy`), the
//! winner's sidecar ([`onebrain_search::holder`]) says who it is. This module
//! turns that into the one user-facing sentence every busy surface shares —
//! the daemon log, the daemon client's error (gateway `brain_search`, routed
//! CLI verbs), `search status`, `doctor` and the direct CLI open path.
//!
//! A holder is named only when its pid is alive right now. A missing sidecar
//! (holder older than v3.5.1), a dead pid (crashed holder), or a platform with
//! no liveness probe all give the generic message — never a pid that may
//! already belong to someone else.

use std::path::Path;

use onebrain_search::holder::{read_holder, HolderInfo};

use crate::cli::Cmd;
use crate::commands::daemon_client::own_version;
use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};

/// The message when the holder can't be named.
pub(crate) const GENERIC_BUSY: &str = "the search index is in use by another onebrain process; \
     if you just upgraded, restart open agent sessions (Claude Code / Codex / Gemini)";

/// The role this process records in its holder sidecar, from the command it
/// was dispatched with. Set once at dispatch via
/// [`onebrain_search::holder::set_process_role`].
pub(crate) fn holder_role(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Mcp => "mcp",
        Cmd::Daemon(_) => "daemon",
        Cmd::Gateway(_) => "gateway",
        Cmd::Serve(_) => "serve",
        _ => "cli",
    }
}

/// Who holds the lock of the collection at `cache_dir`, as one sentence.
pub(crate) fn busy_message(cache_dir: &Path) -> String {
    describe(read_holder(cache_dir).as_ref(), pid_alive, own_version())
}

/// [`busy_message`] for a vault root (the daemon client knows the vault, not
/// the cache dir). Resolves the collection read-only — never persists a
/// generated name.
pub(crate) fn busy_message_for_vault(vault_root: &Path) -> String {
    match collection_name_readonly(vault_root) {
        Ok(collection) => busy_message(&collection_cache_dir(&collection)),
        Err(_) => GENERIC_BUSY.to_string(),
    }
}

fn describe(
    holder: Option<&HolderInfo>,
    alive: impl Fn(u32) -> Option<bool>,
    own_version: &str,
) -> String {
    let Some(h) = holder.filter(|h| alive(h.pid) == Some(true)) else {
        return GENERIC_BUSY.to_string();
    };
    let who = format!(
        "the search index is in use by onebrain {} {} (pid {})",
        h.role, h.version, h.pid
    );
    if h.version == own_version {
        return format!("{who} — retry once it releases the lock");
    }
    let fix = match h.role.as_str() {
        "mcp" => "restart that agent session (Claude Code / Codex / Gemini)",
        "daemon" => "stop it with `onebrain daemon stop --all`",
        _ => "stop that process",
    };
    format!("{who} — from before an upgrade, {fix}")
}

/// `Some(alive)` where this platform can probe a pid, `None` where it can't
/// (the caller then never names the holder). pid 0 and pids that don't fit a
/// signed pid are never a real holder — on Unix, `kill` would read them as a
/// process group.
fn pid_alive(pid: u32) -> Option<bool> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Some(false);
    }
    #[cfg(any(unix, windows))]
    {
        Some(crate::commands::daemon::pid_exists(pid))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_search::engine::Engine;
    use onebrain_search::CollectionLayout;

    const MODEL: &str = "multilingual-e5-small";

    fn write_sidecar(cache_dir: &Path, info: &HolderInfo) {
        std::fs::write(
            CollectionLayout::new(cache_dir).holder_path(),
            serde_json::to_vec(info).unwrap(),
        )
        .unwrap();
    }

    /// A pid that existed and has exited (reaped), so it's dead right now.
    fn dead_pid() -> u32 {
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap();
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// #426: a live holder from another version is named — role, version,
    /// pid — with the upgrade hint. The sidecar here is the one the real
    /// `Engine::open` wrote, so skipping that write turns this red.
    #[test]
    fn names_a_live_holder_from_before_an_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), MODEL).unwrap();
        let busy = Engine::open(dir.path(), MODEL).err().expect("lock is held");
        assert!(onebrain_search::error::is_engine_busy(&busy), "{busy:#}");

        let msg = describe(read_holder(dir.path()).as_ref(), pid_alive, "999.0.0");
        let expected = format!(
            "the search index is in use by onebrain {} {} (pid {}) — from before an upgrade",
            onebrain_search::holder::process_role(),
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        assert!(msg.starts_with(&expected), "{msg}");
    }

    #[test]
    fn names_a_live_same_version_holder_without_the_upgrade_hint() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), MODEL).unwrap();

        let msg = busy_message(dir.path());
        assert!(
            msg.contains(&format!("(pid {})", std::process::id())),
            "{msg}"
        );
        assert!(msg.ends_with("retry once it releases the lock"), "{msg}");
        assert!(!msg.contains("upgrade"), "{msg}");
    }

    /// A pre-3.5.1 holder writes no sidecar → the generic upgrade hint.
    #[test]
    fn no_sidecar_gives_the_generic_message() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), MODEL).unwrap();
        std::fs::remove_file(CollectionLayout::new(dir.path()).holder_path()).unwrap();

        assert_eq!(busy_message(dir.path()), GENERIC_BUSY);
    }

    /// A sidecar left by a holder that has died never names its pid.
    #[test]
    fn dead_pid_gives_the_generic_message() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dead_pid();
        assert_eq!(pid_alive(pid), Some(false), "pid {pid} should be dead");
        write_sidecar(
            dir.path(),
            &HolderInfo {
                pid,
                version: "3.4.25".to_string(),
                exe: "/old/onebrain".to_string(),
                role: "mcp".to_string(),
                started: 1,
            },
        );

        let msg = busy_message(dir.path());
        assert_eq!(msg, GENERIC_BUSY);
        assert!(!msg.contains(&pid.to_string()), "{msg}");
    }

    /// No liveness probe on this platform → never name the holder.
    #[test]
    fn unknown_liveness_gives_the_generic_message() {
        let info = HolderInfo {
            pid: std::process::id(),
            version: "3.4.25".to_string(),
            exe: String::new(),
            role: "mcp".to_string(),
            started: 1,
        };
        assert_eq!(describe(Some(&info), |_| None, "3.5.1"), GENERIC_BUSY);
    }

    #[test]
    fn upgrade_hint_matches_the_holders_role() {
        let mut info = HolderInfo {
            pid: 42,
            version: "3.4.25".to_string(),
            exe: String::new(),
            role: "mcp".to_string(),
            started: 1,
        };
        let alive = |_| Some(true);
        assert_eq!(
            describe(Some(&info), alive, "3.5.1"),
            "the search index is in use by onebrain mcp 3.4.25 (pid 42) — from before an \
             upgrade, restart that agent session (Claude Code / Codex / Gemini)"
        );
        info.role = "daemon".to_string();
        assert!(describe(Some(&info), alive, "3.5.1").ends_with("`onebrain daemon stop --all`"));
        info.role = "cli".to_string();
        assert!(describe(Some(&info), alive, "3.5.1").ends_with("stop that process"));
    }

    #[test]
    fn pid_alive_rejects_group_pids() {
        assert_eq!(pid_alive(0), Some(false));
        assert_eq!(pid_alive(u32::MAX), Some(false));
        assert_eq!(pid_alive(std::process::id()), Some(true));
    }

    #[test]
    fn holder_role_follows_the_dispatched_command() {
        use clap::Parser;
        let role =
            |args: &[&str]| holder_role(&crate::cli::Cli::try_parse_from(args).unwrap().command);
        assert_eq!(role(&["onebrain", "mcp"]), "mcp");
        assert_eq!(role(&["onebrain", "daemon", "__run"]), "daemon");
        assert_eq!(role(&["onebrain", "gateway", "run"]), "gateway");
        assert_eq!(role(&["onebrain", "search", "status"]), "cli");
    }
}
