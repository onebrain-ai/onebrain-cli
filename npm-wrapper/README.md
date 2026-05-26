# @onebrain-ai/cli

npm wrapper for the [OneBrain CLI](https://github.com/onebrain-ai/onebrain-cli) — the local-first Rust binary that powers the [OneBrain](https://onebrain.run) personal AI OS for Obsidian.

```bash
npm install -g @onebrain-ai/cli
onebrain --version
```

> **This package is a thin installer.** On install, `postinstall.js` downloads the matching platform-native binary from the corresponding [GitHub Release](https://github.com/onebrain-ai/onebrain-cli/releases), verifies its SHA-256, and stages it at `./bin/onebrain` (`onebrain.exe` on Windows); the `onebrain` command is a small Node shim that execs the native binary.
>
> 📖 **Full documentation** — install paths, the command tree, output modes, architecture, and roadmap — lives in the **[repo README](https://github.com/onebrain-ai/onebrain-cli#readme)**.

## Supported platforms

The postinstall auto-detects your host and pulls the matching binary. On Linux it detects libc (glibc vs musl) and 32-bit ARM version (ARMv6 vs ARMv7) at install time — every Raspberry Pi from Pi 1 to Pi 5 is covered.

| Host | Binary |
|---|---|
| macOS · Apple Silicon (M1–M5) | `onebrain-aarch64-apple-darwin.tar.gz` |
| macOS · Intel | `onebrain-x86_64-apple-darwin.tar.gz` |
| Linux · ARM64 glibc (Pi 3/4/5 64-bit OS · Pi Zero 2 W) | `onebrain-aarch64-unknown-linux-gnu.tar.gz` |
| Linux · ARMv7 32-bit (Pi 2 v1.1+ · Pi 3/4/5 32-bit OS) | `onebrain-armv7-unknown-linux-gnueabihf.tar.gz` |
| Linux · ARMv6 32-bit (Pi 1 · Pi Zero · Pi Zero W) | `onebrain-arm-unknown-linux-gnueabihf.tar.gz` |
| Linux · x86_64 glibc | `onebrain-x86_64-unknown-linux-gnu.tar.gz` |
| Linux · x86_64 musl / Alpine | `onebrain-x86_64-unknown-linux-musl.tar.gz` |
| Windows · ARM64 | `onebrain-aarch64-pc-windows-msvc.zip` |
| Windows · x86_64 | `onebrain-x86_64-pc-windows-msvc.zip` |

If auto-detection misfires, override it: `ONEBRAIN_CLI_LIBC=glibc|musl` or `ONEBRAIN_CLI_ARM=v6|v7` before `npm install`. (Note: only `x86_64` musl is published — `arm64` musl hosts should use a glibc base or build from source.)

## Other install paths

- **Homebrew** (macOS, canonical) — `brew install onebrain-ai/onebrain/onebrain`
- **Direct download** — grab the binary for your platform from the [latest release](https://github.com/onebrain-ai/onebrain-cli/releases/latest)
- **Self-update** once installed — `onebrain update` (Homebrew installs delegate to `brew upgrade`)

All paths resolve to the same per-platform binary — see the [repo README](https://github.com/onebrain-ai/onebrain-cli#install) for the full comparison.

## Skipping postinstall (CI)

For environments that supply the binary out-of-band:

```bash
ONEBRAIN_CLI_SKIP_POSTINSTALL=1 npm install -g @onebrain-ai/cli
```

The shim runs but exits `127` until the binary is staged at `node_modules/@onebrain-ai/cli/bin/onebrain`.

## Releasing & license

Source lives at [`npm-wrapper/`](https://github.com/onebrain-ai/onebrain-cli/tree/main/npm-wrapper). CI publishes on each stable `vMAJOR.MINOR.PATCH` tag via npm Trusted Publishers (OIDC, no long-lived token) with `--provenance` for a Sigstore attestation — never published manually, and prerelease tags (containing `-`) are skipped. The wrapper version always equals the binary release version.

[AGPL-3.0-only](https://github.com/onebrain-ai/onebrain-cli/blob/main/LICENSE), matching the upstream CLI binary. For commercial licensing, contact [hello@onebrain.run](mailto:hello@onebrain.run).
