use crate::output::OutputMode;
use serde::Serialize;

/// Successful session-init output · matches Bun v2.3.3 JSON shape byte-for-byte.
#[derive(Debug, Serialize)]
pub struct SessionInitOutput {
    pub datetime: String,
    pub session_token: String,
    pub qmd_unembedded: usize,
}

/// Serialize a hook-protocol block / output to the wire format for the given
/// output mode.
///
/// **Default = JSON** to preserve back-compat with Claude Code's SessionStart
/// hook parser (which expects JSON unconditionally). Only an explicit
/// `--yaml` / `--output yaml` flips to YAML; every other mode (text · table
/// · tsv · default json) keeps JSON because the hook-protocol block has no
/// sensible columnar form and downstream tooling depends on the JSON shape.
pub fn serialize_for_mode<T: Serialize>(value: &T, mode: &OutputMode) -> String {
    match mode {
        OutputMode::Yaml => {
            // `serde_yaml` always succeeds for our static block shapes (no
            // exotic types), but if it ever did fail the fallback to JSON
            // keeps the hook contract honest rather than panicking.
            serde_yaml::to_string(value)
                .unwrap_or_else(|_| serde_json::to_string(value).unwrap_or_else(|_| String::new()))
        }
        _ => serde_json::to_string(value).unwrap_or_else(|_| String::new()),
    }
}

/// Block output emitted when session-init can't proceed.
///
/// Two reasons (R1 C2):
/// - `onebrain-vault-not-found` — no `vault.yml` / `onebrain.yml` found
///   anywhere up from cwd. Renamed in v3.1 from the v3.0 spelling
///   `onebrain-init-required` (clearer · brand-prefixed).
/// - `onebrain-vault-malformed` — vault config exists but failed to parse.
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
            reason: "onebrain-vault-not-found",
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
