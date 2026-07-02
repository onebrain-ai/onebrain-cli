//! Shared text-layout primitives for the grouped status convention.
//!
//! The convention (first shipped in `search status` / `search reindex`, now the
//! house style for every status-like text output):
//!
//! - Section header: `{emoji}  {Title}` — two spaces after the emoji, a
//!   Capitalized title.
//! - Body rows: `    {label:<LABEL_W}{value}` — four leading spaces + a
//!   fixed-width label column so every value lines up regardless of how wide a
//!   terminal font draws an emoji. No emoji inside a row (semantic glyphs like
//!   `✅` / `⚠️` inside a *value* are fine).
//!
//! Alignment never depends on emoji width because the emoji lives only on the
//! section header, and body rows are indented with plain spaces.

/// Width of the label column in a body row. `Last indexed` (12 chars) is the
/// longest label used today; 14 leaves two trailing spaces after it. Kept
/// crate-wide so `search status`, `search reindex`, and any other grouped
/// output share one alignment.
pub(crate) const LABEL_W: usize = 14;

/// A section header: `{emoji}  {title}` (two spaces after the emoji).
pub(crate) fn section(emoji: &str, title: &str) -> String {
    format!("{emoji}  {title}")
}

/// One indented `label → value` body row: four leading spaces + a
/// [`LABEL_W`]-wide label column, then the value. No emoji in the label
/// (semantic glyphs may appear inside `value`).
pub(crate) fn item(label: &str, value: &str) -> String {
    format!("    {label:<LABEL_W$}{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_puts_two_spaces_after_emoji() {
        assert_eq!(section("📊", "Index"), "📊  Index");
    }

    #[test]
    fn item_pads_label_to_fixed_width() {
        // Short label is padded out to LABEL_W so the value starts at a
        // stable column (4 leading spaces + 14-char label = column 18).
        let row = item("Docs", "7");
        assert_eq!(row, "    Docs          7");
        assert_eq!(row.find('7'), Some(4 + LABEL_W));
    }

    #[test]
    fn item_with_max_width_label_keeps_two_trailing_spaces() {
        // `Last indexed` (12 chars) is the longest real label; LABEL_W=14
        // guarantees ≥2 spaces before the value.
        let row = item("Last indexed", "never");
        assert_eq!(row, "    Last indexed  never");
    }
}
