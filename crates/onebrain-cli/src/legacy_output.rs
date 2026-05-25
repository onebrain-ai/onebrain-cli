use serde::Serialize;

/// Successful session-init output · matches Bun v2.3.3 JSON shape byte-for-byte.
#[derive(Debug, Serialize)]
pub struct SessionInitOutput {
    pub datetime: String,
    pub session_token: String,
    pub qmd_unembedded: usize,
}

/// Block output emitted when session-init can't proceed.
///
/// Two reasons (R1 C2):
/// - `onebrain-init-required` — no `vault.yml` found anywhere up from cwd.
/// - `onebrain-vault-malformed` — `vault.yml` exists but failed to parse.
///   Carries an `error_detail` field so the SessionStart hook consumer can
///   surface the parse-error message to the user.
#[derive(Debug, Serialize)]
pub struct SessionInitBlock {
    pub decision: &'static str,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
}

impl SessionInitBlock {
    pub fn init_required() -> Self {
        Self {
            decision: "block",
            reason: "onebrain-init-required",
            error_detail: None,
        }
    }

    /// `vault.yml` exists but is unreadable / malformed. Distinct reason
    /// so the SessionStart consumer can route the user to "fix your
    /// vault.yml" instead of "run /onboarding".
    pub fn vault_malformed(detail: impl Into<String>) -> Self {
        Self {
            decision: "block",
            reason: "onebrain-vault-malformed",
            error_detail: Some(detail.into()),
        }
    }
}
