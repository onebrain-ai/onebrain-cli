# 0006 — Locked `<noun> <verb>` command tree

- **Status:** accepted
- **Date:** 2026-05-24

## Context

v3.0 inherited a flat, ad-hoc command surface from the Bun era: `session-init`, `orphan-scan`, `qmd-reindex`, `register-hooks`, `register-schedule`, `run-skill`, `vault-sync`. No consistent grammar, hard to discover, and every new feature invented its own name. The plugin and hooks hard-code these names, so the surface needs to be *stable* — but it also needs room to grow into daemon, RPC, bundles, and more.

## Decision

Adopt a singular-noun, two-level grammar — **`onebrain <noun> <verb>`** — and lock the full tree shape up front:

- **3 root verbs:** `init`, `update`, `doctor`.
- **Resource groups** with verbs: `session init`, `checkpoint stop/reset/orphans`, `qmd reindex/embed/status`, `vault sync/current`, `harness detect`, `plugin install/update/migrate`, `schedule register`, `skill run`.
- The full tree (27 entries) is **locked for v3.2+**: the ~200 verbs that aren't built yet are stubbed and return a stable `E_NOT_IMPLEMENTED` (exit 72), so the grammar can't drift while features land one at a time.
- Old flat names ship as **hidden clap aliases** that print a one-time migration notice and dispatch to the new handler. `plugin update` rewrites hook paths and launchd plists to the new names. Aliases are removed no earlier than a future major.

## Consequences

- **Predictable + discoverable** — once you know the nouns, you can guess the verbs; `--help` shows only the working surface.
- **A stable contract** for hooks/plugin/scripts, with room to grow — a new feature slots into the locked tree without renaming anything.
- **No flag-day break** — existing installs keep working through the alias layer + auto-rewrite.
- **Cost:** a migration layer to maintain (aliases, the rewriter, the migration notice), and stub verbs that exist but only error until implemented.
