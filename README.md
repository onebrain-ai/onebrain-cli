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
  <strong>Local-first Rust CLI that powers the OneBrain personal AI OS for Obsidian.</strong><br>
  <sub>Vault scaffolding · plugin sync · scheduled skills · diagnostics · self-update — across Claude Code, Gemini CLI, Codex, and Qwen.</sub>
</p>

<p align="center">
  <a href="https://github.com/onebrain-ai/onebrain-cli/releases/latest"><img alt="onebrain-cli release" src="https://img.shields.io/github/v/release/onebrain-ai/onebrain-cli?include_prereleases&style=for-the-badge&logo=rust&color=cb3837&label=onebrain-cli"></a>
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

**`onebrain`** is the local-first Rust binary at the heart of [OneBrain](https://onebrain.run) — a personal AI operating system that lives in your Obsidian vault. It scaffolds new vaults, syncs the OneBrain plugin from GitHub, registers AI-harness hooks, runs scheduled skills via the OS scheduler, diagnoses vault health, and updates itself.

The CLI is **cross-harness** — paired with the [OneBrain plugin](https://github.com/onebrain-ai/onebrain) (slash commands, skills, agents), it works under Claude Code, Gemini CLI, Codex, and Qwen. Your vault and your agent identity stay on your machine.

### Why OneBrain CLI

- **Local-first** — Your vault, your data, your AI memory. No cloud round-trip required for any operation.
- **Cross-harness** — One CLI binary serves any agentic harness (Claude Code, Gemini CLI, Codex, Qwen) via the plugin contract.
- **Native Rust** — 4.6 MB stripped binary, ~2 MB private memory per invocation, < 50 ms cold start, single static executable on every platform.
- **Stable JSON contracts** — `doctor --json`, `update --plan`, `update --check --json` carry frozen schemas across v3.x for scripting and plugin integration.
- **Trustworthy install path** — Self-update fetches signed-at-TLS binaries directly from GitHub Releases. No npm/bun, no package-manager middleware.

## Status

**v3.0.0 — stable GA release** (released 2026-05-22). Complete Rust rewrite of the v2.3.3 TypeScript/Bun CLI: all 13 subcommands ported with Bun parity, 7-platform binary release pipeline, direct GitHub Release self-update path. See [CHANGELOG.md](CHANGELOG.md) for the full release narrative.

## Quickstart

Get from zero to a working OneBrain vault in three steps:

```bash
# 1. Download the binary for your platform from the latest GitHub Release
#    (full table below — pick the matching target triple)
curl -L -o onebrain.tar.gz \
  https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-aarch64-apple-darwin.tar.gz
tar xzf onebrain.tar.gz
sudo install onebrain /usr/local/bin/

# 2. Verify the install
onebrain --version
# → onebrain 3.0.0

# 3. Scaffold a new vault and let init pull the OneBrain plugin
mkdir my-vault && cd my-vault
onebrain init --yes
```

Then open the vault directory in your preferred AI harness (Claude Code, Gemini CLI, etc.) and run `/onboarding` to finish setup.

> `init` runs an embedded `vault-sync` step that downloads the OneBrain plugin tarball from GitHub. If the network step fails, the scaffold (vault.yml, PARA folders, Stop hook, schedule preset) is still intact and the binary prints a clear `onebrain vault-sync` retry hint. Pass `--no-sync` for offline / CI scaffolding.

## Install

### Pre-built binaries (recommended)

Pick the archive that matches your machine from the [latest release](https://github.com/onebrain-ai/onebrain-cli/releases/latest):

| Platform | Architecture | Archive |
|---|---|---|
| ![macOS](https://img.shields.io/badge/macOS-000000?logo=apple&logoColor=white&style=flat) | Apple Silicon (M1–M5) | `onebrain-aarch64-apple-darwin.tar.gz` |
| ![macOS](https://img.shields.io/badge/macOS-000000?logo=apple&logoColor=white&style=flat) | Intel | `onebrain-x86_64-apple-darwin.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | ARM64 (glibc) | `onebrain-aarch64-unknown-linux-gnu.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | x86_64 (glibc) | `onebrain-x86_64-unknown-linux-gnu.tar.gz` |
| ![Linux](https://img.shields.io/badge/Linux-FCC624?logo=linux&logoColor=black&style=flat) | x86_64 (musl / Alpine / static) | `onebrain-x86_64-unknown-linux-musl.tar.gz` |
| ![Windows](https://img.shields.io/badge/Windows-0078D4?logo=windows&logoColor=white&style=flat) | ARM64 | `onebrain-aarch64-pc-windows-msvc.zip` |
| ![Windows](https://img.shields.io/badge/Windows-0078D4?logo=windows&logoColor=white&style=flat) | x86_64 | `onebrain-x86_64-pc-windows-msvc.zip` |

Each archive ships with a `.sha256` next to it for manual verification. Filenames use canonical Rust target triples — `cargo-binstall onebrain-cli` and custom installer scripts pick them up unmodified.

### Self-update

After the initial install, refresh in place:

```bash
onebrain update                # prompt-and-confirm
onebrain update --check        # dry-run (compare current vs latest)
onebrain update --plan         # machine-readable JSON plan
```

The install path resolves the current target triple at runtime, downloads the matching GitHub Release tarball over HTTPS (rustls TLS), and atomically swaps the running binary (Unix single-rename; Windows rustup-style two-step with rollback on failure). No package-manager middleware.

### Other channels (planned)

- **npm wrapper** — `@onebrain-ai/cli@3.0.0` on npm. Targets v3.0.x.
- **Homebrew tap** — `onebrain-ai/homebrew-onebrain` for `brew install onebrain`. Targets v3.0.x.
- **`cargo-binstall`** — works today (canonical Rust target triples in the GitHub Release assets).

Neither npm nor Homebrew is published at GA — use `onebrain update` or download manually until those land.

### Security / trust model

`onebrain update` authenticates downloaded binaries solely via **GitHub's TLS chain** (rustls validation, no opt-out). At v3.0.0 GA the CLI does not verify a SHA-256 checksum or cosign signature against the downloaded binary itself; this matches the rustup / deno / bun baseline. Checksum verification is tracked for the v3.0.x security hardening cycle.

On networks running a corporate MITM proxy, the trust boundary becomes whatever certificate the proxy presents. If that matters to your threat model, verify the published `.sha256` files manually after each `onebrain update`.

Report security issues privately via the channel documented in [`CONTRIBUTING.md`](CONTRIBUTING.md).

### Build from source

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli
# → target/release/onebrain
```

Requires a recent stable Rust toolchain (`rustup default stable`). No `unsafe` blocks in OneBrain crates; the workspace builds cleanly on Linux, macOS, and Windows.

## Subcommands

`onebrain --help` lists all 13 subcommands. Grouped by purpose:

| Category | Subcommand | Purpose |
|---|---|---|
| **Setup** | `init` | Scaffold a new vault (`vault.yml`, PARA folders, hooks, schedule preset, plugin sync). |
| | `vault-sync` | Download the latest OneBrain plugin tarball from GitHub and overlay it on the vault. |
| | `register-hooks` | Wire `.claude/settings.json` (or `.gemini/`) Stop / PostToolUse hooks for the current harness. |
| | `register-schedule` | Compile `vault.yml schedule:` entries into OS scheduler artifacts (`launchd` on macOS). |
| **Runtime** | `session-init` | Resolve session token + qmd embedding status for the harness `SessionStart` hook. |
| | `checkpoint` | Emit a session checkpoint snapshot (called by the Stop hook on threshold). |
| | `harness` | Detect / report the current AI harness (claude / gemini / direct). |
| | `run-skill` | Invoke a OneBrain skill via headless Claude Code (used by scheduled skills). |
| **Maintenance** | `doctor` | 8 read-only health checks + 5 `--fix` recipes; `--json` for machine-readable output. |
| | `migrate` | Apply named idempotent vault migrations (e.g. `backfill-recapped`). |
| | `orphan-scan` | Count orphan checkpoint files (used by the harness `SessionStart` hook). |
| | `qmd-reindex` | Refresh the qmd BM25 index after vault content changes. |
| **Updates** | `update` | Self-update the binary from the latest GitHub Release; `--check`, `--plan` modes for scripting. |

## Performance

Measured against the v2.3.3 TypeScript/Bun CLI on the same hardware (Apple M1, macOS) running `onebrain doctor` warm:

| Metric | v2.3.3 (Bun) | v3.0.0 (Rust) | Δ |
|---|---|---|---|
| Stripped binary size | 57.8 MB | 4.6 MB | **−92%** |
| Private memory per invocation (peak) | ~21 MB | ~2 MB | **~10× less** |
| Cold start | ~120 ms | < 50 ms | **~2.5× faster** |
| Warm `doctor` wall time | ~980 ms | ~890 ms | ~9% faster |
| `update --check` (warm cache) | ~480 ms | ~10 ms | **~48× faster** |

Numbers from the v3.0.0-alpha cycle dogfood. Use `cargo build --release` profile settings (LTO thin, `strip = "symbols"`, `codegen-units = 1`, `panic = "abort"`) to reproduce binary size.

## Architecture

Four-crate Cargo workspace:

```
onebrain-cli         Binary crate — clap dispatch over 13 subcommands
  │
  ├─ onebrain-fs     Vault walks · frontmatter parsing · plugin tarball overlay
  │                  · init bootstrap · doctor checks · update install path
  │
  ├─ onebrain-cache  Session token resolution · launchd plist generation
  │                  · qmd status detection
  │
  └─ onebrain-core   Types · config parsing · path resolution (zero filesystem deps)
```

Workspace inheritance keeps `[workspace.package]` fields (`version`, `edition`, `license`, `repository`) in one place. The workspace root sets `publish = false`; all four crates inherit it via `publish.workspace = true` — only the compiled binary ships.

Test pyramid (4 layers): inline unit + `assert_cmd` integration + `insta` snapshots + Layer 4 golden-master parity vs Bun v2.3.3. 670 tests passing at GA, gating CI on fmt + clippy `-D warnings` + a 3-platform test matrix (Ubuntu, macOS, Windows).

## Development

```bash
rustup default stable
cargo install cargo-insta   # snapshot review

# Full check (matches CI)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Layer 4 parity vs Bun v2.3.3 (needs the Bun artifact)
export BUN_BINARY=/path/to/onebrain-v2.3.3
cargo test -p parity --release

# Snapshot workflow (insta)
cargo test                  # tests fail on snapshot mismatch
cargo insta review          # interactive approve/reject
```

PR conventions: feature branch → git worktree → 3-round parallel review (correctness / behavior / security) → squash-merge with `--delete-branch`. English-only repo (no Thai or other non-English text in committed files). One version bump per PR. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full workflow.

## Related projects

- **[onebrain-ai/onebrain](https://github.com/onebrain-ai/onebrain)** — OneBrain plugin (slash commands, skills, agents, hooks). Pairs with this CLI.
- **[onebrain.run](https://onebrain.run)** — Marketing site, install one-liner, public roadmap.
- **[OneBrain Cloud](https://onebrain.run) (waitlist)** — Hosted agent runtime + multi-device sync. Planning phase.

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup, build + test commands, PR conventions, and the security-issue reporting channel. New contributors are encouraged to start with issues tagged `good-first-issue`.

## License

[AGPL-3.0-only](LICENSE). If you make a modified version of OneBrain CLI available to users over a network (AGPL §13 — including SaaS, internal APIs, and any networked interaction), you must release your modifications under the same license. For commercial licensing inquiries, contact [hello@onebrain.run](mailto:hello@onebrain.run).
