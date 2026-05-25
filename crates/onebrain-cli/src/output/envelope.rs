//! Canonical JSON output envelope (skill-alignment §4.3).
//!
//! Every v3.1 command emits this exact shape under `--json` / `--yaml`. The
//! `version` field is `"1"` for the entire v3.x line; bumped only when the
//! envelope schema itself breaks (would require a v4).

use serde::Serialize;
use std::path::PathBuf;

/// Schema version of the envelope itself. Bumped only when the envelope
/// breaks (e.g. field renames, type changes). Distinct from CLI version.
pub const ENVELOPE_VERSION: &str = "1";

/// Top-level wrapper for every v3.1 JSON output. Generic over the
/// command-specific `data` payload so each command can declare its own
/// strongly-typed result struct.
#[derive(Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    /// Envelope schema version. Always `"1"` in v3.1.
    pub version: &'static str,

    /// Dotted command name — `"task.list"`, `"memory.add"`,
    /// `"vault.current"`. Single source of truth for downstream parsers that
    /// switch on command identity.
    pub command: String,

    /// `true` when the command achieved its intended effect (a successful
    /// `task list` returns `ok: true` even if zero tasks matched).
    pub ok: bool,

    /// Active vault metadata when the command resolved a vault. `None` for
    /// vault-free commands (`init`, `update`, `harness detect`, `date *`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault: Option<VaultInfo>,

    /// Command-specific payload. `None` only on hard errors (when `error`
    /// carries the detail).
    pub data: Option<T>,

    /// Soft warnings — partial results, deprecation notices, missing
    /// optional inputs. Empty `[]` (not `null`) when nothing to warn about,
    /// per stable schema discipline.
    pub warnings: Vec<Warning>,

    /// Populated when `ok == false`. Stable `code` field matches
    /// `CoreError::error_code()` (e.g. `E_VAULT_NOT_FOUND`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

impl<T: Serialize> Envelope<T> {
    /// Build an `ok=true` envelope. Use for the happy path of any command.
    pub fn ok(command: impl Into<String>, vault: Option<VaultInfo>, data: T) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            command: command.into(),
            ok: true,
            vault,
            data: Some(data),
            warnings: Vec::new(),
            error: None,
        }
    }

    /// Build an envelope reporting a hard error. `data` is `None`; the
    /// caller's error code + message goes in `error`.
    ///
    /// Part of the v3.1 stable envelope API. v3.1 commands return errors
    /// via `anyhow::Error → exit::exit_code_for` so they don't construct
    /// envelopes for the failure path; v3.2+ commands that opt into the
    /// error-envelope shape will call this directly.
    #[allow(dead_code)]
    pub fn err(command: impl Into<String>, vault: Option<VaultInfo>, error: ErrorInfo) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            command: command.into(),
            ok: false,
            vault,
            data: None,
            warnings: Vec::new(),
            error: Some(error),
        }
    }

    /// Append a warning. Returns `Self` to allow chaining inside command
    /// builders. v3.1's implemented commands don't emit warnings yet;
    /// v3.2+ commands (e.g. `vault sync` with partial-success reports)
    /// will call this.
    #[allow(dead_code)]
    pub fn with_warning(mut self, code: impl Into<String>, message: impl Into<String>) -> Self {
        self.warnings.push(Warning {
            code: code.into(),
            message: message.into(),
        });
        self
    }
}

/// Per-vault metadata included in every envelope from a vault-required
/// command. The `name` is the vault directory basename; `path` is absolute.
#[derive(Debug, Serialize, Clone)]
pub struct VaultInfo {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Serialize, Clone)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ErrorInfo {
    /// Stable `E_*` code from `CoreError::error_code()`.
    pub code: String,
    pub message: String,
}

impl ErrorInfo {
    /// Convenience constructor. Used by `Envelope::err` and command bodies
    /// that build error envelopes manually (v3.2+).
    #[allow(dead_code)]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Serialize)]
    struct Payload {
        count: u32,
    }

    #[test]
    fn ok_envelope_serializes_with_all_fields() {
        let env = Envelope::ok(
            "task.list",
            Some(VaultInfo {
                name: "ob-1".into(),
                path: "/tmp/ob-1".into(),
            }),
            Payload { count: 3 },
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["version"], "1");
        assert_eq!(v["command"], "task.list");
        assert_eq!(v["ok"], true);
        assert_eq!(v["vault"]["name"], "ob-1");
        assert_eq!(v["data"]["count"], 3);
        assert_eq!(v["warnings"], json!([]));
        // error skipped when None.
        assert!(v.get("error").is_none());
    }

    #[test]
    fn err_envelope_omits_data_and_includes_error() {
        let env: Envelope<Payload> = Envelope::err(
            "task.list",
            None,
            ErrorInfo::new("E_VAULT_NOT_FOUND", "no vault"),
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "E_VAULT_NOT_FOUND");
        assert_eq!(v["error"]["message"], "no vault");
        // data is null (not skipped) so consumers can rely on its presence.
        assert!(v["data"].is_null());
        // vault skipped when None.
        assert!(v.get("vault").is_none());
    }

    #[test]
    fn with_warning_appends_in_order() {
        let env = Envelope::ok("x.y", None, Payload { count: 1 })
            .with_warning("W1", "first")
            .with_warning("W2", "second");
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["warnings"][0]["code"], "W1");
        assert_eq!(v["warnings"][1]["code"], "W2");
    }

    #[test]
    fn envelope_version_is_stable_one() {
        assert_eq!(ENVELOPE_VERSION, "1");
    }
}
