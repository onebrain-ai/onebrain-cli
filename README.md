<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/banner-dark.svg">
    <img alt="OneBrain CLI — Your AI Thinking Partner" src="assets/banner-light.svg" width="640">
  </picture>
</p>

<p align="center"><em>Your AI Thinking Partner</em></p>

<p align="center">
  <strong>The local-first Rust CLI that powers the OneBrain personal AI OS.</strong><br>
  <sub>Vault scaffolding · plugin sync · scheduled skills · diagnostics · self-update — across Claude Code and Gemini CLI.</sub>
</p>

<p align="center">
  <a href="https://github.com/onebrain-ai/onebrain-cli/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/onebrain-ai/onebrain-cli?include_prereleases&style=for-the-badge&logo=rust&color=cb3837&label=release"></a>
  <a href="https://www.npmjs.com/package/@onebrain-ai/cli"><img alt="npm" src="https://img.shields.io/npm/v/@onebrain-ai/cli?style=for-the-badge&logo=npm&color=cb3837&label=npm"></a>
  <a href="https://github.com/onebrain-ai/onebrain-cli/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/onebrain-ai/onebrain-cli/ci.yml?branch=main&style=for-the-badge&logo=githubactions&logoColor=white&label=CI"></a>
  <a href="LICENSE-MIT"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT_OR_Apache--2.0-7c3aed?style=for-the-badge"></a>
</p>
<p align="center">
  <a href="https://onebrain.run"><img alt="Website" src="https://img.shields.io/badge/onebrain.run-0a0a14?style=for-the-badge&labelColor=ff2d92"></a>
  <a href="https://x.com/onebrain_run"><img alt="@onebrain_run on X" src="https://img.shields.io/badge/follow-@onebrain__run-000000?style=for-the-badge&logo=x&logoColor=white"></a>
  <a href="https://github.com/onebrain-ai/onebrain-cli/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/onebrain-ai/onebrain-cli?style=for-the-badge&color=00f3ff&logo=github"></a>
</p>

---

## What is OneBrain CLI?

