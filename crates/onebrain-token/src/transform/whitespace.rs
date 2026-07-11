//! Lossless whitespace compaction: trims trailing line whitespace, collapses
//! runs of blank lines to at most one, and collapses interior runs of
//! spaces/tabs to a single space — while preserving leading indentation
//! (markdown lists, nested content) untouched. Purely cosmetic; no prose
//! character is removed.

use super::{Payload, Transform, TransformCtx};
use crate::level::OptLevel;

pub struct Whitespace;

impl Transform for Whitespace {
    fn name(&self) -> &'static str {
        "whitespace"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Conservative
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
    let lines: Vec<String> = text.lines().map(collapse_line).collect();

    let mut out_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut blank_run = 0u32;
    for line in lines {
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out_lines.push(line);
            }
        } else {
            blank_run = 0;
            out_lines.push(line);
        }
    }
    out_lines.join("\n")
}

/// Trims trailing whitespace and collapses interior runs of spaces/tabs to
/// one space, while leaving leading indentation exactly as-is.
fn collapse_line(line: &str) -> String {
    let trimmed_end = line.trim_end();
    let indent_len = trimmed_end.len() - trimmed_end.trim_start().len();
    let (indent, rest) = trimmed_end.split_at(indent_len);

    let mut collapsed = String::with_capacity(rest.len());
    let mut last_was_space = false;
    for ch in rest.chars() {
        if ch == ' ' || ch == '\t' {
            if !last_was_space {
                collapsed.push(' ');
            }
            last_was_space = true;
        } else {
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
}
