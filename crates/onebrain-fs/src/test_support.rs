//! Test-only guard for mutating process-global environment variables.
//!
//! Shared by `doctor::vault_yml`, `doctor::vault_yml_keys`, and `update`
//! tests. The process environment is shared by every test thread in this
//! binary, so a bare `set_var`/`remove_var` pair risks leaking the value
//! into whatever other test happens to run concurrently if a panic lands
//! between the two calls. Route env mutations through [`EnvGuard`] instead:
//! it restores the prior value on drop (including on panic unwind).
//!
//! Two constructors cover the two shapes callers need:
//! - [`EnvGuard::set`] acquires a crate-wide lock for the guard's lifetime —
//!   use this for a standalone env mutation so it can't interleave with any
//!   other env-mutating test in this crate.
//! - [`EnvGuard::set_within_lock`] skips the lock — use this only when the
//!   caller already holds it via an outer `set()` call in the same thread
//!   (the lock is not reentrant; nesting two `set()` calls deadlocks).

use std::ffi::OsStr;

pub(crate) struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
    _lock: Option<std::sync::MutexGuard<'static, ()>>,
}

impl EnvGuard {
    /// Set `key`, holding the shared lock for the guard's lifetime. Safe to
    /// call standalone; do not nest under another `set()` call on the same
    /// thread.
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A poisoned lock only means an earlier test panicked mid-guard —
        // its Drop already restored the env, so recover rather than panic.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::new(key, value, Some(lock))
    }

    /// Set `key` without acquiring the lock — for a second env mutation
    /// nested inside a scope that already holds it via [`EnvGuard::set`].
    pub(crate) fn set_within_lock(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        Self::new(key, value, None)
    }

    fn new(
        key: &'static str,
        value: impl AsRef<OsStr>,
        lock: Option<std::sync::MutexGuard<'static, ()>>,
    ) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key,
            previous,
            _lock: lock,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}
