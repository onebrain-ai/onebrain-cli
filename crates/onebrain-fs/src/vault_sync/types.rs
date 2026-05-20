//! Public types for `vault_sync` — options struct + result struct.
//!
//! Mirrors Bun's `VaultSyncOptions` / `VaultSyncResult` shape so parity tests
//! can serialize results to JSON and diff against the Bun output.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Caller-injected hook for HTTP fetches. Returns the response body bytes on
/// success; the error string surfaces in `VaultSyncResult::error`.
///
/// Production wires `download::default_fetch`, tests inject a closure returning
/// canned bytes or a `mockito` server URL.
pub type FetchFn = Box<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

/// Caller-injected unlink hook. Used by tests to simulate `EACCES` on a subset
/// of stale files (Bun test 13).
pub type UnlinkFn = Box<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

/// Caller-injected clock — defaults to `Utc::now`.
pub type NowFn = Box<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Options for `run_vault_sync`. Every field is optional; the defaults mirror
/// the Bun `runVaultSync` defaults.
#[derive(Default)]
pub struct VaultSyncOptions {
    /// Overrides `vault.yml::update_channel` branch resolution.
    pub branch: Option<String>,
    /// Mock fetch — defaults to a real `reqwest::blocking` GET against GitHub.
    pub fetch_fn: Option<FetchFn>,
    /// Override path to `installed_plugins.json` (test-only).
    pub installed_plugins_path: Option<PathBuf>,
    /// Override the plugins cache dir (test-only).
    pub installed_plugins_cache_dir: Option<PathBuf>,
    /// Override TTY detection — defaults to `io::stdout().is_terminal()`.
    pub is_tty: Option<bool>,
    /// Injectable unlink — defaults to `std::fs::remove_file`.
    pub unlink_fn: Option<UnlinkFn>,
    /// When true, also extract `.obsidian/` from the tarball (init only).
    pub include_obsidian: bool,
    /// When true, suppress the intro/outro frame and use the embedded `▸ `
    /// step prefix (called as a sub-operation from `init`).
    pub embedded: bool,
    /// Injectable clock — defaults to `Utc::now`.
    pub now_fn: Option<NowFn>,
    /// Redirect non-TTY `PlainProgress` output away from stdout. When `Some`,
    /// status lines route to the provided writer; used by `doctor --json` to
    /// keep the JSON document as the sole stdout payload. Forces
    /// `PlainProgress` (no TTY spinner) regardless of `is_tty`. Has no effect
    /// on TTY mode (spinner) — that path always uses real stdout.
    pub progress_writer: Option<Box<dyn std::io::Write + Send>>,
}

/// Result of a vault-sync invocation. Serialized verbatim for `--json` output
/// in future slices and for parity diffs against the Bun result.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VaultSyncResult {
    pub ok: bool,
    pub version: String,
    pub branch: String,
    #[serde(rename = "filesAdded")]
    pub files_added: u64,
    #[serde(rename = "filesRemoved")]
    pub files_removed: u64,
    #[serde(rename = "importsAdded")]
    pub imports_added: u64,
    #[serde(rename = "pinSkipped")]
    pub pin_skipped: bool,
    #[serde(rename = "cacheRemoved")]
    pub cache_removed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl VaultSyncResult {
    /// Initial state for a sync that hasn't started yet — `ok: false` until the
    /// orchestrator flips it on success. `branch` is the only field the caller
    /// can fill in confidently before step 1, so it's required up-front.
    pub(crate) fn pending(branch: String) -> Self {
        Self {
            ok: false,
            version: "unknown".to_string(),
            branch,
            files_added: 0,
            files_removed: 0,
            imports_added: 0,
            pin_skipped: true,
            cache_removed: 0,
            error: None,
        }
    }
}
