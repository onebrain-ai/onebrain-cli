//! Slice 13 — `vault-sync` · pulls the upstream `onebrain-ai/onebrain` release
//! tarball from GitHub and overlays the bundled plugin, harness, and root
//! docs onto a vault on disk. Mirrors the Bun source at
//! `src/commands/internal/vault-sync.ts` (v2.3.x).
//!
//! Public surface:
//!   - `run_vault_sync(vault_root, opts) -> VaultSyncResult` is the entry point.
//!   - `VaultSyncOptions` exposes the same injection hooks as Bun's options:
//!     `fetch_fn`, `unlink_fn`, `now_fn`, `installed_plugins_path`, …
//!   - `resolve_branch` + `build_tar_spawn_overrides` + `normalize_path` are
//!     re-exported for parity tests and external callers.
//!
//! Sub-modules each cover one ordered step from Bun:
//!   1. `download` — fetch tarball + extract (steps 1)
//!   2. `walker`   — recursive file listing helper
//!   3. `sync`     — overlay plugin, .gemini/, .obsidian (steps 2–4)
//!   4. `docs`     — copy root docs (step 5)
//!   5. `harness_merge` — merge @-imports into CLAUDE/GEMINI/AGENTS.md (step 6)
//!   6. `vault_yml`     — write update_channel to vault.yml (step 7)
//!   7. `pin`           — refresh installed_plugins.json entries (step 8)
//!   8. `cache_clean`   — rm -rf stale onebrain cache versions (step 9)
//!   9. `progress`      — TTY indicatif spinner / non-TTY plain `vault-sync:` lines

pub mod branch;
pub mod cache_clean;
pub mod docs;
pub mod download;
pub mod harness_merge;
pub mod pin;
pub mod progress;
pub mod sync;
pub mod vault_yml;
pub mod walker;

mod orchestrate;
mod types;

pub use branch::{resolve_branch, DEFAULT_UPDATE_CHANNEL, VALID_UPDATE_CHANNELS};
pub use download::build_tar_spawn_overrides;
pub use orchestrate::{default_installed_plugins_path, read_plugin_version, run_vault_sync};
pub use pin::normalize_path;
pub use types::{VaultSyncOptions, VaultSyncResult};
