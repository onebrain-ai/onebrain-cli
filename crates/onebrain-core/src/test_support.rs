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

use std::ffi::OsStr;

pub(crate) struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A poisoned lock only means an earlier test panicked mid-guard —
        // its Drop already restored the env, so recover rather than panic.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key,
            prev,
            _lock: lock,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}
