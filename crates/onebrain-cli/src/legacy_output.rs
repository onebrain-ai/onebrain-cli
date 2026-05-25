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
/// **v3.1 behaviour:** default (`OutputMode::Text { .. }`) renderers live
/// next to each command's body — text formatting is shape-specific (a session
/// metadata line vs. a "no vault found" message vs. an orphan-count line) so
/// the generic serializer here does NOT attempt a generic text fallback.
/// Callers must dispatch on `OutputMode::Text { .. }` before invoking this
/// function. For the structured branches (`Json` / `Yaml` / `Table` / `Tsv`)
/// this function is the single emit point; it keeps the JSON shape rules
/// (compact vs. pretty) in one place and falls back to compact JSON if YAML
/// emission ever fails (defensive — `serde_yaml` doesn't fail for our static
/// shapes today).
///
/// `--pretty` flag is honoured for explicit JSON mode (`OutputMode::Json
/// { pretty: true }`). YAML is already multi-line / "pretty" by construction.
///
/// **Hook-protocol contract:** machine consumers (Claude Code SessionStart /
/// Stop hooks) must pass `--json` explicitly in v3.1+. The hook rewriter
/// adds the flag during `onebrain plugin update`; fresh installs scaffold
/// it directly. See `v31/hook_rewriter.rs` + `register_hooks/hooks.rs`.
pub fn serialize_for_mode<T: Serialize>(value: &T, mode: &OutputMode) -> String {
    match mode {
        OutputMode::Yaml => {
            serde_yaml::to_string(value).unwrap_or_else(|_| serialize_json(value, false))
        }
        OutputMode::Json { pretty } => serialize_json(value, *pretty),
        // Table / Tsv have no columnar slot for hook-protocol blocks. Fall
        // back to compact JSON so the consumer still gets parseable output;
        // commands that care about text rendering branch on
        // `OutputMode::Text { .. }` before calling this function.
        OutputMode::Table | OutputMode::Tsv => serialize_json(value, false),
        // Text mode shouldn't reach here — callers handle text rendering
        // themselves. Defensive: emit compact JSON if it does (no panic).
        OutputMode::Text { pretty, .. } => serialize_json(value, *pretty),
    }
}

fn serialize_json<T: Serialize>(value: &T, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| String::new())
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| String::new())
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
