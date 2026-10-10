//! Name the process holding a search collection's lock (#426).
//!
//! When `Engine::open` loses the collection-lock race (`EngineBusy`), the
//! winner's sidecar ([`onebrain_search::holder`]) says who it is. This module
//! turns that into the one user-facing sentence every busy surface shares —
//! the daemon log, the daemon client's error (the gateway log, routed CLI
//! verbs), `search status`, `doctor` and the direct CLI open path.
//!
//! A holder is named only when its pid is alive right now, is not this
//! process, and its record looks like one `onebrain` wrote. A missing sidecar
//! (holder older than v3.5.1), a dead pid (crashed holder), our own pid, an
//! unknown role, or a platform with no liveness probe all give the generic
//! message — never a pid that may belong to someone else.

use std::cmp::Ordering;
use std::path::Path;

use onebrain_search::holder::{read_holder, HolderInfo};

use crate::cli::Cmd;
use crate::commands::daemon_client::own_version;
use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};

/// The message when the holder can't be named.
pub(crate) const GENERIC_BUSY: &str = "the search index is in use by another onebrain process; \
     if you just upgraded, restart open agent sessions (Claude Code / Codex / Gemini)";

/// Every role [`holder_role`] can record. A sidecar naming anything else
/// wasn't written by us, so its holder is never named.
const KNOWN_ROLES: [&str; 5] = ["mcp", "daemon", "gateway", "serve", "cli"];

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
/// `vault` (when the caller knows it) makes a daemon holder's stop command
/// target exactly that vault's daemon.
pub(crate) fn busy_message(cache_dir: &Path, vault: Option<&Path>) -> String {
    describe(
        read_holder(cache_dir).as_ref(),
        pid_alive,
        own_version(),
        std::process::id(),
        vault,
    )
}

/// [`busy_message`] for a vault root (the daemon client knows the vault, not
/// the cache dir). Resolves the collection read-only — never persists a
/// generated name.
pub(crate) fn busy_message_for_vault(vault_root: &Path) -> String {
    match collection_name_readonly(vault_root) {
        Ok(collection) => busy_message(&collection_cache_dir(&collection), Some(vault_root)),
        Err(_) => GENERIC_BUSY.to_string(),
    }
}

fn describe(
    holder: Option<&HolderInfo>,
    alive: impl Fn(u32) -> Option<bool>,
    own_version: &str,
    own_pid: u32,
    vault: Option<&Path>,
) -> String {
    // Never name ourselves (we are the loser, not the holder — a record with
    // our pid is stale or recycled), a role we never write, or a dead pid.
    let Some(h) = holder.filter(|h| {
        h.pid != own_pid && KNOWN_ROLES.contains(&h.role.as_str()) && alive(h.pid) == Some(true)
    }) else {
        return GENERIC_BUSY.to_string();
    };
    let version = clean_version(&h.version);
    let who = |v: Option<&str>| match v {
        Some(v) => format!(
            "the search index is in use by onebrain {} {v} (pid {})",
            h.role, h.pid
        ),
        None => format!(
            "the search index is in use by onebrain {} (pid {})",
            h.role, h.pid
        ),
    };
    let fix = stop_hint(&h.role, vault);
    match version.and_then(|v| compare_versions(v, own_version)) {
        Some(Ordering::Equal) if version == Some(own_version) => {
            format!("{} — retry once it releases the lock", who(version))
        }
        Some(Ordering::Less) => format!("{} — from before an upgrade, {fix}", who(version)),
        // Newer, or a version we can't order against ours: say only that it
        // differs — "before an upgrade" would be a guess.
        _ => match version {
            Some(v) => format!("{} — a different onebrain version ({v}), {fix}", who(None)),
            None => format!("{} — a different onebrain version, {fix}", who(None)),
        },
    }
}

/// How to free the lock from a holder of `role`. A daemon gets the narrowest
/// stop: `daemon stop --vault <vault>` stops only that vault's slot
/// (`daemon.rs` `run_stop`); without a known vault, plain `daemon stop` run
/// inside the vault targets the same cwd-resolved slot.
fn stop_hint(role: &str, vault: Option<&Path>) -> String {
    match (role, vault) {
        ("mcp", _) => "restart that agent session (Claude Code / Codex / Gemini)".to_string(),
        ("daemon", Some(v)) => {
            let v = v.display().to_string();
            let v = if v.contains(char::is_whitespace) {
                format!("\"{v}\"")
            } else {
                v
            };
            format!("stop it with `onebrain daemon stop --vault {v}`")
        }
        ("daemon", None) => "stop it with `onebrain daemon stop` run inside that vault".to_string(),
        _ => "stop that process".to_string(),
    }
}

