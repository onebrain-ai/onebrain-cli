//! Lock-holder sidecar (#426): who holds a collection's lock right now.
//!
//! The collection lock ([`CollectionLayout::lock_path`]) is a redb exclusive
//! open, so a process that loses the race only learns "someone holds it" —
//! [`crate::error::EngineBusy`]. After an upgrade that someone is usually an
//! `onebrain mcp` server still running the OLD binary inside an open agent
//! session, and "locked by another process" gives the user nothing to act on.
//!
//! So the winner records itself in a small JSON sidecar next to the lock
//! ([`CollectionLayout::holder_path`]) as soon as it holds the lock, and
//! removes it again before releasing the lock. A loser reads it to name the
//! holder. The sidecar is advisory only: the lock itself stays the redb open,
//! and a missing, unreadable or stale sidecar just means "can't name the
//! holder". Callers decide whether the recorded pid is still alive — this
//! crate has no process-probing dependency, so liveness lives in the CLI.
//!
//! Holders older than v3.5.1 never write a sidecar; they can't be named.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::layout::CollectionLayout;

/// What kind of `onebrain` process this is, recorded in the sidecar so the
/// busy message can say "onebrain mcp" rather than just a pid. Set once by
/// the CLI's dispatch; see [`set_process_role`].
static PROCESS_ROLE: OnceLock<&'static str> = OnceLock::new();

/// Record this process's role (`"mcp"`, `"daemon"`, `"gateway"`, `"cli"`, …)
/// for every sidecar it writes. First call wins; later calls are ignored, so
/// the role is fixed for the process lifetime.
pub fn set_process_role(role: &'static str) {
    let _ = PROCESS_ROLE.set(role);
}

/// This process's role, `"cli"` when [`set_process_role`] was never called.
pub fn process_role() -> &'static str {
    PROCESS_ROLE.get().copied().unwrap_or("cli")
}

/// The sidecar contents: who holds the collection lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderInfo {
    pub pid: u32,
    /// The holder's `onebrain` version (this crate shares the workspace
    /// version, so it is the CLI version).
    pub version: String,
    /// The holder's executable path, as it saw it at open time.
    pub exe: String,
    pub role: String,
    /// Epoch seconds when the holder took the lock.
    pub started: u64,
}

impl HolderInfo {
    /// The record for THIS process.
    pub fn current() -> Self {
        HolderInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            exe: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            role: process_role().to_string(),
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// Read the holder sidecar of the collection rooted at `cache_dir`. `None`
/// when there is none, or it can't be read or parsed.
pub fn read_holder(cache_dir: &Path) -> Option<HolderInfo> {
    read_at(&CollectionLayout::new(cache_dir).holder_path())
}

fn read_at(path: &Path) -> Option<HolderInfo> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Write `info` to `path` atomically (temp file + rename), so a concurrent
/// reader sees the old record or the new one, never a torn write.
fn write_at(path: &Path, info: &HolderInfo) -> std::io::Result<()> {
    let body = serde_json::to_vec(info).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("holder.{}.tmp", info.pid));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Remove the sidecar at `path` only when it names `pid`. A holder that is
/// itself stale — its record was already replaced by the next holder (e.g.
/// after it crashed and its lock was freed by the OS) — must not delete the
/// next holder's record.
fn remove_if_owned(path: &Path, pid: u32) {
    if read_at(path).is_some_and(|h| h.pid == pid) {
        let _ = std::fs::remove_file(path);
    }
}

/// Owns this process's sidecar for as long as the collection lock is held.
/// Created right after the lock is taken; dropped BEFORE the lock is released
/// (the `Engine` declares it ahead of its lock field, and `open_inner` binds it
/// after the lock, so both normal drop and an early-return error unwind drop
/// it first). A sidecar therefore never outlives a lock its writer released
/// cleanly; one left by a crash names a dead pid, which callers treat as
/// "unknown holder".
pub(crate) struct HolderGuard {
    path: PathBuf,
}

impl HolderGuard {
    /// Record this process as the holder at `path`. Best-effort: a failed write
    /// costs only the holder's name in a loser's busy message, never the open.
    /// But it must not leave an OLDER record standing — we hold the lock now,
    /// so any existing record is stale, and a live pid in it (recycled, or a
    /// process that merely outlived its lock) would be misnamed as the holder.
    pub(crate) fn record(path: PathBuf) -> Self {
        if write_at(&path, &HolderInfo::current()).is_err() {
            let _ = std::fs::remove_file(&path);
        }
        HolderGuard { path }
    }
}

impl Drop for HolderGuard {
    fn drop(&mut self) {
        remove_if_owned(&self.path, std::process::id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;

    fn other_holder() -> HolderInfo {
        HolderInfo {
            // Not our pid; any value that differs works for the ownership check.
            pid: std::process::id().wrapping_add(1),
            version: "9.9.9".to_string(),
            exe: "/old/onebrain".to_string(),
            role: "mcp".to_string(),
            started: 1,
        }
    }

    #[test]
    fn open_records_this_process_and_drop_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path(), "multilingual-e5-small").unwrap();

        let h = read_holder(dir.path()).expect("an open engine records its holder");
        assert_eq!(h.pid, std::process::id());
        assert_eq!(h.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(h.role, process_role());
        assert!(!h.exe.is_empty());

        drop(engine);
        assert!(
            read_holder(dir.path()).is_none(),
            "a clean drop must remove its own sidecar"
        );
    }

    /// #426: a holder whose record was already replaced by the next holder
    /// must leave that record alone on drop.
    #[test]
    fn stale_holder_drop_keeps_the_next_holders_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = CollectionLayout::new(dir.path()).holder_path();
        let engine = Engine::open(dir.path(), "multilingual-e5-small").unwrap();

        // The next holder has since written its own record.
        write_at(&path, &other_holder()).unwrap();
        drop(engine);

        assert_eq!(
            read_holder(dir.path()),
            Some(other_holder()),
            "a stale holder's drop deleted the next holder's sidecar"
        );
    }

    /// A sidecar left behind by a holder that crashed is replaced by the next
    /// successful open.
    #[test]
    fn open_overwrites_a_crashed_holders_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = CollectionLayout::new(dir.path()).holder_path();
        write_at(&path, &other_holder()).unwrap();

        let _engine = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        assert_eq!(read_holder(dir.path()).unwrap().pid, std::process::id());
    }

    /// #426: when our own record can't be written, a stale record from an
    /// earlier holder must not survive under us. The write is made to fail by
    /// occupying its temp path with a directory.
    #[test]
    fn failed_write_removes_a_stale_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = CollectionLayout::new(dir.path()).holder_path();
        write_at(&path, &other_holder()).unwrap();
        std::fs::create_dir(path.with_extension(format!("holder.{}.tmp", std::process::id())))
            .unwrap();

        let _engine = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        assert!(
            read_holder(dir.path()).is_none(),
            "a stale holder record survived under a live new holder"
        );
    }

    /// The loser of the lock race must not touch the winner's sidecar.
    #[test]
    fn busy_open_leaves_the_holders_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        let before = read_holder(dir.path()).unwrap();

        let err = Engine::open(dir.path(), "multilingual-e5-small")
            .err()
            .expect("second open is busy");
        assert!(crate::error::is_engine_busy(&err), "{err:#}");
        assert_eq!(read_holder(dir.path()), Some(before));
    }

    #[test]
    fn read_holder_is_none_for_garbage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(CollectionLayout::new(dir.path()).holder_path(), b"not json").unwrap();
        assert!(read_holder(dir.path()).is_none());
    }
}
