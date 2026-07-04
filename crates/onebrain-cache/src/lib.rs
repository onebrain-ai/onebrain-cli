//! Session token resolution · launchd plist generation · checkpoint state.

pub mod checkpoint;
pub mod error;
pub mod session_token;
pub mod state;

pub use checkpoint::{handle_reset, handle_stop};
pub use error::{CacheError, Result};
pub use session_token::{
    clean_stale_state_file, find_claude_ancestor_pid, resolve_session_token, ProcInfo, ProcLookup,
    ResolveInputs,
};
pub use state::{read_state, write_state, CheckpointState};
