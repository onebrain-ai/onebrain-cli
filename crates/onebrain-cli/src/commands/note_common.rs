//! Shared helpers for the `note` scan verbs (`search`/`backlinks`/`orphans`/
//! `move`).
//!
//! The fs layer records notes it could not read as UTF-8 text in a `skipped`
//! field rather than silently dropping them (which would yield false orphans,
//! undercounted backlinks, or dangling links after a move). The handlers turn a
//! non-empty `skipped` list into a soft envelope warning so both machine and
//! human consumers learn the results may be incomplete.

use std::path::Path;

/// Stable warning code emitted when one or more notes were skipped.
pub const W_NOTES_SKIPPED: &str = "W_NOTES_SKIPPED";

/// Build the warning message for a non-empty `skipped` list, or `None` when
/// nothing was skipped. Paths are vault-relative (as recorded by the fs layer).
pub fn skipped_warning(skipped: &[impl AsRef<Path>]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let paths = skipped
        .iter()
        .map(|p| p.as_ref().display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{} note(s) unreadable and skipped (results may be incomplete): {paths}",
        skipped.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn none_when_empty() {
        let empty: Vec<PathBuf> = Vec::new();
        assert!(skipped_warning(&empty).is_none());
    }

    #[test]
    fn message_counts_and_lists_paths() {
        let skipped = vec![PathBuf::from("a.md"), PathBuf::from("sub/b.md")];
        let msg = skipped_warning(&skipped).unwrap();
        assert!(msg.starts_with("2 note(s) unreadable and skipped"));
        assert!(msg.contains("a.md"));
        assert!(msg.contains("sub/b.md"));
    }
}
