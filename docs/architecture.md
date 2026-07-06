# Architecture

OneBrain CLI is a five-crate Cargo workspace. Only the binary ships; the library crates exist to keep responsibilities separated and testable.

```
onebrain-cli          Binary crate — clap dispatch over the v3.1 command tree,
  │                   output rendering, TTY/spinner concerns. Knows about the user.
  │
  ├─ onebrain-search  Native vault search — tantivy BM25 · fastembed embeddings
  │                   · flat vector store · RRF hybrid · Tier-2 cross-encoder rerank.
  │                   Knows about the index.
  │
  ├─ onebrain-fs      Vault walks · frontmatter parsing · plugin tarball overlay
  │                   · init bootstrap · doctor checks · update install path · backups.
  │                   Knows about the filesystem.
  │
  ├─ onebrain-cache   Session token resolution · launchd plist generation
  │                   · checkpoint cadence state · search status detection.
  │                   Host/runtime state.
  │
  └─ onebrain-core    Types · config parsing · path resolution. Zero filesystem deps —
                      pure logic, the easiest crate to unit-test.
```

## Dependency direction

The arrow points *down* — higher crates depend on lower ones, never the reverse:

```
onebrain-cli ──▶ onebrain-fs ──▶ onebrain-core
       │                              ▲
       ├────────▶ onebrain-cache ─────┘
       │
       └────────▶ onebrain-search     (standalone — no workspace deps)
```

- **`onebrain-core` depends on nothing in the workspace.** It holds the config types (`VaultConfig`), error types, and path/vault resolution (`resolve_vault`). Because it touches no filesystem, its tests are fast and deterministic. (The `Envelope<T>` output shape lives one layer up, in the `onebrain-cli` binary — see [How a command flows](#how-a-command-flows).)
- **`onebrain-fs` and `onebrain-cache` depend only on `onebrain-core`.** They turn pure types into real effects (reading a vault, writing a plist, swapping a binary).
- **`onebrain-search` depends on nothing in the workspace either** — it wraps its vendored search stack (tantivy, fastembed, redb) behind one `Engine` type and knows nothing about vault config or output shapes. Only the binary depends on it; the CLI's `search_common` module maps `search.collection` config to the engine's on-disk cache dir.
- **`onebrain-cli` is the only crate that talks to the user.** clap parsing, output formatting, colors, and the `indicatif` spinner all live here. The library crates emit data; the binary decides how to render it.

This is the classic "push side effects to the edges" layering: the testable core has no I/O, the I/O crates have no UI, and the UI crate orchestrates.

## How a command flows

Taking `onebrain doctor --json` as the worked example:

1. **`onebrain-cli/src/main.rs`** parses argv with clap into the command tree (`<noun> <verb>`), resolves global flags (`--vault`, `--output`/`--json`/`--yaml`), and dispatches to `commands::doctor`.
2. **`commands/doctor.rs`** resolves the vault root (via `onebrain-core`'s resolver), then asks **`onebrain-fs`** to run the checks.
3. **`onebrain-fs/src/doctor/`** runs each `Box<dyn Check>` and returns plain data (`Vec<DoctorResult>`) — no printing, no colors.
4. Back in the binary, the report is wrapped in the canonical `Envelope<T>` and handed to `serialize_for_mode`, which renders text / JSON / YAML based on the resolved `OutputMode`.

The same shape holds for every command: **parse → resolve → do work in a library crate → render in the binary.** The library never decides output format; the binary never decides business logic.

## Why `publish = false`

Workspace inheritance keeps `[workspace.package]` fields (`version`, `edition`, `license`, `repository`) in one place. The workspace root sets `publish = false` and every crate inherits it via `publish.workspace = true`. The library crates are implementation detail, not a public Rust API — only the compiled `onebrain` binary is a product. This keeps us free to refactor crate boundaries without semver obligations to crates.io consumers, and it reflects the Path-B product boundary (Studio spawns the binary as a sidecar rather than importing these crates). With the workspace now permissively licensed (`MIT OR Apache-2.0`), that boundary is a product/architecture choice — no longer forced by copyleft as it was under AGPL.

## Testing & CI

Test pyramid (3 layers since v3.1.0): inline unit + `assert_cmd` integration + `insta` snapshots, 900+ tests passing. CI gates on `fmt` + `clippy -D warnings` + a 3-platform matrix (Ubuntu, macOS, Windows). The v2.x Bun golden-master parity layer was retired in v3.1.0; the v3.1 `Envelope` shape and the output-format matrix now own the canonical-contract role. The tests that pin the output contract are listed in [`CONTRIBUTING.md`](../CONTRIBUTING.md#build--test).

## Performance

The Rust rewrite milestone, measured against the v2.3.3 TypeScript/Bun CLI on the same hardware (Apple M1, macOS) running `onebrain doctor` warm:

| Metric | v2.3.3 (Bun) | v3.0.0 (Rust) | Δ |
|---|---|---|---|
| Stripped binary size | 57.8 MB | 4.6 MB | **−92%** |
| Private memory per invocation (peak) | ~21 MB | ~2 MB | **~10× less** |
| Cold start | ~120 ms | < 50 ms | **~2.5× faster** |
| Warm `doctor` wall time | ~980 ms | ~890 ms | ~9% faster |
| `update --check` (warm cache) | ~480 ms | ~10 ms | **~48× faster** |

Figures are the v3.0.0 rewrite-milestone dogfood (against the v2.3.3 Bun CLI). The binary has since grown well past 4.6 MB — v3.4 embedded the native search engine (tantivy + fastembed + ONNX Runtime), so a full semantic-search build is ~27 MB while keyword-only targets stay smaller (see the [platform table](platform-support.md)). The memory / cold-start / update-check wins above still hold. Reproduce with the release profile (`lto = "thin"`, `strip = "symbols"`, `codegen-units = 1`, `panic = "abort"`).

## Where to go next

- The *why* behind specific choices: [`decisions/`](decisions/).
- The Rust idioms these crates use: [`rust-patterns.md`](rust-patterns.md).
- Crate-by-crate source map, plus the [`onebrain mcp` API reference](reference/mcp.md): [`reference/`](reference/).