/// The sidecar's version, if it is short printable ASCII; anything else is
/// dropped rather than echoed to the user.
fn clean_version(v: &str) -> Option<&str> {
    (!v.is_empty() && v.len() <= 32 && v.chars().all(|c| c.is_ascii_graphic())).then_some(v)
}

/// Order two dotted-numeric versions (`3.4.25` < `3.5.1`). `None` when either
/// isn't plain dotted numbers (pre-release / build suffixes included) — the
/// caller treats that as "different", never as older.
fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    fn parse(v: &str) -> Option<Vec<u64>> {
        v.split('.').map(|p| p.parse::<u64>().ok()).collect()
    }
    let (mut a, mut b) = (parse(a)?, parse(b)?);
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    Some(a.cmp(&b))
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

/// Test helpers shared with the other modules' busy-holder tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    use onebrain_search::holder::HolderInfo;
    use onebrain_search::CollectionLayout;

    /// A live process that is not this one, killed on drop — stands in for a
    /// holder in another process.
    pub(crate) struct OtherProcess(std::process::Child);

    impl OtherProcess {
        pub(crate) fn spawn() -> Self {
            #[cfg(windows)]
            let child = std::process::Command::new("ping")
                .args(["-n", "120", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            #[cfg(not(windows))]
            let child = std::process::Command::new("sleep")
                .arg("120")
                .spawn()
                .unwrap();
            OtherProcess(child)
        }

        pub(crate) fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for OtherProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub(crate) fn holder(pid: u32, role: &str, version: &str) -> HolderInfo {
        HolderInfo {
            pid,
            version: version.to_string(),
            exe: "/other/onebrain".to_string(),
            role: role.to_string(),
            started: 1,
        }
    }

    pub(crate) fn write_sidecar(cache_dir: &Path, info: &HolderInfo) {
        std::fs::write(
            CollectionLayout::new(cache_dir).holder_path(),
            serde_json::to_vec(info).unwrap(),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{holder, write_sidecar, OtherProcess};
    use super::*;
    use onebrain_search::engine::Engine;
    use onebrain_search::CollectionLayout;

    const MODEL: &str = "multilingual-e5-small";

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

    fn always_alive(_: u32) -> Option<bool> {
        Some(true)
    }

    /// Not this test's pid, so `describe` treats the holder as another process.
    const READER_PID: u32 = 1;

    /// #426: a live holder from an older version is named — role, version,
    /// pid — with the upgrade hint. The sidecar here is the one the real
    /// `Engine::open` wrote, read as if by another process, so skipping that
    /// write turns this red.
    #[test]
    fn names_a_live_holder_from_before_an_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), MODEL).unwrap();
        let busy = Engine::open(dir.path(), MODEL).err().expect("lock is held");
        assert!(onebrain_search::error::is_engine_busy(&busy), "{busy:#}");

        let msg = describe(
            read_holder(dir.path()).as_ref(),
            pid_alive,
            "999.0.0",
            READER_PID,
            None,
        );
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
        let other = OtherProcess::spawn();
        write_sidecar(dir.path(), &holder(other.pid(), "mcp", own_version()));

        let msg = busy_message(dir.path(), None);
        assert_eq!(
            msg,
            format!(
                "the search index is in use by onebrain mcp {} (pid {}) — retry once it \
                 releases the lock",
                own_version(),
                other.pid()
            )
        );
    }

    /// The loser must never name itself: a record carrying our own pid is
    /// stale (or a recycled pid), not the live holder.
    #[test]
    fn own_pid_is_never_named() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), MODEL).unwrap();
        assert_eq!(read_holder(dir.path()).unwrap().pid, std::process::id());

        assert_eq!(busy_message(dir.path(), None), GENERIC_BUSY);
    }

    /// "Before an upgrade" only when the holder is OLDER; a newer holder gets
    /// the neutral "different version" hint.
    #[test]
    fn newer_holder_gets_the_neutral_hint() {
        let h = holder(42, "mcp", "3.6.0");
        assert_eq!(
            describe(Some(&h), always_alive, "3.5.1", READER_PID, None),
            "the search index is in use by onebrain mcp (pid 42) — a different onebrain \
             version (3.6.0), restart that agent session (Claude Code / Codex / Gemini)"
        );
    }

    #[test]
    fn unparseable_version_gets_the_neutral_hint() {
        for v in ["3.6.0-rc.1", "dev", "3.5"] {
            let msg = describe(
                Some(&holder(42, "cli", v)),
                always_alive,
                "3.5.1-beta",
                READER_PID,
                None,
            );
            assert!(msg.contains("a different onebrain version"), "{v}: {msg}");
            assert!(!msg.contains("upgrade"), "{v}: {msg}");
        }
        // Numerically equal but differently spelled is "different", not "same".
        let msg = describe(
            Some(&holder(42, "cli", "3.5.1.0")),
            always_alive,
            "3.5.1",
            READER_PID,
            None,
        );
        assert!(
            msg.contains("a different onebrain version (3.5.1.0)"),
            "{msg}"
        );
    }

    #[test]
    fn compare_versions_orders_numerically() {
        assert_eq!(compare_versions("3.4.25", "3.5.1"), Some(Ordering::Less));
        assert_eq!(compare_versions("3.10.0", "3.9.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("3.5", "3.5.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("3.5.1-rc", "3.5.1"), None);
    }

    /// A pre-3.5.1 holder writes no sidecar → the generic upgrade hint.
    #[test]
    fn no_sidecar_gives_the_generic_message() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), MODEL).unwrap();
        std::fs::remove_file(CollectionLayout::new(dir.path()).holder_path()).unwrap();

        assert_eq!(busy_message(dir.path(), None), GENERIC_BUSY);
    }

    /// A sidecar left by a holder that has died never names its pid.
    #[test]
    fn dead_pid_gives_the_generic_message() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dead_pid();
        assert_eq!(pid_alive(pid), Some(false), "pid {pid} should be dead");
        write_sidecar(dir.path(), &holder(pid, "mcp", "3.4.25"));

        let msg = busy_message(dir.path(), None);
        assert_eq!(msg, GENERIC_BUSY);
        assert!(!msg.contains(&pid.to_string()), "{msg}");
    }

    /// No liveness probe on this platform → never name the holder.
    #[test]
    fn unknown_liveness_gives_the_generic_message() {
        let h = holder(42, "mcp", "3.4.25");
        assert_eq!(
            describe(Some(&h), |_| None, "3.5.1", READER_PID, None),
            GENERIC_BUSY
        );
    }

    /// Sidecar fields are printed only when they look like ours.
    #[test]
    fn sidecar_fields_are_sanitized() {
        let alive = always_alive;
        let h = holder(42, "evil\x1b[2Jrole", "3.4.25");
        assert_eq!(
            describe(Some(&h), alive, "3.5.1", READER_PID, None),
            GENERIC_BUSY
        );

        let h = holder(42, "mcp", "3.4.25\x1b[31m");
        let msg = describe(Some(&h), alive, "3.5.1", READER_PID, None);
        assert!(!msg.contains('\x1b'), "{msg:?}");
        assert!(
            msg.starts_with(
                "the search index is in use by onebrain mcp (pid 42) — a different onebrain \
                 version, restart"
            ),
            "{msg}"
        );
    }

    #[test]
    fn upgrade_hint_matches_the_holders_role() {
        let mut h = holder(42, "mcp", "3.4.25");
        assert_eq!(
            describe(Some(&h), always_alive, "3.5.1", READER_PID, None),
            "the search index is in use by onebrain mcp 3.4.25 (pid 42) — from before an \
             upgrade, restart that agent session (Claude Code / Codex / Gemini)"
        );
        h.role = "daemon".to_string();
        assert!(describe(Some(&h), always_alive, "3.5.1", READER_PID, None)
            .ends_with("stop it with `onebrain daemon stop` run inside that vault"));
        assert!(describe(
            Some(&h),
            always_alive,
            "3.5.1",
            READER_PID,
            Some(Path::new("/v/my vault"))
        )
        .ends_with("stop it with `onebrain daemon stop --vault \"/v/my vault\"`"));
        assert!(describe(
            Some(&h),
            always_alive,
            "3.5.1",
            READER_PID,
            Some(Path::new("/v/ob"))
        )
        .ends_with("stop it with `onebrain daemon stop --vault /v/ob`"));
        h.role = "cli".to_string();
        assert!(describe(Some(&h), always_alive, "3.5.1", READER_PID, None)
            .ends_with("stop that process"));
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
        assert!(KNOWN_ROLES.contains(&role(&["onebrain", "serve"])));
    }
}
