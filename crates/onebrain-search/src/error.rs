//! Typed engine-open failures that callers must distinguish from a generic
//! I/O error.
//!
//! redb is a **single-process** embedded database: it takes an exclusive
//! advisory lock on its backing file at open time, so a second process (or a
//! second in-process handle) that opens the same `engine.redb` / vector-meta
//! db fails with [`redb::DatabaseError::DatabaseAlreadyOpen`]. In OneBrain this
//! happens routinely: the long-lived `onebrain mcp` server holds the search
//! engine open for a whole session, so any other command that opens the same
//! collection (`search status`, `search query`, a reindex hook) hits the lock.
//!
//! Before v3.4.6 that lock error was swallowed or collapsed into a generic
//! `E_INTERNAL` — `search status` even reported zeros as if the index were
//! healthy. [`EngineBusy`] makes the contention **honest**: [`Engine::open`]
//! classifies the redb lock case into this typed error, and the CLI maps it to
//! a stable `E_ENGINE_BUSY` code + a dedicated exit code so every surface
//! (status, query, hook skips, exit codes) reports the same thing.

use std::fmt;

/// The search engine could not be opened because its on-disk database is
/// already locked by another process (typically the `onebrain mcp` server) or
/// another live handle in this process.
///
/// redb is single-process by design; this is expected contention, not
/// corruption. Callers should surface it as a transient "busy, try again"
/// state rather than a hard failure — the lock is released when the holder
/// exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineBusy;

impl fmt::Display for EngineBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "search engine is busy — its index is locked by another process \
             (e.g. the `onebrain mcp` server); try again once it releases the lock"
        )
    }
}

impl std::error::Error for EngineBusy {}

/// `true` when `err` (or anything in its `anyhow` cause chain) is redb's
/// "database already open / cannot acquire lock" signal.
///
/// Matches on the typed redb error kind ([`redb::DatabaseError::DatabaseAlreadyOpen`]
/// and the equivalent [`redb::Error::DatabaseAlreadyOpen`] that
/// `Database::create` surfaces after a `?` conversion), NOT a fragile
/// substring of the Display text — so a redb message reword can't silently
/// break the classification. A defensive string check is kept as a last
/// resort for any redb path that erases the typed variant (see the test).
pub fn is_redb_lock_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(db_err) = cause.downcast_ref::<redb::DatabaseError>() {
            if matches!(db_err, redb::DatabaseError::DatabaseAlreadyOpen) {
                return true;
            }
        }
        if let Some(top) = cause.downcast_ref::<redb::Error>() {
            if matches!(top, redb::Error::DatabaseAlreadyOpen) {
                return true;
            }
        }
    }
    // Defensive fallback: if some redb path erased the typed variant into a
    // plain error, still catch the canonical lock phrasing. redb prints
    // "Database already open. Cannot acquire lock." for this case.
    let msg = format!("{err:#}");
    msg.contains("already open") && msg.contains("acquire lock")
}

/// If `err` is the redb lock case, replace it with the typed [`EngineBusy`]
/// error (preserving the human-readable chain as context); otherwise return
/// `err` unchanged. Called on every [`Engine::open`] result so downstream
/// callers can `downcast_ref::<EngineBusy>()` regardless of which internal
/// db (vector-meta or `engine.redb`) tripped the lock.
pub fn classify_open_error(err: anyhow::Error) -> anyhow::Error {
    if is_redb_lock_error(&err) {
        // Keep the original chain as context so `-v` / logs still show the
        // path, but the *typed* head is EngineBusy for machine routing.
        anyhow::Error::new(EngineBusy).context(err.to_string())
    } else {
        err
    }
}

/// `true` when `err` (or its cause chain) is the typed [`EngineBusy`] error.
/// The canonical predicate CLI surfaces use to branch on lock contention.
pub fn is_engine_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.is::<EngineBusy>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_busy_displays_a_helpful_message() {
        let msg = EngineBusy.to_string();
        assert!(msg.contains("busy"), "{msg}");
        assert!(msg.contains("locked by another process"), "{msg}");
    }

    #[test]
    fn is_redb_lock_error_true_for_typed_database_already_open() {
        let err: anyhow::Error = anyhow::Error::new(redb::DatabaseError::DatabaseAlreadyOpen)
            .context("opening redb database /x/engine.redb");
        assert!(is_redb_lock_error(&err));
    }

    #[test]
    fn is_redb_lock_error_true_for_top_level_variant() {
        let err: anyhow::Error = anyhow::Error::new(redb::Error::DatabaseAlreadyOpen);
        assert!(is_redb_lock_error(&err));
    }

    #[test]
    fn is_redb_lock_error_false_for_unrelated_error() {
        let err = anyhow::anyhow!("some unrelated failure");
        assert!(!is_redb_lock_error(&err));
    }

    #[test]
    fn is_redb_lock_error_matches_string_fallback() {
        // A redb path that lost the typed variant but kept the canonical
        // phrasing must still classify as a lock error.
        let err = anyhow::anyhow!("Database already open. Cannot acquire lock.");
        assert!(is_redb_lock_error(&err));
    }

    #[test]
    fn classify_open_error_converts_lock_to_engine_busy() {
        let err: anyhow::Error = anyhow::Error::new(redb::DatabaseError::DatabaseAlreadyOpen)
            .context("opening redb database /x/engine.redb");
        let classified = classify_open_error(err);
        assert!(is_engine_busy(&classified));
        assert!(classified.downcast_ref::<EngineBusy>().is_some());
    }

    #[test]
    fn classify_open_error_passes_through_non_lock_errors() {
        let err = anyhow::anyhow!("disk full");
        let classified = classify_open_error(err);
        assert!(!is_engine_busy(&classified));
        assert_eq!(classified.to_string(), "disk full");
    }

    #[test]
    fn is_engine_busy_false_for_plain_error() {
        assert!(!is_engine_busy(&anyhow::anyhow!("nope")));
    }
}
