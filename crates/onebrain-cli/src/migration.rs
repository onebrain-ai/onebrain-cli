//! Migration notice — print v3.0→v3.1 rename guidance once per command.
//!
//! State file: `migration-shown.txt` under the platform data dir
//! (`~/Library/Application Support/onebrain/` on macOS · `$XDG_DATA_HOME/onebrain/`
//! or `~/.local/share/onebrain/` on Linux · `%APPDATA%\onebrain\` on Windows),
//! one alias name per line. On first invocation of each v3.0 alias we
//! append the name and print a stderr notice; subsequent invocations
//! within or across processes find the name in the file and stay silent.
//!
//! Power users can suppress entirely via `ONEBRAIN_QUIET_MIGRATION=1`.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const STATE_FILENAME: &str = "migration-shown.txt";
const ENV_SUPPRESS: &str = "ONEBRAIN_QUIET_MIGRATION";

/// Ensures the "could not persist migration state" warning is emitted at most
/// once per process. Without this, every alias invocation in a process that
/// can't write to the state dir would emit the warning, defeating the point
/// of the migration notice's one-time semantics.
static MIGRATION_WARN_EMITTED: AtomicBool = AtomicBool::new(false);

/// Side-effect-free check: should we emit a migration notice for `old`?
///
/// Pure function over the state-file directory so unit tests don't touch
/// `~/.cache`. Returns `false` when the env-var suppression is active OR the
/// state file already records `old`.
pub fn should_show(state_dir: &std::path::Path, old: &str, env_suppress: bool) -> bool {
    if env_suppress {
        return false;
    }
    let state_file = state_dir.join(STATE_FILENAME);
    match fs::read_to_string(&state_file) {
        Ok(s) => !s.lines().any(|line| line.trim() == old),
        Err(_) => true, // missing state file → first run
    }
}

/// Record `old` in the state file so future invocations stay silent.
///
/// Persistent write failures (read-only cache dir, full disk, etc.) would
/// otherwise cause the migration notice to repeat on every invocation. We
/// emit ONE stderr warning per process when the first failure is observed
/// (gated by `MIGRATION_WARN_EMITTED` so the warning itself doesn't spam),
/// then continue — the user keeps seeing the notice, but is also told why.
/// Returns `true` on success, `false` when persistence failed.
pub fn record(state_dir: &std::path::Path, old: &str) -> bool {
    let state_file = state_dir.join(STATE_FILENAME);
    if let Err(e) = fs::create_dir_all(state_dir) {
        warn_persist_failure(&state_file, &e);
        return false;
    }
    // Read-modify-write to dedupe even if a parallel process raced us.
    let mut seen: HashSet<String> = match fs::read_to_string(&state_file) {
        Ok(s) => s.lines().map(|l| l.trim().to_string()).collect(),
        Err(_) => HashSet::new(),
    };
    if seen.insert(old.to_string()) {
        // New entry — rewrite the file with all known aliases on one line each.
        let mut sorted: Vec<&str> = seen.iter().map(|s| s.as_str()).collect();
        sorted.sort();
        let body = sorted.join("\n");
        if let Err(e) = fs::write(&state_file, body) {
            warn_persist_failure(&state_file, &e);
            return false;
        }
    }
    true
}

/// One-shot stderr warning emitted on the first persistent write failure
/// per process. Subsequent failures stay silent — the user already knows
/// the notice will repeat.
fn warn_persist_failure(state_file: &std::path::Path, err: &std::io::Error) {
    if !MIGRATION_WARN_EMITTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "onebrain: warning: could not persist migration-shown state at {}: {} — notice will repeat",
            state_file.display(),
            err
        );
    }
}

