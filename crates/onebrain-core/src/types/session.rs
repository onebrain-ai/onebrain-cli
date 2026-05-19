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

    /// Build a token from an arbitrary string by stripping all non-alphanumeric chars,
    /// then truncating to at most `max_len` chars.
    ///
    /// Mirrors Bun v2.3.3's `.replace(/[^a-zA-Z0-9]/g, '').slice(0, 8)` — strip first,
    /// then cap. Returns `None` if the result would be empty.
    pub fn sanitize_truncated(raw: &str, max_len: usize) -> Option<Self> {
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(max_len)
            .collect();
        if cleaned.is_empty() {
            None
        } else {
            Some(SessionToken(cleaned))
        }
    }

    /// Build a token from an already-sanitized literal · panics if invalid.
    ///
    /// Internal API — intended only for tests and the random-fallback path in
    /// `onebrain-cache`. Prefer [`Self::sanitize`] or [`Self::sanitize_truncated`]
    /// at API boundaries.
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

impl std::fmt::Display for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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

    #[test]
    fn sanitize_truncated_strips_then_caps_to_max_len() {
        // Bun: `"abc-12345678".replace(/[^a-zA-Z0-9]/g, '').slice(0, 8)` = "abc12345"
        let t = SessionToken::sanitize_truncated("abc-12345678", 8).unwrap();
        assert_eq!(t.as_str(), "abc12345");
    }

    #[test]
    fn sanitize_truncated_returns_none_if_empty_after_strip() {
        assert!(SessionToken::sanitize_truncated("---", 8).is_none());
    }

    #[test]
    fn sanitize_truncated_preserves_when_shorter_than_cap() {
        let t = SessionToken::sanitize_truncated("ab", 8).unwrap();
        assert_eq!(t.as_str(), "ab");
    }

    #[test]
    fn display_renders_inner_str() {
        let t = SessionToken::sanitize("abc123").unwrap();
        assert_eq!(format!("{t}"), "abc123");
    }
}
