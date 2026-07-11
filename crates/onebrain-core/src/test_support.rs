//! Test-only guard for mutating process-global environment variables.
//!
//! Shared by `config` and `path` tests, both of which toggle
//! `ONEBRAIN_QUIET_VAULT_YML_DEPRECATION` around calls into vault-root
//! resolution. The process environment is shared by every test thread in
//! this binary, so a bare `set_var`/`remove_var` pair risks leaking the
//! value into whatever other test happens to run concurrently if a panic
//! lands between the two calls. Route env mutations through [`EnvVarGuard`]
//! instead: it restores the prior value on drop (including on panic
//! unwind) and holds a crate-wide lock for its lifetime so no other env
//! mutation in this crate's tests can interleave with it.
//!
//! [`EnvVarGuard::set_vars`] sets any number of pairs under one lock
//! acquisition; [`EnvVarGuard::set`] is the single-pair convenience wrapper
//! most callers use. Mirrors `onebrain_fs::test_support::EnvGuard` and
//! `onebrain-cli`'s `test_env` (#226) — same shape, same `unsafe {}` +
//! `#[allow(unused_unsafe)]` wrapping around `std::env::set_var`/
//! `remove_var` anticipating the edition where those become `unsafe fn`.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    /// Set one env var under the shared lock; restored when the guard drops.
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        Self::set_vars(&[(key, value.as_ref())])
    }

    /// Set several env vars atomically under one shared-lock acquisition.
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
            // tests.
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var(key, value);
            }
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..) {
            match previous {
                Some(value) => {
                    // SAFETY: the guard still holds `ENV_LOCK` during Drop.
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
        let key_a = "ONEBRAIN_CORE_TEST_SUPPORT_ENV_GUARD_A";
        let key_b = "ONEBRAIN_CORE_TEST_SUPPORT_ENV_GUARD_B";
        {
            let _g = EnvVarGuard::set_vars(&[
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
}
