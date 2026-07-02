# 0014 — Index scope: Markdown-only, built-in skips, `search.exclude`

- **Status:** accepted
- **Date:** 2026-07-02

## Context

The first full-vault reindex walked everything under the vault root: 1,208 documents, of which ~450 were junk — library READMEs under an attachment's `node_modules/`, archived projects, hidden tool state. Junk documents don't just waste embedding time; they compete with real notes in every ranked list and directly caused "results aren't relevant" reports.

## Decision

Three layers, from immutable to user-owned:

1. **Hard scope:** only `*.md` files are ever indexed; other file types are never touched (binary content enters the index only after a future `/import` converts it to Markdown).
2. **Built-in skips (not configurable):** hidden directories (`.obsidian`, `.git`, `.claude`, …) and `node_modules` at any depth.
3. **`search.exclude` (vault config):** user patterns — a bare name matches a directory at any depth, a `/`-containing entry is a vault-relative path prefix. Defaults to `["attachments"]` (the copied-file staging area, not knowledge notes); an explicit `exclude: []` opts out. Applied identically by `reindex` and `status`, so pending counts never disagree with the walk.

Index keys are vault-relative forward-slash paths; a moved or renamed file is a remove+add pair that plain `reindex` detects by itself (hash diff + sweep).

## Consequences

- The real vault dropped from 1,208 to ~720 indexed docs and relevance complaints stopped; reindex time fell proportionally.
- Excluding a folder later leaves stale docs in the index until the next `reindex` sweep — acceptable because the sweep is automatic on every run.
- `attachments` as a *default* exclusion is opinionated: a user who keeps notes there must opt out. Chosen because the vault convention defines it as import staging, and the failure mode (junk ranked above notes) is worse than the opt-out cost.
