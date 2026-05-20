# OneBrain CLI

Personal AI OS CLI for Obsidian — Rust rewrite of v2.3.3 (TypeScript/Bun).

## Status

**v3.0.0-alpha.3** — all 13 Bun-parity slices shipped. Feature-complete vs v2.3.3; pre-GA hardening + npm wrapper + Homebrew tap in progress. See [latest release](https://github.com/onebrain-ai/onebrain-cli/releases/latest) for downloads.

## Install

Pick the binary that matches your machine, untar/unzip, and put `onebrain` on your PATH:

| Platform | Architecture | File |
|---|---|---|
| 🍎 macOS | Apple Silicon (M1/M2/M3/M4) | `onebrain-aarch64-apple-darwin.tar.gz` |
| 🍎 macOS | Intel | `onebrain-x86_64-apple-darwin.tar.gz` |
| 🐧 Linux (glibc) | ARM64 | `onebrain-aarch64-unknown-linux-gnu.tar.gz` |
| 🐧 Linux (glibc) | x86_64 | `onebrain-x86_64-unknown-linux-gnu.tar.gz` |
| 🐧 Linux (musl / Alpine / static) | x86_64 | `onebrain-x86_64-unknown-linux-musl.tar.gz` |
| 🪟 Windows | ARM64 | `onebrain-aarch64-pc-windows-msvc.zip` |
| 🪟 Windows | x86_64 | `onebrain-x86_64-pc-windows-msvc.zip` |

Filenames use canonical Rust target triples for tooling compatibility (`cargo-binstall`, custom installer scripts). Every release also publishes a `.sha256` next to each archive.

npm wrapper (`@onebrain-ai/cli`) and Homebrew tap (`onebrain-ai/onebrain`) ship alongside the v3.0.0 GA release.

### Build from source

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli   # builds the onebrain-cli crate
# → target/release/onebrain                (binary name set via [[bin]] in crates/onebrain-cli/Cargo.toml)
```

## Quickstart — bootstrap a new vault

```bash
mkdir my-vault && cd my-vault
onebrain init --yes                    # scaffold + auto vault-sync (downloads plugin tarball)
# Open the directory in Claude Code, then run /onboarding to finish setup.
```

`init` runs the embedded `vault-sync` step automatically; if it fails (no network, GitHub down), the scaffold (vault.yml, PARA folders, Stop hook, schedule preset) is still intact and the binary prints a clear "re-run `onebrain vault-sync`" hint. Pass `--no-sync` to skip the network step for offline / CI scaffolding.

## Development

```bash
rustup default stable
cargo install cargo-insta   # snapshot review

# Full check (matches CI)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Parity vs Bun v2.3.3 (Layer 4)
export BUN_BINARY=/path/to/onebrain-v2.3.3
cargo test -p parity --release

# Snapshot workflow
cargo test                  # tests fail on snapshot mismatch
cargo insta review          # interactive approve/reject
```

## Architecture

4-crate Cargo workspace:

- `onebrain-core` — types, config parsing, path resolution (zero filesystem deps)
- `onebrain-fs` — vault walks, frontmatter scans, plugin tarball overlay, init bootstrap
- `onebrain-cache` — session token resolution, plist generation, qmd status
- `onebrain-cli` — binary crate · produces the **`onebrain`** binary · clap dispatch over 13 subcommands (all Bun-parity ported · slices 1–13 shipped)

Subcommands: `session-init` · `orphan-scan` · `qmd-reindex` · `checkpoint` · `harness` · `doctor` · `register-hooks` · `register-schedule` · `migrate` · `init` · `update` · `run-skill` · `vault-sync`.

`CHANGELOG.md` tracks slice-by-slice port progress.

## Contributing

PRs welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for dev setup, build + test commands, PR conventions (worktree, version bump, English-only, 3-round review floor), and the security-issue reporting channel.

## License

AGPL-3.0-only · see `LICENSE`.