/// Default state directory: `~/Library/Application Support/onebrain/` on macOS ·
/// `$XDG_DATA_HOME/onebrain/` or `~/.local/share/onebrain/` on Linux ·
/// `%APPDATA%\onebrain\` on Windows.
///
/// Relocated from the OS-purgeable cache dir (`dirs::cache_dir()`) to the
/// persistent data dir (`dirs::data_dir()`) in v3.4.5 — `~/Library/Caches` &
/// friends can be wiped by OS storage cleanup, which silently destroyed an
/// expensive model download + index (issue #114). The one-time move of
/// pre-existing state is handled by [`migrate_search_cache`]; see ADR 0021.
///
/// **Test override:** `ONEBRAIN_CACHE_DIR` env var (name kept for
/// compatibility with the existing test suite), when set, replaces the
/// `dirs::data_dir()` lookup entirely — which also collapses the cache→data
/// migration to a no-op. Cross-platform — `HOME` / `XDG_DATA_HOME` don't
/// influence `dirs::data_dir()` on Windows (which reads `%APPDATA%` via the
/// Known Folders API), so tests need an explicit knob to redirect state into a
/// tempdir. Production should never set this.
pub fn default_state_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("ONEBRAIN_CACHE_DIR") {
        return PathBuf::from(override_dir);
    }
    dirs::data_dir()
        .map(|d| d.join("onebrain"))
        .unwrap_or_else(|| PathBuf::from("/tmp/onebrain"))
}

/// One-time relocation of the native-search state (`search/` subtree — models,
/// tantivy index, vector store, `engine.redb`) from the legacy OS-purgeable
/// cache location to the persistent data dir (issue #114 · ADR 0021).
///
/// Moves `<cache_dir>/onebrain/search` → `<data_dir>/onebrain/search` exactly
/// once, and **only** the `search/` subtree — the sibling
/// `latest-release.json` update-check cache is resolved independently via
/// `dirs::cache_dir()` (see `onebrain_fs::update`) and is genuinely disposable,
/// so it deliberately stays in the cache dir and must not be swept along.
///
/// No-op when: the `ONEBRAIN_CACHE_DIR` test override is set (cache and data
/// roots collapse to one dir), the platform dirs can't be resolved, there's
/// nothing at the old path, or the destination already exists. Best-effort —
/// a move failure is reported to stderr but never aborts the command; the
/// engine recreates an empty index at the new location and the old data is
/// left intact for manual recovery.
pub fn migrate_search_cache() {
    // Test override collapses cache and data to one dir → nothing to migrate.
    if std::env::var_os("ONEBRAIN_CACHE_DIR").is_some() {
        return;
    }
    let (Some(cache_root), Some(data_root)) = (dirs::cache_dir(), dirs::data_dir()) else {
        return;
    };
    let old = cache_root.join("onebrain").join("search");
    let new = data_root.join("onebrain").join("search");
    migrate_dir_once(&old, &new);
}

/// Rename `old` → `new` iff `old` exists and `new` does not, creating `new`'s
/// parent first. Pure over its path arguments (no platform-dir lookups), so
/// unit tests exercise it against tempdirs. Emits one stderr warning on
/// failure and returns without panicking; never overwrites an existing `new`.
fn migrate_dir_once(old: &Path, new: &Path) {
    if !old.exists() || new.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "onebrain: warning: could not create {} for search-cache relocation: {e}",
                parent.display()
            );
            return;
        }
    }
    if let Err(e) = fs::rename(old, new) {
        eprintln!(
            "onebrain: warning: could not relocate search cache {} → {}: {e} \
             (old data preserved — run `onebrain search reindex` if search is empty)",
            old.display(),
            new.display()
        );
    }
}

/// Print the user-facing notice. Stderr only — stdout stays clean for JSON
/// consumers. Pure formatter so callers can swap the writer in tests.
pub fn write_notice<W: Write>(mut w: W, old: &str, new_path: &str) -> std::io::Result<()> {
    writeln!(
        w,
        "⚠️  v3.1: `onebrain {}` has been renamed to `onebrain {}`.",
        old, new_path
    )?;
    writeln!(
        w,
        "   The old command still works, but please update hooks/scripts when convenient."
    )?;
    writeln!(
        w,
        "   (This notice shows once per command. Suppress with {}=1.)",
        ENV_SUPPRESS
    )?;
    Ok(())
}

