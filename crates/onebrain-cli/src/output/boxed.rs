//! Shared title-in-border box renderer — `┌ Title ─…─┐` … `└…┘`.
//!
//! Extracted from `search model list` (#195) so `doctor`'s bottom Summary box
//! and the model/reranker tables render with one identical box style and can't
//! drift. Width fits the widest content row, capped at [`MAX_BOX_WIDTH`]; wider
//! rows get their tail truncated with `…`. Per-row SGR wraps the *padded*
//! content INSIDE the borders — width math is always done on the uncolored
//! text, so the right border stays flush in colour and no-colour modes alike.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Bold-green SGR prefix — the active model's row (matches the interactive TUI).
pub(crate) const ACTIVE_ROW_ANSI: &str = "\x1b[1;32m";
pub(crate) const ANSI_RESET: &str = "\x1b[0m";

/// Hard cap on a rendered box's TOTAL display width (borders included). A long
/// row (a ~95-column reranker NOTE, a long doctor message) would otherwise push
/// the border past a normal terminal and wrap — visually "breaking" the box.
/// Rows wider than the cap get their tail truncated with an ellipsis.
pub(crate) const MAX_BOX_WIDTH: usize = 100;

/// Pad `s` with trailing spaces to `w` DISPLAY columns (not chars/bytes) —
/// `unicode-width` keeps the box's right border flush even with the ✓ ● — cells
/// and `·` note separators.
pub(crate) fn pad_display(s: &str, w: usize) -> String {
    let vis = s.width();
    format!("{s}{}", " ".repeat(w.saturating_sub(vis)))
}

/// Truncate `s` to at most `max` DISPLAY columns (unicode-width-aware, like
/// [`pad_display`]), appending `…` — which itself occupies one of the `max`
/// columns — when anything was cut. Strings already within the cap pass
/// through untouched.
pub(crate) fn truncate_display(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1); // reserve one column for the `…`
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Assemble a titled single-line box around pre-formatted content rows.
///
/// `content` is `(uncoloured row text, optional SGR prefix)`. Rows are first
/// capped to [`MAX_BOX_WIDTH`] via [`truncate_display`] (the 4 non-content
/// columns are the two `│` borders + their 1-space pads), then padded to one
/// shared inner width. When `color` is set and a row carries `Some(sgr)`, its
/// padded content is wrapped in `sgr … RESET` INSIDE the borders — padding is
/// always computed on the uncoloured text, so the right border stays flush in
/// both modes.
pub(crate) fn render_boxed_table(
    title: &str,
    content: &[(String, Option<&str>)],
    color: bool,
) -> Vec<String> {
    let capped: Vec<(String, Option<&str>)> = content
        .iter()
        .map(|(l, sgr)| (truncate_display(l, MAX_BOX_WIDTH - 4), *sgr))
        .collect();
    let inner = capped
        .iter()
        .map(|(l, _)| l.width())
        .max()
        .unwrap_or(0)
        .max(title.width());
    // Padded content width between the two `│` (1-space pad each side).
    let boxed = inner + 2;
    let mut lines = Vec::with_capacity(capped.len() + 2);
    lines.push(format!(
        "┌{title}{}┐",
        "─".repeat(boxed.saturating_sub(title.width()))
    ));
    for (l, sgr) in &capped {
        let padded = pad_display(l, inner);
        match (color, sgr) {
            (true, Some(sgr)) => lines.push(format!("│ {sgr}{padded}{ANSI_RESET} │")),
            _ => lines.push(format!("│ {padded} │")),
        }
    }
    lines.push(format!("└{}┘", "─".repeat(boxed)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn truncate_display_is_width_aware_and_appends_ellipsis() {
        // Short strings pass through untouched.
        assert_eq!(truncate_display("abc", 10), "abc");
        // Exact fit passes through untouched (no gratuitous ellipsis).
        assert_eq!(truncate_display("abcde", 5), "abcde");
        // Over-long input is cut to the cap WITH the ellipsis counted in.
        let cut = truncate_display("abcdefghij", 5);
        assert_eq!(cut, "abcd…");
        assert_eq!(cut.width(), 5);
        // Width-aware: wide CJK chars count 2 columns, so fewer fit.
        let cjk = truncate_display("ไทย中文字字字", 6);
        assert!(cjk.ends_with('…'), "{cjk}");
        assert!(cjk.width() <= 6, "width {} of {cjk}", cjk.width());
    }

    #[test]
    fn render_boxed_table_borders_and_width_are_flush() {
        let rows = vec![
            ("short".to_string(), None),
            ("a much longer content row".to_string(), None),
        ];
        let lines = render_boxed_table(" Title ", &rows, false);
        let w = lines[0].width();
        for l in &lines {
            assert_eq!(l.width(), w, "line width mismatch: {l:?}");
        }
        assert!(lines[0].starts_with("┌ Title "));
        assert!(lines.last().unwrap().starts_with('└'));
    }

    #[test]
    fn render_boxed_table_color_wraps_content_only_and_keeps_width() {
        let rows = vec![("row".to_string(), Some(ACTIVE_ROW_ANSI))];
        let colored = render_boxed_table(" T ", &rows, true);
        // The content line carries SGR; the borders do not.
        assert!(colored[1].contains(ACTIVE_ROW_ANSI));
        assert!(colored[1].ends_with(&format!("{ANSI_RESET} │")));
        // Width math is done on uncoloured text: stripping SGR leaves each line
        // the same visible width as the no-color render.
        let mono = render_boxed_table(" T ", &[("row".to_string(), None)], false);
        let stripped: Vec<String> = colored
            .iter()
            .map(|l| l.replace(ACTIVE_ROW_ANSI, "").replace(ANSI_RESET, ""))
            .collect();
        assert_eq!(stripped, mono);
    }
}
