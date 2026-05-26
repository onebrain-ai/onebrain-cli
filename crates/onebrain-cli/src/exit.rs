//! Exit-code mapping (skill-alignment §4.8).
//!
//! Single source of truth for `CoreError → i32 exit code`. Walks the full
//! `anyhow` chain so a `CoreError` wrapped inside `FsError`/`CacheError` from
//! a sibling library still yields its specific code.
//!
//! Stable for v3.x — adding a new code is a minor bump, changing/removing one
//! is a breaking change.

use onebrain_core::CoreError;

/// Stable exit codes — public so the alias-dispatcher and integration tests
/// can reference them by name. `EXIT_OK` / `EXIT_INVALID_ARGS` are the
/// canonical values returned by the OS (process success / clap parse
/// failure) rather than by our dispatcher; we still expose them as named
/// constants so test assertions stay readable.
#[allow(dead_code)]
pub const EXIT_OK: i32 = 0;
pub const EXIT_GENERIC: i32 = 1;
#[allow(dead_code)]
pub const EXIT_INVALID_ARGS: i32 = 2;
pub const EXIT_VAULT_NOT_FOUND: i32 = 64;
pub const EXIT_INVALID_YAML: i32 = 65;
pub const EXIT_FS_ERROR: i32 = 66;
pub const EXIT_CACHE_ERROR: i32 = 67;
pub const EXIT_NETWORK: i32 = 68;
pub const EXIT_INVALID_DATE: i32 = 70;
pub const EXIT_INVALID_TARGET: i32 = 71;
pub const EXIT_NOT_IMPLEMENTED: i32 = 72;
pub const EXIT_RPC_HANDSHAKE: i32 = 73;
pub const EXIT_AUTH_FAILED: i32 = 74;
pub const EXIT_INIT_TARGET_NOT_EMPTY: i32 = 75;
pub const EXIT_ROLLBACK_INCOMPLETE: i32 = 76;

/// Map a `CoreError` directly to its stable exit code.
pub fn exit_code_for_core(err: &CoreError) -> i32 {
    match err {
        CoreError::VaultYamlMissing { .. } => EXIT_VAULT_NOT_FOUND,
        CoreError::VaultNotFound { .. } => EXIT_VAULT_NOT_FOUND,
        CoreError::NotAVault { .. } => EXIT_VAULT_NOT_FOUND,
        CoreError::InvalidYaml(_) => EXIT_INVALID_YAML,
        CoreError::FsError { .. } => EXIT_FS_ERROR,
        CoreError::CacheError(_) => EXIT_CACHE_ERROR,
        CoreError::Network(_) => EXIT_NETWORK,
        CoreError::InvalidDate(_) => EXIT_INVALID_DATE,
        CoreError::InvalidTarget(_) => EXIT_INVALID_TARGET,
        CoreError::NotImplemented(_) => EXIT_NOT_IMPLEMENTED,
        CoreError::RpcHandshake(_) => EXIT_RPC_HANDSHAKE,
        CoreError::AuthFailed(_) => EXIT_AUTH_FAILED,
        CoreError::InitTargetNotEmpty(_) => EXIT_INIT_TARGET_NOT_EMPTY,
        CoreError::RollbackIncomplete(_) => EXIT_ROLLBACK_INCOMPLETE,
    }
}

