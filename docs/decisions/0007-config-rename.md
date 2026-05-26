# 0007 — Config rename `vault.yml → onebrain.yml`

- **Status:** accepted
- **Date:** 2026-05-25

## Context

The per-vault config file was named `vault.yml` — a generic name that says nothing about what owns it. As OneBrain grew a brand and other tools learned to coexist in the same vault, an unambiguous `onebrain.yml` reads far better. The user base was still small ("ยังไม่กระทบเยอะ"), so the migration cost of a rename was low — a good moment to do it before it gets expensive.

## Decision

Make `onebrain.yml` the canonical filename. To avoid breaking existing vaults, the CLI **dual-reads**: it prefers `onebrain.yml` and falls back to `vault.yml` with a one-time `W_VAULT_YML_DEPRECATED` stderr warning (suppressible). `onebrain init` writes `onebrain.yml`. A `vault-config-migration` doctor check fires when a legacy `vault.yml` is found, and `doctor --fix` migrates it with a single atomic `fs::rename` (idempotent — removes the stale legacy file if both exist). `vault.yml` support is dropped entirely in **v4.0**.

This decision is also where the **timestamped-backup invariant** was hardened (v3.1.1): any operation that overwrites, migrates, or removes a config file first copies it to `<vault>/.onebrain-backups/<file>.<YYYYMMDD-HHMMSS>.bak`, and refuses the write if the backup can't be made. A config-loss incident (an `init --force` that clobbered `qmd_collection`) motivated making backups a hard precondition rather than a nicety.

## Consequences

- **Brand-clear config** with a safe transition — dual-read + deprecation warning + auto-migrate means nobody's vault breaks.
- **Config changes are recoverable** — every destructive config write leaves a timestamped backup behind.
- **Cost:** a dual-read code path and a migration doctor check to carry until v4.0 retires `vault.yml`.
