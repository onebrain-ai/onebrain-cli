use serde::Serialize;

/// Session-unique identifier · alphanumeric only · same value within a calendar day.
///
/// See `INSTRUCTIONS.md` Auto Checkpoint section for the resolution priority chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionToken(String);

impl SessionToken {
    /// Build a token from an arbitrary string by stripping all non-alphanumeric chars.
    /// Returns `None` if the result would be empty.
    pub fn sanitize(raw: &str) -> Option<Self> {
        let cleaned: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if cleaned.is_empty() {
            None
        } else {
            Some(SessionToken(cleaned))
        }
    }

    /// Build a token from an already-sanitized literal · panics if invalid.
    /// Use only for tests and the random-fallback path.
    pub fn from_clean(s: String) -> Self {
        assert!(
            s.chars().all(|c| c.is_ascii_alphanumeric()),
            "SessionToken::from_clean called with non-alphanumeric input: {s:?}"
        );
        assert!(!s.is_empty(), "SessionToken cannot be empty");
        SessionToken(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_special_chars() {
        let t = SessionToken::sanitize("%pane-12").unwrap();
        assert_eq!(t.as_str(), "pane12");
    }

    #[test]
    fn sanitize_returns_none_for_empty_after_strip() {
        assert!(SessionToken::sanitize("---").is_none());
    }

    #[test]
    fn sanitize_preserves_pure_alphanumeric() {
        let t = SessionToken::sanitize("90355").unwrap();
        assert_eq!(t.as_str(), "90355");
    }

    #[test]
    fn from_clean_accepts_alphanumeric() {
        let t = SessionToken::from_clean("abc123".to_string());
        assert_eq!(t.as_str(), "abc123");
    }

    #[test]
    #[should_panic(expected = "non-alphanumeric")]
    fn from_clean_panics_on_dash() {
        let _ = SessionToken::from_clean("ab-cd".to_string());
    }
}