**`onebrain`** is the local-first Rust binary at the heart of [OneBrain](https://onebrain.run) — a personal AI operating system that lives in your local knowledge vault (plain Markdown files on your machine). It scaffolds new vaults, syncs the OneBrain plugin from GitHub, wires AI-harness hooks, runs scheduled skills through the OS scheduler, diagnoses vault health, and updates itself.

The CLI is **cross-harness**: paired with the [OneBrain plugin](https://github.com/onebrain-ai/onebrain) (slash commands, skills, agents), it runs under Claude Code and Gemini CLI against the same vault contract.

### Why OneBrain CLI

Point an AI agent at a vault and it improvises — a different pile of `grep` / `ls` / `find` / `sed` each time, behaving differently on each harness and re-derived every session: slow, token-hungry, non-portable, sometimes wrong. **OneBrain CLI replaces that improvisation with one deterministic binary.**

- **Same behavior on every harness & model** — Claude Code and Gemini CLI both run `onebrain <noun> <verb>` and get identical output; switch harness without re-testing how your vault gets touched.
- **Cross-platform, one command** — the *same* `onebrain <noun> <verb>` runs on macOS, Linux, and Windows (Apple Silicon & Intel, x86_64 & ARM down to a Pi Zero) and returns the *same* typed result on every OS. Write a hook or script once; it behaves identically everywhere — no per-platform shell quirks (`sed`/`find`/path-separator differences) to work around.
- **Yours to extend, no waiting** — add a capability the harness/LLM doesn't have yet and every agent can use it immediately; they only learn the command, not implement the feature.
- **No re-deriving solved workflows** — search, capture, consolidate, checkpoint live in the binary, so the agent calls one command instead of re-reasoning the recipe each session. Fewer tokens, no drift.
- **Deterministic & safe** — a typed command with a frozen `Envelope` can't half-finish or quietly differ like an ad-hoc `rm` / `sed` pipeline. Same input → same output, scriptable by hooks.
- **Fast** — the binary returns in under 50 ms, skipping the latency of several tool calls for what's already one operation.
- **Local-first** — your vault, your data, your AI memory; no cloud round-trip.
- **Trustworthy install** — self-update verifies the binary's SHA-256 before swapping.

## Features

- **🔍 Built-in native search** — hybrid keyword + semantic search over your vault's Markdown, multilingual (Thai/CJK-aware keyword bigrams + ~100-language embeddings), zero external dependencies: no Node, no Python, no separate search binary to install.
- **🔌 Built-in MCP server** — `onebrain mcp` plugs OneBrain into Claude Code, Cursor, or any MCP client as a vault search engine; see [Use as a standalone vault-search MCP](#use-as-a-standalone-vault-search-mcp).
- **Single static binary** — one self-contained file, no runtime to install, cross-platform down to a Raspberry Pi Zero.
- **Embedded web UI** — `onebrain serve` hosts a local, token-gated file explorer + reading view + search panel + agent chat, with nothing extra to install.
- **Interactive model picker** — a TUI over 6 embedding models (see [Choosing an embedding model](docs/reference/onebrain-search.md#choosing-an-embedding-model)) to trade off speed, accuracy, and Thai support.
- **Vault doctor + config migration** — eleven health checks with `--fix` recipes, plus automatic `onebrain.yml` schema migration.
- **Local scheduler** — cron-style recurring skills (daily briefing, weekly review, reindex) via the OS scheduler, no cloud runner needed.

## Status

**v3.4.x — stable & production-ready, in active maintenance.** GA since v3.0.0 (2026-05-22), shipping ~weekly themed minors. The v3.3 line landed the daemon foundation — `onebrain serve` hosts a local web UI (embedded in the binary) over a token-gated vault JSON API. The v3.4 line introduces the native Rust search engine (the `onebrain-search` crate + `onebrain search` verbs) and a native MCP server (`onebrain mcp`, v3.4.1), replacing the external Node search dependency across v3.4.x with full cutover — **0 node/python deps** — at v3.4.5. Version history + direction in the [Roadmap](#roadmap); full detail in [CHANGELOG.md](CHANGELOG.md).

## Quickstart

From zero to a working OneBrain vault in three steps.

```bash
# 1. Install (macOS — Homebrew is the canonical channel)
brew install onebrain-ai/onebrain/onebrain

# 2. Verify
onebrain --version
# → onebrain 3.4.4

# 3. Scaffold a vault and let init pull the OneBrain plugin
mkdir my-vault && cd my-vault
onebrain init --yes
```

Then open the vault in your AI harness (Claude Code, Gemini CLI, …) and run `/onboarding` to finish setup.

> On Linux/Windows, grab the matching binary from the [latest release](https://github.com/onebrain-ai/onebrain-cli/releases/latest) (table below) or `npm install -g @onebrain-ai/cli`. `init` runs an embedded `vault sync` step that downloads the plugin tarball; if the network step fails, the scaffold (`onebrain.yml`, PARA folders, Stop hook, schedule preset) stays intact and the binary prints an `onebrain vault sync` retry hint. Pass `--no-sync` for offline / CI scaffolding.

## Install

### Pre-built binaries

Pick the archive that matches your machine from the [latest release](https://github.com/onebrain-ai/onebrain-cli/releases/latest):

| Platform | Architecture | File |
|---|---|---|
| [![macOS](https://img.shields.io/badge/macOS-000000?logo=apple&logoColor=white&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-aarch64-apple-darwin.tar.gz) | Apple Silicon (M1–M5) | `onebrain-aarch64-apple-darwin.tar.gz` |
| [![macOS](https://img.shields.io/badge/macOS-000000?logo=apple&logoColor=white&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-x86_64-apple-darwin.tar.gz) | Intel | `onebrain-x86_64-apple-darwin.tar.gz` |
| [![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-aarch64-unknown-linux-gnu.tar.gz) | ARM64 (glibc · Pi 3/4/5 64-bit OS · Pi Zero 2 W) | `onebrain-aarch64-unknown-linux-gnu.tar.gz` |
| [![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-armv7-unknown-linux-gnueabihf.tar.gz) | ARMv7 32-bit (Pi 2 v1.1+ · Pi 3/4/5 32-bit OS) | `onebrain-armv7-unknown-linux-gnueabihf.tar.gz` |
| [![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-arm-unknown-linux-gnueabihf.tar.gz) | ARMv6 32-bit (Pi 1 · Pi Zero · Pi Zero W) | `onebrain-arm-unknown-linux-gnueabihf.tar.gz` |
| [![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-x86_64-unknown-linux-gnu.tar.gz) | x86_64 (glibc) | `onebrain-x86_64-unknown-linux-gnu.tar.gz` |
| [![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-x86_64-unknown-linux-musl.tar.gz) | x86_64 (musl / Alpine / static) | `onebrain-x86_64-unknown-linux-musl.tar.gz` |
| [![Windows](https://img.shields.io/badge/Windows-0078D4?logo=windows&logoColor=white&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-aarch64-pc-windows-msvc.zip) | ARM64 | `onebrain-aarch64-pc-windows-msvc.zip` |
| [![Windows](https://img.shields.io/badge/Windows-0078D4?logo=windows&logoColor=white&style=flat)](https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-x86_64-pc-windows-msvc.zip) | x86_64 | `onebrain-x86_64-pc-windows-msvc.zip` |

**Click a platform badge to download that file from the latest release.** Each archive ships with a matching `.sha256` for manual verification. Filenames use canonical Rust target triples, so installer scripts can parse them unmodified.

```bash
# Manual install (any Unix)
curl -L -o onebrain.tar.gz \
  https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-aarch64-apple-darwin.tar.gz
tar xzf onebrain.tar.gz
sudo install onebrain /usr/local/bin/
```

### Channels

| Channel | Command | Notes |
|---|---|---|
| **Homebrew** (macOS, canonical) | `brew install onebrain-ai/onebrain/onebrain` | Formula at [`onebrain-ai/homebrew-onebrain`](https://github.com/onebrain-ai/homebrew-onebrain), bumped on every tag. |
| **npm wrapper** | `npm install -g @onebrain-ai/cli` | Source at [`npm-wrapper/`](npm-wrapper/); CI publishes on every stable tag via npm Trusted Publishers + `--provenance`. Verifies the release SHA-256 before extracting. |
| **Direct download** | table above | Pick your triple, drop the binary on `PATH`. |

All channels resolve to the same per-platform binary published in the matching GitHub Release.

### Self-update

After the initial install, refresh in place:

```bash
onebrain update                # prompt-and-confirm
onebrain update --check        # dry-run (compare current vs latest)
onebrain update --plan         # machine-readable JSON plan
```

On an interactive terminal, `update` shows a framed `🧠 OneBrain Update` header and a braille spinner while it checks for (and downloads) a new version; piped / `--json` / `--plan` runs stay plain.

`onebrain update` auto-detects how the binary was installed and uses the right path so package-manager metadata never desyncs:

- **Homebrew** (binary under the Cellar) — refreshes the `onebrain-ai/onebrain` tap, then runs `brew upgrade onebrain`. The tap refresh (added v3.2.17) means a freshly-released version applies in one `onebrain update` with no manual `brew update`.
- **npm** (under `node_modules/@onebrain-ai/`) — runs `npm install -g @onebrain-ai/cli@<version>` (added v3.2.17).
- **Direct download** (a plain file we own) — resolves the current target triple, downloads the matching GitHub Release tarball over HTTPS (rustls TLS), verifies its SHA-256, and atomically swaps the running binary (Unix single-rename; Windows rustup-style two-step with rollback on failure).

After any path, a post-install guard runs `onebrain --version` from PATH and confirms the upgrade actually took effect.

### Build from source

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli
# → target/release/onebrain
```

Requires a recent stable Rust toolchain (`rustup default stable`). The only `unsafe` in OneBrain crates is a single `libc::getuid()` call (the launchd plist UID); the workspace otherwise builds cleanly on Linux, macOS, and Windows.

## Command surface

v3.1 locks a singular-noun, two-level grammar — `onebrain <noun> <verb>` — so every command path is predictable. Five root verbs handle the common flow; eleven resource groups cluster the rest.

```text
onebrain
├── init                       create / re-scaffold a vault (--yes · --force · --no-sync)
├── update                     self-update the binary (--check · --plan)
├── doctor [--fix]             11 health checks + auto-repair recipes
├── serve                      local web UI + vault JSON API (--port · --host · --open)
├── mcp                        MCP stdio server — vault search tools (--vault)
│
├── vault       sync · current
├── session     init
├── checkpoint  stop · reset · orphans
├── search      query · search · vsearch · get · status · reindex · model
├── note        read · list · find · search · stat · new · append · edit
│               · move · mkdir · archive · delete · orphans · backlinks
├── task        list
├── plugin      install · update · migrate
├── schedule    register · list
├── skill       run
└── harness     detect
```

| Group | Verbs | Purpose |
|---|---|---|
| **Setup** | `init`, `plugin install`, `vault sync` | Scaffold `onebrain.yml` + PARA folders, register the plugin with the harness, overlay the latest plugin tarball. |
| **Runtime** (hook protocol) | `session init`, `checkpoint stop · reset · orphans`, `search reindex · search reindex --lex-only · search reindex --pending-only` | Called by the harness `SessionStart` / `Stop` / `PostToolUse` hooks. `search reindex --lex-only` runs on PostToolUse (incremental keyword-index pass, no model load); `search reindex --pending-only` runs on Stop (embeds pending docs in background). Emit hard-wired JSON; banner suppressed for clean machine stdio. |
| **Search** | `search query · search · vsearch · get · status · reindex · model` | Native hybrid search (tantivy BM25 + fastembed embeddings, RRF-fused) over the vault's `*.md` notes, plus embedding-model management with an interactive TUI. The native `search` verbs are the sole search surface as of v3.4.5, completing the v3.4 native-search cutover (the previous external search commands were removed, not deprecated). `search reindex` flags: `--lex-only` (incremental keyword-index pass; never loads/downloads the embedding model; changed docs stay pending for the next embed pass), `--pending-only` (embeds only pending-vector docs; loads the model only when there is pending work; in `--json` mode detaches to background). |
| **MCP** | `mcp` | Stdio [Model Context Protocol](docs/reference/mcp.md) server exposing `query`/`get`/`multi_get`/`status` over the native search engine — for Claude Code, Cursor, or any MCP client. See [`docs/reference/mcp.md`](docs/reference/mcp.md) for the full tool reference. |
| **Notes** | `note read · list · find · search · stat · new · append · edit · move · mkdir · archive · delete · orphans · backlinks` | Structured vault-note operations — wikilink-aware moves, dated archiving, orphan/backlink graph queries. |
| **Tasks** | `task list` | List dated vault tasks (fence-aware), filterable by due date and folder. |
| **Web UI** | `serve` | Host the binary-embedded web UI + token-gated vault JSON API on `127.0.0.1:6789` — file explorer, reading view, search panel, agent chat; `--open` launches the browser. |
| **Maintenance** | `doctor [--fix]`, `plugin update · migrate`, `schedule register` | Eleven read-only checks + `--fix` recipes, self-update the binary + rewrite hooks + rebind launchd plists, compile the `onebrain.yml schedule:` block into OS scheduler artifacts. |
| **Diagnostics** | `vault current`, `harness detect` | Report which mechanism resolved the active vault, and which AI harness is running. |

> The tree shape is **locked for v3.2+** — 200+ verbs beyond the working set above are stubbed with a stable `E_NOT_IMPLEMENTED` (exit 72) so the grammar can't drift while features land. Hidden v3.0 flat aliases (`session-init`, `qmd-reindex`, `register-hooks`, …) still dispatch, printing a one-time migration notice (silence with `ONEBRAIN_QUIET_MIGRATION=1`); they're removed no earlier than v4.

### Platform support — semantic vs keyword search

Every release target ships a binary with the **full CLI** and **keyword (lexical/BM25) search**. **Semantic** search (vector + hybrid `query`, plus `vsearch` and `model set`) additionally needs an ONNX Runtime prebuilt, which isn't available on every platform — so some targets ship **keyword-only**. The tiering is driven by the `ort-sys` prebuilt list ([ADR 0017](docs/decisions/0017-platform-tiered-semantic-search.md)); this table, the release-workflow matrix, and the ADR all agree. Windows-arm64 is cross-compiled from x64 and native-dep changes are matrix-tested before tagging ([ADR 0018](docs/decisions/0018-release-build-strategy-lessons.md)).

| Target | Binary | Keyword search (lex) | Semantic search (vector/hybrid) | Notes |
|---|---|---|---|---|
| macOS arm64 (Apple Silicon) — `aarch64-apple-darwin` | ✅ | ✅ | ✅ | |
| macOS x64 (Intel) — `x86_64-apple-darwin` | ✅ | ✅ | ❌ lex-only | no ONNX Runtime prebuilt for darwin-x64 |
| Linux x64 glibc — `x86_64-unknown-linux-gnu` | ✅ | ✅ | ✅ | |
| Linux ARM64 glibc — `aarch64-unknown-linux-gnu` (Pi 3/4/5 64-bit) | ✅ | ✅ | ✅ | |
| Linux x64 musl / Alpine — `x86_64-unknown-linux-musl` | ✅ | ✅ | ❌ lex-only | ONNX Runtime prebuilt is glibc-only, not musl |
| Linux ARMv7 32-bit — `armv7-unknown-linux-gnueabihf` (Pi 2/3/4/5 32-bit OS) | ✅ | ✅ | ❌ lex-only | onnxruntime has no 32-bit ARM support |
| Linux ARMv6 32-bit — `arm-unknown-linux-gnueabihf` (Pi 1 · Zero) | ✅ | ✅ | ❌ lex-only | onnxruntime has no 32-bit ARM support |
| Windows x64 — `x86_64-pc-windows-msvc` | ✅ | ✅ | ✅ | |
| Windows ARM64 — `aarch64-pc-windows-msvc` | ✅ | ✅ | ✅ | |

On a keyword-only (lex-only) binary: `search search`, `get`, `status`, and `reindex` work fully; hybrid `query` falls back to keyword ranking with a one-line notice; `vsearch` and `model set` report that semantic search is unavailable in that build.

## Local web UI

`onebrain serve` starts a local, token-gated HTTP server that hosts the **OneBrain web UI** — a file explorer, a reading view (markdown, code, PDF, Office docs, images, audio/video, Jupyter notebooks), a native-search-backed search panel, and an agent chat — over a small vault JSON API.

```bash
onebrain serve          # → http://127.0.0.1:6789/?token=<TOKEN>   (Ctrl-C to stop)
onebrain serve --open   # …and open it in your browser
```

The web UI is **embedded in the binary** — a release `onebrain` ships the latest build and `serve` mounts it at `/`, so there's nothing extra to install. Pass `--dir <dist>` only to override the bundle (web UI development against a live daemon).

- **Token-gated** — every request (and the SPA shell itself) needs the per-session token printed in the URL, sent as the `X-OneBrain-Token` header, a `?token=` query param, or an `HttpOnly` cookie.
- **Loopback by default** (`127.0.0.1:6789`). `--host 0.0.0.0` self-hosts remotely but serves plain HTTP — put a TLS tunnel/proxy (Cloudflare Tunnel, Tailscale Serve, Caddy) in front; `serve` warns loudly when you bind beyond loopback.
- **Hardened surface** — confined to the vault (tooling dirs like `.git`/`.claude` are refused), script-carrying files forced to download, a strict CSP, and the agent subprocess never inherits the daemon token. See [Security & trust model](#security--trust-model).

## Use as a standalone vault-search MCP

`onebrain mcp` also works as a generic local Markdown search engine for **any** folder that has an `onebrain.yml`, not just a full OneBrain vault. Hybrid lex + semantic search, multilingual, single binary, no Node/Python.

Minimal `onebrain.yml` (folder defaults + a search collection name are all it needs):

```yaml
folders:
  inbox: 00-inbox
  projects: 01-projects
search:
  collection: my-notes
```

Then index and register it in any MCP client:

```bash
onebrain search reindex --vault /path/to/notes
```

```json
{ "vault-search": { "command": "onebrain", "args": ["mcp", "--vault", "/path/to/notes"] } }
```

See [`docs/reference/mcp.md`](docs/reference/mcp.md) for the full tool reference (parameters, result shapes, call examples). A zero-config `--dir`-only mode (no `onebrain.yml` required) is on the backlog for v3.4.3+ — not yet available.

## Output modes

Interactive commands default to human-readable `text`; pass a flag for structured output. Every structured payload is wrapped in the canonical `Envelope<T>`:

```bash
onebrain doctor                 # TTY: animated per-check report, colorized
onebrain doctor --json          # { version, command, ok, vault, data, warnings, error }
onebrain vault current --yaml   # same envelope, YAML
onebrain search status --json | jq .data
```

- `--output {text,json,yaml,table,tsv}` — full matrix on every command; `--json` / `--yaml` are shorthands.
- `--pretty` forces indented JSON even when stdout is piped; `--no-color` (or `NO_COLOR`) forces monochrome; `-q` drops info logs (errors still hit stderr).
- Output auto-adapts: piped/CI invocations drop color and the startup banner, so machine consumers get clean bytes with no flags. Closed-pipe writes (`onebrain search reindex | head`) exit `0`, not a panic.

## Security & trust model

`onebrain update` authenticates downloaded binaries two ways: **GitHub's TLS chain** (rustls validation, no opt-out) secures the transport, and since v3.1.4 a **SHA-256 check** verifies the archive against its published `.sha256` *before* the swap — an unverifiable or mismatched asset is refused and the live binary is left untouched. The npm wrapper runs the same SHA-256 check before extracting. What's *not* yet done is cosign/signature verification: the checksum is an integrity check, not an authenticity one (an attacker who controls the serving origin could serve a matching archive + `.sha256` pair), so signing is tracked as a follow-up.

On networks running a corporate MITM proxy, the trust boundary becomes whatever certificate the proxy presents. If that matters to your threat model, verify the published `.sha256` files manually after each update.

Every operation that overwrites, migrates, or removes a config file first copies it to `<vault>/.onebrain-backups/<file>.<YYYYMMDD-HHMMSS>.bak` — the backup is a hard precondition, so the write is refused if the backup can't be made.

Report security issues privately via the channel documented in [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Performance

The Rust rewrite milestone, measured against the v2.3.3 TypeScript/Bun CLI on the same hardware (Apple M1, macOS) running `onebrain doctor` warm:

| Metric | v2.3.3 (Bun) | v3.0.0 (Rust) | Δ |
|---|---|---|---|
| Stripped binary size | 57.8 MB | 4.6 MB | **−92%** |
| Private memory per invocation (peak) | ~21 MB | ~2 MB | **~10× less** |
| Cold start | ~120 ms | < 50 ms | **~2.5× faster** |
| Warm `doctor` wall time | ~980 ms | ~890 ms | ~9% faster |
| `update --check` (warm cache) | ~480 ms | ~10 ms | **~48× faster** |

Figures are the v3.0.0 rewrite-milestone dogfood (against the v2.3.3 Bun CLI). The binary has since grown well past 4.6 MB — v3.4 embedded the native search engine (tantivy + fastembed + ONNX Runtime), so a full semantic-search build is ~27 MB while keyword-only targets stay smaller (see the [platform table](#platform-support--semantic-vs-keyword-search)). The memory / cold-start / update-check wins above still hold. Reproduce with the release profile (`lto = "thin"`, `strip = "symbols"`, `codegen-units = 1`, `panic = "abort"`).

## Architecture

Five-crate Cargo workspace:

```text
onebrain-cli          Binary crate — clap dispatch over the v3.1 command tree
  │
  ├─ onebrain-search  Native vault search — tantivy BM25 · fastembed embeddings
  │                   · flat vector store · RRF hybrid. Knows about the index.
  │
  ├─ onebrain-fs      Vault walks · frontmatter parsing · plugin tarball overlay
  │                   · init bootstrap · doctor checks · update install path · backups
  │
  ├─ onebrain-cache   Session token resolution · launchd plist generation
  │                   · search status detection
  │
  └─ onebrain-core    Types · config parsing · path resolution (zero filesystem deps)
```

(`onebrain-search` is deliberately standalone — it depends on no other workspace crate, only the CLI depends on it.)

Workspace inheritance keeps `[workspace.package]` fields (`version`, `edition`, `license`, `repository`) in one place. The root sets `publish = false`; all five crates inherit it via `publish.workspace = true` — only the compiled binary ships.

Test pyramid (3 layers since v3.1.0): inline unit + `assert_cmd` integration + `insta` snapshots, 900+ tests passing. CI gates on `fmt` + `clippy -D warnings` + a 3-platform matrix (Ubuntu, macOS, Windows). The v2.x Bun golden-master parity layer was retired in v3.1.0; the v3.1 `Envelope` shape and the output-format matrix now own the canonical-contract role.

> **Design notes & Rust patterns** → [`docs/`](docs/): an architecture deep-dive, the [search architecture reference](docs/architecture/search.md) (index storage, reindex/embed pipeline, every search mode end-to-end), decision records (ADRs), and a guided tour of the idiomatic Rust this codebase uses — written for contributors, people studying the source, and Rust learners.

## Roadmap

> Directional — themes are committed, timing flexes with the weekly-minor cadence (≈ one themed minor per week). The live public roadmap is at [onebrain.run](https://onebrain.run).

> Major/minor only — see [CHANGELOG](CHANGELOG.md) for per-patch detail.

### ✅ Shipped
- [x] **v3.0** — Rust rewrite GA · 9-platform release pipeline · stable JSON contracts.
- [x] **v3.1** — Consistency standard: locked `<noun> <verb>` command tree · canonical `Envelope` output · branded banner · `vault.yml → onebrain.yml` · `qmd embed` · `schedule register` `onebrain.yml` support · self-update hardening (SHA-256 verify + Homebrew-aware).

### 🚧 Phase 1 · perceptual speed + skill alignment (v3.2–v3.8)
- [x] **v3.2** — `note` resource group (11 verbs) · grouped `doctor` UX with braille spinner + one-pass `--fix` · animated `onebrain update` · `skill run --harness {claude,gemini}` + `--model <m>` + headless startup-skip handshake + in-place spinner · `harness run [PROMPT] --mode {with-context,ad-hoc}` for ad-hoc prompts through claude / gemini (reads stdin when omitted) · auto-checkpoint hook fix (`CLAUDE_CODE_SESSION_ID` top-priority token + anchored `last_ts` so the time threshold actually fires) · `--vault` accepted everywhere.
- [x] **v3.3** — Daemon foundation: `onebrain serve` — a local **web UI embedded in the binary** over a token-gated vault JSON API (file explorer · reading view · search panel · agent chat), on a security-hardened surface (whole-surface token gate · vault path confinement · CSP + forced-attachment · agent env isolation).
- [ ] **v3.4** — **Native Rust search — replaces qmd** (mini-epic across v3.4.x; exit: **0 node/python deps**):
  - [x] **v3.4.0** — `onebrain-search` engine (tantivy BM25 + fastembed embeddings + RRF hybrid, ~100-language semantic + Thai/CJK keyword) · `onebrain search` verbs (`query/search/vsearch/get/status/reindex/model` + interactive model TUI) · doctor native-search checks · `qmd_collection` → `search.collection` migration.
  - [x] **v3.4.1** — native MCP server (`onebrain mcp`, rmcp): qmd-compatible `query/get/multi_get/status` tools, client-side RRF fusion, native `session init` probe (drops the qmd subprocess). Plugin `.mcp.json` server-key rename (`qmd` → `search`) is staged for the v3.4.5 cutover — see [ADR 0019](docs/decisions/0019-native-mcp-server-staged-qmd-cutover.md).
  - [x] **v3.4.2** — **security fix**: the `serve`/daemon session auth token now comes from the OS CSPRNG on every platform (`getrandom`), removing the time-seeded, guessable Windows fallback. *(Interleaved ahead of the qmd epic — see below.)*
  - [x] **v3.4.3** — housekeeping bundle: scheduler polish (cron steps/lists · plist collision · `schedule list`) · CI `lex-only` test job · minor fixes.
  - [x] **v3.4.4** — scheduler runs actually fire: `onebrain` is now put on the headless-`claude` child's PATH so cron skills no longer exit 78 (#124) · generated plists emit `skill run`, not the deprecated `run-skill` (#125). *(Interleaved scheduler-fix patch, ahead of the qmd epic.)*
  - [ ] **v3.4.5** — **native search · no dependency · auto reindex/embed · model reindex ux/ui** (the qmd epic — [milestone 1](https://github.com/onebrain-ai/onebrain-cli/milestone/1); accumulates several PRs, tagged once all tracks land):
    - [x] relocate the search cache off OS-purgeable `~/Library/Caches` → persistent data dir (#114 → #129)
    - [x] no-model reindex UX — active only when downloaded, e5-small default, MCP no-index fallback signal (#130 → #134)
    - [x] remove **`@tobilu/qmd`** — 0 node/python deps: serve/WebUI search → native, drop `qmd embed`/probe (#131)
    - [ ] plugin cutover — `.mcp.json` key + `/qmd` → `/search` skill (onebrain-ai/onebrain#206, ADR 0019)
    - [x] auto reindex/embed hook — PostToolUse lex-now + Stop embed-deferred (#133 → #141)
  - [ ] **v3.4.6** — relevance polish: rerank (bge-reranker-v2-m3) · query expansion · nlpo3 Thai word-seg · custom ONNX models.
- [ ] **v3.5.x** — **"Desktop + Deeplinks"** (mini-epic): `onebrain desktop` native app + deeplinks + standalone webui file access (`link`/`token`/`desktop` verbs, vault_id, tickets) — the agent hands you a clickable, section-precise webui URL for any vault file; completely replaces Obsidian.
- [ ] **v3.6** — **WebUI Terminal sessions** (mini-epic): run `onebrain`/`claude`/`codex` in the WebUI from anywhere (Tailscale); persistent term-server survives daemon restart.
- [ ] **v3.7** — Bootstrap + native verbs *(was v3.5)*: startup / wrapup / daily / tasks → 1 call per ceremony (import content-verbs anchored ~v3.7.x).
- [ ] **v3.8** — Warm daemon + RPC *(was v3.6)*: kill cold process-start; keeps the native index + embed model hot (absorbs the old RPC-layer milestone).

### 📦 Phase 2 · bundles (v3.9–v3.12)
- [ ] Bundle CLI (`onebrain bundle install/list/info/lint/…`) · four first-party bundles (`dashboard` · `synthesis` · `research` · `scheduler`) · core skills slimmed 32 → 18 · `onebrain.run/bundles` portal.

### 🔭 Signal-driven (Tier 2/3)
- [ ] Broader harness support — Codex, Qwen, and other agentic harnesses beyond today's Claude Code + Gemini CLI.
- [ ] Tiered memory + behavior tracking · proactive surfacing · daemon background synthesis · Avatar Mesh (one agent identity across machines) · Telegram / MCP gateway · OneBrain Studio · OneBrain Sync (cross-machine continuity for vault + memory + live session — harness-independent, end-to-end encrypted; sync + storage only, no hosted agent runtime).

### 🏁 v4.0 · breaking
- [ ] Drop `vault.yml` dual-read (canonical `onebrain.yml` only) · retire the hidden v3.0 aliases.

## Development

```bash
rustup default stable
cargo install cargo-insta   # snapshot review

# Full check (matches CI)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Snapshot workflow (insta)
cargo test                  # tests fail on snapshot mismatch
cargo insta review          # interactive approve/reject
```

The output contract is pinned by `crates/onebrain-cli/tests/v31_envelope_snapshots.rs` (Envelope shape · insta), `tests/output_format_matrix.rs` (default / `--json` / `--json --pretty` / `--yaml`), `tests/user_flows.rs` (new-user / hook-consumer / error-recovery), and `tests/v31_integration.rs` (v3.0 alias migration).

PR conventions: feature branch → git worktree → 3-round parallel review (correctness / behavior / security) → squash-merge with `--delete-branch`. English-only repo. One version bump per PR. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full workflow.

## Related projects

- **[onebrain-ai/onebrain](https://github.com/onebrain-ai/onebrain)** — OneBrain plugin (slash commands, skills, agents, hooks). Pairs with this CLI.
- **[onebrain.run](https://onebrain.run)** — Marketing site, install one-liner, public roadmap.
- **OneBrain Sync** *(planned)* — Cross-machine continuity for your vault, memory, and live session — harness-independent, end-to-end encrypted. Sync + storage only, no hosted agent runtime. Idea stage (replaces the dropped OneBrain Cloud).

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup, build + test commands, PR conventions, and the security-issue reporting channel. New contributors are encouraged to start with issues tagged `good-first-issue`.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option — the permissive dual license used across OneBrain. Use it in open or closed source. Questions: [hello@onebrain.run](mailto:hello@onebrain.run).
