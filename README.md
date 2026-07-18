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

Point an AI agent at a vault and it improvises — a different pile of `grep` / `find` / `sed` each time, re-derived every session. OneBrain CLI replaces that improvisation with **one deterministic binary**: same command, same typed result, on every harness and every OS, in under 50 ms. Full rationale: [Why OneBrain CLI](docs/why.md).

**Status:** v3.4.x — stable & production-ready, in active maintenance. GA since v3.0.0 (2026-05-22), shipping ~weekly themed minors. Direction in the [Roadmap](#roadmap); per-release detail in [CHANGELOG.md](CHANGELOG.md).

## Install

```bash
brew install onebrain-ai/onebrain/onebrain    # macOS — Homebrew is the canonical channel
```

On Linux/Windows: `npm install -g @onebrain-ai/cli`, or grab a pre-built binary for any of 9 targets (macOS · Linux incl. Raspberry Pi · Windows) from the [latest release](https://github.com/onebrain-ai/onebrain-cli/releases/latest). After installing once, `onebrain update` refreshes in place, SHA-256-verified.

→ **[Full install guide](docs/install.md)** — platform binary table, channels, self-update behavior, build from source, and the [security & trust model](docs/install.md#security--trust-model).

## Quickstart

From zero to a working OneBrain vault in three steps:

```bash
# 1. Verify the install
onebrain --version
# → onebrain 3.4.14

# 2. Scaffold a vault and let init pull the OneBrain plugin
mkdir my-vault && cd my-vault
onebrain init --yes

# 3. Open the vault in your AI harness (Claude Code, Gemini CLI, …)
#    and run /onboarding to finish setup.
```

> `init` runs an embedded `vault sync` step that downloads the plugin tarball; if the network step fails, the scaffold (`onebrain.yml`, PARA folders, Stop hook, schedule preset) stays intact and the binary prints an `onebrain vault sync` retry hint. Pass `--no-sync` for offline / CI scaffolding.

## Features

- **🔍 Built-in native search** — hybrid keyword + semantic search over your vault's Markdown, multilingual (Thai/CJK-aware keyword bigrams + ~100-language embeddings), zero external dependencies: no Node, no Python, no separate search binary to install. Semantic availability per platform: [support matrix](docs/platform-support.md).
- **🔌 Built-in MCP server** — `onebrain mcp` plugs OneBrain into Claude Code, Cursor, or any MCP client as a vault search engine — works standalone on any Markdown folder too. See [`docs/reference/mcp.md`](docs/reference/mcp.md).
- **📉 Token optimization** — uses less of your context than anything else, measured: a 4-rung level ladder (lossless by default), an already-sent ledger that turns a repeat doc read into a small reference receipt instead of resending the full body, and `onebrain token gain` reporting exactly what was saved. See [`docs/token-optimization.md`](docs/token-optimization.md).
- **Single static binary** — one self-contained file, no runtime to install, cross-platform down to a Raspberry Pi Zero.
- **Embedded web UI** — `onebrain serve` hosts a local, token-gated file explorer + reading view + search panel + agent chat, with nothing extra to install. See [`docs/serve.md`](docs/serve.md).
- **Interactive model picker** — a TUI over 6 embedding models (see [Choosing an embedding model](docs/reference/onebrain-search.md#choosing-an-embedding-model)) to trade off speed, accuracy, and Thai support.
- **Vault doctor + config migration** — twelve health checks with `--fix` recipes (out-of-range config values reset to their documented defaults, comments preserved), plus automatic `onebrain.yml` schema migration.
- **Local scheduler** — cron-style recurring skills (daily briefing, weekly review, reindex) via the OS scheduler, no cloud runner needed.

## Commands

Five root verbs cover the common flow; eleven `<noun> <verb>` groups cluster the rest:

```text
onebrain init | update | doctor | serve | mcp                # root verbs
onebrain <noun> <verb>                                       # vault · session · checkpoint · search
                                                             # · note · task · plugin · schedule · token
                                                             # · skill · harness
```

Interactive commands print human-readable text; every command also speaks structured `--json` / `--yaml` wrapped in the canonical `Envelope`.

→ **[Full command reference](docs/commands.md)** — the complete tree, every verb group, and the output-mode matrix.

## Documentation

| Guide | What's inside |
|---|---|
| [Install](docs/install.md) | Channels, platform binaries, self-update, build from source, security & trust model. |
| [Commands](docs/commands.md) | Full `<noun> <verb>` tree, verb groups, output modes. |
| [Token optimization](docs/token-optimization.md) | Level ladder, cache, gain reporting, read hook. |
| [Platform support](docs/platform-support.md) | Semantic vs keyword-only search per release target. |
| [Local web UI](docs/serve.md) | `onebrain serve` — embedded web UI + vault JSON API. |
| [MCP reference](docs/reference/mcp.md) | `onebrain mcp` tools, client registration, standalone vault-search use. |
| [Why OneBrain CLI](docs/why.md) | The case for one deterministic vault binary. |
| [Architecture](docs/architecture.md) | Five-crate workspace, command flow, testing, performance benchmarks. |
| [Search internals](docs/architecture/search.md) | Index storage, reindex/embed pipeline, every search mode end-to-end. |
| [Design docs & ADRs](docs/) | Decision records, Rust patterns, crate-by-crate source reference. |

## Roadmap

> Directional — themes are committed, timing flexes with the weekly-minor cadence (≈ one themed minor per week). The live public roadmap is at [onebrain.run](https://onebrain.run).

> Major/minor only — see [CHANGELOG](CHANGELOG.md) for per-patch detail.

### ✅ Foundation (v3.0–v3.4) · the system base everything else builds on
- [x] **v3.0** — Rust rewrite GA · 9-platform release pipeline · stable JSON contracts.
- [x] **v3.1** — Consistency standard: locked `<noun> <verb>` command tree · canonical `Envelope` output · `vault.yml → onebrain.yml`.
- [x] **v3.2** — `note` resource group · grouped `doctor` UX · `skill run`/`harness run` for headless skills.
- [x] **v3.3** — Daemon foundation: `onebrain serve` — embedded web UI over a token-gated vault JSON API.
- [x] **v3.4** — **Native Search + Warm Daemon** *(complete, v3.4.0–v3.4.13)*: native Rust search replaces qmd (native engine + `onebrain search` verbs · native MCP server · 0 node/python deps, v3.4.5), a **warm daemon** that owns the engine so mcp + CLI coexist across live sessions (v3.4.6) — per-vault daemon slots since v3.4.13, a Tier-2 cross-encoder reranker (`onebrain-rerank-v1`) for calibrated result relevance (v3.4.7), self-documenting `onebrain.yml` + doctor validation (v3.4.8), and the **token-optimization layer** — transform ladder, already-sent ledger with a production-gating read-hook, and `onebrain token gain` telemetry (v3.4.10–v3.4.13) ([milestone](https://github.com/onebrain-ai/onebrain-cli/milestone/4)).

### 🖥️ Phase 1 · Product surfaces — replace Obsidian (v3.5–v3.6)
- [ ] **v3.5** — **"Desktop + Deeplinks"**: `onebrain desktop` native app + deeplinks + standalone webui file access (`link`/`token`/`desktop` verbs, vault_id, tickets) — the agent hands you a clickable, section-precise webui URL for any vault file; completely replaces Obsidian.
- [ ] **v3.6** — **Terminal sessions** (mini-epic): run `onebrain`/`claude`/`codex` from a persistent term-server (survives daemon restart · reachable over Tailscale) — in **both** the WebUI and the v3.5 desktop shell. *Builds on v3.5 desktop.*

### ⚡ Phase 2 · Speed & skills (v3.7–v3.8)
- [ ] **v3.7** — **Bootstrap + native verbs + skill optimization**: startup / wrapup / daily / tasks → 1 call per ceremony; native settings-merge + vault migrations in `plugin update`; skill-body optimization pass (import content-verbs anchored ~v3.7.x).
- [ ] **v3.8** *(may not ship)* — **remaining cleanup**: full daemon refactor of surfaces beyond mcp + search + daily-brief precompute — only if not already absorbed by v3.4 + v3.7.

### 📦 Phase 3 · bundles (v3.9–v3.12)
- [ ] Bundle CLI (`onebrain bundle install/list/info/lint/…`) · four first-party bundles (`dashboard` · `synthesis` · `research` · `scheduler`) · core skills slimmed 32 → 18 · `onebrain.run/bundles` portal.

### 🔭 Signal-driven (Tier 2/3)
- [ ] Broader harness support — Codex, Qwen, and other agentic harnesses beyond today's Claude Code + Gemini CLI.
- [ ] Tiered memory + behavior tracking · proactive surfacing · daemon background synthesis · Avatar Mesh (one agent identity across machines) · Telegram / MCP gateway · OneBrain Studio · OneBrain Sync (cross-machine continuity for vault + memory + live session — harness-independent, end-to-end encrypted; sync + storage only, no hosted agent runtime).

### 🏁 v4.0 · breaking
- [ ] Drop `vault.yml` dual-read (canonical `onebrain.yml` only) · retire the hidden v3.0 aliases.

## Related projects

- **[onebrain-ai/onebrain](https://github.com/onebrain-ai/onebrain)** — OneBrain plugin (slash commands, skills, agents, hooks). Pairs with this CLI.
- **[onebrain.run](https://onebrain.run)** — Marketing site, install one-liner, public roadmap.
- **OneBrain Sync** *(planned)* — Cross-machine continuity for your vault, memory, and live session — harness-independent, end-to-end encrypted. Sync + storage only, no hosted agent runtime. Idea stage (replaces the dropped OneBrain Cloud).

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for dev setup, build + test commands, PR conventions, and the security-issue reporting channel. New contributors are encouraged to start with issues tagged `good-first-issue`; design notes and Rust patterns for orientation live in [`docs/`](docs/).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option — the permissive dual license used across OneBrain. Use it in open or closed source. Questions: [hello@onebrain.run](mailto:hello@onebrain.run).
