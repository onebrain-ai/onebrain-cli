# Contributing to OneBrain CLI

Thanks for your interest in OneBrain CLI! This document covers everything you need to send a clean, ready-to-merge PR.

## Quick links

- [Dev setup](#dev-setup)
- [Build + test](#build--test)
- [PR conventions](#pr-conventions)
- [Commit & branch conventions](#commit--branch-conventions)
- [Code review](#code-review)
- [Reporting issues](#reporting-issues)

---

## Dev setup

You need a stable Rust toolchain (1.83+ is what CI runs against). Install via [rustup](https://rustup.rs):

```bash
rustup default stable
rustup component add rustfmt clippy
cargo install cargo-insta            # interactive snapshot review
```

Clone and bootstrap:

```bash
git clone https://github.com/onebrain-ai/onebrain-cli
cd onebrain-cli
cargo build --workspace
```

## Build + test

The full suite matches CI:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Snapshot tests use [`insta`](https://insta.rs/) — review mismatches interactively:

```bash
cargo insta review   # approve or reject each diff
```

v3.x output contract is pinned by snapshot + parametric suites under `crates/onebrain-cli/tests/`: `v31_envelope_snapshots.rs` (canonical `Envelope` shape via insta), `output_format_matrix.rs` (every structured-output command × default/`--json`/`--json --pretty`/`--yaml`), `user_flows.rs` (end-to-end personas), `v31_integration.rs` (v3.0 alias migration). The legacy v2.x Bun parity suite was retired in v3.1.0 — the Bun reference binary is no longer published; only the deprecated `@onebrain-ai/cli@2.4.x` npm package remains.

## PR conventions

OneBrain CLI follows a focused, predictable PR workflow. Drift from any of the rules below typically blocks review.

### Branch + worktree

- Branch off `origin/main`.
- Use a worktree at `.worktree/<branch-name>` inside the repo (keeps the main checkout clean while a PR is in flight). Worktrees are auto-cleaned on `git worktree remove`.

```bash
git fetch origin main
git worktree add .worktree/my-feature -b feat/my-feature origin/main
cd .worktree/my-feature
```

### Version + CHANGELOG bump

- **One version bump per PR.** Bump `[workspace.package].version` in the root `Cargo.toml` exactly once, to the next semver (or next prerelease counter for `-alpha.N` / `-beta.N` / `-rc.N`).
- Update `CHANGELOG.md`: add a new `## vX.Y.Z — <subject>` entry above the previous version. Keep entries to **max ~8 bullet lines** per version; one tight line per substantive change. Reference PR numbers (`(PR #NN)`) where it adds context.
- Update the frontmatter `latest_version:` + `released:` at the top of `CHANGELOG.md`.

### npm wrapper version

The npm wrapper source lives at [`npm-wrapper/`](npm-wrapper/). Its `package.json` `version` field is a local-dev placeholder — CI rewrites it on tag push (`Sync wrapper version to git tag` step in `.github/workflows/release.yml`) so the wrapper version always equals the binary release version. Keep the checked-in value in step with the most recent stable tag, but **never run `npm publish` from a local clone**: the `npm-publish` job uses npm Trusted Publishers (OIDC) and is the only authorized publisher — no `NPM_TOKEN` is configured for human use. Editing wrapper source in a temp directory and publishing manually is the exact foot-gun that lost the original `/tmp/` source before this PR.

### English-only in repo files

All committed text — source code, comments, docs, commit messages, PR descriptions, CHANGELOG entries — must be in English. Local/private notes can use any language; once it lands in this repo, it's English.

### PR title + description must stay in sync

Every time you push a new commit that changes scope, update the PR title and description so reviewers always see the current state. Stale PR text wastes reviewer cycles.

### CI must be green before merge

Don't merge a PR while any required check is failing — fix the root cause instead of disabling the check.

## Commit & branch conventions

- Conventional-commit style is preferred (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`, `ci:`). Scopes welcome (`fix(slice-13): ...`).
- Squash merges only. The squash commit message should match the final PR title.
- `--delete-branch` on merge so the branch list stays clean.
- Never `--force` push to `main`. Force-push to your own PR branch is fine when it preserves review history-sense (e.g. rebasing onto a fresh `main`).

## Code review

OneBrain CLI uses a **3-round review floor** before merge. Each round catches progressively deeper issues, so don't treat 3 as a target — complex PRs often need 4–5. Typical review angles per round:

1. **Round 1** — surface correctness: API shape, types, error paths, obvious logic gaps.
2. **Round 2** — boundary cases, concurrency, error chaining, IO edge cases.
3. **Round 3** — security (untrusted input, command injection, path traversal, secret leakage), consistency with sibling modules, naming conventions, Bun-parity diffs.

When 2+ independent reviewers flag the same issue, treat it as consensus — fix immediately rather than negotiating.

## Reporting issues

- **Bugs**: open a GitHub issue with the binary version (`onebrain --version`), platform (`uname -a` or Windows equivalent), reproduction steps, and the actual vs. expected behaviour.
- **Security issues**: do NOT open a public issue. Email `security@onebrain.run` instead.
- **Feature proposals**: open an issue framed as a problem (not a solution) first; we'll discuss approach before any code lands.

---

License: MIT OR Apache-2.0 · see [`LICENSE-MIT`](LICENSE-MIT) / [`LICENSE-APACHE`](LICENSE-APACHE). By contributing, you agree your contributions are dual-licensed under the same terms.
