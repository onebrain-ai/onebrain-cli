<!-- Banner pinned to a plugin-repo commit SHA (not the mutable `main` branch)
     so a future asset restructure on onebrain-ai/onebrain can't silently
     404 this image. Bump the SHA when refreshing the brand assets. -->
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/onebrain-ai/onebrain/200f113b27d3354f4a274c5d8aed1ba3b7c689cb/assets/header-dark.png">
    <img alt="OneBrain — Your AI Thinking Partner" src="https://raw.githubusercontent.com/onebrain-ai/onebrain/200f113b27d3354f4a274c5d8aed1ba3b7c689cb/assets/header-light.png" width="640">
  </picture>
</p>

<p align="center"><em>Your AI Thinking Partner</em></p>

<p align="center">
  <strong>The local-first Rust CLI that powers the OneBrain personal AI OS for Obsidian.</strong><br>
  <sub>Vault scaffolding · plugin sync · scheduled skills · diagnostics · self-update — across Claude Code and Gemini CLI.</sub>
</p>

<p align="center">
  <a href="https://github.com/onebrain-ai/onebrain-cli/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/onebrain-ai/onebrain-cli?include_prereleases&style=for-the-badge&logo=rust&color=cb3837&label=release"></a>
  <a href="https://www.npmjs.com/package/@onebrain-ai/cli"><img alt="npm" src="https://img.shields.io/npm/v/@onebrain-ai/cli?style=for-the-badge&logo=npm&color=cb3837&label=npm"></a>
  <a href="https://github.com/onebrain-ai/onebrain-cli/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/onebrain-ai/onebrain-cli/ci.yml?branch=main&style=for-the-badge&logo=githubactions&logoColor=white&label=CI"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-AGPL--3.0-7c3aed?style=for-the-badge"></a>
</p>
<p align="center">
  <a href="https://onebrain.run"><img alt="Website" src="https://img.shields.io/badge/onebrain.run-0a0a14?style=for-the-badge&labelColor=ff2d92"></a>
  <a href="https://x.com/onebrain_run"><img alt="@onebrain_run on X" src="https://img.shields.io/badge/follow-@onebrain__run-000000?style=for-the-badge&logo=x&logoColor=white"></a>
  <a href="https://github.com/onebrain-ai/onebrain-cli/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/onebrain-ai/onebrain-cli?style=for-the-badge&color=00f3ff&logo=github"></a>
</p>

---

## What is OneBrain CLI?