/// One-shot helper: check state, print notice if needed, record. Wires the
/// three primitives together for the alias dispatcher in `main.rs`.
pub fn print_once(old: &str, new_path: &str) {
    let env_suppress = matches!(
        std::env::var(ENV_SUPPRESS).ok().as_deref(),
        Some("1") | Some("true")
    );
    let dir = default_state_dir();
    if should_show(&dir, old, env_suppress) {
        let _ = write_notice(std::io::stderr(), old, new_path);
        record(&dir, old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_run_shows_when_state_dir_missing() {
        let dir = tempdir().unwrap();
        // No state file exists yet.
        assert!(should_show(dir.path(), "session-init", false));
    }

    #[test]
    fn env_suppress_blocks_show() {
        let dir = tempdir().unwrap();
        assert!(!should_show(dir.path(), "session-init", true));
    }

    #[test]
    fn record_then_check_is_silent() {
        let dir = tempdir().unwrap();
        assert!(should_show(dir.path(), "orphan-scan", false));
        assert!(record(dir.path(), "orphan-scan"));
        assert!(!should_show(dir.path(), "orphan-scan", false));
    }

    #[test]
    fn record_dedupes_within_file() {
        let dir = tempdir().unwrap();
        assert!(record(dir.path(), "x"));
        assert!(record(dir.path(), "x"));
        assert!(record(dir.path(), "y"));
        let body = fs::read_to_string(dir.path().join(STATE_FILENAME)).unwrap();
        // Sorted, deduped.
        assert_eq!(body, "x\ny");
    }

    #[test]
    fn other_command_still_shows() {
        let dir = tempdir().unwrap();
        assert!(record(dir.path(), "session-init"));
        // A different alias has not been recorded yet.
        assert!(should_show(dir.path(), "orphan-scan", false));
    }

    #[test]
    fn record_returns_false_when_cannot_create_dir() {
        // Force a create_dir_all failure by pointing at a path whose parent
        // is an existing regular file — POSIX returns ENOTDIR.
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        fs::write(&blocker, b"").unwrap();
        let bad_state_dir = blocker.join("cache");
        // First call surfaces false (write failure).
        assert!(!record(&bad_state_dir, "session-init"));
        // Per the contract: failure does NOT panic and notice will repeat —
        // should_show stays true because the state file was never written.
        assert!(should_show(&bad_state_dir, "session-init", false));
    }

    #[test]
    fn write_notice_includes_old_and_new() {
        let mut buf = Vec::new();
        write_notice(&mut buf, "session-init", "session init").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("session-init"));
        assert!(s.contains("session init"));
        assert!(s.contains("ONEBRAIN_QUIET_MIGRATION"));
    }

    /// Exercises the `if let Err(e) = fs::write(&state_file, body)` arm in
    /// `record()` — `create_dir_all` succeeds but the subsequent `fs::write`
    /// fails because the directory is read-only.
    #[test]
    #[cfg(unix)]
    fn record_returns_false_when_state_file_write_fails() {
        // Skip if running as root (root ignores read-only permission bits).
        extern "C" {
            fn geteuid() -> u32;
        }
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // Remove write permission so fs::write inside record() will fail.
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = record(&state_dir, "test-alias");
        // Restore before TempDir drops so the cleanup can remove the directory.
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !result,
            "record must return false when the state file write fails"
        );
    }

    #[test]
    fn migrate_moves_when_old_exists_and_new_absent() {
        let root = tempdir().unwrap();
        let old = root.path().join("cache/onebrain/search");
        let new = root.path().join("data/onebrain/search");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("engine.redb"), b"index").unwrap();

        migrate_dir_once(&old, &new);

        assert!(!old.exists(), "old search dir should be gone after move");
        assert_eq!(fs::read(new.join("engine.redb")).unwrap(), b"index");
    }

    #[test]
    fn migrate_is_noop_when_new_already_exists() {
        let root = tempdir().unwrap();
        let old = root.path().join("cache/onebrain/search");
        let new = root.path().join("data/onebrain/search");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("engine.redb"), b"OLD").unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join("engine.redb"), b"NEW").unwrap();

        migrate_dir_once(&old, &new);

        // Destination is never clobbered; source is left untouched.
        assert_eq!(fs::read(new.join("engine.redb")).unwrap(), b"NEW");
        assert_eq!(fs::read(old.join("engine.redb")).unwrap(), b"OLD");
    }

    #[test]
    fn migrate_is_noop_when_old_absent() {
        let root = tempdir().unwrap();
        let old = root.path().join("cache/onebrain/search");
        let new = root.path().join("data/onebrain/search");

        migrate_dir_once(&old, &new);

        assert!(
            !new.exists(),
            "nothing to migrate → new must not be created"
        );
    }
}
