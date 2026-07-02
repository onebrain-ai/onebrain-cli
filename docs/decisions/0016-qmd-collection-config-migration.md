# 0016 — `qmd_collection` → `search.collection`, migrated by `doctor --fix`

- **Status:** accepted
- **Date:** 2026-07-02

## Context

The native engine (ADR 0012) namespaces its config under `search.*`, but existing vaults carry the legacy top-level `qmd_collection` key. Silently reading the old key forever means two names for one concept; dropping it abruptly breaks every existing vault. Precedent: ADR 0007 migrated `vault.yml` → `onebrain.yml` using `doctor --fix` as the migration vehicle.

## Decision

- `search.collection` is the canonical key. Resolution order: explicit `search.collection` → legacy `qmd_collection` (read-fallback, kept for un-migrated vaults) → auto-generated `<vault-dir>-<sha256[:6]>` persisted on first use.
- `doctor` flags a present `qmd_collection` as a warning; **`doctor --fix` migrates it**: copies the value into `search.collection` (only if unset — never overwrites), then deletes the legacy key, under the standard config-backup + atomic-write pattern. The post-fix re-check reads from disk, so a successful migration clears its own warning in the same run.
- Doctor's index health check reports the native `search` engine; the qmd embedding check and its fixer are removed (qmd itself remains installed-but-legacy until v3.4.2 removes it).

## Consequences

- One migration path, self-healing, no manual YAML surgery — consistent with 0007, so users learn the pattern once.
- Vaults that never run `doctor --fix` keep working via the read-fallback indefinitely; the fallback is the price of not breaking anyone, and it retires with qmd in v3.4.2.
- Removing the key disables the legacy qmd MCP/hook plugin-side before the native MCP lands (v3.4.1) — an accepted gap during the transition, mitigated by the CLI verbs being fully usable.
