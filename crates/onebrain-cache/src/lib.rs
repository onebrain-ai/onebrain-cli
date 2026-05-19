//! Session token resolution · launchd plist generation · qmd index query / reindex · checkpoint state.

pub mod checkpoint;
pub mod error;
pub mod qmd;
pub mod qmd_reindex;
pub mod session_token;
pub mod state;

pub use error::{CacheError, Result};
pub use qmd::query_unembedded_count;
pub use qmd_reindex::{build_qmd_spawn_args, qmd_reindex, SpawnOs};
pub use session_token::{
    clean_stale_state_file, find_claude_ancestor_pid, resolve_session_token, ProcInfo, ProcLookup,
    ResolveInputs,
};
pub use state::{read_state, write_state, CheckpointState};
