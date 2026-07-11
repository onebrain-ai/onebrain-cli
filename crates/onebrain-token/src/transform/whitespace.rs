//! Lossless whitespace compaction: trims trailing line whitespace, collapses
//! runs of blank lines to at most one, and collapses interior runs of
//! *spaces* to a single space — while preserving leading indentation
//! (markdown lists, nested content), tabs (structurally significant), and
//! everything inside fenced code blocks (``` / ~~~) untouched. Purely
//! cosmetic; no prose character and no significant whitespace is removed.

use super::{is_doc_surface, Payload, Transform, TransformCtx};
use crate::gain::Surface;
use crate::level::OptLevel;

pub struct Whitespace;

impl Transform for Whitespace {
    fn name(&self) -> &'static str {
        "whitespace"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Conservative
    }

    fn applies_to(&self, surface: Surface) -> bool {
        is_doc_surface(surface)
    }

    fn lossy(&self) -> bool {
        false
    }

    fn apply(&self, input: &Payload, _ctx: &TransformCtx) -> Payload {
        Payload {
            text: compact_whitespace(&input.text),
            signals: input.signals.clone(),
        }
    }
}

fn compact_whitespace(text: &str) -> String {
    // Preserve the document's line-ending style: `.lines()` strips `\r`, so a
    // CRLF doc would silently normalize to LF on rejoin. A `get` on a CRLF
    // vault note must keep CRLF (CRLF-safety, matching frontmatter).
    let eol = detect_eol(text);

    let mut out_lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut blank_run = 0u32;

    for line in text.lines() {
        // A fenced-code delimiter (``` or ~~~) toggles fence state. The
        // delimiter line itself is preserved verbatim (it may carry an info
        // string like ```rust).
        if is_fence_marker(line) {
            in_fence = !in_fence;
            blank_run = 0;
            out_lines.push(line.to_string());
            continue;
        }

        // Inside a fence: preserve every line verbatim — interior alignment
        // (ASCII tables, TSV, terminal output) and blank lines are all
        // significant (§4: never silently destroy structure).
        if in_fence {
            out_lines.push(line.to_string());
            continue;
        }

        let collapsed = collapse_line(line);
        if collapsed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out_lines.push(collapsed);
            }
        } else {
            blank_run = 0;
            out_lines.push(collapsed);
        }
    }
    out_lines.join(eol)
}

