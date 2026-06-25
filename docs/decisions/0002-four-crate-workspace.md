# 0002 — Four-crate workspace split

- **Status:** accepted
- **Date:** 2026-05-19

## Context

A single crate would mix three concerns that change for different reasons and want different tests: pure logic (config parsing, path rules), filesystem effects (vault walks, install), and the user-facing binary (argument parsing, output, colors). Mixing them makes the pure logic slow to test (it drags in I/O) and blurs where a change belongs.

## Decision

Split into a four-crate Cargo workspace with a strict downward dependency direction:

- **`onebrain-core`** — types, config parsing, path resolution, scheduler model. **Zero filesystem deps.** Depends on nothing in the workspace.
- **`onebrain-cache`** — host/runtime state: session tokens, checkpoint cadence, qmd status. Depends only on `core`.
- **`onebrain-fs`** — filesystem effects: vault walks, init, doctor, the update install path, vault sync, backups. Depends only on `core`.
- **`onebrain-cli`** — the binary: clap dispatch, output rendering, TTY/banner. The only crate that talks to the user. Depends on `core`, `fs`, `cache`.

The workspace root sets `publish = false`; every crate inherits it.

## Consequences

- **`core` is trivially testable** — no I/O means fast, deterministic unit tests, and it's where the tricky pure logic lives.
- **Clear "where does this go?"** — business logic in a library crate, rendering in the binary; the library never decides output format, the binary never decides logic.
- **Refactor freedom** — `publish = false` means no crates.io semver obligations; crate boundaries can move freely. This also reflects the Path-B product boundary: Studio spawns the `onebrain` binary as a sidecar rather than importing these crates — a product/architecture choice (no longer copyleft-forced now that the workspace is `MIT OR Apache-2.0`).
- **Cost:** more crates to navigate and a little re-export boilerplate. The [code reference](../reference/) exists partly to offset the navigation cost.
