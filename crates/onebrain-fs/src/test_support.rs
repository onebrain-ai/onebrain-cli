//! Test-only guard for mutating process-global environment variables.
//!
//! Shared by `doctor::vault_yml`, `doctor::vault_yml_keys`, and `update`
//! tests. The process environment is shared by every test thread in this
//! binary, so a bare `set_var`/`remove_var` pair risks leaking the value
//! into whatever other test happens to run concurrently if a panic lands
//! between the two calls. Route env mutations through [`EnvGuard`] instead:
//! it restores the prior value on drop (including on panic unwind).
//!
//! [`EnvGuard::set_vars`] sets any number of pairs under ONE lock acquisition
//! atomically — this is the only mutation entry point. An earlier version
//! offered a second constructor, `set_within_lock`, for a caller that needed
//! a second env mutation nested inside a scope already holding the lock via
//! `set()`; that shape's "only call this under an outer lock" contract was
//! enforced by convention alone (a doc comment, not a runtime check), so a
//! future caller invoking it standalone would race every other test in this
//! crate without ever seeing a warning. `set_vars` replaces both: callers
//! that used to nest a `set()` + `set_within_lock()` pair now pass every key
//! they need to ONE `set_vars` call (see `update::tests::with_cache_path`
//! for the pattern) — issue #226.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds the crate-wide env lock for its lifetime and restores each
/// variable's prior state (previous value, or unset) on drop — including on
/// panic unwind, so one failing test can't leak env state into a later one.
pub(crate) struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Set one env var under the shared lock; restored when the guard drops.
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        Self::set_vars(&[(key, value.as_ref())])
    }

    /// Set several env vars atomically under ONE shared-lock acquisition —
    /// use this instead of two separate `set()` calls, which would deadlock
    /// on the non-reentrant lock.
    pub(crate) fn set_vars(pairs: &[(&'static str, &OsStr)]) -> Self {
        // A poisoned lock only means an earlier test panicked mid-guard —
        // its Drop already restored the env, so recover rather than panic.
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let saved = pairs
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in pairs {
            // SAFETY: the shared lock (held above) serializes this
            // mutation against every other env mutation in this crate's
            // tests — the single-process precondition edition-2024 will
            // require `unsafe` to assert. Wrapped now for forward
            // compatibility; `allow` suppresses today's unused-unsafe
            // warning under the current edition.
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var(key, value);
            }
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Runs before `_lock` releases (fields drop in declaration order —
        // `saved` before `_lock`), so restoration stays serialized.
        for (key, previous) in self.saved.drain(..) {
            match previous {
                Some(value) => {
                    // SAFETY: see the constructor — the guard still holds
                    // `ENV_LOCK` during Drop.
                    #[allow(unused_unsafe)]
                    unsafe {
                        std::env::set_var(key, value)
                    };
                }
                None => {
                    // SAFETY: same rationale.
                    #[allow(unused_unsafe)]
                    unsafe {
                        std::env::remove_var(key)
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_vars_sets_and_restores_multiple_vars_atomically() {
        let key_a = "ONEBRAIN_FS_TEST_SUPPORT_ENV_GUARD_A";
        let key_b = "ONEBRAIN_FS_TEST_SUPPORT_ENV_GUARD_B";
        {
            let _g = EnvGuard::set_vars(&[
                (key_a, OsStr::new("value-a")),
                (key_b, OsStr::new("value-b")),
            ]);
            assert_eq!(std::env::var(key_a).unwrap(), "value-a");
            assert_eq!(std::env::var(key_b).unwrap(), "value-b");
        }
        assert!(
            std::env::var_os(key_a).is_none(),
            "must be restored to absent"
        );
        assert!(
            std::env::var_os(key_b).is_none(),
            "must be restored to absent"
        );
    }

    #[test]
    fn set_vars_restores_on_panic_unwind() {
        let key = "ONEBRAIN_FS_TEST_SUPPORT_ENV_GUARD_PANIC";
        let result = std::panic::catch_unwind(|| {
            let _g = EnvGuard::set_vars(&[(key, OsStr::new("during-panic"))]);
            assert_eq!(std::env::var(key).unwrap(), "during-panic");
            panic!("intentional test panic");
        });
        assert!(result.is_err());
        assert!(
            std::env::var_os(key).is_none(),
            "must be restored even after a panic"
        );
    }
}
