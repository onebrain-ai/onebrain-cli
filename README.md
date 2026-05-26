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
  <sub>Vault scaffolding · plugin sync · scheduled skills · diagnostics · self-update — across Claude Code, Gemini CLI, Codex, and Qwen.</sub>
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

```text
░█▀█░█▀█░█▀▀░█▀▄░█▀▄░█▀█░▀█▀░█▀█      One in gray, Brain in OneBrain pink (#ff2d92) —
░█░█░█░█░█▀▀░█▀▄░█▀▄░█▀█░░█░░█░█      the wordmark that greets every interactive session.
 ▀▀▀ ▀ ▀ ▀▀▀ ▀▀  ▀ ▀ ▀ ▀ ▀▀▀ ▀ ▀
Your AI Thinking Partner · v3.1.2
```

`onebrain` is a single ~5 MB static binary. No runtime, no package-manager middleware, no cloud round-trip — your vault, your agent's memory, and your AI identity stay on your machine.

## What is OneBrain CLI?

**`onebrain`** is the local-first Rust binary at the heart of [OneBrain](https://onebrain.run) — a personal AI operating system that lives in your Obsidian vault. It scaffolds new vaults, syncs the OneBrain plugin from GitHub, wires AI-harness hooks, runs scheduled skills through the OS scheduler, diagnoses vault health, and updates itself.

The CLI is **cross-harness**: paired with the [OneBrain plugin](https://github.com/onebrain-ai/onebrain) (slash commands, skills, agents), it runs under Claude Code, Gemini CLI, Codex, and Qwen against the same vault contract.

### Why OneBrain CLI

- **Local-first** — Your vault, your data, your AI memory. No cloud round-trip required for any operation.
- **Cross-harness** — One binary serves any agentic harness (Claude Code, Gemini CLI, Codex, Qwen) through a single plugin contract.
- **Native Rust** — ~5 MB stripped static binary, ~2 MB private memory per call, sub-50 ms cold start, zero `unsafe` in OneBrain crates.
- **Stable contracts** — Every command speaks a canonical `Envelope<T>` JSON/YAML shape with a frozen schema across v3.x, so hooks and scripts never guess at output.
- **Trustworthy install** — Self-update fetches binaries straight from GitHub Releases over TLS; the npm wrapper verifies a published SHA-256 before extracting.

## Status

**v3.1.2 — stable, in active maintenance** (released 2026-05-26). v3.1 locked the command tree into a singular-noun two-level grammar (`onebrain <noun> <verb>`), made human-readable `text` the default output with `--json` / `--yaml` opt-in, renamed the vault config `vault.yml → onebrain.yml` (dual-read for back-compat), and grew the release matrix to **9 platforms** (every Raspberry Pi from Pi 1 to Pi 5 now has a published binary). The full narrative lives in [CHANGELOG.md](CHANGELOG.md).

## Quickstart

From zero to a working OneBrain vault in three steps.

```bash
# 1. Install (macOS — Homebrew is the canonical channel)
brew install onebrain-ai/onebrain/onebrain

# 2. Verify
onebrain --version
# → onebrain 3.1.2

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
| ![macOS](https://img.shields.io/badge/macOS-000000?logo=apple&logoColor=white&style=flat) | Apple Silicon (M1–M5) | `onebrain-aarch64-apple-darwin.tar.gz` |
| ![macOS](https://img.shields.io/badge/macOS-000000?logo=apple&logoColor=white&style=flat) | Intel | `onebrain-x86_64-apple-darwin.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | ARM64 (glibc · Pi 3/4/5 64-bit OS · Pi Zero 2 W) | `onebrain-aarch64-unknown-linux-gnu.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | ARMv7 32-bit (Pi 2 v1.1+ · Pi 3/4/5 32-bit OS) | `onebrain-armv7-unknown-linux-gnueabihf.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | ARMv6 32-bit (Pi 1 · Pi Zero · Pi Zero W) | `onebrain-arm-unknown-linux-gnueabihf.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | x86_64 (glibc) | `onebrain-x86_64-unknown-linux-gnu.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | x86_64 (musl / Alpine / static) | `onebrain-x86_64-unknown-linux-musl.tar.gz` |
| ![Windows](https://img.shields.io/badge/Windows-0078D4?logo=windows&logoColor=white&style=flat) | ARM64 | `onebrain-aarch64-pc-windows-msvc.zip` |
| ![Windows](https://img.shields.io/badge/Windows-0078D4?logo=windows&logoColor=white&style=flat) | x86_64 | `onebrain-x86_64-pc-windows-msvc.zip` |

Each archive ships with a matching `.sha256` for manual verification. Filenames use canonical Rust target triples, so `cargo-binstall` and custom installer scripts pick them up unmodified.

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
| **cargo-binstall** | `cargo binstall onebrain-cli` | Resolves the canonical target triple from the GitHub Release assets. |
| **Direct download** | table above | Pick your triple, drop the binary on `PATH`. |

All channels resolve to the same per-platform binary published in the matching GitHub Release.

### Self-update

After the initial install, refresh in place:

```bash
onebrain update                # prompt-and-confirm
onebrain update --check        # dry-run (compare current vs latest)
onebrain update --plan         # machine-readable JSON plan
```

The install path resolves the current target triple at runtime, downloads the matching GitHub Release tarball over HTTPS (rustls TLS), and atomically swaps the running binary (Unix single-rename; Windows rustup-style two-step with rollback on failure). No package-manager middleware.

> **Homebrew users:** prefer `brew upgrade onebrain` over `onebrain update` — `update` installs through npm and swaps the binary in place, which diverges from the brew-managed symlink. (Brew-aware delegation is on the [roadmap](#roadmap).)

### Build from source

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli
# → target/release/onebrain
```

Requires a recent stable Rust toolchain (`rustup default stable`). No `unsafe` blocks in OneBrain crates; the workspace builds cleanly on Linux, macOS, and Windows.

## Command surface

v3.1 locks a singular-noun, two-level grammar — `onebrain <noun> <verb>` — so every command path is predictable. Three root verbs handle the common flow; ten resource groups cluster the rest.

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

`onebrain update` authenticates downloaded binaries via **GitHub's TLS chain** (rustls validation, no opt-out). The CLI does not yet verify a SHA-256 checksum or cosign signature against the self-updated binary itself — this matches the rustup / deno / bun baseline. (The **npm wrapper** *does* verify the published `.sha256` before extracting.) Self-update checksum verification is tracked on the [roadmap](#roadmap).

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

## Roadmap

> Directional, drawn from the CHANGELOG and known follow-ups — not a delivery commitment. The public roadmap lives at [onebrain.run](https://onebrain.run).

- **Now (v3.1.x)** — Locked command tree · canonical `Envelope` output · `onebrain.yml` config with timestamped backups · `qmd status`/`embed` · 9-platform release matrix.
- **Next (v3.2+)** — Fill in stubbed verbs (`E_NOT_IMPLEMENTED` → real) under the locked grammar · SHA-256 / signature verification on the self-update binary · Homebrew-aware `onebrain update` (delegate to `brew upgrade` instead of swapping the brew binary in place).
- **Later (v4.0)** — Drop `vault.yml` dual-read (canonical `onebrain.yml` only) · retire the hidden v3.0 flat aliases.
- **Beyond** — [OneBrain Cloud](https://onebrain.run): hosted agent runtime + multi-device vault sync (planning phase, waitlist open).

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
