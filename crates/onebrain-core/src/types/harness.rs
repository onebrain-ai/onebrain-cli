//! Harness identifier · which AI runtime is in use.

use serde::Serialize;

/// AI runtime harness identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Gemini,
    Direct,
}

impl Harness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Gemini => "gemini",
            Harness::Direct => "direct",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_lowercase() {
        assert_eq!(Harness::Claude.as_str(), "claude");
        assert_eq!(Harness::Gemini.as_str(), "gemini");
        assert_eq!(Harness::Direct.as_str(), "direct");
    }
}
