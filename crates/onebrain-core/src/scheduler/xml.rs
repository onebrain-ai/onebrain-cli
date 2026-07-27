//! XML escaping, shared by every renderer that emits XML.
//!
//! Lives here rather than in `launchd.rs` because the Windows backend needs the
//! same routine, and a scheduler-neutral layer should not import from a
//! platform-specific one.

/// Escape XML-sensitive characters for attribute and text-content positions.
///
/// Order matters: `&` is replaced first, so the literal ampersand introduced by
/// `&amp;` is not escaped a second time.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn escapes_ampersand_first() {
        assert_eq!(escape("a & b"), "a &amp; b");
    }

    #[test]
    fn escapes_all_four_sensitive_chars() {
        assert_eq!(
            escape("<a href=\"x\">&</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;"
        );
    }

    #[test]
    fn an_already_escaped_entity_is_not_double_escaped_into_nonsense() {
        // The ampersand-first ordering is the whole reason this is one function
        // and not four chained replaces at each call site.
        assert_eq!(escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        assert_eq!(
            escape("/Users/u/Library/Logs/onebrain"),
            "/Users/u/Library/Logs/onebrain"
        );
    }
}