/// Walk the full `anyhow` error chain, classify by the first known error
/// type encountered. Falls back to wrapper-specific codes for legacy crate
/// errors (`FsError` / `CacheError`), then probes for a bare
/// `std::io::Error` (broken pipe / permission denied get specific exits),
/// then finally collapses to `EXIT_GENERIC`.
pub fn exit_code_for(err: &anyhow::Error) -> i32 {
    for cause in err.chain() {
        if let Some(core) = cause.downcast_ref::<CoreError>() {
            return exit_code_for_core(core);
        }
        // `#[error(transparent)]` on `FsError::Core` delegates Display
        // through, but anyhow's `chain()` walks `std::error::Error::source()`
        // which thiserror only wires up for explicit `#[source]` fields —
        // not for transparent passthrough. So a `FsError::Core(CoreError)`
        // shows up in the chain as `FsError` but its inner `CoreError` is
        // never iterated. Probe for that pattern here so the specific
        // exit code (e.g. 75 for E_INIT_TARGET_NOT_EMPTY) wins over the
        // generic EXIT_FS_ERROR catch-all.
        if let Some(onebrain_fs::FsError::Core(core)) = cause.downcast_ref::<onebrain_fs::FsError>()
        {
            return exit_code_for_core(core);
        }
    }
    // No CoreError anywhere in the chain — classify by the root wrapper for
    // back-compat with v3.0 callers.
    if err.downcast_ref::<onebrain_fs::FsError>().is_some() {
        return EXIT_FS_ERROR;
    }
    if err.downcast_ref::<onebrain_cache::CacheError>().is_some() {
        return EXIT_CACHE_ERROR;
    }
    // Walk for a bare std::io::Error in the chain. Broken pipe is the
    // POSIX-correct "the consumer hung up" — treat as exit 0 so
    // `onebrain ... | head -1` doesn't show an error. Permission denied
    // maps to EXIT_FS_ERROR (66) per skill-alignment §4.8 (E_FS_ERROR is
    // already the bucket for EACCES). Everything else falls through.
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            match io_err.kind() {
                std::io::ErrorKind::BrokenPipe => return EXIT_OK,
                std::io::ErrorKind::PermissionDenied => return EXIT_FS_ERROR,
                _ => {}
            }
        }
        // R2-M4: serde_json::Error wraps the underlying io::Error in its
        // own enum (its source() returns None), so the plain
        // `downcast_ref::<io::Error>` walk misses it. Use
        // `io_error_kind()` to recover the original ErrorKind. Without
        // this, `onebrain ... --json | head -1` would surface exit 1
        // (generic failure) instead of the POSIX-correct 0.
        if let Some(json_err) = cause.downcast_ref::<serde_json::Error>() {
            if let Some(kind) = json_err.io_error_kind() {
                match kind {
                    std::io::ErrorKind::BrokenPipe => return EXIT_OK,
                    std::io::ErrorKind::PermissionDenied => return EXIT_FS_ERROR,
                    _ => {}
                }
            }
        }
    }
    EXIT_GENERIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn vault_not_found_maps_to_64() {
        let e = CoreError::VaultNotFound {
            cwd: PathBuf::from("/x"),
        };
        assert_eq!(exit_code_for_core(&e), 64);
    }

    #[test]
    fn invalid_yaml_maps_to_65() {
        // Provoke a real serde_yaml::Error so we don't have to construct one.
        let yaml_err: serde_yaml::Error =
            serde_yaml::from_str::<serde_yaml::Value>("not: : valid").unwrap_err();
        let e = CoreError::InvalidYaml(yaml_err);
        assert_eq!(exit_code_for_core(&e), 65);
    }

    #[test]
    fn fs_error_maps_to_66() {
        let e = CoreError::FsError {
            path: PathBuf::from("/x"),
            source: std::io::Error::other("denied"),
        };
        assert_eq!(exit_code_for_core(&e), 66);
    }

    #[test]
    fn cache_error_maps_to_67() {
        assert_eq!(exit_code_for_core(&CoreError::CacheError("bad".into())), 67);
    }

    #[test]
    fn network_maps_to_68() {
        assert_eq!(exit_code_for_core(&CoreError::Network("dns".into())), 68);
    }

    #[test]
    fn invalid_date_maps_to_70() {
        assert_eq!(
            exit_code_for_core(&CoreError::InvalidDate("bad".into())),
            70
        );
    }

    #[test]
    fn invalid_target_maps_to_71() {
        assert_eq!(
            exit_code_for_core(&CoreError::InvalidTarget("path".into())),
            71
        );
    }

    #[test]
    fn not_implemented_maps_to_72() {
        assert_eq!(
            exit_code_for_core(&CoreError::NotImplemented("note search".into())),
            72
        );
    }

    #[test]
    fn rpc_handshake_maps_to_73() {
        assert_eq!(
            exit_code_for_core(&CoreError::RpcHandshake("v1".into())),
            73
        );
    }

    #[test]
    fn auth_failed_maps_to_74() {
        assert_eq!(exit_code_for_core(&CoreError::AuthFailed("no".into())), 74);
    }

    #[test]
    fn init_target_not_empty_maps_to_75() {
        assert_eq!(
            exit_code_for_core(&CoreError::InitTargetNotEmpty("contents".into())),
            75
        );
    }

    #[test]
    fn rollback_incomplete_maps_to_76() {
        assert_eq!(
            exit_code_for_core(&CoreError::RollbackIncomplete(
                "could not restore a.md".into()
            )),
            76
        );
    }

    #[test]
    fn rollback_incomplete_wrapped_in_fs_error_still_maps_to_76() {
        // Real-world path: move_note returns FsError::Core(RollbackIncomplete).
        // The exit-code walker must reach the wrapped CoreError, not fall
        // through to the FsError generic mapping (which would emit 66).
        let fs_err: onebrain_fs::FsError =
            onebrain_fs::FsError::Core(CoreError::RollbackIncomplete("a.md".into()));
        let e: anyhow::Error = anyhow::Error::from(fs_err);
        assert_eq!(exit_code_for(&e), 76);
    }

    #[test]
    fn init_target_not_empty_wrapped_in_fs_error_still_maps_to_75() {
        // Real-world path: run_init returns FsError::Core(InitTargetNotEmpty).
        // The exit-code walker must reach the wrapped CoreError, not fall
        // through to the FsError generic mapping (which would emit 66).
        let fs_err: onebrain_fs::FsError =
            onebrain_fs::FsError::Core(CoreError::InitTargetNotEmpty("non-empty".into()));
        let e: anyhow::Error = anyhow::Error::from(fs_err);
        assert_eq!(exit_code_for(&e), 75);
    }

    #[test]
    fn anyhow_chain_finds_wrapped_core_error() {
        // Mirror the round-1 finding: a `CoreError` inside a `.context()`
        // wrapper still resolves to its specific code.
        let core: anyhow::Error = anyhow::Error::new(CoreError::VaultNotFound {
            cwd: PathBuf::from("/x"),
        });
        let wrapped = core.context("loading vault config");
        assert_eq!(exit_code_for(&wrapped), 64);
    }

    #[test]
    fn anyhow_chain_falls_back_to_generic() {
        let e: anyhow::Error = anyhow::anyhow!("some random error");
        assert_eq!(exit_code_for(&e), 1);
    }

    #[test]
    fn broken_pipe_classifies_to_zero() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "downstream closed");
        let e: anyhow::Error = anyhow::Error::from(io_err);
        assert_eq!(exit_code_for(&e), EXIT_OK);
    }

    #[test]
    fn broken_pipe_wrapped_in_context_still_classifies_to_zero() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "downstream closed");
        let e: anyhow::Error = anyhow::Error::from(io_err).context("rendering envelope");
        assert_eq!(exit_code_for(&e), EXIT_OK);
    }

    #[test]
    fn permission_denied_classifies_to_fs_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES");
        let e: anyhow::Error = anyhow::Error::from(io_err);
        assert_eq!(exit_code_for(&e), EXIT_FS_ERROR);
    }

    #[test]
    fn serde_json_broken_pipe_classifies_to_zero() {
        // R2-M4: serde_json wraps the io::Error in its own enum so the
        // plain io::Error chain walk misses it; io_error_kind() recovers
        // the original kind. Without this an `onebrain … --json | head -1`
        // pipeline would surface exit 1 instead of POSIX-correct 0.
        struct BrokenWriter;
        impl std::io::Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
        }
        let json_err: serde_json::Error =
            serde_json::to_writer(BrokenWriter, &serde_json::json!({"x":1})).unwrap_err();
        assert_eq!(
            json_err.io_error_kind(),
            Some(std::io::ErrorKind::BrokenPipe)
        );
        let e: anyhow::Error = anyhow::Error::from(json_err);
        assert_eq!(exit_code_for(&e), EXIT_OK);
    }
}
