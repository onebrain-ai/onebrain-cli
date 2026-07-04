# Install

Every channel below resolves to the same per-platform binary published in the matching GitHub Release. Homebrew is the canonical channel on macOS; on Linux/Windows, grab the matching pre-built binary or use the npm wrapper.

## Channels

| Channel | Command | Notes |
|---|---|---|
| **Homebrew** (macOS, canonical) | `brew install onebrain-ai/onebrain/onebrain` | Formula at [`onebrain-ai/homebrew-onebrain`](https://github.com/onebrain-ai/homebrew-onebrain), bumped on every tag. |
| **npm wrapper** | `npm install -g @onebrain-ai/cli` | Source at [`npm-wrapper/`](../npm-wrapper/); CI publishes on every stable tag via npm Trusted Publishers + `--provenance`. Verifies the release SHA-256 before extracting. |
| **Direct download** | table below | Pick your triple, drop the binary on `PATH`. |

## Pre-built binaries

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

Which targets ship full semantic search vs keyword-only: see the [platform-support matrix](platform-support.md).

```bash
# Manual install (any Unix)
curl -L -o onebrain.tar.gz \
  https://github.com/onebrain-ai/onebrain-cli/releases/latest/download/onebrain-aarch64-apple-darwin.tar.gz
tar xzf onebrain.tar.gz
sudo install onebrain /usr/local/bin/
```

## Self-update

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

## Build from source

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --release -p onebrain-cli
# → target/release/onebrain
```

Requires a recent stable Rust toolchain (`rustup default stable`). The only `unsafe` in OneBrain crates is a single `libc::getuid()` call (the launchd plist UID); the workspace otherwise builds cleanly on Linux, macOS, and Windows.

## Security & trust model

`onebrain update` authenticates downloaded binaries two ways: **GitHub's TLS chain** (rustls validation, no opt-out) secures the transport, and since v3.1.4 a **SHA-256 check** verifies the archive against its published `.sha256` *before* the swap — an unverifiable or mismatched asset is refused and the live binary is left untouched. The npm wrapper runs the same SHA-256 check before extracting. What's *not* yet done is cosign/signature verification: the checksum is an integrity check, not an authenticity one (an attacker who controls the serving origin could serve a matching archive + `.sha256` pair), so signing is tracked as a follow-up.

On networks running a corporate MITM proxy, the trust boundary becomes whatever certificate the proxy presents. If that matters to your threat model, verify the published `.sha256` files manually after each update.

Every operation that overwrites, migrates, or removes a config file first copies it to `<vault>/.onebrain-backups/<file>.<YYYYMMDD-HHMMSS>.bak` — the backup is a hard precondition, so the write is refused if the backup can't be made.

Report security issues privately via the channel documented in [`CONTRIBUTING.md`](../CONTRIBUTING.md).