**`onebrain`** is the local-first Rust binary at the heart of [OneBrain](https://onebrain.run) — a personal AI operating system that lives in your Obsidian vault. It scaffolds new vaults, syncs the OneBrain plugin from GitHub, wires AI-harness hooks, runs scheduled skills through the OS scheduler, diagnoses vault health, and updates itself.

The CLI is **cross-harness**: paired with the [OneBrain plugin](https://github.com/onebrain-ai/onebrain) (slash commands, skills, agents), it runs under Claude Code and Gemini CLI against the same vault contract.

### Why OneBrain CLI

Point an AI agent at a vault and it improvises — a different pile of `grep` / `ls` / `find` / `sed` each time, behaving differently on each harness and re-derived every session: slow, token-hungry, non-portable, sometimes wrong. **OneBrain CLI replaces that improvisation with one deterministic binary.**

- **Same behavior on every harness & model** — Claude Code and Gemini CLI both run `onebrain <noun> <verb>` and get identical output; switch harness without re-testing how your vault gets touched.
- **Yours to extend, no waiting** — add a capability the harness/LLM doesn't have yet and every agent can use it immediately; they only learn the command, not implement the feature.
- **No re-deriving solved workflows** — search, capture, consolidate, checkpoint live in the binary, so the agent calls one command instead of re-reasoning the recipe each session. Fewer tokens, no drift.
- **Deterministic & safe** — a typed command with a frozen `Envelope` can't half-finish or quietly differ like an ad-hoc `rm` / `sed` pipeline. Same input → same output, scriptable by hooks.
- **Fast** — a ~5 MB binary returns in under 50 ms, skipping the latency of several tool calls for what's already one operation.
- **Local-first** — your vault, your data, your AI memory; no cloud round-trip.
- **Trustworthy install** — self-update verifies the binary's SHA-256 before swapping.

## Status

**v3.2.0 — stable & production-ready, in active maintenance.** GA since v3.0.0 (2026-05-22), shipping ~weekly themed minors. Version history + direction in the [Roadmap](#roadmap); full detail in [CHANGELOG.md](CHANGELOG.md).

## Quickstart

From zero to a working OneBrain vault in three steps.

```bash
# 1. Install (macOS — Homebrew is the canonical channel)
brew install onebrain-ai/onebrain/onebrain

# 2. Verify
onebrain --version
# → onebrain 3.1.5

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

The install path resolves the current target triple at runtime, downloads the matching GitHub Release tarball over HTTPS (rustls TLS), and atomically swaps the running binary (Unix single-rename; Windows rustup-style two-step with rollback on failure). No package-manager middleware.

> **Homebrew users:** since v3.1.4, `onebrain update` auto-detects a brew-managed install (binary under the Cellar) and delegates to `brew upgrade onebrain`, so it stays in sync with brew's metadata — no manual step needed.

### Build from source

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli
# → target/release/onebrain
```

Requires a recent stable Rust toolchain (`rustup default stable`). The only `unsafe` in OneBrain crates is a single `libc::getuid()` call (the launchd plist UID); the workspace otherwise builds cleanly on Linux, macOS, and Windows.

## Command surface

v3.1 locks a singular-noun, two-level grammar — `onebrain <noun> <verb>` — so every command path is predictable. Three root verbs handle the common flow; eight resource groups cluster the rest.

```text
onebrain
├── init                       create / re-scaffold a vault (--yes · --force · --no-sync)
├── update                     self-update the binary (--check · --plan)
├── doctor [--fix]             9 health checks + auto-repair recipes
│
├── vault       sync · current
├── session     init
├── checkpoint  stop · reset · orphans
├── qmd         reindex · embed · status
├── plugin      install · update · migrate
├── schedule    register
├── skill       run
└── harness     detect
```

| Group | Verbs | Purpose |
|---|---|---|
| **Setup** | `init`, `plugin install`, `vault sync` | Scaffold `onebrain.yml` + PARA folders, register the plugin with the harness, overlay the latest plugin tarball. |
| **Runtime** (hook protocol) | `session init`, `checkpoint stop · reset · orphans`, `qmd reindex` | Called by the harness `SessionStart` / `Stop` / `PostToolUse` hooks. Emit hard-wired JSON; banner suppressed for clean machine stdio. |
| **Search** | `qmd reindex · embed · status` | Rebuild the qmd index, re-embed documents, report index + embedding health. |
| **Maintenance** | `doctor [--fix]`, `plugin update · migrate`, `schedule register` | Nine read-only checks + `--fix` recipes, self-update the binary + rewrite hooks + rebind launchd plists, compile the `onebrain.yml schedule:` block into OS scheduler artifacts. |
| **Diagnostics** | `vault current`, `harness detect` | Report which mechanism resolved the active vault, and which AI harness is running. |

> The tree shape is **locked for v3.2+** — 200+ verbs beyond the working set above are stubbed with a stable `E_NOT_IMPLEMENTED` (exit 72) so the grammar can't drift while features land. Hidden v3.0 flat aliases (`session-init`, `qmd-reindex`, `register-hooks`, …) still dispatch, printing a one-time migration notice (silence with `ONEBRAIN_QUIET_MIGRATION=1`); they're removed no earlier than v4.

## Output modes

Interactive commands default to human-readable `text`; pass a flag for structured output. Every structured payload is wrapped in the canonical `Envelope<T>`:

```bash
onebrain doctor                 # TTY: animated per-check report, colorized
onebrain doctor --json          # { version, command, ok, vault, data, warnings, error }
onebrain vault current --yaml   # same envelope, YAML
onebrain qmd status --json | jq .data
```

- `--output {text,json,yaml,table,tsv}` — full matrix on every command; `--json` / `--yaml` are shorthands.
- `--pretty` forces indented JSON even when stdout is piped; `--no-color` (or `NO_COLOR`) forces monochrome; `-q` drops info logs (errors still hit stderr).
- Output auto-adapts: piped/CI invocations drop color and the startup banner, so machine consumers get clean bytes with no flags. Closed-pipe writes (`onebrain qmd reindex | head`) exit `0`, not a panic.

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

Figures from the v3.0.0 rewrite-milestone dogfood; the binary has since grown to ~5 MB as v3.1 added the branded banner and `qmd status`/`embed`. Reproduce the size with the release profile (`lto = "thin"`, `strip = "symbols"`, `codegen-units = 1`, `panic = "abort"`).

## Architecture

Four-crate Cargo workspace:

```text
onebrain-cli         Binary crate — clap dispatch over the v3.1 command tree
  │
  ├─ onebrain-fs     Vault walks · frontmatter parsing · plugin tarball overlay
  │                  · init bootstrap · doctor checks · update install path · backups
  │
  ├─ onebrain-cache  Session token resolution · launchd plist generation
  │                  · qmd status detection
  │
  └─ onebrain-core   Types · config parsing · path resolution (zero filesystem deps)
```

Workspace inheritance keeps `[workspace.package]` fields (`version`, `edition`, `license`, `repository`) in one place. The root sets `publish = false`; all four crates inherit it via `publish.workspace = true` — only the compiled binary ships.

Test pyramid (3 layers since v3.1.0): inline unit + `assert_cmd` integration + `insta` snapshots, 900+ tests passing. CI gates on `fmt` + `clippy -D warnings` + a 3-platform matrix (Ubuntu, macOS, Windows). The v2.x Bun golden-master parity layer was retired in v3.1.0; the v3.1 `Envelope` shape and the output-format matrix now own the canonical-contract role.

> **Design notes & Rust patterns** → [`docs/`](docs/): an architecture deep-dive, decision records (ADRs), and a guided tour of the idiomatic Rust this codebase uses — written for contributors, people studying the source, and Rust learners.

## Roadmap

> Directional — themes are committed, timing flexes with the weekly-minor cadence (≈ one themed minor per week). The live public roadmap is at [onebrain.run](https://onebrain.run).

### ✅ Shipped
- [x] **v3.0** — Rust rewrite GA · 9-platform release pipeline · stable JSON contracts.
- [x] **v3.1.0** — Consistency standard: locked `<noun> <verb>` command tree · canonical `Envelope` output · branded banner · `vault.yml → onebrain.yml`.
- [x] **v3.1.1** — Config-loss fix (`init --force` no longer clobbers config) + timestamped config backups · doctor `onebrain.yml` labels + animated TTY · `qmd status`.
- [x] **v3.1.2** — `onebrain qmd embed` implemented.
- [x] **v3.1.3** — `schedule register` dual-reads `onebrain.yml`.
- [x] **v3.1.4** — Self-update hardening: SHA-256 verification of the downloaded binary before swap + Homebrew-aware `onebrain update` (delegates to `brew upgrade`). Cosign/signature verification remains a follow-up.
- [x] **v3.1.5** — `onebrain update` validation fix: accepts clap's bare `--version` output (was a false "Binary validation failed" on every upgrade) and now confirms the on-PATH binary reports the just-installed version (catches no-op `brew upgrade` / PATH shadowing) with specific failure messages.

### 🚧 Phase 1 · perceptual speed + skill alignment (v3.2–v3.7)
- [x] **v3.2.0** — `note` resource group (11 verbs: `search` · `list` · `find` · `read` · `stat` · `backlinks` · `orphans` · `append` · `new` · `archive` · `move`) — native vault note ops replacing ad-hoc `grep` / `ls` / `find` / `cat`; canonical `Envelope`, vault-required, transactional `move` with vault-wide wikilink rewrite.
- [x] **v3.2.1** — `doctor` grouped UX: 9 checks bucketed into 4 sections under a `🧠 OneBrain Doctor` header via a reusable braille-spinner progress primitive + verdict footer · qmd-hook detector recognizes both `qmd reindex` and the legacy `qmd-reindex` form (`--fix` migrates + dedups) · banner logo gradient.
- [x] **v3.2.2** — Animated `onebrain update` (framed header + braille spinner on fetch/install, the version check padded to a visible beat) · banner vertical gradient + vertical-only mono fallback · `doctor` spinner now rotates and paces per check. Shared header/spinner/pacing primitives across `doctor` + `update`.
- [x] **v3.2.3** — `skill run` walk-up from cwd (runs inside a vault without an explicit path) + null-stdin headless hardening · global `--vault` now accepted on every command (`skill run` / `schedule register` / `plugin migrate` had a `--vault-dir` field-name collision suppressing it) · `doctor` stamps `stats.last_doctor_run` (+ `last_doctor_fix` on `--fix`).
- [ ] **v3.3** — Daemon foundation: `onebrain daemon start/stop/status` + structured logging.
- [ ] **v3.4** — RPC layer: stdio JSON-RPC 2.0 over a Unix socket with auto-spawn.
- [ ] **v3.5** — Skill-speed rewrites (`/daily`, `/wrapup`) + `checkpoint recover`.
- [ ] **v3.6** — Capture pipeline (`/capture`, `/bookmark`, `/braindump`).
- [ ] **v3.7** — Inbox + tasks pipeline + `/consolidate`.

### 📦 Phase 2 · bundles (v3.8–v3.11)
- [ ] Bundle CLI (`onebrain bundle install/list/info/lint/…`) · four first-party bundles (`dashboard` · `synthesis` · `research` · `scheduler`) · core skills slimmed 32 → 18 · `onebrain.run/bundles` portal.

### 🔭 Signal-driven (Tier 2/3)
- [ ] Broader harness support — Codex, Qwen, and other agentic harnesses beyond today's Claude Code + Gemini CLI.
- [ ] Tiered memory + behavior tracking · proactive surfacing · daemon background synthesis · Avatar Mesh (one agent identity across machines) · Telegram / MCP gateway · OneBrain Studio + [OneBrain Cloud](https://onebrain.run) federation.

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
- **[OneBrain Cloud](https://onebrain.run) (waitlist)** — Hosted agent runtime + multi-device sync. Planning phase.

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup, build + test commands, PR conventions, and the security-issue reporting channel. New contributors are encouraged to start with issues tagged `good-first-issue`.

## License

[AGPL-3.0-only](LICENSE). If you make a modified version of OneBrain CLI available to users over a network (AGPL §13 — including SaaS, internal APIs, and any networked interaction), you must release your modifications under the same license. For commercial licensing inquiries, contact [hello@onebrain.run](mailto:hello@onebrain.run).
