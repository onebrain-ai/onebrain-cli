//! Markdown frontmatter parsing helper · CRLF-aware · YAML-mapping only.

use serde_yaml::Value;

/// Parse the YAML frontmatter block from a markdown document.
///
/// Recognises `---\n...\n---\n` (or CRLF) delimiters. Returns `Some(Value::Mapping)`
/// only if the frontmatter parses successfully AND is a YAML mapping. Returns `None`
/// for any other shape, missing delimiters, or parse errors — callers treat None as
/// "no frontmatter / unparseable" rather than as an error.
// used by upcoming has_manual_session_log in Task 4
#[allow(dead_code)]
pub fn parse_frontmatter(raw_text: &str) -> Option<Value> {
    let text = raw_text.replace("\r\n", "\n");
    let after_open = text.strip_prefix("---\n")?;
    let close_idx = after_open.find("\n---")?;
    let block = after_open[..close_idx].trim();
    let parsed: Value = serde_yaml::from_str(block).ok()?;
    if parsed.is_mapping() {
        Some(parsed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_newline_frontmatter() {
        let v = parse_frontmatter("---\nauto-saved: true\n---\nbody\n").unwrap();
        assert_eq!(v.get("auto-saved").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn parses_crlf_frontmatter() {
        let v = parse_frontmatter("---\r\nauto-saved: false\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(v.get("auto-saved").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn returns_none_when_no_leading_dashes() {
        assert!(parse_frontmatter("body only, no frontmatter").is_none());
    }

    #[test]
    fn returns_none_when_no_closing_delimiter() {
        assert!(parse_frontmatter("---\nincomplete: yes").is_none());
    }

    #[test]
    fn returns_none_when_frontmatter_is_not_a_mapping() {
        // A YAML sequence at the root is valid YAML but not a mapping.
        assert!(parse_frontmatter("---\n- one\n- two\n---\nbody").is_none());
    }

    #[test]
    fn returns_none_on_invalid_yaml() {
        assert!(parse_frontmatter("---\nnot: : valid\n---\nbody").is_none());
    }
}
