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
}

impl DoctorResult {
    pub fn ok(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            status: DoctorStatus::Ok,
            message: message.into(),
            hint: None,
            details: vec![],
        }
    }
    pub fn warn(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            status: DoctorStatus::Warn,
            message: message.into(),
            hint: None,
            details: vec![],
        }
    }
    pub fn error(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            status: DoctorStatus::Error,
            message: message.into(),
            hint: None,
            details: vec![],
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
