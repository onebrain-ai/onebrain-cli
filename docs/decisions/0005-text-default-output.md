# 0005 — Text-default output · hooks inject `--json`

- **Status:** accepted
- **Date:** 2026-05-25

## Context

A v3.1.0 release candidate defaulted every command to JSON output. It passed tests and three review rounds — because the reviewers checked the code against the spec, and the *spec itself* was the thing that was wrong. The moment the author ran a command in a real terminal, JSON-by-default felt obviously wrong for a human at a TTY. (A reminder that surface inspection catches what `assert!(success())` can't.)

But the machine consumers — the plugin's `SessionStart` / `Stop` / `PostToolUse` hooks — genuinely want JSON, with no human in the loop.

## Decision

Default to human-readable **text**; make `--json` / `--yaml` opt-in. To keep machine consumers working without manual flags, the hook rewriter and the `init` scaffold **inject `--json` into hook command entries automatically** (idempotent — it respects a pre-existing `--json` / `--yaml` / `--output` choice and won't double-add). Two hook-only commands (`checkpoint stop`, `qmd reindex`) still hard-wire JSON regardless of flags, since they are never invoked interactively.

Output also auto-adapts to the environment: piped / CI invocations drop color and the startup banner, so a consumer that forgets the flag still gets clean (if text) bytes, and closed-pipe writes exit `0` instead of panicking.

## Consequences

- **Interactive UX is right by default**, and machine output stays clean and explicit.
- **Existing installs migrate automatically** — `plugin update` rewrites `~/.claude/settings.json` hook entries to carry `--json`.
- **The lesson stuck:** the v3.1 output-format test matrix now asserts *not-JSON* for the default mode and parses YAML for the yaml mode, rather than only asserting the command succeeded.
- **Cost:** hook entries must carry `--json`; the idempotent injection + a doctor check keep that from drifting.
