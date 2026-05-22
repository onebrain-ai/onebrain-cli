# @onebrain-ai/cli

npm wrapper for the [OneBrain CLI](https://github.com/onebrain-ai/onebrain-cli) — the local-first Rust binary that powers the [OneBrain](https://onebrain.run) personal AI OS for Obsidian.

```bash
npm install -g @onebrain-ai/cli
onebrain --version
# → onebrain 3.0.0
```

## What this package does

On install, the `postinstall.js` script downloads the platform-native binary from the matching [GitHub Release](https://github.com/onebrain-ai/onebrain-cli/releases) and places it at `./bin/onebrain` (or `onebrain.exe` on Windows). The `onebrain` command is a thin Node shim that execs the native binary.

| Host | Binary downloaded |
|---|---|
| macOS Apple Silicon | `onebrain-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `onebrain-x86_64-apple-darwin.tar.gz` |
| Linux ARM64 (glibc) | `onebrain-aarch64-unknown-linux-gnu.tar.gz` |
| Linux x86_64 (glibc) | `onebrain-x86_64-unknown-linux-gnu.tar.gz` |
| Windows ARM64 | `onebrain-aarch64-pc-windows-msvc.zip` |
| Windows x86_64 | `onebrain-x86_64-pc-windows-msvc.zip` |

Linux musl users — install directly from GitHub Releases (musl binary is published there but the npm wrapper currently maps glibc).

## Alternative install paths

```bash
# Homebrew (macOS / Linux)
brew tap onebrain-ai/onebrain
brew install onebrain

# Direct GitHub Release download
curl -L https://github.com/onebrain-ai/onebrain-cli/releases/latest

# In-place self-update once installed
onebrain update
```

The npm wrapper, Homebrew formula, and direct GH Release download all converge on the same binary — pick the one that fits your environment.

## Skipping postinstall

For CI environments that supply the binary out-of-band, set `ONEBRAIN_CLI_SKIP_POSTINSTALL=1` before `npm install`:

```bash
ONEBRAIN_CLI_SKIP_POSTINSTALL=1 npm install -g @onebrain-ai/cli
```

The shim still runs but exits with `command not found` (127) until the binary is staged at `node_modules/@onebrain-ai/cli/bin/onebrain`.

## Migration from v2.x

This is the first v3 release on npm. v2.x was the TypeScript/Bun implementation and is now deprecated — use this v3 package going forward. See the [v3.0.0 CHANGELOG](https://github.com/onebrain-ai/onebrain-cli/blob/main/CHANGELOG.md) for the full migration story.

## Releasing

Source for this package lives at `npm-wrapper/` in the [`onebrain-ai/onebrain-cli`](https://github.com/onebrain-ai/onebrain-cli) repository. Publishes happen automatically from the `npm-publish` job in `.github/workflows/release.yml` whenever a stable `vMAJOR.MINOR.PATCH` tag is pushed:

1. The job uses npm Trusted Publishers (OIDC `id-token: write`) — there is no long-lived `NPM_TOKEN` secret to rotate.
2. `npm version "$VERSION" --no-git-tag-version --allow-same-version` rewrites `package.json` to match the git tag, so the wrapper version always equals the binary release version.
3. `npm publish --access public --provenance` ships the package with a Sigstore attestation linking it to the exact workflow run and commit.

Tags containing `-` (e.g. `v3.0.1-rc.1`) are treated as prereleases and skip the npm publish step. Do not publish this package manually from a local clone — the trusted-publisher policy only honors publishes that originate from this workflow.

## License

[AGPL-3.0-only](LICENSE) — matches the upstream CLI binary. If you make a modified version available to users over a network (AGPL §13 — SaaS, internal APIs, any networked interaction), you must release your modifications under the same license. For commercial licensing inquiries, contact [hello@onebrain.run](mailto:hello@onebrain.run).