/// The dominant line ending of `text`: `\r\n` if any CRLF is present, else
/// `\n`. Used so line-oriented transforms rejoin without silently converting
/// a CRLF document to LF.
pub(super) fn detect_eol(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// True for a fenced-code-block delimiter line (``` or ~~~, after any leading
/// indentation).
fn is_fence_marker(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Trims trailing whitespace and collapses interior runs of *spaces* to a
/// single space, while leaving leading indentation exactly as-is. Tabs are
/// left untouched everywhere — they are structurally significant (TSV,
/// alignment) and must never be collapsed or converted to a space.
fn collapse_line(line: &str) -> String {
    let trimmed_end = line.trim_end();
    let indent_len = trimmed_end.len() - trimmed_end.trim_start().len();
    let (indent, rest) = trimmed_end.split_at(indent_len);

    let mut collapsed = String::with_capacity(rest.len());
    let mut last_was_space = false;
    for ch in rest.chars() {
        if ch == ' ' {
            if !last_was_space {
                collapsed.push(' ');
            }
            last_was_space = true;
        } else {
            // Tabs and every other char pass through untouched.
            collapsed.push(ch);
            last_was_space = false;
        }
    }

    format!("{indent}{collapsed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/whitespace/input.md");

    #[test]
    fn strips_trailing_whitespace_on_every_line() {
        let out = Whitespace.apply(&Payload::new(INPUT), &TransformCtx::default());
        for line in out.text.lines() {
            assert_eq!(
                line,
                line.trim_end(),
                "line has trailing whitespace: {line:?}"
            );
        }
    }

    #[test]
    fn collapses_runs_of_blank_lines_to_at_most_one() {
        let out = Whitespace.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert!(
            !out.text.contains("\n\n\n"),
            "output still has 2+ consecutive blank lines:\n{}",
            out.text
        );
    }

    #[test]
    fn collapses_interior_double_spaces_but_preserves_leading_indent() {
        let out = Whitespace.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert!(
            out.text.contains("  - indented list item stays indented"),
            "leading indentation must survive:\n{}",
            out.text
        );
        assert!(
            out.text.contains("second item with internal double spaces"),
            "interior double spaces should collapse to one:\n{}",
            out.text
        );
    }

    #[test]
    fn is_lossless_and_emits_no_signal() {
        assert_eq!(Whitespace.min_level(), OptLevel::Conservative);
        assert!(!Whitespace.lossy());
        let out = Whitespace.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert!(out.signals.is_empty());
    }

    #[test]
    fn is_idempotent() {
        let once = Whitespace.apply(&Payload::new(INPUT), &TransformCtx::default());
        let twice = Whitespace.apply(&once, &TransformCtx::default());
        assert_eq!(once, twice);
    }

    const FENCED: &str = include_str!("../../tests/fixtures/whitespace/fenced.md");

    #[test]
    fn preserves_aligned_columns_and_tabs_inside_a_code_fence() {
        let out = Whitespace
            .apply(&Payload::new(FENCED), &TransformCtx::default())
            .text;

        // The entire fenced block must survive byte-for-byte: aligned
        // columns (multi-space) and the tab-separated line untouched.
        for verbatim in [
            "col1      col2      col3",
            "alpha     100       yes",
            "beta      2000      no",
            "x\ty\tz",
        ] {
            assert!(
                out.contains(verbatim),
                "fenced line was corrupted; expected {verbatim:?} in:\n{out}"
            );
        }
    }

    #[test]
    fn still_collapses_double_spaces_outside_the_fence() {
        let out = Whitespace
            .apply(&Payload::new(FENCED), &TransformCtx::default())
            .text;
        assert!(
            out.contains("Some prose with double spaces here."),
            "prose OUTSIDE the fence must still collapse:\n{out}"
        );
        assert!(
            out.contains("More prose after."),
            "prose after the fence must still collapse:\n{out}"
        );
    }

    #[test]
    fn is_idempotent_with_a_fence() {
        let once = Whitespace.apply(&Payload::new(FENCED), &TransformCtx::default());
        let twice = Whitespace.apply(&once, &TransformCtx::default());
        assert_eq!(once, twice);
    }

    #[test]
    fn leaves_a_bare_tab_line_untouched_even_outside_a_fence() {
        // Tabs are structurally significant everywhere; a TSV row outside a
        // fence keeps its tabs (only space runs collapse).
        let input = Payload::new("a\tb\tc\nx  y  z");
        let out = Whitespace.apply(&input, &TransformCtx::default()).text;
        assert!(out.contains("a\tb\tc"), "tabs must survive: {out:?}");
        assert!(out.contains("x y z"), "space runs still collapse: {out:?}");
    }

    #[test]
    fn preserves_crlf_line_endings_on_a_crlf_doc() {
        // `.lines()` strips `\r`; without eol detection the rejoin would
        // silently convert a CRLF vault note to LF on every `get`. Built from
        // an explicit `\r\n` literal (platform-independent).
        let input = Payload::new("a  b\r\nc  d\r\n");
        let out = Whitespace.apply(&input, &TransformCtx::default()).text;
        assert!(
            out.contains("\r\n"),
            "CRLF line endings must be preserved, got {out:?}"
        );
        assert!(out.contains("a b"), "space runs still collapse: {out:?}");
    }

    #[test]
    fn keeps_lf_on_an_lf_doc() {
        let input = Payload::new("a  b\nc  d\n");
        let out = Whitespace.apply(&input, &TransformCtx::default()).text;
        assert!(
            !out.contains('\r'),
            "must NOT introduce CR into an LF doc: {out:?}"
        );
    }
}
