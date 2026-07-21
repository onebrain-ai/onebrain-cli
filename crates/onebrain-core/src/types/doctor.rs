use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorResult {
    pub check: String,
    pub status: DoctorStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// Stable, machine-readable identity of *which* finding a check produced,
    /// independent of the prose in [`Self::message`].
    ///
    /// A check that can return several distinct findings under one `check`
    /// name must stamp each one with its own code, and every consumer that
    /// needs to know *which* finding fired — fix routing, tests — keys on this
    /// instead of matching message text. Prose is a UX surface: it gets
    /// reworded, and two findings can legitimately share a phrase or a hint,
    /// at which point string matching silently pins the wrong arm.
    ///
    /// `&'static str` on purpose: a code is a compile-time constant owned by
    /// the check that emits it, never a runtime-formatted string.
    ///
    /// Populated so far only by the `lex-index` check (v3.4.17). Every other
    /// check leaves it `None` and stays on its current message-based
    /// classification in `planned_action` — converting the whole doctor check
    /// registry is deliberately out of scope for that fix.
    ///
    /// `#[serde(skip)]`: an internal routing identity, not part of the
    /// `doctor --json` contract. Skipping it keeps the rendered output — text
    /// and JSON alike — byte-identical to before the field existed.
    #[serde(skip)]
    pub code: Option<&'static str>,
}

impl DoctorResult {
    pub fn ok(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            status: DoctorStatus::Ok,
            message: message.into(),
            hint: None,
            details: vec![],
            code: None,
        }
    }
    pub fn warn(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            status: DoctorStatus::Warn,
            message: message.into(),
            hint: None,
            details: vec![],
            code: None,
        }
    }
    pub fn error(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            status: DoctorStatus::Error,
            message: message.into(),
            hint: None,
            details: vec![],
            code: None,
        }
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }
    /// Stamp this result with its stable finding identity. See
    /// [`DoctorResult::code`] for why consumers must key on this rather than
    /// on the message text.
    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ok_constructor_no_hint_no_details() {
        let r = DoctorResult::ok("foo", "bar");
        assert_eq!(r.status, DoctorStatus::Ok);
        assert!(r.hint.is_none());
        assert!(r.details.is_empty());
    }
    #[test]
    fn warn_with_hint_round_trip() {
        let r = DoctorResult::warn("foo", "msg").with_hint("hint");
        assert_eq!(r.hint.as_deref(), Some("hint"));
    }
    #[test]
    fn status_serializes_lowercase() {
        let r = DoctorResult::error("c", "m");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"status\":\"error\""));
    }
}
