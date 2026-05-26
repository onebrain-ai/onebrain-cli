use std::path::PathBuf;
use thiserror::Error;

/// Canonical OneBrain error taxonomy. Each variant maps 1:1 to a stable
/// `E_*` code + exit code per skill-alignment §4.8. The binary crate
/// (`onebrain-cli`) uses [`CoreError::error_code`] + an `exit_code_for` helper
/// to render the user-facing envelope + process exit; library crates
/// pattern-match.
///
/// Adding a new variant is a minor version bump. Changing or removing one is a
/// breaking change.
#[derive(Error, Debug)]
pub enum CoreError {
    // ─── Legacy variants (kept verbatim for v3.0 compat) ────────────────
    #[error("vault.yml not found at {path}")]
    VaultYamlMissing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("vault.yml has invalid syntax")]
    InvalidYaml(#[from] serde_yaml::Error),

    #[error("path is not a valid vault root: {path}")]
    NotAVault { path: PathBuf },

    // ─── v3.1 additions (skill-alignment §4.8) ──────────────────────────
    /// No vault.yml found by walking up from cwd / `--vault` path. Exit 64.
    /// Distinct from `VaultYamlMissing` (single read attempt failure with a
    /// wrapped io::Error) — this variant is the outcome of the full walk-up
    /// resolution chain reporting "no vault anywhere".
    #[error("no OneBrain vault found by walking up from {cwd}")]
    VaultNotFound { cwd: PathBuf },

    /// Generic filesystem I/O failure (EACCES, ENOSPC, ENOENT, etc.). Exit 66.
    #[error("filesystem error at {path}: {source}")]
    FsError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Session-state / cache write failure. Exit 67.
    #[error("cache error: {0}")]
    CacheError(String),

    /// Network failure (GitHub fetch, plugin update, etc.). Exit 68.
    #[error("network error: {0}")]
    Network(String),

    /// Bad `YYYY-MM-DD` string. Exit 70.
    #[error("invalid date: {0}")]
    InvalidDate(String),

    /// Path outside vault / not found. Exit 71.
    #[error("invalid target: {0}")]
    InvalidTarget(String),

    /// Known-deferred feature (placeholder command not yet implemented). Exit 72.
    #[error("not implemented: {0}")]
    NotImplemented(String),

    /// RPC protocol mismatch. Exit 73. Reserved for v3.x+ HTTP server.
    #[error("RPC handshake failed: {0}")]
    RpcHandshake(String),

    /// RPC auth token missing or wrong. Exit 74.
    #[error("RPC auth failed: {0}")]
    AuthFailed(String),

    /// `onebrain init` aborted because the target directory is not empty
    /// and `--force` was not passed (or the user declined the interactive
    /// prompt). Exit 75. The wrapped string holds a user-facing message
    /// summarising the target contents.
    #[error("init: {0}")]
    InitTargetNotEmpty(String),

    /// A transactional operation failed AND its rollback also failed, so the
    /// vault may be left in an inconsistent (half-rewritten) state. Exit 76.
    /// The wrapped string names which files could not be restored. Distinct
    /// from a clean rollback (which returns the original error unchanged) —
    /// this variant signals the "left exactly as it started" guarantee was
    /// broken.
    #[error("rollback incomplete — vault may be inconsistent: {0}")]
    RollbackIncomplete(String),
}

impl CoreError {
    /// Stable machine-readable error code per skill-alignment §4.8. Always
    /// uppercase with `E_` prefix; included verbatim in the JSON envelope's
    /// `error.code` field.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::VaultYamlMissing { .. } | Self::VaultNotFound { .. } => "E_VAULT_NOT_FOUND",
            Self::InvalidYaml(_) => "E_INVALID_YAML",
            Self::NotAVault { .. } => "E_VAULT_NOT_FOUND",
            Self::FsError { .. } => "E_FS_ERROR",
            Self::CacheError(_) => "E_CACHE_ERROR",
            Self::Network(_) => "E_NETWORK",
            Self::InvalidDate(_) => "E_INVALID_DATE",
            Self::InvalidTarget(_) => "E_INVALID_TARGET",
            Self::NotImplemented(_) => "E_NOT_IMPLEMENTED",
            Self::RpcHandshake(_) => "E_RPC_HANDSHAKE",
            Self::AuthFailed(_) => "E_AUTH_FAILED",
            Self::InitTargetNotEmpty(_) => "E_INIT_TARGET_NOT_EMPTY",
            Self::RollbackIncomplete(_) => "E_ROLLBACK_INCOMPLETE",
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn error_codes_are_stable() {
        // Lock the public code surface. Any change to these strings is a
        // breaking-change tagged release per skill-alignment §4.8.
        assert_eq!(
            CoreError::VaultNotFound {
                cwd: PathBuf::from("/x")
            }
            .error_code(),
            "E_VAULT_NOT_FOUND"
        );
        assert_eq!(
            CoreError::FsError {
                path: PathBuf::from("/x"),
                source: std::io::Error::other("nope"),
            }
            .error_code(),
            "E_FS_ERROR"
        );
        assert_eq!(
            CoreError::CacheError("bad".into()).error_code(),
            "E_CACHE_ERROR"
        );
        assert_eq!(CoreError::Network("dns".into()).error_code(), "E_NETWORK");
        assert_eq!(
            CoreError::InvalidDate("2026-13-01".into()).error_code(),
            "E_INVALID_DATE"
        );
        assert_eq!(
            CoreError::InvalidTarget("../escape".into()).error_code(),
            "E_INVALID_TARGET"
        );
        assert_eq!(
            CoreError::NotImplemented("note search".into()).error_code(),
            "E_NOT_IMPLEMENTED"
        );
        assert_eq!(
            CoreError::RpcHandshake("v1 vs v2".into()).error_code(),
            "E_RPC_HANDSHAKE"
        );
        assert_eq!(
            CoreError::AuthFailed("no token".into()).error_code(),
            "E_AUTH_FAILED"
        );
        assert_eq!(
            CoreError::InitTargetNotEmpty("3 files, 2 folders".into()).error_code(),
            "E_INIT_TARGET_NOT_EMPTY"
        );
        assert_eq!(
            CoreError::RollbackIncomplete("could not restore a.md".into()).error_code(),
            "E_ROLLBACK_INCOMPLETE"
        );
    }
}
