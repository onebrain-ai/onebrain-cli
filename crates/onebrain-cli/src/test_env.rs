//! Test-only guard for mutating process-global environment variables.
//!
//! The environment is shared by every test thread in this binary, so ALL
//! tests that `set_var`/`remove_var` must serialize on the SAME lock —
//! module-private mutexes (one each in `server::search`, `session_init`,
//! `banner`) only serialized within their own module and still raced each
//! other on `ONEBRAIN_CACHE_DIR` (intermittent failures under the default
//! parallel runner). Route every env mutation in this crate's tests through
//! [`set_vars`] instead of declaring a local lock.
//!
//! The guard also makes failures non-cascading: the previous value is
//! restored on drop (including panic unwind), and a poisoned lock is
//! recovered rather than propagated, so one failing test can't leak env
//! state into — or poison the lock for — every later env-dependent test.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds the process-wide env lock for its lifetime and restores each
/// variable's prior state (previous value, or unset) on drop.
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

/// Set one env var under the shared lock; restored when the guard drops.
pub(crate) fn set_var(key: &'static str, value: impl AsRef<OsStr>) -> EnvVarGuard {
    set_vars(&[(key, value.as_ref())])
}

/// Set several env vars atomically under the shared lock (one guard — calling
/// [`set_var`] twice would deadlock on the non-reentrant lock).
pub(crate) fn set_vars(pairs: &[(&'static str, &OsStr)]) -> EnvVarGuard {
    // A poisoned lock only means an earlier test panicked mid-guard; its
    // Drop already restored the env, so the data is consistent — recover.
    let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let saved = pairs
        .iter()
        .map(|(key, _)| (*key, std::env::var_os(key)))
        .collect();
    for (key, value) in pairs {
        // SAFETY: the shared lock (held above) serializes this mutation
        // against every other env mutation in this crate's tests — the
        // single-process precondition edition-2024 will require `unsafe`
        // to assert. Wrapped now for forward compatibility; `allow`
        // suppresses today's unused-unsafe warning under the current
        // edition (parity with the Drop impl below — #226).
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(key, value);
        }
    }
    EnvVarGuard { saved, _lock: lock }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // Runs before `_lock` is released (fields drop after the body).
        for (key, prev) in self.saved.drain(..) {
            match prev {
                Some(value) => {
                    // SAFETY: the guard still holds `ENV_LOCK` during Drop, so
                    // restoration is serialized with other mutations that
                    // follow this helper's contract.
                    #[allow(unused_unsafe)]
                    unsafe {
                        std::env::set_var(key, value)
                    };
                }
                None => {
                    // SAFETY: same rationale — lock held while mutating the
                    // process environment during restoration.
                    #[allow(unused_unsafe)]
                    unsafe {
                        std::env::remove_var(key)
                    };
                }
            }
        }
    }
}
