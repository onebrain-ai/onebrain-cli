# 0015 — Grouped text-output convention (emoji sections, aligned rows, no frames)

- **Status:** accepted
- **Date:** 2026-07-02

## Context

Each command grew its own text layout: framed boxes (doctor, serve), flat emoji-prefixed lines (status, reindex), ad-hoc spacing. Two rendering realities kept breaking them: terminal fonts disagree about emoji cell width (a single space after an emoji is swallowed; label columns aligned by emoji width go ragged), and framed borders either render dashed (`│` + line spacing) or chunky. Every screenshot review found a new misalignment.

## Decision

One convention for human-facing text output, implemented as shared helpers (`output::layout::{section, item}` + `Section::with_emoji` on the progress renderer):

- **Section header:** `{emoji}␣␣{Title}` — emoji only on headers, two spaces after every emoji, Capitalized first word.
- **Body rows:** four-space indent, fixed-width label column (`{label:<14}`), values flush — alignment never depends on emoji rendering because rows contain no emoji (semantic glyphs like ✓/⚠ live in *values*).
- **No frames.** Blank line between sections; optional trailing `💡` hint line. Verdict glyphs and exit-code contracts (doctor) are unchanged by presentation.
- JSON/YAML envelopes are exempt: additive fields only, never re-shaped for cosmetics.

Applied to `search status/reindex`, `doctor`, `serve`, `qmd status`; data tables (`model list`, the model TUI) share the boxed-table look with `unicode-width`-computed padding instead.

## Consequences

- New commands get a correct layout for free by calling the helpers; screenshot-driven alignment fixes stop being a per-command whack-a-mole.
- The convention is deliberately plain (no color-dependence, no frames), so it renders identically in Warp, the Obsidian terminal, CI logs, and Telegram-relayed monospace.
- Existing string-assert tests broke wholesale when adopted — accepted one-time cost; tests now pin the convention, making accidental drift loud.
