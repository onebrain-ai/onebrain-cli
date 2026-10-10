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
  ├─ onebrain-cache   Session token resolution · checkpoint cadence state
  │                   · search status detection.
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

## The gateway process (v3.5)

`onebrain gateway run` is a second long-lived process in the same binary ([ADR 0036](decisions/0036-remote-mcp-gateway-separate-process.md)). It is the only surface meant to be reachable from outside, and it lives entirely in `onebrain-cli` (`src/commands/gateway/`, see the [module map](reference/onebrain-cli.md#srccommandsgateway)).

```
 Claude app (phone)
      │ HTTPS
      ▼
 Cloudflare edge ──▶ cloudflared (LaunchAgent, outbound-only tunnel)
                          │ http://127.0.0.1:7717   (original Host forwarded)
                          ▼
 onebrain gateway run  (LaunchAgent, binds 127.0.0.1 only)
   ├─ host_guard      Host / Origin / proxy-header checks on every route
   ├─ /.well-known/*, /register, /authorize, /token     OAuth 2.1 server  (oauth_routes, auth/)
   ├─ /mcp            bearer middleware ▶ rmcp streamable HTTP ▶ GatewayServer (Brain pack)
   │                      └─ policy gate ▶ approval registry ▶ audit log
   ├─ /approvals      loopback-only operator page
   └─ approval channels: macOS dialog · /approvals · Telegram bot
                          │ loopback HTTP
                          ▼
              per-vault warm daemon (unchanged, ADR 0033)  ·  vault files
```

Request path for a gated write such as `brain_capture`: `host_guard` → bearer check (scope covers the pack) → policy class and mode → if approval is needed, register a pending approval, open an SSE stream with keep-alives ([ADR 0039](decisions/0039-gateway-approval-keepalive-and-wait-clamp.md)) and prompt every configured channel → first answer wins → run the tool → append one audit line. Reads go through the same gate with mode `auto`. `brain_search` is always routed to the vault's daemon; the gateway never opens a search engine itself.

**State.** Config is `~/.onebrain/gateway.yml` (machine-level, read once at startup). Runtime data lives under `~/.onebrain/gateway/`, directory `0700`, files `0600`:

| Path | Contents |
|---|---|
| `clients.json`, `codes.json`, `tokens.json`, `pairing.json` | OAuth clients, pending authorization codes, access and refresh token records, the pairing code. |
| `auth.lock` | Advisory lock held across every read-modify-write of the four files ([ADR 0041](decisions/0041-gateway-auth-store-lock-and-toctou-recheck.md)). |
| `audit/YYYY-MM.jsonl` | Append-only audit log, one redacted JSON line per tool call. |
| `tunnel.token` | The Cloudflare tunnel token, written by `gateway tunnel setup`. |
| `telegram-<hash>.offset` | Telegram `getUpdates` cursor, keyed by a hash of the bot token. |

On macOS, `gateway service install` also writes `~/Library/LaunchAgents/com.onebrain.gateway.plist` and `com.onebrain.gateway-tunnel.plist`, with logs in `~/Library/Logs/onebrain/` ([ADR 0042](decisions/0042-gateway-tunnel-and-launchagent-service.md)).

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

- The gateway's setup, config and security model: [`gateway.md`](gateway.md).
- The *why* behind specific choices: [`decisions/`](decisions/).
- The Rust idioms these crates use: [`rust-patterns.md`](rust-patterns.md).
- Crate-by-crate source map, plus the [`onebrain mcp` API reference](reference/mcp.md): [`reference/`](reference/).
