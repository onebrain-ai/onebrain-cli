use serde::Serialize;

/// Successful session-init output · matches Bun v2.3.3 JSON shape byte-for-byte.
#[derive(Debug, Serialize)]
pub struct SessionInitOutput {
    pub datetime: String,
    pub session_token: String,
    pub qmd_unembedded: usize,
}

/// Block output emitted when no vault.yml is found anywhere up from cwd.
#[derive(Debug, Serialize)]
pub struct SessionInitBlock {
    pub decision: &'static str,
    pub reason: &'static str,
}

impl SessionInitBlock {
    pub fn init_required() -> Self {
        Self {
            decision: "block",
            reason: "onebrain-init-required",
        }
    }
}
