# OneBrain CLI

Personal AI OS CLI for Obsidian — Rust rewrite of v2.3.3 (TypeScript/Bun).

## Status

v3.0 development · GA target 2026-06-30. See design spec in the OneBrain vault.

## Install

> First user-facing prerelease: [`v3.0.0-alpha.1`](https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.1) — 7-platform binaries (darwin arm64/x64 · linux arm64/x64/musl-x64 · windows x64/arm64) attached to the GitHub Release. Build from source remains supported for development.

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli   # builds the onebrain-cli crate
# → target/release/onebrain                (binary name set via [[bin]] in crates/onebrain-cli/Cargo.toml)
```

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
- `onebrain-fs` — vault walks, frontmatter scans
- `onebrain-cache` — session token resolution, plist generation, qmd status
- `onebrain-cli` — binary crate · produces the **`onebrain`** binary · clap dispatch · 13 subcommands (`session-init` + `orphan-scan` + `qmd-reindex` + `checkpoint` + `harness` wired in v3.0 Slices 1-5)

See `01-projects/onebrain/shared/2026-05-14-rust-cli-rewrite-design.md` for the full design rationale (OneBrain vault).

## License

AGPL-3.0-only · see `LICENSE`. Trademark "OneBrain" pending (USPTO 2026-06-30).
