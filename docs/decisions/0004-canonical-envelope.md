# 0004 — Canonical `Envelope<T>` output shape

- **Status:** accepted
- **Date:** 2026-05-25

## Context

In v3.0 each command shaped its `--json` output however was convenient. Consumers — the plugin's hooks, the `/update` skill, user scripts — had to special-case every command's payload, and there was no stable place to carry metadata (which vault? any warnings? did it actually succeed?). Adding a field to one command risked surprising a consumer that hard-coded the old shape.

## Decision

Every structured-output command wraps its payload in one generic envelope (skill-alignment §4.3):

```jsonc
{
  "version": "3.1.x",      // CLI version that produced this
  "command": "doctor",      // which command
  "ok": true,               // success flag — read this, not the exit code, in JSON
  "vault": "/path/…",       // omitted when not vault-scoped
  "data": { … },            // the command-specific payload (the generic T)
  "warnings": [],           // always an array, never null
  "error": { … }            // omitted when ok
}
```

`vault` and `error` are dropped via `skip_serializing_if` when absent; `warnings` is always `[]` (not `null`) so consumers never branch on missing-vs-empty. `--yaml` emits the same shape. One dispatcher — `serialize_for_mode` in `onebrain-cli/src/output/` — renders text / JSON / YAML / table / TSV from the same envelope. The schema is **frozen across v3.x**.

## Consequences

- **Consumers parse one shape, always** — read `ok` + `data`, ignore the rest, and new fields never break them.
- **Metadata has a home** — version, vault, warnings, structured errors travel with every response.
- **One rendering path** — adding an output format is a change in one dispatcher, not in every command.
- **Cost:** a little envelope verbosity around small payloads, and a frozen schema means v4 is where any breaking change to the shape has to wait.
