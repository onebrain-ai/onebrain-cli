---
latest_version: 3.4.1
released: 2026-07-03
---

# OneBrain CLI Changelog (v3.x · Rust)

All notable changes to the OneBrain CLI binary (`onebrain`) in the v3.x Rust rewrite.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

> **Versioning:** CLI version is tracked in workspace `Cargo.toml`. v3.x is the Rust port of [v2.x (TypeScript/Bun)](https://github.com/onebrain-ai/onebrain). `v3.0.0-alpha.1` is the first user-facing alpha (binary artifacts published to GitHub Releases for 7 platforms).

## [3.4.1] — 2026-07-03 — native search MCP server

- **`onebrain mcp`** — MCP stdio server (rmcp) over the native engine: `query` (lex/vec/hyde sub-queries, RRF-fused), `get`, `multi_get`, `status` — qmd-compatible tool surface.
- **`session init`** now probes the native index for `qmd_unembedded` (no qmd subprocess; same JSON contract).
- `dot_scalar` gains a debug-build equal-length assertion; simsimd fallback now logs before returning NEG_INFINITY.
- ADR 0018 polish: sysroot typo + win-arm64 decision restructured into sub-bullets.

## [3.4.0] — 2026-07-01 — native search engine (`onebrain-search`)

- **Native Rust search engine**: tantivy BM25 + fastembed embeddings + flat mmap vector store + RRF hybrid ranking — no Node/Python runtime.
- **`onebrain search query/search/vsearch/get/status/reindex`** (`--json`) plus `search model list/set` and an interactive TTY model picker.
- **Multilingual**: ~100-language semantic search (default `multilingual-e5-small`, swappable) + no-space-script keyword bigrams for Thai/CJK/Lao/Khmer/Myanmar.
- **Swappable embedding model** via `search model set` — rebuilds the vector store and re-embeds; `bge-m3` is the best-accuracy upgrade path.
- **Platform-tiered semantic search** (rustls, not openssl): targets with no ONNX Runtime prebuilt — x64 macOS, musl, 32-bit ARM (Raspberry Pi) — ship a lex-only binary (full CLI + keyword search; `query` degrades, `vsearch`/`model set` error). Gated by the `semantic` cargo feature (default ON). See [ADR 0017](docs/decisions/0017-platform-tiered-semantic-search.md) + the README platform-support matrix.
- Runs **alongside qmd** (engine milestone only) — MCP swap and qmd removal land in follow-up milestones.
- **Release cross-toolchains fixed** so all 9 targets build: `aarch64-unknown-linux-gnu` installs `g++-*` for onnxruntime's C++ runtime (`-lstdc++`); `aarch64-pc-windows-msvc` activates the arm64 MSVC toolset so simsimd's C build links. Plus a main-branch review sweep (webview redirect-hop off-by-one, translate error logging, static gzip q-value robustness, gzip-embed script hardening).

## [3.3.27] — 2026-07-02 — translate bridge for select-to-lookup

- **`POST /api/translate`** — server-side bridge to Google's free gtx endpoint (`{text, from?, to}` → `{translated, detected_from, truncated}`); 5,000-char cap, 8 s timeout, fixed host (no SSRF surface). Powers the WebUI select-to-lookup Translate action.
- **Webview preflight now resolves scheme-relative and absolute-path redirect `Location`s** (RFC 3986) — th.wikipedia's `Special:Search` redirects with `//host/…` and was wrongly reported unframeable; SSRF posture unchanged (targets stay http(s), path-relative still rejected).

## [3.3.26] — 2026-07-02 — release embeds the prebuilt webui dist

- **Release workflow downloads the prebuilt web UI instead of rebuilding it.** onebrain-webui now publishes its dist as a GH Release tarball (+sha256) on every merged version; our release's `webui` job fetches the version pinned in `crates/onebrain-cli/Cargo.toml` `[package.metadata.webui]` and verifies its sha256 against the pin — the dist is built exactly once (in its own repo), releases are minutes faster, and re-running an old tag embeds the same webui bytes (reproducible).
- **Fail-closed:** missing/malformed pin metadata, malformed version/sha, missing release asset, or a hash mismatch all abort the release loudly — no silent fallback. Download job runs with `contents: read` only.

## [3.3.25] — 2026-07-01 — webview preflight route

- **`GET /api/webview/preflight?url=`** — inspects a URL's `X-Frame-Options` and CSP `frame-ancestors` headers and returns `{frameable}`, so the web UI can decide whether to embed an external link in an iframe or open it in a new tab. The header probe runs via `ureq` on a blocking thread (`spawn_blocking`); http/https only, 5-second timeout, headers-only (body discarded).
- **Fail-safe:** any failure — bad scheme, network error, timeout — degrades to `frameable:false` and never returns an HTTP error to the caller. Pure header-parsing (`frameable_from_headers`) is unit-tested independently of the network.

## [3.3.24] — 2026-07-01 — serve robots.txt (the one unauthenticated route)

- **`GET /robots.txt` is served without a token** (a private-instance `User-agent: * / Disallow: /`). It's the single exemption to the whole-surface token gate: static boilerplate with no vault data and no filesystem access, so it leaks nothing the bare `401` didn't, and it satisfies the well-known-file convention that crawlers fetch `robots.txt` unauthenticated. The SPA shell, every asset, and every `/api` route still require a token.
- **Verb-restricted (GET/HEAD only)** so the exemption never widens the CSRF surface — a `POST /robots.txt` still `401`s. Fixes the Lighthouse SEO `robots-txt` audit (desktop SEO 91 → 100).

## [3.3.23] — 2026-07-01 — gzip-precompress the embedded web UI

- **Precompressed web UI assets.** The embedded UI's hashed `assets/` are gzipped at build time (`scripts/gzip-embed-assets.sh`, wired into the release workflow); `serve` detects the gzip magic and hands them back with `Content-Encoding: gzip` (the browser inflates). Release binary **~16.2 MB → ~9.3 MB (−43%, ~6.9 MB)** — smaller downloads (npm/brew) + disk.
- **Zero new dependencies, cross-compiles cleanly.** Uses pure-Rust `flate2` (miniz_oxide — already linked via onebrain-fs) only as a fallback for the rare client without `Accept-Encoding: gzip`; browsers get the bytes gzipped over the wire (smaller transfer, no server-side work). No C library, so it builds on every release target unchanged.
- **No effect on non-`serve` commands** (they never touch the embed) and non-`assets/` files (`index.html`, `version.json`, `changelog.json`) stay raw. Detection is by gzip magic bytes, so serving is correct whether or not a given build gzipped the assets.

## [3.3.22] — 2026-07-01 — serve banner + embedded web UI version

- **`onebrain serve` reports the bundled web UI version + release date.** Reads `version.json` (embedded assets, or a `--dir` dist on disk) plus the sibling `changelog.json`'s `released` date, shown inline as `OneBrain Web UI vX.Y.Z (YYYY-MM-DD)` — onebrain-webui ≥ 0.1.1 emits both. An absent marker degrades gracefully (version without date, or the plain source description).
- **Prettier startup banner.** Framed, emoji-prefixed layout (`🧠 🔗 📂 🎨 ⏹️`) mirroring OneBrain's session-greeting look, replacing the flat `serving on … / vault: / dist:` lines.
- `server::{webui_version, webui_released}` + their pure `parse_*` helpers added (split out + unit-tested). The dist's `version.json`/`changelog.json` are served as static assets by the existing catch-all handler, so the web UI can fetch them too.
- No behavior change to routing/auth — startup output only.

## [3.3.21] — 2026-06-30 — coverage phase 3d (dispatch.rs exit-code integration tests)

- **test(cli): cover the `dispatch()` `process::exit` arms via integration tests** (+9 assert_cmd tests, no production change) — `v31/dispatch.rs` 91.08% → **95.64%**. Verbs `qmd reindex`, `plugin install`/`migrate`, `skill show`/`info`, and the early vault/arg-guard exit paths of `serve`/`harness run`/`skill run` (which return before any subprocess), asserting exact exit codes (0/1/64/66).
- Core line coverage 95.03% → **95.21%** (`scripts/coverage.sh`).
- Residual `dispatch()` arms documented in `docs/coverage.md` — real network/subprocess/TTY paths (`vault sync` git, `skill run`/`harness run` claude/gemini spawn, `daemon` fork, `plugin update` TTY render) that an integration test cannot safely trigger.
- No behavior change — tests + docs only.

## [3.3.20] — 2026-06-30 — coverage phase 3b + 3c (server/api.rs + command residuals)

- **test(server): cover the JSON API handlers** (+28 oneshot/unit tests) — `server/api.rs` 69.56% → **87.06%**. Extends the `tower::oneshot` harness in `server/tests.rs`: no-vault 503 for every vault handler, byte-range 206 + `Content-Range`, forced-attachment, raw upload, `If-Match` overwrite, move/folder 404/409/415/422, method 405, plus `ApiError`/`From<FsError>` mapping.
- **test(cli/fs): close residual branches in the command layer** (+47 tests) — `v31/dispatch.rs` 88.69% → **91.08%** (plugin-update detail/verdict/JSON-envelope helpers), `onebrain-fs/src/update/mod.rs` 89.62% → **92.62%** (release-payload parse, date/version extraction, cache, env-override), `commands/register_schedule.rs` 91.30% → **93.09%** (collision labels, schedulable validation, status/remove/resume, quiet branches), `commands/doctor.rs` → 94.21% (json-mode fix paths, runtime-block edge cases).
- Core line coverage 94.28% → **95.03%** (`scripts/coverage.sh`).
- **Documented the realistic coverage ceiling** in `docs/coverage.md`: literal 100% is unreachable on stable (no inline `// coverage:ignore`; `#[coverage(off)]` is nightly-only, rust-lang/rust#84605). Genuinely-unreachable lines (`spawn_blocking` `JoinError` closures, `process::exit` dispatch arms, real network/subprocess paths, TTY-only renders, post-`canonicalize` I/O faults) are listed as residuals, not ignored. Target: ≈99% core + documented residuals + a ratcheting CI gate.
- No behavior change — tests + docs only.

## [3.3.19] — 2026-06-30 — coverage phase 3 (fs cluster)

- **test(fs): close coverage gaps across the onebrain-fs cluster** (+94 tests, no production change) — `note/archive.rs` 80.25% → 94.20%, `init/mod.rs` 89.11% → 94.14%, `vault_sync/pin.rs` 93.43% → 97.16%, `register_hooks/settings.rs` 78.26% → 85.71%, `register_hooks/hooks.rs` → 97.83%, `doctor/vault_yml_keys.rs` → 97.45%, `v31/hook_rewriter.rs` → 97.98%, plus `note/move.rs`, `init/marketplace.rs`, `vault_sync/{orchestrate,sync}.rs`, `migrate.rs`, `output/dispatcher.rs` — every target file improved.
- Tests target real error/edge paths (missing/malformed files, permission failures, idempotency + fallback branches) with meaningful assertions; permission-denial tests are `#[cfg(unix)]`-gated.
- Core line coverage (per `scripts/coverage.sh`) 93.62% → **94.28%** / 94.63% region; ~1,429 missed lines remain. Residuals (hard `EXDEV`/network/edge paths) tracked in `docs/coverage.md`.
- No behavior change — tests only.

## [3.3.18] — 2026-06-29 — coverage phase 2 (command modules)

- **test(cli): close coverage gaps in the command-module layer** — `commands/doctor.rs` 87.55% → 94.20%, `commands/register_schedule.rs` 72.08% → 91.30%, `vault_ctx.rs` 51.35% → 100%, `commands/run_skill.rs` 78.82% → 79.17% (+110 tests across health-check branches, plist render/status/remove paths, vault resolution, and the headless skill-run argv shapes).
- Core line coverage (per `scripts/coverage.sh`, exclusions applied) 92.59% → **93.62%**. Residuals are documented in `docs/coverage.md` — all interactive-TTY spinners, real subprocess/network calls, or OS-specific branches.
- Test isolation hardening — the `plugin-cache`/`qmd-embeddings` fix paths now run via subprocess with a tempdir `$HOME`/`PATH` so the destructive cache sweep can never touch the real `~/.claude` cache; dropped a thread-unsafe process-`PATH` mutation and tightened two assertions that didn't verify their result.
- No behavior change — tests only.

## [3.3.17] — 2026-06-29 — fix `onebrain update` hang on Homebrew + tighter --help indent

- **fix(update): `onebrain update` no longer hangs on Homebrew.** Homebrew 4.4+ made `brew upgrade` prompt "Do you want to proceed? [y/n]" by default; the install spinner redrew the TTY over brew's readline, corrupting the y/n input into an endless "Invalid input" loop. `brew_upgrade` now sets `HOMEBREW_NO_ASK=1` so brew auto-proceeds (the user already opted in by running `onebrain update`). Version-safe — older brew without ask-mode ignores it; the `--yes` flag is deliberately NOT used because it errors on pre-ask-mode brew.
- **style(cli): tighter `--help` layout** — category headings now sit flush at the left margin and commands indent 2 spaces (was 2-space heading / 4-space commands), so the grouped command list reads closer to the edge.

## [3.3.16] — 2026-06-29 — coverage foundation + dispatch tests

- **test(cli): coverage tooling** — add `scripts/coverage.sh` (wraps `cargo llvm-cov --workspace` with a documented `--ignore-filename-regex` exclusion allowlist) and `docs/coverage.md` (the excluded-files list + rationale + measured baselines). Targets 100% line coverage on testable "core" code; genuinely-unreachable code (network installs, blocking servers, TTY wizards, OS probes) is excluded explicitly, not silently.
- **test(cli): cover `v31/dispatch.rs` stub + verb arms** — parametric exit-code tests over all hidden stub verbs (72 inside a vault / 64 outside) plus real daemon/schedule/completions arms. dispatch.rs 76.94% → 86.70% line.
- Measured baselines: whole-workspace 89.58% line; core (exclusions applied) 92.59% line. No behavior change — tests + docs only.

## [3.3.15] — 2026-06-29 — categorized root --help

- **feat(cli): group root `--help` commands into named category sections** — `onebrain --help` now renders four sections (⚙️ System Management, 🧠 Vault Management, 🔄 Session Management, 🚀 Launch Management) instead of one flat Commands list.
- Category headings show their emoji on a terminal and render plain (no emoji) when stdout is piped/redirected, so `onebrain --help | cat` stays clean. The usage line keeps `<COMMAND>` and the Options section is unchanged.
- Descriptions are pulled live from clap `about` annotations — no hardcoded strings; the block can never drift from the source of truth.
- Subcommand help (`onebrain note --help`, etc.) is untouched — clap handles those paths unchanged.
- Drift-guard test: CI fails if any visible root subcommand is missing from CATEGORIES, or any category entry is stale.
- Options section preserves compact format (description inline, `[default:]` wraps) — not affected by the categorized block injection.
- Fixed `is_root_help_request` to not intercept `--version` / `-V` (was returning categorized help instead of version output).

## [3.3.14] — 2026-06-29 — surface note + task in --help

- **feat(cli): surface the `note` and `task` command groups in `onebrain --help`** — both were implemented but `#[command(hide = true)]`, so users couldn't discover them. All 14 `note` verbs + `task list` are real; they now appear (with descriptions) under the resource-group cluster.
- Stub verbs `task add` / `task done` stay hidden until implemented; the all-stub groups (avatar, bookmark, daemon, …) and v3.0 legacy aliases remain hidden as before.
- Added unit + integration tests asserting `note`/`task` are visible, `task list` shows under `task --help`, and `task add`/`done` + stub groups stay hidden.

## [3.3.13] — 2026-06-29 — fence-aware task scan + task list verb

- fix(fs): `scan_tasks` now skips checkbox lines inside `` ` `` `` ` `` `` ` `` / `~~~` fenced code blocks — demo/fixture tasks in plan & spec docs no longer pollute task scans (also fixes the daemon `/api/vault/tasks` endpoint)
- feat(cli): implement `onebrain task list` — fence-aware dated-task listing with `--due-by <today|YYYY-MM-DD>`, repeatable `--folder`, and `--all`; JSON envelope `task.list`

## [3.3.12] — 2026-06-28 — serve: --dir help matches the embedded UI

- docs(serve): the `serve --help` `--dir` text + the `ServeConfig.dist_dir` doc said "Omit to run API-only (a placeholder page is served)" — stale since the web UI was embedded in the binary (v3.3.10). They now read "Omit to serve the embedded UI" (override with `--dir` for web-UI dev), matching the v3.3.11 startup-banner fix.

## [3.3.11] — 2026-06-28 — serve: embedded-UI banner + API hardening

- fix(serve): the startup banner reports `dist: (embedded web UI)` when the binary ships a bundled UI — previously it printed `(none — API only, placeholder page)` for any no-`--dir` run, wrongly implying the embedded web UI wasn't being served.
- fix(serve): OWASP A03 — read endpoints `GET /api/vault/file` and `/api/vault/raw` now refuse vault tooling dirs (`.git`/`.obsidian`/`.claude`/`.trash`/`node_modules`), matching the write paths; previously a direct path read could pull tree-hidden files that may hold secrets (e.g. `.claude/settings.local.json`).
- fix(serve): OWASP A03 — the `claude` chat subprocess argv ends options with `--` before the message, so a chat message starting with `-`/`--` is the positional prompt, never a smuggled claude flag (extends the leading-dash guard already on `--resume`/`--model`).

## [3.3.10] — 2026-06-27 — serve: qmd-backed vault search

- feat(serve): new `GET /api/vault/search?q=&mode=lex|hybrid` for the web UI's search panel — shells out to the `qmd` index: `lex` = BM25 keyword (fast, no LLM), `hybrid` = keyword + semantic vector (one query embedding, local rerank). Returns ranked `{hits, mode}` with qmd's `qmd://<collection>/…` URIs mapped to vault-relative paths (other-collection hits dropped). Read-only; the query is a single argv (no shell injection); 30s timeout with child reaping; cached binary path.
- fix(serve): the endpoint returns 503 when `qmd_collection` is absent or the `qmd` binary is missing, so the web UI falls back to its own filename/path search instead of running qmd unscoped.

## [3.3.9] — 2026-06-27 — serve: web UI preview support (framing, media)

- fix(serve): security headers relaxed from `X-Frame-Options: DENY` + CSP `frame-ancestors 'none'` to `SAMEORIGIN` / `frame-ancestors 'self'`, so the web UI can frame its own `/api/vault/raw` endpoint to preview PDFs in an `<iframe>` — previously both directives blocked the same-origin frame and the PDF pane rendered blank. Cross-origin framing (clickjacking) is still denied.
- fix(serve): CSP `img-src` now allows `blob:` so the pptx preview's embedded media (logos, photos the renderer materialises as same-origin blob: URLs) can load — without it every embedded slide image was blocked.
- feat(serve): `/api/vault/raw` now sends audio/video `Content-Type`s and honours `Range` requests (`206 Partial Content` + `Accept-Ranges: bytes`), so the web UI's native `<audio>`/`<video>` player can stream + seek (Safari refuses a `200` full-body for media).
- fix(serve): harden `/api/vault/raw` against stored XSS now that same-origin framing is allowed — script-carrying types (`.svg`/`.html`/`.xml`) are served as `application/octet-stream` + `Content-Disposition: attachment` (with a fail-safe bare `attachment`), so navigating straight to a raw URL can't execute an attacker-authored SVG/HTML in the app origin; the download filename is stripped of control chars to block CR/LF header injection.
- fix(serve): OWASP hardening — a pinned `ONEBRAIN_TOKEN` now requires ≥ 32 chars (was 16; the auto-generated token is already 128-bit), and the `claude` agent subprocess no longer inherits the daemon's own `ONEBRAIN_TOKEN` (it doesn't need the credential that gates the HTTP API).

## [3.3.8] — 2026-06-27 — serve: download keeps the original filename

- fix(serve): `GET /api/vault/raw?…&download=1` now sends `Content-Disposition: attachment` with the file's real name (RFC 5987 `filename*`, so spaces and non-ASCII/Thai names survive), so the web UI's download button saves the original filename + extension instead of the blob-URL id some webviews fall back to. Without the flag the endpoint still serves inline for image/PDF preview.

## [3.3.7] — 2026-06-26 — serve: allow data: fonts for the Office-doc preview

- fix(serve): `Content-Security-Policy` now allows `data:` fonts (`font-src 'self' data:`) so the web UI's Office-document preview can render the slide/text fonts that PowerPoint (and Word) embed as data-URIs — without it the browser blocked every embedded `@font-face` and slides rendered blank. Images already permitted `data:`; fonts now match. No other directive changed.

## [3.3.6] — 2026-06-26 — serve: security hardening (token gating · CSP · stable token)

- feat(serve): the whole router is now token-gated — every route and method (SPA, file read/write, `/api/vault/raw` + `/upload`, `/api/chat`) requires the per-session token via `X-OneBrain-Token`, `Authorization: Bearer`, `?token=` (GET/HEAD only — CSRF-safe), or the `onebrain_token` cookie (set on query-auth; `HttpOnly`, `SameSite=Strict`, `Secure` on https). Token compared with `constant_time_eq`.
- feat(serve): a security-headers middleware (outermost layer) sets CSP (`object-src 'none'`, `base-uri 'self'`, `frame-ancestors 'none'`, …), `X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`, `COOP`, and HSTS on https — so the token-bearing page is hardened against XSS-exfiltration and clickjacking even when exposed via a tunnel.
- feat(serve): `resolve_token` honours `$ONEBRAIN_TOKEN` (≥16 chars) so the token can stay stable across restarts (otherwise a fresh random token each launch); the injected `window.__ONEBRAIN_TOKEN__` is JSON-escaped.
- fix(serve): chat request bodies are capped (`MAX_MESSAGE_BYTES`) and `serve` warns when binding a non-loopback address over plain HTTP.

## [3.3.5] — 2026-06-26 — tasks: scan projects + areas only

- fix(tasks): `GET /api/vault/tasks` (and the scan it backs) now scans only the configured project + area folders — default `01-projects/` + `02-areas/` — instead of the whole vault, so READMEs, inbox, knowledge, and resource notes no longer surface as actionable todos. Folder names come from the vault config (with a trailing-slash guard so `projects: 01-projects/` doesn't become a `//` prefix that matches nothing) and fall back to the PARA defaults when config can't be read.

## [3.3.4] — 2026-06-26 — doctor qmd: unknown-not-zero parity

- fix(doctor): `onebrain doctor`'s qmd-embeddings check now reports `qmd status unavailable` when `qmd status` prints a `Total:` line but no `Pending:` line (incomplete/corrupted output), instead of inventing `0 unembedded`. This carries the v3.3.3 null-not-zero rule to the last consumer of the shared probe: session-init already leaves the count `null` (`unembedded_from_probe` → `None`) for the same input, so every consumer now treats it as unknown rather than inventing a zero. Still non-fatal (`ok`).

## [3.3.3] — 2026-06-26 — qmd probe: one shared source of truth · 15 s timeout · null-not-zero

- fix(qmd): session-init's unembedded count and `onebrain qmd status` no longer report a false `0` / "not installed" when `qmd status` is slow. The shared probe timed out at 2 s, but a real index can take ~10 s — `onebrain doctor` already learned this (3 s → 15 s in v3.2.4) and the fix was never carried over. Bumped to 15 s, enforced by a compile-time guard.
- perf(session-init): the startup probe uses a tighter 5 s cap (vs 15 s for `qmd status` / `doctor`) so a slow/hung qmd can't freeze the greeting — on timeout it degrades to `null` ("unknown"), never a false `0`. One shared probe, two caps documented by intent.
- feat(session-init): `qmd_unembedded` is now `null` (not `0`) when the qmd probe can't determine the count (missing / timed out / unparseable), so a probe failure is distinguishable from a genuine zero and no longer silently hides pending embeddings at startup. Additive change to the internal hook protocol — the JSON key is always present and `0`/`N>0` are unchanged. The SessionStart consumer should treat `null` as "unknown" (companion INSTRUCTIONS.md update); text mode prints `qmd index: unknown (qmd unavailable)`.
- fix(qmd): robust `qmd` resolution — the probe looks in (and runs qmd with) the bun-global dir so a `bun install -g qmd` install resolves under a restricted launcher PATH (hook / launchd / Obsidian terminal) and a located-but-interpreted qmd finds its own interpreter. Mirrors doctor's long-standing lookup.
- refactor(qmd): unified the duplicated qmd-status probes — `onebrain-cache::qmd` is now the single source of truth (spawn · PATH · timeout · parse) reused by session-init, `qmd status`, and `onebrain doctor` (which previously had its own copy), so they can't drift again.
- **`serve`/`daemon` default port `4317` → `6789`** — `4317`/`4318` collided with OpenTelemetry OTLP (gRPC/HTTP); `6789` is memorable and avoids the busy round ports. Override with `--port` as before.
- chore(license): relicensed from `AGPL-3.0-only` to `MIT OR Apache-2.0` — permissive dual license, applied org-wide. Sole-author relicense; effective for all releases from here on.

## [3.3.2] — 2026-06-25 — note edit / delete / mkdir CLI verbs

- feat(note): `onebrain note edit <path> <content>` — verbatim overwrite (or create) a note via the shared `onebrain_fs::note::write_note` primitive.
- feat(note): `onebrain note delete <path>` — move a note to `.trash` via `delete_note`.
- feat(note): `onebrain note mkdir <path>` — create a folder via `create_folder`.
- These are the CLI counterparts to the v3.3.1 daemon write endpoints — both surfaces now share ONE write/delete/folder implementation (no duplication).

## [3.3.1] — 2026-06-24 — daemon write / media / chat surface

- feat(daemon): note write surface — `POST/PUT/DELETE /api/vault/file` (create /
  overwrite with `rev` conflict-check / move-to-`.trash`), `POST /api/vault/move`
  (rename + rewrite incoming wikilinks), `POST`+`DELETE /api/vault/folder`.
- feat(daemon): `GET /api/vault/raw` (bytes + content-type for image/PDF preview)
  and `POST /api/vault/upload` (binary attachments), behind a `DefaultBodyLimit`.
- feat(daemon): `GET /api/vault/tasks` — vault-wide dated Obsidian-Tasks scan.
- feat(daemon): `POST /api/chat` — SSE stream over a `claude -p` agent turn
  (concurrency-capped; process-group kill on client disconnect).
- feat(auth): accept the per-session token via `?token=` query on GET/HEAD only
  (image/raw fetches that can't set a header); writes stay header-only.
- refactor(core): handlers are thin veneers over shared `onebrain_fs` primitives —
  `note::{write_note,delete_note,create_folder,delete_folder}` + new
  `task::scan_tasks`; `note::TOOLING_DIRS` made public — so the CLI and daemon
  share ONE implementation per vault operation (no duplication).

## [3.3.0] — 2026-06-05 — daemon foundation + HTTP surface

- feat(daemon): `onebrain daemon start|stop|status` — `start` self-respawns a
  detached process (`setsid` + `chdir`, log → `~/.onebrain/run/daemon.log`) tracked
  by `daemon.pid` with a session-leader identity probe; `stop` SIGTERMs + clears it.
- feat(serve): `onebrain serve [--dir <dist>] [--port] [--host] [--open]` brings up
  ONE local HTTP surface — static SPA (token-injected `index.html`, `..` fallback)
  + a read-only vault JSON API (`/api/config`, `/api/vault/tree`, `/api/vault/file`).
  Per-session token gates `/api/*` (401 without); path-traversal is rejected (400).
  `daemon __run` now runs the SAME server (SIGTERM shutdown vs `serve`'s Ctrl-C).
  Security: no-vault → 503 (never falls back to `/`, so `?path=etc/passwd` can't leak the host); constant-time token compare; token never written to the log (log 0600, run dir 0700); 10 MB file cap → 413; 4xx error mapping.
- deps: net-new compiled crates are `axum 0.8` + `tower` + `tower-http` (fs) — `tokio`/`hyper`/`http`
  were already transitive deps at step 1 (the deliberate v3.3 daemon binary-size tradeoff) · `tracing` · `nix`.

## [3.2.21] — 2026-05-30 — cache-clean hardening

- fix(cache-clean): orphan cache dirs under an UNregistered marketplace are now
  swept even when a registered marketplace exists (was gated on `dirs.is_empty()`).
- fix(cache-clean): `remove_dir_all` failures are surfaced (counted + stderr
  warning) instead of silently dropped — `clean_plugin_cache` now returns
  `CacheCleanOutcome { removed, failed }`.
- Verified the Step 9 sweep runs unconditionally on every real Claude update
  (only `--dry-run` skips); added a clarifying comment.

## [3.2.20] — 2026-05-29 — completions: exclude hidden commands

- fix(cli): shell completions no longer list hidden/internal/legacy subcommands
  (avatar, daemon, session-init, …) — clap_complete's aot generators don't honor
  `hide`, so completions are now generated from a recursively hidden-filtered
  command tree.

## [3.2.19] — 2026-05-29 — shell completions

- feat(cli): `onebrain completions <SHELL>` — hidden subcommand emitting a shell
  completion script (bash · zsh · fish · powershell · elvish) via clap_complete.
- feat(cli): optional shell-aware hint after interactive `onebrain init` (detects
  `$SHELL`); enables Homebrew formula completion auto-install (tap PR follows).

## [3.2.18] — 2026-05-29 — dependency + size cleanup (reqwest→ureq · serde_yaml_ng · async-stack drop)

- **Perf/size: `reqwest` → `ureq` (blocking sync HTTP).** The 4 GitHub/tarball fetch sites (`update` · `vault-sync`) now use `ureq`, which carries no async runtime. This removes the entire async stack — `tokio` · `hyper` · `h2` · `tower` · `tower-http` · `hyper-rustls` · `tokio-rustls` · `hyper-util` — from the release binary (all pulled in only by reqwest). TLS stays rustls. Result: **−342 KB** binary (3.34 → 3.01 MB) · **−54 crates** (178 → 124) · **~12% faster clean build** (41.0s → 36.2s). Runtime of everyday commands is unchanged — they never made HTTP calls, and these fetch paths are network-bound.
- **Internal: removed the dead `tokio_helper` runtime shim.** It was reserved for "future async commands (v3.1+ serve)" with zero callers and was the only direct `tokio` user, so dropping it lets the async stack leave entirely. The daemon (v3.3) re-introduces `tokio` deliberately for its async RPC server.
- **Dep: `serde_yaml` (archived/unmaintained upstream) → `serde_yaml_ng`** via a package-rename alias — an actively-maintained drop-in, zero code changes.
- **Internal: dropped the unused `clap` `env` feature** (no `#[arg(env)]` anywhere in the tree).
- **Internal: unified the two `plugin update` text renderers** into one `render_plugin_update_inner`, removing the `PluginUpdateTextData` trait that existed only to make the static renderer generic over a test double (−~80 LOC · Code-Simplifier finding on PR #57).
- **Internal: renamed `vault_sync::run_silent` + `register_schedule::run_quiet` → both `run_embedded`** — same intent (the embedded-from-plugin-update entry point), now a consistent name (Consistency reviewer finding on PR #57).

## [3.2.17] — 2026-05-29 — `onebrain update`: refresh Homebrew tap before upgrade + dedicated npm channel

- **Fix: `onebrain update` on a Homebrew install now refreshes the `onebrain-ai/onebrain` tap before `brew upgrade`.** `brew upgrade` does not fetch new formulae, so running `onebrain update` right after a release found a stale local formula, no-op'd ("already installed"), and the post-install version guard then reported the upgrade "may not have taken effect" (a confusing false-ish failure that forced a manual `brew update && brew upgrade onebrain`). The fix git-pulls **only** our tap (not a full `brew update` — stays fast) so the freshly-published formula is visible and the upgrade applies in one `onebrain update`. Best-effort + non-fatal: a refresh failure falls through to `brew upgrade` exactly as before.
- **Feat: `onebrain update` now has a dedicated npm channel.** A binary installed via `npm i -g @onebrain-ai/cli` previously fell through to the Direct path, which swaps the file in place and desyncs npm's metadata (the same divergence the Homebrew path avoids). It's now detected by the `@onebrain-ai` `node_modules` scope and updated via `npm install -g @onebrain-ai/cli@<version>`. This completes the "delegate to the package manager, never swap a managed binary" model across all three channels — **Homebrew · npm · Direct download**.

## [3.2.16] — 2026-05-29 — plugin-cache doctor check + orphan cleanup + post-update reload hint

- **Fix: stale plugin-cache orphans no longer silently shadow the vault-local plugin.** A leftover `~/.claude/plugins/cache/<mkt>/onebrain/<version>/` (orphaned by a marketplace install that predated a vault-local update) could make Claude Code load OLD skills while `INSTRUCTIONS.md` came from the current copy. vault-sync Step 9 already pruned these during a sync; this adds proactive detection so an orphan created outside an update is caught on the next `doctor` run.
- **Feat: new `doctor` `plugin-cache` check** (Vault structure section) reports stale cached plugin versions (cache vs the vault-local pin). `--fix` prunes them, then re-detects to report an honest result — `Failed` (non-zero exit), not a misleading `Fixed`, if a removal hit a permission error.
- **Feat: `plugin update` prints a post-update reload next-step** (`↻ /reload-plugins …`) whenever a real version change lands, so the running session picks up the new plugin; `/wrapup` + reopen is called out for `INSTRUCTIONS.md`/`CLAUDE.md` changes. Fires on partial-failure-with-version-change too (the version already landed on disk).
- **Internal: hardened cache cleanup** — `detect_stale_plugin_cache` + `clean_plugin_cache` share one enumeration pass (`version_dirs_under`), skip symlinks (`symlink_metadata`, no follow), and reject path-traversal marketplace keys (`onebrain@../…`). `installed_plugins.json` path resolution consolidated into one shared `default_installed_plugins_path`.

## [3.2.15] — 2026-05-28 — `--help` compact-with-wrap · plugin update polish · per-command emoji · version tracking · `--json` minified

- **Break: `--help` reverts to compact layout** (command + description on the same line) — v3.2.12's blanket `next_line_help = true` pushed every arg into long format, which user testing flagged as "ดูยาก" (hard to read). For args carrying `[default]` + `[possible values]` (`-o, --output`, `--mode`, `--harness` on `skill run` / `harness run`), the description still renders inline but the bracketed value block wraps to a new indented line below. Achieved by `hide_default_value = true` + `hide_possible_values = true` + a manual `\n[default: …, possible values: …]` tail on the `help` string.
- **Polish: per-command framed-header emoji differentiated.** Pre-3.2.15 both `doctor` and `update` used 🧠 (same as the OneBrain wordmark banner above them); v3.2.15: `doctor` → 🔬, `update` → 🚀, `plugin update` → 🔄. Each command now reads at a glance and stops competing with the brand glyph.
- **Polish: `onebrain plugin update` no longer leaks the orchestrator's per-step `▸ <label>` lines above its framed report.** v3.2.13's `vault_sync::run_embedded` silenced the intro/outro frame but kept the step lines; v3.2.15 routes through new `vault_sync::run_silent` which forces the progress reporter to `io::sink()`. The framed `🔄  Plugin Update` header now appears as the FIRST thing in the report (right after the brand banner), with the animated spinner as the only progress signal during work — matches `doctor`/`update`'s established UX. `run_embedded` (intro-only suppression) removed; no remaining caller wanted the in-between mode.
- **Feat: `plugin update` now reports current + latest plugin version explicitly.** The vault-sync step row reads `vX → vY` (real update), `vX · up-to-date` (rerun on the same version), or `installed vY` (fresh install) instead of the pre-3.2.15 collapse to `done` / `skipped`. The verdict footer also surfaces the delta — `updated v3.1.3 → v3.1.4` for a real bump, `update complete · v3.1.4` for no-version-change work, `already up-to-date · v3.1.4` for idempotent reruns, and `dry-run · current v3.1.4` for `--dry-run`. JSON envelope gains `version_before` / `version_after` fields (additive — both `#[serde(skip_serializing_if = "Option::is_none")]`).
- **Break: `--json` (and `--output json`) now emits MINIFIED single-line JSON.** Pre-3.2.15 auto-prettified the JSON when stdout was a TTY, which was convenient for humans glancing at a structured response interactively but made the "copy/paste into curl / a script" path noisier. To get indented JSON now, pass `--json --pretty` (or `--output json --pretty`) explicitly.
- **Break: `--output table` and `--output tsv` removed.** Both variants fell through to the JSON encoder unchanged for every command, so the format flag silently lied — `--output table doctor` emitted the same minified JSON as `--output json doctor`. Per user testing, dropped both from the `OutputMode` enum and the `--output` value parser; the remaining set is `text` / `json` / `yaml`. If a future command genuinely needs columnar output we'll add the column extractor and renderer together with the variant.
- **Polish: `skill run --help` and `harness run --help` no longer duplicate the flag breakdown above their Options: section.** Reverted v3.2.15 round-2's parent-listing flag-per-line breakdown on `SkillVerb::Run` / `HarnessVerb::Run` — clap's renderer auto-switches to long format (option name on its own line + indented description) on the verb-level help whenever the variant's `about` contains a newline. Trade-off chosen per user testing: parent-level `skill --help` / `harness --help` Commands: rows now show a one-line summary, but the verb-level `--help` Options: section is COMPACT (matches top-level `onebrain --help`) and still wraps `[default]` + `[possible values]` to an indented next line for args that carry both.
- **Polish: positional `<NAME>` args on `skill info` / `skill show` / `skill bootstrap` (and the hidden `bundle install/show/info/init/lint/update/remove`) now carry a description in the Arguments: section.** Previously they rendered as a bare `<NAME>` with no help text; the variant doc-comment described what the verb does but didn't propagate to the positional arg.

## [3.2.14] — 2026-05-28 — `plugin update` animated spinner pacing (doctor/update parity)

- **Polish: `onebrain plugin update` now animates the three step rows with the same braille spinner + random 800–2000ms pacing that `doctor` and `onebrain update` use.** Each step paints `⠋ <label>` with the cycling spinner for the random dwell, then `\r`-clears and writes the resolved `✓ <label>  <detail>` row — so the framed report reads as live work instead of an instant flash. Animation gates on a real-colour stdout TTY AND respects `--quiet` (`should_animate(mode, stdout_is_tty, cli.quiet)`); pipes / CI / structured output / `--quiet` runs all fall through to the static `_to` path unchanged.
- **Internal: new `render_plugin_update_animated` / `render_plugin_update_animated_to` pair** in `crate::v31::dispatch`. The `_to` variant accepts an injectable `Write` plus a `step_delay_override: Option<Duration>` so unit tests can pass `Some(Duration::ZERO)` and assert spinner artefacts (`\r\x1b[K`, braille frames, post-animation resolved lines) without sleeping. `ProgressRenderer::set_step_delay` promoted from `#[cfg(test)]` to a first-class crate seam now that a second command reuses the animation infrastructure.

## [3.2.13] — 2026-05-28 — `plugin update` UX polish: framed report (doctor-style) · silenced sub-output

- **Polish: `onebrain plugin update` now renders a framed doctor-style report** instead of a key:value summary. New layout: ⚡-flanked header (`⚡  Plugin Update`), one "Update steps" section with ✓-glyphed step lines (`vault sync` / `hooks` / `launchd plists`), and a verdict footer that mirrors `doctor`'s ` ✓  <text>             <total> steps` shape. Partial failures attach an indented `└ <reason>` hint under the verdict line, matching doctor's warn/fail hint rendering.
- **Polish: removed "OneBrain Vault Sync" intro/outro frame leakage from inside the workflow.** Routes the sub-call through a new `vault_sync::run_embedded` helper that sets `VaultSyncOptions::embedded = true`, so the orchestrator skips its own banner + outro under plugin update (still emits transient step spinners during long fetches so the user sees activity).
- **Polish: silenced `register_schedule`'s per-plist `✓ Wrote …` chatter** when invoked from plugin update. New `register_schedule::run_quiet` entry suppresses the progress/summary `println!`s; the direct `onebrain schedule register` surface keeps its existing output untouched. Returns the count of plists actually written so the framed report can render `N refreshed` / `no schedule entries` / `failed` instead of collapsing every outcome to a misleading `done`.
- **Polish: non-TTY (CI / scheduler / piped stdout) sub-output is now silenced too.** `PlainProgress::with_embedded` added so the orchestrator's `vault-sync: <step>` lines and `vault-sync: done` outro suppress on the embedded path. Pre-round-2 only the TTY path honored the `embedded` flag — non-TTY runs leaked the raw lines through plugin update's framed report.
- **Fix: partial-failure path no longer paints the failing step with `✓`.** When `register_schedule::run_quiet` returns `Err`, the launchd plist step row now renders with `✗ launchd plists  failed` (matching the verdict glyph at the footer). Pre-round-2 the step row stayed `✓ launchd plists  skipped` and silently misrepresented the failed step as succeeded. Multi-line `partial_failure` reasons flatten to single-line ` · `-separated text so the indented `└ <reason>` hint layout stays intact.

## [3.2.12] — 2026-05-28 — `--help` long-format · `[default]` / `[possible values]` wrap onto their own line

- **Polish: every `--help` screen now uses the long format** (description below the option name, `[default]` and `[possible values]` on their own lines). Added `next_line_help = true` + `propagate_version = true` to the root `Cli` struct — both settings propagate to every subcommand automatically. Trade-off vs v3.2.11's "compact" `harness run --help`: more vertical space per option, but every value block (defaults, possible values, enum variants) now gets the breathing room it needs to stay readable.
- **Restored: `HarnessMode::WithContext` / `HarnessMode::AdHoc` variant docs** — v3.2.11 stripped them to keep `harness run --help` compact, but the long format makes that compromise unnecessary. The `Possible values:` block under `--mode` now renders each variant's full semantics inline (vault required vs cwd=$TMPDIR, what flags get passed to the harness, etc.), so the user no longer has to cross-reference the `--mode <MODE>` summary against the source to know what each value does.

## [3.2.11] — 2026-05-28 — help cleanup: `--help` only · `skill help` → `skill show` · `harness run --help` compact · banner consistency

- **Break: `<group> help` subcommand removed across the tree.** Every `*Cmd` group now sets `disable_help_subcommand = true`, so `onebrain plugin help` / `onebrain harness help` / etc. error with "unrecognized subcommand 'help'" instead of duplicating the `--help` flag. Use `onebrain <group> --help` (or `-h`) everywhere — one help surface, no parallel keyword form.
- **Rename: `skill help <NAME>` → `skill show <NAME>`.** The verb renders SKILL.md body (skill workflow markdown), which is a different concept from clap's `--help` (CLI usage). Pairing them on the same verb caused the "which help is this?" confusion the original verb name carried. Module-level docs in `commands/skill_inspect.rs` spell out the distinction. Same rename applied to the hidden stub `bundle help` → `bundle show`.
- **Fix: bare `onebrain harness` now emits the brand banner before showing help.** Pre-parse banner pass only fires when argv contains `--help`/`-h`/`help` or no subcommand at all — it missed `arg_required_else_help` group hops. `main` now uses `try_parse()` and emits the banner ahead of `clap::error::Error::exit()` for `DisplayHelp*` error kinds, fixing the missing-banner regression on `onebrain harness` (and any future group that adopts `arg_required_else_help`).
- **Fix: `onebrain skill show <NAME>` no longer prints the banner twice.** The duplicate emission came from `argv_requests_help` matching the literal `help` keyword in `skill help <name>` (pre-parse banner) AND `dispatch` emitting again on successful parse. Renaming the verb to `show` removes the keyword collision; the heuristic is unchanged.
- **Polish: `harness run --help` rewritten to the compact one-line style** used by `skill run --help` — `--harness {claude,gemini}` · `--model <m>` · `--mode {with-context,ad-hoc}` listed inline so flags surface in the group-level `harness --help` without a paragraph of prose.
- **Polish: no more banner above "unrecognized subcommand 'help'" errors.** `argv_requests_help` (the pre-parse banner gate) no longer matches the literal `help` keyword — only `--help` / `-h` / no-subcommand-at-all. Removing `help` from clap as a subcommand made the keyword path strictly an error, so emitting the banner above it was noise. `MissingSubcommand` is wired into the `try_parse` interception path so future clap versions that switch error kinds keep the banner. Adds regression tests: `bare_group_emits_banner_above_help` (issue #2 path) and `argv_requests_help_rejects_legacy_help_keyword` (issue #1's structural fix at the heuristic level).

## [3.2.10] — 2026-05-28 — `skill info` / `skill help`, harness/skill UX polish, `--json` passthrough

- **Feat: `onebrain skill info <NAME>`** — print a skill's frontmatter (name · description · schedulable · required_args). JSON / YAML modes supported (`--json` / `--yaml`); text mode renders key:value lines. Previously stubbed.
- **Feat: `onebrain skill help <NAME>`** — print the SKILL.md body (the human-readable workflow). Text mode dumps the markdown verbatim; `--json` wraps as `{"name":...,"body":...}`. Previously stubbed. Exit codes mirror the standard: 64 (empty name), 66 (missing skill), 78 (no vault), 65 (malformed YAML).
- **Feat: `--json` on `skill run` / `harness run` passes through to the harness** — maps to `claude --output-format json` / `gemini --output-format json` so the captured stdout is the harness's native structured response (tool calls, metadata) instead of free text.
- **Polish: `onebrain harness` (no verb) now prints help instead of silently running `detect`** — drops the v3.0 flat-form back-compat. Use `onebrain harness detect` explicitly (or any other verb). `arg_required_else_help = true` does the work.
- **Polish: `harness run` description rewritten** to drop the now-stale `/onebrain:<skill>` cross-reference and surface the `--harness` / `--model` / `--mode` flags inline in the summary so they show in `harness --help` at the group level. Same treatment for `skill run`. The `harness` command `about` is now "Detect or run an AI harness (claude / gemini)" (was misleading "Detect Claude Code runtime" — predated `harness run`).

## [3.2.9] — 2026-05-28 — `harness run` polish: spinner subject + true ad-hoc

- **Fix: `--mode ad-hoc` now actually skips vault context.** v3.2.8 used `cwd = $PWD`, which let `claude` / `gemini` auto-walk-up from a vault subdir and silently re-load OneBrain's `CLAUDE.md` / `GEMINI.md` — running ad-hoc from inside the vault still returned the OneBrain agent name and persona, defeating the mode. Now forces `cwd = $TMPDIR` so ad-hoc means ad-hoc regardless of where the user invoked from. User-level config (`~/.claude/CLAUDE.md`) still loads — that is separate from cwd-based project context.
- **Polish: `harness run` watched spinner now says "on the prompt" instead of "on the skill"** — copy-paste leak from `skill run`. The shared `spawn_harness` helper now takes a `subject: &str` parameter so each command labels itself correctly (`skill run` → "the skill", `harness run` → "the prompt"). Internal-only API change.

## [3.2.8] — 2026-05-28 — `onebrain harness run` (ad-hoc prompts through claude / gemini)

- **Feat: `onebrain harness run [PROMPT]`** — send an ad-hoc prompt to the chosen agent harness (`--harness {claude,gemini}`, default claude) with `--model <m>` passthrough. Verbatim prompt (no `/onebrain:<skill>` namespacing — for that use `skill run`). Reads stdin if `[PROMPT]` is omitted, so `cat note.md | onebrain harness run --harness gemini --model gemini-2.5-flash` composes naturally.
- **Two modes via `--mode {with-context,ad-hoc}`** (default `with-context`): with-context loads the vault's CLAUDE.md / INSTRUCTIONS.md / GEMINI.md via `--add-dir` / `--include-directories` and `cwd = <vault>` (vault required, exit 78 if missing); ad-hoc skips the vault flag entirely and runs with `cwd = $PWD` so the harness answers the raw prompt with no OneBrain context attached (vault not required). Empty prompt rejected with exit 64.
- **Internal:** refactored the shared spawn path so `harness_argv(harness, prompt, context_dir: Option<&str>, model)` builds argv for both modes — `Some(<vault>)` adds the context-dir flag, `None` skips it. `skill run` always passes `Some(vault)`; `harness run` passes `None` for ad-hoc. `spawn_harness`, `ONEBRAIN_HEADLESS=1`, the in-place spinner, and the piped-output capture are all reused unchanged.

## [3.2.7] — 2026-05-28 — `skill run` in-place spinner (no more heartbeat scrollback)

- **UX: `skill run` shows an in-place `indicatif` spinner on a watched run** — replaces the per-10s "still running (Ns)" newline heartbeat (which flooded scrollback during long runs) with a single ticking spinner line that updates in place: `⠋ Running claude on the skill headlessly · 23s`. Cleared on completion before the harness's output prints, so nothing ever overlaps the spinner.
- **Internal: pipe the harness's stdout/stderr into in-process buffers via two reader threads while `child.wait()` blocks** (keeping the child handle so a `wait()` error can still `kill()` + `wait()` the harness instead of leaking an orphan that keeps burning API tokens). The captured streams flush once on exit, with `flush()` to handle piped stdout (`onebrain skill run … | tee log`). `claude -p` / `gemini -p` already buffer-flush-at-end (no real-time streaming), so capturing loses nothing — and it removes the v3.2.4 spinner-vs-flush race that originally forced the plain-newline heartbeat. Non-interactive runs (launchd scheduler, piped, CI) keep inherited stdio + blocking `command.status()` — quiet logs, no spinner.

## [3.2.6] — 2026-05-28 — `skill run` harness + model selection · faster headless runs

- **Feat: `skill run --harness {claude,gemini}`** (default `claude`) — run a OneBrain skill through either agent runtime. claude → `claude -p … --add-dir`; gemini → `gemini -p … --include-directories --approval-mode yolo` (yolo so an unattended skill that runs `onebrain` shell commands or writes files isn't blocked on an approval prompt — same trust model as `claude -p` under the scheduler allow-list). Gemini headless verified: namespaced commands (`/onebrain:daily`), GEMINI.md tool mapping, and the `AfterAgent`/`AfterTool` hooks all wire up.
- **Feat: `skill run --model <m>`** — passed through to the harness (`claude --model <m>` / `gemini -m <m>`); omit to keep the harness default. The biggest raw-speed lever for headless runs — a faster model (`claude-haiku-4-5`, `gemini-2.5-flash`) speeds up every turn.
- **Perf: skip the interactive startup ceremony in headless runs.** `skill run` sets `ONEBRAIN_HEADLESS=1` on the harness child; `onebrain session init` now reports `headless: true`, which lets INSTRUCTIONS.md skip the greeting + status + memory/inbox/task/orphan scans (Steps 2–4) and go straight to the requested skill. Requires the plugin update that consumes the flag.
- **Internal:** generalized claude-only binary resolution to `resolve_claude_bin` + `resolve_gemini_bin` over a shared `resolve_bin` (env override `CLAUDE_BIN`/`GEMINI_BIN` → `~/.local/bin` → Homebrew → `/usr/local/bin` → bare); `ClaudeBinResolution` → `HarnessBinResolution`.

## [3.2.5] — 2026-05-27 — checkpoint hook actually fires now

- **Fix: the auto-checkpoint safety net never fired.** Two compounding root causes left `07-logs/checkpoint/` empty across every session. This release fixes both so long-running sessions get their crash-recovery snapshots.
- **Root cause 1 — session token churned, so the message counter never accumulated.** The token was resolved from terminal env vars (`WT_SESSION`/`TMUX_PANE`/`TERM_SESSION_ID`) then the `claude` process PID; in the Obsidian terminal and Claude Desktop those env vars are unset, so the token tracked the PID and reset on every Claude Code restart — the count split across many counters and never reached the 15-message threshold.
- **Fix: `CLAUDE_CODE_SESSION_ID` is now the top-priority token source (new layer 0).** Claude Code sets it on every host (terminal, Obsidian, Claude Desktop, IDE, agent-teams) and it is unique per session — so the counter is stable across PID churn and, crucially, distinct sessions sharing one terminal (agent-teams mode) no longer collide on one counter. Falls back to the existing layer 1–8 chain when absent (older Claude Code).
- **Root cause 2 — the 30-minute time threshold was dead for a session's first checkpoint.** `last_ts` stayed `0` until the first count-based block fired, which forced `elapsed` to `0`, so a long-but-quiet session (e.g. 8 turns over an hour) never snapshotted.
- **Fix: anchor `last_ts` on the first stop** so the minutes threshold starts ticking immediately; a long session now checkpoints on time even without 15 messages.

## [3.2.4] — 2026-05-27 — doctor `--fix` UX overhaul · qmd timeout · skill-run feedback

- **`doctor --fix` is now one pass with a confirmation step:** the report is shown once, the planned auto-fixes are previewed as a bulleted list, then a `[y/N]` prompt confirms before anything changes — followed by a single final verdict footer (was: full report → fixes → full report *again*). `--json` / `--yes` / non-interactive runs auto-proceed so the `/doctor` skill and cron aren't blocked. Manual-only issues (e.g. `qmd_collection not set`) no longer trigger a misleading "Apply fixes?" prompt.
- **Feat: `doctor --fix` creates missing vault folders** — a new `folders` recipe `mkdir`s any missing standard folders, named from `onebrain.yml` (so a customised `folders:` layout gets the right directories) plus `00-inbox/imports`.
- **Fix: `doctor` qmd check timeout 3 s → 15 s** — a real index (tens of MB) can take ~10 s for `qmd status`, so the old 3 s cap reported a spurious "qmd status unavailable (timeout)" on healthy, well-populated collections.
- **Polish: `doctor` frame rules span the longest line** — the header/footer rules widen to cover the widest content row (e.g. a long "Missing: …" hint) instead of stopping short; the `--fix` footer hint is shown only when something is actually auto-fixable.
- **Feat: `skill run` shows progress** — on an interactive TTY it prints a start line and an elapsed heartbeat (`… still running (Ns)`) while `claude -p` runs the (buffered, often slow) headless session, so the terminal no longer looks frozen. Scheduler/piped runs stay quiet for clean logs.
- **Feat: `skill run` accepts `--skill <name>`** as an alias for the positional name, for parity with the scheduler's `run-skill --skill` form (`skill run daily` and `skill run --skill /daily` are equivalent).
- **Polish: `--vault` is the single documented vault flag** — `--vault-dir` is now a hidden back-compat alias on every command (it used to show alongside the global `--vault`, which was confusing). The launchd scheduler and existing scripts keep working.
- **Chore:** removed the dead `.ci-trigger` scaffold file.

## [3.2.3] — 2026-05-27 — `skill run` fixes · `--vault` everywhere · doctor stamps last-run

- **Fix: `onebrain skill run` works from inside a vault** — it now resolves the vault through the canonical chain (`--vault` flag → `ONEBRAIN_VAULT` → walk-up from cwd) instead of demanding an explicit path, so `onebrain skill run daily` just works when run inside a vault. No vault found anywhere → `E_VAULT_NOT_FOUND` (exit 64).
- **Hardening: `onebrain skill run` gives the spawned `claude -p` a null stdin** so it can't block reading an inherited interactive TTY (`claude -p` appends piped stdin to the prompt; launchd already passed null stdin, this aligns manual runs). stdout/stderr stay inherited. (Does not address `claude -p` showing no output while a long skill like `/daily` runs — that responsiveness work is a follow-up.)
- **Fix: global `--vault` accepted on every command** — `skill run`, `schedule register`, and `plugin migrate` named their local `--vault-dir` field `vault`, colliding with the global `--vault` arg id and making clap reject `--vault` on those leaves ("unexpected argument '--vault'"). Renamed the field to `vault_dir`; `--vault` now propagates everywhere and `--vault-dir` stays as a back-compat alias for the scheduler.
- **Feat: `onebrain doctor` stamps `stats.last_doctor_run`** in `onebrain.yml` on every run (and `last_doctor_fix` when `--fix` ran), so a terminal `onebrain doctor` keeps the timestamp current like the `/doctor` skill already does. Best-effort (never changes the exit code), comment-preserving line edit, and a no-op when the value is already today's date.

## [3.2.2] — 2026-05-27 — animated `update` + banner/doctor polish

- **Feat: `onebrain update` animated TTY** — a framed `🧠 OneBrain Update` header plus a braille spinner (matching `doctor`) on the two phases that take time: `fetch` (the version check) and `install` (the download). The check is padded to a deliberate beat so it reads as real work even on a warm cache or when already up to date. Piped / `--json` / non-TTY output is unchanged.
- **Polish: banner vertical gradient** — a top-lit within-tone shade is layered on the horizontal cyan→purple→pink hue (top row keeps the exact anchors, lower rows darken). The non-truecolor fallback is now a vertical-only gray ramp (one tone shaded top→bottom, no left→right variation), and the stray leading shade pixel in front of the `O` is removed.
- **Polish: `doctor` spinner + footer** — the per-step spinner now visibly rotates (was a frozen first frame) and paces 800–2000 ms per check so the reveal is watchable; the summary-footer rule is widened to span the verdict line; a space is added after the `🧠` glyph so it no longer butts against the title.
- **Internal:** the framed header (rule + `🧠` title), braille `SPINNER_FRAMES`, and the pacing band are extracted into `output::` so `doctor` and `update` share one look without duplication — each keeps only its own renderer (sectioned `ProgressRenderer` vs indicatif live ticker).

## [3.2.1] — 2026-05-27 — doctor grouped UX + braille spinner · qmd-hook fix · logo banner gradient

- **Feat: `onebrain doctor` redesign** — the 9 checks are grouped into 4 sections (Config · Vault structure · Integration · Index & state) under a `🧠 OneBrain Doctor · <vault>` header, rendered via a new reusable braille-spinner progress primitive. Passes stay quiet (`✓`); warnings/fails are prominent (`⚠`/`✗`) with the check's hint on an indented `└` line; a summary footer shows the verdict, `N ok · N warnings · N fail`, the total, and the `--fix` next-action. Spinner + per-step pacing gate to a colour, non-quiet, interactive TTY only — piped/`--json`/`--yaml`/`--no-color`/`--quiet` get the instant static layout; `--json`/`--yaml` shape is byte-identical (presentation-only).
- **Fix: `doctor` qmd-hook false "missing"** — the detector now recognizes both the canonical `qmd reindex` form (✓) and the legacy `qmd-reindex` alias (⚠ → advise `--fix`), and flags duplicate hooks; `--fix` migrates legacy→canonical and dedups (register-hooks now writes the canonical form). The qmd-hook check stays gated on `qmd_collection`.
- **Feat: banner wordmark gradient** — a continuous horizontal cyan→purple→pink gradient across `ONEBRAIN` (matching the brain logo) in truecolor; non-truecolor terminals fall back to a light→dark gray ramp.
- **Polish:** `onebrain update`'s post-update hint now names the direct `onebrain plugin update` path alongside `/update` (no need to open Claude just to sync the plugin).

## [3.2.0] — 2026-05-27 — `note` resource group (11 verbs)

- **Feat: `onebrain note <verb>` — 11 native vault note operations** that replace ad-hoc `grep` / `ls` / `find` / `cat`: `search` (substring or `--mode regex`), `list` (metadata, sorted by name/mtime/created), `find` (glob + `--type` + Unix-style `--mtime`), `read` (`--section` / `--frontmatter-only` / `--tasks-only` / `--limit`), `stat` (line/word/link/task/heading counts), `backlinks`, `orphans`, `append` (section-aware), `new` (`--template` + inline `--frontmatter`), `archive` (dated `06-archive/YYYY/MM` bucket), and `move` (transactional vault-wide `[[wikilink]]` rewrite with rollback + `--dry-run`).
- All verbs emit the canonical `Envelope<T>` (text/json/yaml), are vault-required (`E_VAULT_NOT_FOUND`, exit 64), and reject bad regex/glob input with `E_INVALID_TARGET`. Scan verbs report (rather than silently skip) unreadable notes via a warning; a failed `move` rollback surfaces `E_ROLLBACK_INCOMPLETE` (exit 76) naming the unrestored files. Backed by 100+ fs-layer + CLI unit tests plus a 22-case fixture-vault integration suite. No plugin/skill changes — skills adopt these verbs in a later release.

## [3.1.5] — 2026-05-26 — fix: `onebrain update` false-negative binary validation

- **Fix: `onebrain update` no longer reports "Binary validation failed. Check PATH." after a successful upgrade.** The post-install validator matched Bun's `v`-prefixed `--version` shape (`/v\d+\.\d+/`), but the Rust/clap binary prints `onebrain 3.1.4` (no `v`), so every genuine version bump failed the gate even though the binary swap succeeded. Integration tests injected a mock validator, which is why the real format mismatch slipped through — added a regression test against the actual clap output.
- **Hardening: the post-install gate now confirms the `onebrain` on PATH actually reports the just-installed version** (`>= expected`) instead of merely "runs and prints some version" — a no-op `brew upgrade`, or a PATH resolving a different install, no longer passes silently. Validation failures now surface the specific cause (spawn/exec error · unparseable output · stale version on PATH) instead of a blanket "Check PATH".

## [3.1.4] — 2026-05-26 — self-update hardening: SHA-256 verification + Homebrew-aware update

- **Feat: `onebrain update` verifies the downloaded binary's SHA-256** against the published `<archive>.sha256` before the swap. An unverifiable asset (missing/malformed checksum file, or a digest mismatch) is now a hard failure with the live binary left untouched — TLS previously authenticated only the transport, not the bytes. Cosign/signature verification remains a follow-up pending release-side signing.
- **Feat: Homebrew-aware `onebrain update`.** A brew-managed install (binary resolves under `…/Cellar/onebrain/…`) now delegates to `brew upgrade onebrain` rather than swapping the Cellar binary in place, which desynced brew's metadata from disk (the dual-install divergence). Direct / `cargo-binstall` / manual installs keep the fetch-and-swap path; if `brew` isn't runnable the error points the user to run `brew upgrade onebrain` manually.

## [3.1.3] — 2026-05-26 — `schedule register` reads `onebrain.yml`

- **Fix: `onebrain schedule register` now dual-reads the config** (canonical `onebrain.yml` preferred, legacy `vault.yml` fallback). It hardcoded `vault.yml`, so on a v3.1 vault (onebrain.yml only) it found zero schedule entries and silently refused to (re)register/refresh launchd plists — `register`, `--refresh`, `--remove`, and `--status` all came up empty despite a populated `schedule:` block. Same class as the v3.1.1 onebrain.yml carry-through; `register_schedule`'s schedule-entries reader was the one spot still hardcoded (its `resolve_logs_folder` already dual-read via `load_vault_config`).

## [3.1.2] — 2026-05-26 — implement `qmd embed`

- **Feat: `onebrain qmd embed`** — was a `not_implemented` stub (exit 72), yet v3.1.1's `qmd status` told users to "run `onebrain qmd embed`" to clear pending docs. Now implemented: vault-required (exit 64 outside a vault), runs the underlying `qmd embed` in the foreground with inherited stdio so the user sees progress, and surfaces a non-zero `qmd` exit as an error. Completes the loop that `qmd status` points at.

## [3.1.1] — 2026-05-26 — config-loss fix + backups · doctor label rename + animated TTY · `qmd status`

- **Fix (data loss): `onebrain init --force` no longer clobbers an existing config.** Re-init re-registers the plugin + completes the folder scaffold but the fresh template never modelled `qmd_collection` (or custom checkpoint/folders/schedule values), so overwriting silently dropped them. Re-init now preserves the config verbatim (`onebrain.yml: preserved`); missing keys are repaired by `doctor --fix`, not by re-init.
- **Feat: timestamped config backups.** Every operation that overwrites/migrates/removes a config file (`doctor --fix` migration + key-backfill, `vault sync` Step 7) first copies it to `<vault>/.onebrain-backups/<file>.<YYYYMMDD-HHMMSS>.bak`. Backup is a hard precondition — the write is refused if the backup can't be made.
- **Fix: doctor check labels renamed `vault.yml` → `onebrain.yml` / `onebrain.yml-keys`** — match the canonical filename (since v3.1.0) and what the plugin `/doctor` skill already documents. Checks still dual-read (canonical preferred, legacy fallback); only the displayed/JSON `check` name + autofix-dispatch string changed.
- **Fix: stale `vault.yml` in user-facing output → `onebrain.yml`** — the `qmd-embeddings` message, `--help` text (`init --force`, `plugin/vault --branch`, `schedule add/remove/register`), `schedule register` + launchd scheduler errors, and the `vault sync` / orphan-scan stderr notices. Migration-context strings (the `vault-config-migration` check + deprecation warning) intentionally keep `vault.yml`.
- **Feat: `onebrain qmd status`** — reports index + embedding health (collection · indexed · embedded · pending · size · updated) in text/`--json`/`--yaml`. Vault-required (exit 64 outside a vault); `qmd_available: false` when the binary is missing (parses `qmd status` text since qmd ≤ 2.1.0 ignores `--json`).
- **Fix: `session init` unembedded count now works AND is vault-aware.** The v3.0 count went through `qmd status --json`, which qmd ignores → it always reported 0 on real installs. It now parses the text form (shared with `qmd status`) and is queried only when the vault sets `qmd_collection`, so a non-qmd vault reports 0 instead of leaking the global index's pending count.
- **Feat: animated `doctor` on an interactive TTY** — checks reveal one at a time (`⋯ checking <name>…` → result) with a short per-step delay (override/disable via `ONEBRAIN_DOCTOR_STEP_MS`). Piped/non-TTY stdout and `--json`/`--yaml` keep the instant report.

## [3.1.0] — 2026-05-25 — Consistency Standard · locked command tree · canonical JSON envelope

- **R1 branded banner** — interactive sessions print a 5-line FIGlet "Slant" `OneBrain` ASCII-art wordmark in primary `#ff2d92` followed by a dim `Your AI Thinking Partner · vX.Y.Z` tagline to stderr at first paint. Now also fires above every `--help` / `-h` / `help` screen (top-level, group, verb) so the brand line lands on every discovery surface; the duplicate clap `about` brand line was stripped from help stdout to eliminate the visible stacked-brand redundancy. Gated on a 6-rule TTY chain that drops the banner under `--quiet`, structured-output modes (`--json`/`--yaml`/`--output table|tsv`), `NO_COLOR`, `TERM=dumb`, `CI=true`, piped stdout, `--no-color`, or `--version`/`-V` (version-only intent). Hook-protocol commands (`session init`, `checkpoint *`, `qmd reindex` + v3.0 aliases) suppress the banner unconditionally so machine-output stdio stays clean.
- **Locked 27-entry command tree** — 3 root verbs (`init`/`update`/`doctor`) + 24 resource groups, all paths singular-noun 2-level `onebrain <noun> <verb>`. Working verbs in v3.1: `session init`, `checkpoint stop/reset/orphans`, `qmd reindex`, `vault sync/current`, `harness detect`, `plugin install/update/migrate`, `schedule register`, `skill run`. Other 200+ verbs stubbed with stable `E_NOT_IMPLEMENTED` exit 72 — tree shape locked for v3.2+. `onebrain --help` shows only the visible surface (stub-only groups + stub verbs hidden, domain-clustered ordering, one-line `about` per group, dev-log preamble stripped) — typed stub commands still dispatch.
- **Hidden v3.0 aliases for back-compat** — `session-init`, `orphan-scan`, `qmd-reindex`, `register-hooks`, `register-schedule`, `migrate`, `vault-sync`, `run-skill` all still work; each prints a one-time migration notice on stderr (suppressible via `ONEBRAIN_QUIET_MIGRATION=1`) and dispatches to the v3.1 handler. Notice state persists in `~/.cache/onebrain/migration-shown.txt`. Aliases removed in a future major (no v3.x removal scheduled).
- **Canonical `Envelope<T>` JSON shape** — every `--json` output wraps payloads in `{version, command, ok, vault, data, warnings, error}` (skill-alignment §4.3). `vault` and `error` are omitted via `skip_serializing_if` when `None`; `warnings` is always `[]` (not `null`) for stable schema. `--yaml` emits the same shape; `--output {text,json,yaml,table,tsv}` matrix mandatory on every command.
- **`--vault` global flag + walk-up resolver + `ONEBRAIN_VAULT` env** — documented priority `--vault <PATH>` > `ONEBRAIN_VAULT` env > walk-up from cwd, encoded in `onebrain_core::path::resolve_vault` and surfaced by the new `onebrain vault current` verb (reports which mechanism resolved the vault).
- **`plugin update` semantic swap** — was = pull plugin overlay; now = self-update the CLI binary (was `onebrain update`). The legacy plugin-overlay behaviour moved under `plugin update`'s vault-side step: pulls the plugin tarball, auto-rewrites `~/.claude/settings.json` hook `args[]` from v3.0 to v3.1 paths (`session-init` → `session init`, `orphan-scan` → `checkpoint orphans`, `qmd-reindex` → `qmd reindex`), and re-runs `schedule register` so launchd plists rebind. Fully idempotent.
- **`onebrain init` drops `--vault-dir` in favor of the global `--vault`** — was a v3.0 legacy that duplicated the global flag in `init --help`. Hidden v3.0 aliases (`session-init`, `register-hooks`, …) keep `--vault-dir` for back-compat through v3.5. On `init`, `--vault PATH` always means "create vault at PATH" (walk-up discovery doesn't apply — init creates, doesn't consume). v3.0 callers using `onebrain init --vault-dir X` must update to `onebrain --vault X init`.
- **Fix: `onebrain init` now registers the plugin with Claude Code AND prompts before initializing in a non-empty directory** — fresh init writes `<vault>/.claude-plugin/marketplace.json` AND merges `enabledPlugins.onebrain@onebrain = true` into `.claude/settings.json` (idempotent: hand-tweaks preserved on re-init, atomic merge skips write when key already true). Classifies the target (missing → create / empty → proceed / has vault.yml → defer to overwrite guard / non-empty non-vault → require confirmation); `--yes` without `--force` fails closed; structured-output modes emit canonical `E_INIT_TARGET_NOT_EMPTY` exit 75 envelope without prompting. Round-4 hardening: marketplace.json write failure now returns Err (was swallowed stderr warning); `--force` re-init validates manifest shape and rewrites malformed/wrong-shape via atomic tmp+rename; non-bool `enabledPlugins.onebrain@onebrain` is refused without `--force` (exit 75) and warned-on-replace with `--force`; `--force` no longer bypasses `safety::classify` so EACCES / target-is-a-file surface as proper FsError before partial writes; CLI prompt dispatcher debug_asserts on unknown prompts so future drift between `init/mod.rs` and `commands/init.rs` can't silently abort. Vaults init'd before this fix need to re-run `onebrain init --force` once to pick up the registration.
- **Partial-failure envelope contract (`E_PLUGIN_UPDATE_PARTIAL`) + `BrokenPipe` → exit 0** — long-running multi-step commands (plugin update, vault sync, schedule register) that complete part of their work before failing now emit `Envelope<T>::partial` with `ok = false`, `error.code = E_PLUGIN_UPDATE_PARTIAL`, and the completed-state snapshot in `data`; exit code 1 distinguishes partial from total failure; JSON / YAML / table / TSV all carry the same shape. Writes to closed-pipe stdout (`onebrain qmd reindex | head`, `onebrain doctor --json | grep`) now exit cleanly with code 0 instead of dumping a Rust panic; the dispatcher catches `io::ErrorKind::BrokenPipe` on every envelope-write surface. `serde_json::Error` bridged to `io::Error` via `io::Error::other` at the formatter boundary so envelope rendering doesn't pull a `From<serde_json::Error>` into `FsError`.
- **Output-format compliance · default = text · explicit flag = honored** — interactive commands now default to human-readable text and honor `--json` (compact) · `--json --pretty` (indented) · `--yaml` · `--output {json,yaml}` consistently. Covers `session init`, `checkpoint orphans`, `harness`, `doctor`, `update`. v3.1.0-rc regressions caught in audit: `harness` emitted JSON unconditionally; `doctor --yaml` returned the JSON envelope; `update --yaml` silently fell back to text — all now route through the canonical `serialize_for_mode` dispatcher. Two hook-only commands (`checkpoint stop`, `qmd reindex`) still emit hard-wired JSON regardless of flags — fixed scope for v3.1.1 follow-up; harmless today because both are invoked exclusively by Claude Code hooks that always want JSON. Serialisation failures now warn loudly on stderr instead of emitting silent empty stdout (the v3.0 regression class that motivated the audit). `onebrain plugin update` auto-rewrites `.claude/settings.json` hook entries to include `--json` (idempotent · respects pre-existing `--json` / `--yaml` / `--output` choices). Fresh-install scaffold (`onebrain init`) writes `--json` directly. Block reason renamed `onebrain-init-required` → `onebrain-vault-not-found`. Plugin INSTRUCTIONS.md updated to use `--json` in hook examples.
- **Output-format test matrix · parametric coverage** — added `tests/output_format_matrix.rs` (6 tests × 5 commands · {default, --json, --json --pretty, --yaml, --output json, hook-shape parity}) + `tests/user_flows.rs` (4 end-to-end scenarios: new-user no-vault, established-user, hook-consumer, error-recovery-malformed). Pretty-mode asserts `\n` + 2-space indent; default-mode asserts NOT JSON. Closes the gap where v3.1.0-rc shipped JSON-by-default before the regression was caught.
- **Pre-tag QA artifacts** — `scripts/smoke-ux.sh` exercises every help / default / format-flag surface with one command; output is paste-ready for release-PR review. `docs/release-checklist.md` enumerates every UX item with the test name or shell command that verifies it (no subjective entries).
- **Breaking: config file renamed `vault.yml` → `onebrain.yml`** — `onebrain init` writes the canonical `onebrain.yml` going forward. CLI v3.1+ dual-reads for back-compat (prefer `onebrain.yml`, fallback `vault.yml` with a one-time `W_VAULT_YML_DEPRECATED` stderr warning, suppressible via `ONEBRAIN_QUIET_VAULT_YML_DEPRECATION=1`). New `vault-config-migration` doctor check fires 🟡 when legacy `vault.yml` is detected; `onebrain doctor --fix` performs a single atomic `fs::rename` to migrate (idempotent on split-state vaults: removes the stale legacy file when both filenames exist). v4.0.0 will drop `vault.yml` support entirely.

## [3.0.x] — post-GA follow-ups

These shipped under the v3.0.x patch line after the 2026-05-22 GA and are not part of v3.1.0 itself.

- **npm wrapper source landed in-repo at `npm-wrapper/`** — recovered from the published `@onebrain-ai/cli@3.0.0` tarball after the original `/tmp/`-only source was lost. Files: `package.json`, `postinstall.js`, `bin/onebrain.js`, `README.md`, `LICENSE`. Wrapper `engines.node` raised to `>=20` (Node 18 EOL). Version stays at 3.0.0 to match the live npm package; the next v3 patch will bump together with the binary.
- **CI auto-publishes the npm wrapper on each stable tag** via a new `npm-publish` job in `release.yml`. Uses npm Trusted Publishers (OIDC, `id-token: write`) plus `--provenance` for Sigstore attestation; no long-lived `NPM_TOKEN` secret. `npm version "$VERSION" --no-git-tag-version --allow-same-version` in CI keeps the wrapper version aligned with the git tag. Any tag containing `-` is treated as a prerelease and skipped.
- **postinstall verifies SHA256** against the `.sha256` published alongside each release asset before extracting — closes the supply-chain gap between OIDC-attested wrapper publish and binary download integrity. The wrapper now also wraps extract in `try/finally` so partial archives don't linger in `node_modules/`.
- **bin shim re-raises signal terminations** (`128 + signum`) so Ctrl-C / SIGTERM through the shim is distinguishable from a real error in CI; missing-binary recovery hint now covers both local and global installs.
- **README + CONTRIBUTING signpost the new layout** — wrapper source lives at `npm-wrapper/`, publish is CI-only via Trusted Publishers (no local `npm publish`), Node 18 EOL noted. Root README install table promotes npm + Homebrew out of "planned" since both are live since v3.0.0 GA (2026-05-22).
- **postinstall hardening** — install-time robustness fixes: (a) retry-with-backoff on HTTP 404 (3 attempts at 2s/4s/8s) for both the archive and `.sha256` download — closes the install-race window after `npm publish` when the GitHub Release CDN lags behind the wrapper publish; (b) Alpine / musl detection via `/etc/alpine-release` plus `process.report.glibcVersionRuntime` probe with positive shape verification — maps `linux-x64+musl` to `x86_64-unknown-linux-musl` instead of silently shipping the glibc binary, warns + defaults to `glibc` on unknown shapes instead of falling through silently; (c) Windows tar fallback — tries `tar.exe` first, falls back to PowerShell `Expand-Archive -LiteralPath` on ANY tar failure (covers both pre-1803 absence and GNU-tar-from-MSYS2/Git-for-Windows misroute); (d) post-install smoke test — `onebrain --version` runs at end of postinstall so a wrong libc/arch binary fails loudly at install time instead of segfaulting on first user invocation; (e) escape-hatch env vars `ONEBRAIN_CLI_LIBC=glibc|musl` and `ONEBRAIN_CLI_ARM=v6|v7` for manual override when detection misfires. Followups from PR #29.
- **Raspberry Pi + 32-bit ARM Linux support** — release matrix adds `armv7-unknown-linux-gnueabihf` (Pi 2 v1.1+ · Pi 3/4/5 in 32-bit OS) and `arm-unknown-linux-gnueabihf` (Pi 1 · Pi Zero · Pi Zero W, ARMv6 hardfloat). Cross-compiled on Ubuntu via `gcc-arm-linux-gnueabihf`. Combined with the existing `aarch64-unknown-linux-gnu` (Pi 3/4/5 64-bit OS · Pi Zero 2 W), every Pi from Pi 1 to Pi 5 now has a published binary. postinstall auto-detects ARM version via `process.config.variables.arm_version` + `/proc/cpuinfo` and defaults to ARMv6 (forward-compatible) on inconclusive detection — `ONEBRAIN_CLI_ARM=v7` forces the faster ARMv7 build on Pi 4 32-bit OS. CI cross-compile step generalised from aarch64-only into a single matrix-driven step (`cross_prefix` value derives apt package, linker, and strip binary).

## v3.0.0 — Rust rewrite GA · 7-platform release pipeline · stable JSON contracts

- **Complete Rust rewrite of OneBrain CLI** — replaces v2.x TypeScript/Bun implementation. 4-crate workspace (`onebrain-core`, `onebrain-fs`, `onebrain-cache`, `onebrain-cli`) with all 13 subcommands ported. ~10× less private memory (~21 → ~2 MB per call), 92% smaller binary (57.8 → 4.6 MB stripped), startup within 10 ms of Bun on warm cache. Vault format, `vault.yml` schema, plugin contract, and slash-command surface unchanged from v2.x.
- **7-platform release pipeline** — macOS Apple Silicon + Intel · Linux ARM64 + x86_64 (glibc + musl) · Windows ARM64 + x86_64 binaries published to GitHub Releases per tag. `cargo-binstall`-ready (canonical Rust target triples in asset names) plus human-friendly platform badge table in the release body.
- **`onebrain update` install path** fetches binaries directly from GitHub Releases over HTTPS (rustls TLS), extracts the tarball, and atomically swaps the running binary (Unix single-rename · Windows rustup-style two-step rename + rollback). No npm/bun shell-out anywhere in the v3.x install path.
- **Stable JSON output contracts for v3.x** — `doctor --json`, `update --check --json`, and `update --plan` carry frozen schemas (boolean `ok` + numeric `summary.passing`; `update_available: null` on fetch failure; `fix[]` always present when `--fix` requested; `binary_targets[]` enumerates published `(triple, ext)` pairs). Stability covers v3.x; v4 may break.
- **Trust model** — downloaded binaries authenticated solely by GitHub's TLS chain. No SHA-256/cosign verification at GA (matches the rustup/deno/bun baseline); checksum verification is tracked for v3.0.x security hardening. Users on networks with corporate MITM proxies should be aware that the trust boundary is whatever cert the proxy presents.
- **Skill + scheduler ecosystem wired end-to-end** — `register-hooks`, `register-schedule`, and `run-skill` round-trip with the plugin's Stop / PostToolUse hooks, scheduled-skill launchd plists, and headless Claude Code invocations. Plist generation verified byte-identical to Bun v2.3.3 — Layer 4 parity suite in `tests/parity/` runs on every PR (downloads the upstream Bun v2.3.3 binary as the golden reference) and is reproducible locally with `BUN_BINARY=…`.
- **`doctor`** ships 8 read-only checks (vault.yml · vault.yml-keys · folders · plugin-files · settings-hooks · orphan-checkpoints · qmd-embeddings · claude-settings) and 5 `--fix` recipes (settings-hooks · plugin-files · vault.yml-keys · claude-settings · qmd). Remaining `--fix` recipes + Windows zip extraction in the install path are deferred to v3.0.1.
- **Distribution at GA**: GitHub Releases + `onebrain update` self-install is the primary path. npm-wrapper (`@onebrain-ai/cli@3.0.0` on npm) and Homebrew tap (`onebrain-ai/homebrew-onebrain`) are planned for the v3.0.x window — neither is published at GA. Legacy `npm install -g @onebrain-ai/cli@<3.0.0` versions will be flagged with `npm deprecate` once the wrapper publishes; Bun-era v2.x remains installed for the ~1049 weekly downloaders on pinned versions.

## v3.0.0-alpha.9 — GA candidate: fix `onebrain update` install path · TTY spinner · direct harness · real `--test` · Windows pin

- **`onebrain update` install path rewritten to fetch directly from GitHub Release.** The alpha.1 → alpha.8 builds shelled out to `bun install -g @onebrain-ai/cli@<v>` (Unix) / `npm install -g …` (Windows), but the v3.x Rust binary was never published to npm — every real-world `onebrain update` from alpha.1 through alpha.8 failed with "package exists but version not found." The new path resolves the target triple at runtime via `cfg!` macros, downloads the GitHub Release tarball over HTTPS (rustls TLS cert validation, no opt-out anywhere), extracts the `onebrain` binary, and atomically replaces the running binary via tmp + rename (Unix is single-rename atomic; Windows is a two-step rename with stderr-logged rollback + `.new` cleanup on failure). Real-world updates are now functional. **Windows zip extraction is intentionally stubbed for v3.0.0** — `update --plan` does NOT advertise Windows triples in `binary_targets[]` so Windows users see a clear "unsupported" message + manual-download pointer rather than a misleading install error; zip extraction lands in v3.0.1.
- **Trust model (no checksum verification yet).** The downloaded binary is authenticated solely by GitHub's TLS chain — there is no SHA-256/cosign verification of the asset itself. This matches the rustup/deno/bun baseline; SHA-256 verification is tracked for post-GA. Users on networks with corporate MITM proxies should be aware that the trust boundary is whatever cert their proxy presents.
- **TTY spinner + colorized output for `onebrain update`.** Interactive terminals get an `indicatif` spinner during the download phase + ANSI color on the well-known phase lines ("OneBrain Update" cyan-bold, "done:" green, "already up to date" dim, errors red). Non-TTY output (CI, pipes, redirects) keeps the existing plain-text format byte-for-byte. `--json` continues to suppress all log output for the single-document contract. The spinner mutex uses `unwrap_or_else(|e| e.into_inner())` so a panicked install_fn doesn't poison subsequent invocations (matches the established pattern in `update::tests`).
- **`direct` harness lands in `register-hooks` as a first-class no-op.** Previously vaults without a `.claude/` dir hit the gemini-only "no claude harness" message; now `Direct` mode (no harness dirs detected, or `ONEBRAIN_HARNESS=direct`) prints "direct mode · no hooks to register (run `onebrain` from your shell directly)" and the new `direct_mode` flag on `RegisterHooksResult` lets callers programmatically branch. `RegisterHooksResult` is now `#[non_exhaustive]` so future v3.0.x patches can extend it without breaking out-of-tree consumers.
- **`register-schedule --test <skill>` is now a real implementation.** Replaces the "deferred" stderr stub: walks vault.yml for the matching skill entry, builds the same argv launchd would emit (`onebrain run-skill --vault <path> --skill <name> [--arg key=value ...]`), spawns it synchronously with the parent env, streams stdout/stderr, and propagates the exit code. Lets users validate scheduled skill invocations end-to-end before committing the recurring cron line.
- **`update --plan` JSON now includes `binary_targets[]`** enumerating the six published `(triple, ext)` pairs (macOS + Linux only for now; see Windows note above), so consumers don't have to guess what the `<TRIPLE>` and `<EXT>` placeholders in `binary_url_template` can be.
- **New `UpdateError::Install(String)` variant** replaces `UpdateError::Network` for filesystem / OS errors during the install path (write, rename, chmod, missing parent dir, unsupported target). Previously every install failure surfaced as "Binary install failed: network: …" which misled operators into thinking the connection was at fault. `UpdateError` is `#[non_exhaustive]`.
- **`--vault-dir` flag pattern audit** (Reviewer C-I4 from alpha.8): user-visible flag name is consistent across all subcommands (`--vault-dir` works everywhere). Internal field-naming variance (`vault: Option<PathBuf>` w/ `visible_alias` vs `vault_dir: Option<PathBuf>` w/ direct `long`) is incidental to whether the subcommand also has a positional `vault_root` argument — when it does, the field stays `vault` and the alias picks up `--vault-dir`. No code change; documenting the conclusion here closes the audit.
- Defense-in-depth: `extract_tar_gz` now guards on `entry_type().is_file()` so a malicious tar containing a symlink or directory named `onebrain` cannot be promoted to "the binary". Also added the missing `aarch64-unknown-linux-musl` cfg arm (was falling into the catch-all error on Alpine ARM64). Plus deleted the legacy `build_install_command` + `run_subprocess` (the dead bun/npm code path) along with their 3 tests.
- 670 tests passing · clippy + fmt clean · 3-round review consensus applied.

## v3.0.0-alpha.8 — feat: JSON output modes for `doctor` + `update` · cosmetic

- **`onebrain doctor --json`** emits a single JSON document with `{ok, summary, checks[]}` instead of the plain-text report. `summary.passing` is the count of OK checks (deliberately not `summary.ok` to avoid confusion with the top-level `ok` boolean). Combines with `--fix` — the JSON reflects the post-fix state plus a `fix[]` array of `{check, outcome, message}` per attempted recipe. `fix[]` is always present when `--fix` is requested (even if empty), so consumers can distinguish "user didn't ask to fix" from "user asked but nothing to fix". In JSON mode the recipe status lines and any subprocess output route to stderr — stdout is reserved exclusively for the JSON document, so `cmd 2>/dev/null` always yields parseable JSON. Schema stable for v3.x.
- **`onebrain doctor --json` outside a vault** now emits a JSON failure envelope (`{ok: false, error: "not_in_vault", message: ...}`) on stdout with exit code 1, instead of an anyhow plain-text error.
- **`onebrain update --check --json`** emits `{ok, current, latest, update_available, released_at?}` (plus `error` on failure). `update_available` is `null` (JSON) when the remote fetch failed — consumers should not interpret missing-latest as "no update". `released_at` is RFC-3339 when the GitHub release payload carried it.
- **`onebrain update --plan`** is `--check --json` plus `release_url` and `binary_url_template` fields when an update is available — designed for the `/update` plugin skill. `--plan` implies dry-run (mutually exclusive with `--check` to avoid an ambiguous flag combo).
- **`onebrain vault-sync --vault-dir <path>`** flag-form alternative to the positional `vault_root` argument (mutually exclusive). Matches the `--vault-dir` pattern used across other OneBrain subcommands.
- **`register-schedule` resolves `folders.logs` from vault.yml** instead of hardcoding `07-logs/scheduler/...`. Closes two TODOs. Defense-in-depth: refuses absolute paths or `..` traversals (a malicious vault.yml could otherwise put launchd log files outside the vault); falls back to `07-logs` for any rejection. Falls back to `07-logs` when vault.yml is missing/invalid (operational metadata shouldn't block plist emission).
- 3-round review consensus fix-pass: `version_at_least` promoted from `pub(crate)` to `pub` in onebrain-fs and re-used by the CLI (was duplicated); `progress_writer` option added to `VaultSyncOptions` so doctor's `--fix` can route vault-sync's status lines to stderr; +5 unit tests for `released_at` emission, `update_available: null` on fetch failure, and the path-traversal guard. 662 tests total · clippy clean.

## v3.0.0-alpha.7 — feat(doctor): four new `--fix` recipes (settings-hooks · plugin-files · vault.yml-keys · claude-settings)

- **`doctor --fix` now repairs four more check types** beyond the `qmd-embeddings` recipe that shipped in alpha.5: (a) `settings-hooks` re-runs `register-hooks` idempotently (restores Stop hook + qmd PostToolUse hook + `Bash(onebrain *)` permission); (b) `plugin-files` re-overlays the plugin folder via `vault-sync` (brings INSTRUCTIONS.md / agents/ / skills/ / .claude-plugin/ back if missing); (c) `vault.yml-keys` backfills missing standard folder keys + `update_channel`, **strips deprecated keys** (`onebrain_version`, `method`, `runtime.harness`), and **repairs non-positive** `checkpoint.messages` / `checkpoint.minutes` (resets to 15 / 30); (d) `claude-settings` strips the stale `extraKnownMarketplaces.onebrain` block from `.claude/settings.json`.
- **Dispatch widened to Warn AND Error** so the "missing INSTRUCTIONS.md" / "missing `folders:` block" failure modes — emitted as Error by their respective checks — are now repaired by `--fix` instead of being silently bypassed. Recipes that don't apply to a given error fall through to a Manual message with the original hint.
- **Atomic writes everywhere**: vault.yml and settings.json mutations now go through `.tmp + rename` (matching `register_hooks::write_settings`) so a crash mid-write can't leave a truncated config file. The `claude-settings` recipe also routes through the canonical 4-space JSON formatter, eliminating the indent churn that previously appeared in `git diff` when `--fix` and `register-hooks` ran back-to-back.
- **`fix_plugin_files` now respects the same `refuse_dangerous_vault_path` guard** as `onebrain vault-sync` (extracted into a shared `safety` module), so the recipe cannot accidentally vault-sync into `/` or `$HOME` via a misconfigured `vault.yml`.
- **`orphan-checkpoints` routes to Manual** with a clearer hint: "run `/wrapup` in Claude to consolidate orphan checkpoints into a session log". Auto-deletion is intentionally off the table — orphans may still need to land in a session log, so we steer the user there rather than risking silent data loss.
- Five recipes total now ship with the auto-fix flow. The `vault.yml-keys` Fixed message also calls out "YAML comments not preserved" so users know what changed besides keys (serde_yaml has no comment-preservation mode).

## v3.0.0-alpha.6 — fix(update): target CLI repo + prerelease-safe · ci: GHA Node 24 · docs: README hero + badges

- **`onebrain update` now targets the CLI repo** (`onebrain-ai/onebrain-cli`) instead of the plugin repo (`onebrain-ai/onebrain`). Prior to alpha.6 the endpoint advertised `latest: v2.3.3` (the plugin repo's last Bun binary) for every alpha CLI user — and the non-`--check` form would happily downgrade users from `v3.0.0-alpha.5` to `v2.3.3`. The endpoint also switches from `/releases/latest` (stable-only) to `/releases?per_page=1` (most recent including prereleases) so the CLI's own alpha cycle is visible to itself.
- **Semver-aware version comparison** via the `semver` crate replaces the string equality check the Bun port carried over. `version_at_least(current, candidate)` refuses to install when the local version is already at or ahead of the remote — same user-visible message ("already up to date"), no more silent downgrades.
- **GitHub Actions Node 24 bump**: `actions/checkout@v4` → `@v6`, `actions/upload-artifact@v4` → `@v7`, `actions/download-artifact@v4` → `@v8` across both `ci.yml` and `release.yml`. Clears the 8 deprecation warnings GHA emits for every workflow run ahead of the June 2nd 2026 forced cutover. `Swatinem/rust-cache@v2` stays — upstream maintains v2.x on Node 24.
- **README hero/banner + CLI-only badges**: aligned with the plugin repo's brand presentation (banner asset, brand link, X follow, GitHub stars) but the version badge now tracks `onebrain-ai/onebrain-cli` releases (with `include_prereleases`) rather than the plugin's `@onebrain-ai/cli` npm tag. License badge updated to AGPL-3.0 to match the CLI's actual license.

## v3.0.0-alpha.5 — feat: doctor --fix lands · cleaner --help output

- **`onebrain doctor --fix` now actually attempts repair** instead of emitting a "deferred to v3.0.1" stub. First recipe is `qmd-embeddings` → spawns `qmd embed` and waits for it, streaming the embedder's output. After the fix pass, doctor re-runs all checks and prints a post-fix report so the user sees the new state in a single invocation. Untyped warnings degrade to "manual" with the hint message attached.
- **Removed `(Slice N)` markers from every subcommand description.** They were internal porting bookkeeping and showed up in `--help` (e.g. `Initialize a new vault (Slice 10)`). User-facing strings now drop the parenthetical; internal source comments that reference slice numbers are left as historical context.
- New `FixOutcome { Fixed, Failed, Manual }` enum + summary block ("Fix summary: N fixed · M failed · K manual") so the user can quickly read what actually changed.

## v3.0.0-alpha.4 — perf: faster doctor + warm-cache update --check

- **`update --check` warm-path: 480 ms → 10 ms (~48× faster)** via on-disk JSON cache at `$XDG_CACHE_HOME/onebrain/latest-release.json` with a 1-hour TTL. First call hits GitHub as before; subsequent calls within the hour read the cache instead. New `--fresh` flag bypasses the cache for users who want to force a re-fetch. `ONEBRAIN_RELEASE_CACHE` env var overrides the path for tests.
- **`doctor` wall time: ~980 ms → ~890 ms (~90 ms)** by running the `qmd-embeddings` probe on a background thread while the other 7 cheap checks run serially. Output order is preserved (Bun parity); the win scales as the cheap-check set grows.
- **`qmd-embeddings` probe jitter eliminated** by replacing the 100 ms `try_wait()` poll loop with `wait-timeout`'s blocking `wait_timeout` — the previous loop could sleep past child-exit by up to a full tick.
- **`onebrain update` no longer spawns `onebrain --version` for the current version**, using `env!("CARGO_PKG_VERSION")` instead. Saves ~10 ms per call and removes a PATH dependency (the wrong binary on PATH could previously report a misleading "current" version).
- New unit/integration tests cover the cache hit/miss/staleness paths and the in-process version constant.

## v3.0.0-alpha.3 — fix(parity): close all 6 Bun-CLI argv gaps + init becomes one-step + safety + friendlier release notes

- **`init` now runs `vault-sync` automatically** — collapses the previous 2-step bootstrap (`init` then manual `vault-sync`) into one. Failure is non-fatal: init still exits 0 with a clear "re-run `onebrain vault-sync`" hint. `--no-sync` flag skips the embedded step for offline / CI use
- Close 6 Bun-CLI argv gaps that the Rust port had dropped: `vault-sync --branch <branch>` (was used by `/update` skill mid-flow), `vault-sync [vault_root]` positional, `session-init --vault-dir`, `checkpoint --vault-dir`, `register-schedule --vault`, `init --vault-dir + --force`, `migrate <name> [cutoff_date]` positional (alongside `--cutoff` flag)
- Unify flag surface — every `--vault` flag now accepts `--vault-dir` as a visible clap alias (eliminates "which name does this command use?" footgun)
- `vault-sync` refuses to write at filesystem root (`/`) or `$HOME` literal — defensive guard against `onebrain vault-sync ~` foot-cannons. Arbitrary subdirectories still work, including bootstrap-from-empty-dir
- `migrate <name>` rejects supplying both positional `[cutoff_date]` AND `--cutoff <date>` (clap `conflicts_with` — no more silent precedence)
- GitHub Release body now renders a friendly platform table (macOS Apple Silicon / Intel · Linux ARM64 / x86_64 glibc / x86_64 musl · Windows ARM64 / x86_64) so non-Rust users can pick the right download without parsing target triples. Asset filenames keep their canonical Rust triples for `cargo-binstall` and custom installer scripts
- README rewritten with the platform table + one-step quickstart; CONTRIBUTING.md added covering dev setup, PR conventions (worktree, version bump, English-only, 3-round review), and security-issue channel
- 9 new integration tests covering the new code paths (vault-sync `--branch`, init `--force` / `--vault-dir`, migrate positional / conflicts, register-hooks `--vault-dir` alias) · suite now at 634 passing, 1 ignored, 0 failed

## v3.0.0-alpha.2 — fix(release): Windows TARGET expansion in release pipeline

- Add `shell: bash` to Build/Strip steps so `$TARGET` expands on Windows runners (pwsh default treats it as `$Target:` PowerShell variable namespace) · unblocks 7/7 platform builds (PR #20)

## v3.0.0-alpha.1 — feat(slices-7-13): Bun parity port + 2 v3.0.1 fixes

- Fix `init` reporting `hooks: ok` while `.claude/settings.json` is never written — init now creates `.claude/` before invoking `register-hooks`, so the harness detector finds a claude target and actually writes the Stop hook + 14 permission entries (slice 10 · adds 1 unit + 1 integration regression test)
- Fix `vault-sync` silent non-zero exit when `result.error` is `None` — CLI handler now always prints a `vault-sync: failed:` summary on `!result.ok` (covers any future failure path that forgets to log) and the integration test pins the exit code to exactly `1` rather than just non-zero (slice 13 · adds 1 integration regression test)
- `init` subcommand bootstraps a vault: writes `vault.yml` (update_channel + 8 folder map + checkpoint defaults) · creates the 8 PARA folders + `00-inbox/imports/` · optionally installs a schedule preset (Minimal · Essentials · Maintenance Plus · Skip — names match `_shared/schedule-presets.md`) · best-effort `register-hooks` (failure warned, never fatal) · `--yes` flag for non-interactive CI runs (defaults to Essentials) · `--force` overrides the existing-vault.yml guard · 32 unit + 5 Layer 2 integration tests (PR #16)
- New `onebrain-fs::init` module (`mod.rs` + `wizard.rs` + `folders.rs` + `vault_yml.rs` + `presets.rs`) · five injectable IO closures (`confirm_fn` / `preset_fn` / `register_hooks_fn` / `stdout_lines` / `stderr_lines`) keep unit tests offline and TTY-free · `SchedulePreset` enum + `ScheduleEntry { Skill, Command }` Serde shape matches the existing `vault.yml` `schedule:` block · `inquire 0.7` (new workspace dep) drives the real CLI prompts; tests bypass it entirely (PR #16)
- `vault-sync` subcommand ports Bun's 9-step release-overlay flow: GitHub tarball download (pure-Rust `tar`+`flate2`, no win32 drive-letter footgun) · plugin/.gemini/.obsidian dir overlay with stale-file removal · CONTRIBUTING/CHANGELOG/PLUGIN-CHANGELOG root docs · `@`-import merge into CLAUDE/GEMINI/AGENTS.md · `update_channel` write to vault.yml · `installed_plugins.json` pin (marketplace short-circuit + ENOENT-only orphan dedup + path-normalize match policy + idempotent version refresh + malformed-entry warn-and-skip) · plugin cache prune · indicatif TTY spinner / `vault-sync:` plain non-TTY lines (Bun-parity stdout) · 69 inline unit tests + 6 assert_cmd integration + 1 insta snapshot + 1 parity scaffold (PR #15)
- `register-schedule` subcommand emits launchd plists for `vault.yml` `schedule:` entries · skill mode + command mode + one-shot `at:` · six ops flags (`--dry-run`/`--remove`/`--refresh`/`--resume <skill>`/`--status`/`--test <skill>`) · plist generation via string templating (not `quick-xml`) for byte parity with Bun (verified byte-identical on the recurring-skill snapshot) · 30+ unit + 9 integration + 1 snapshot + 2 parity scaffolds (PR #13)
- New `onebrain-core::scheduler` module ports Bun `src/lib/scheduler/` 1:1: `types` + `cron_parse` + `entry` + `launchd` + `log_paths` + `error` · `SchedulerError` covers 14 named variants for every Bun error string the parity tests assert on · `IndexMap` preserves YAML insertion order for skill-mode args (PR #13)
- One-shot self-deleting plists embed `/bin/sh -c "<cmd>; launchctl bootout gui/<uid>/<label>; rm -f <plist>"` matching Bun byte-for-byte · command-mode binary resolution via the `which` crate · absolute / relative / bare-name dispatch mirrors Bun's `resolveCommandBinary` lexical semantics (no symlink canonicalization) (PR #13)
- New workspace deps: `regex` (cron + at field validation) · `dirs` (portable homedir resolution · stdlib `std::env::home_dir` is deprecated) · `libc` (Unix `getuid` for `launchctl bootout gui/<uid>` field) · `indexmap` (order-preserving args map) (PR #13)
- `--test <skill>` stubbed (deferred to Slice 12 `run-skill`) — flag is parsed, prints the "Testing..." banner, and exits cleanly with a stderr deferral notice (PR #13)
- `Cmd::RegisterSchedule` clap surface expanded from one flag to six · `main.rs` dispatch replaces `todo!()` with `commands::register_schedule::run(...)` (PR #13)
- `register-hooks` subcommand · idempotent `.claude/settings.json` wiring · canonical exec-form Stop hook + conditional PostToolUse `onebrain qmd-reindex` (when `qmd_collection` set in vault.yml) + 14 OneBrain permission entries · legacy shell-form + `checkpoint-hook.sh` + `qmd update …` migration in place · stale-event cleanup (any onebrain-* command under events other than Stop / PostToolUse) · serde_json `preserve_order` so unknown top-level/nested keys survive round-trip in insertion order · flags: `--vault` (rename of Bun's `--vault-dir`) · `--dry-run` (new) · `--remove` (new uninstall path) · 68 unit + 7 Layer 2 integration + 2 Layer 3 snapshot + 3 Layer 4 parity scaffold tests (PR #14)
- `update` subcommand · GitHub releases/latest fetch via `reqwest::blocking` (rustls-tls, no openssl) + atomic install/validate gate · 4 injectable IO closures (fetch / install / validate / current-version) mirror the Bun `runUpdate` API · 20 unit + 6 mockito-backed integration + 2 insta snapshot + 1 parity scaffold · `--check` dry-run flag · `ONEBRAIN_GITHUB_RELEASES_URL` env override for tests · `reqwest 0.12` + `mockito 1` added to workspace deps per spec §2.6 (PR #12)
- `run-skill` subcommand spawns `claude -p "<prompt>" --add-dir <vault>` with the vault as `cwd` and inherited stdio + env (so PATH/HOME survive for Homebrew lookups) · prompt builder namespaces bare names under `onebrain:<skill>` and preserves explicit plugin namespaces · `--arg key=value` repeated, insertion order preserved · `CLAUDE_BIN` env → `$HOME/.local/bin/claude` → `/opt/homebrew/bin/claude` → `/usr/local/bin/claude` → bare `claude` probe order with stderr warning when `CLAUDE_BIN` points to a missing path · exit codes: 78 missing vault.yml · 127 spawn error · 128+sig signal termination (Unix) · child code otherwise · 16 unit + 6 inline + 10 Layer 2 integration (mock claude bash script · no real claude needed in PATH) + 1 Layer 3 argv snapshot + 1 Layer 4 parity scaffold (PR #11)
- `migrate <name> [--cutoff YYYY-MM-DD] [--vault DIR]` subcommand with idempotent `backfill-recapped` migration (walks `[logs]/session/YYYY/MM/*.md`, adds UTC `recapped:` to session-log frontmatter, preserves insertion order, inclusive ISO cutoff, EACCES/malformed → `skipped++` + stderr) · 21 unit + 6 Layer 2 + 1 Layer 3 snapshot + 2 Layer 4 parity scaffold (PR #10)
- `doctor` subcommand with 8 read-only health checks behind `Box<dyn Check>` trait object: vault.yml · vault.yml-keys (required/soft/deprecated schema) · folders (8 PARA dirs) · plugin-files (.claude/plugins/onebrain integrity + stale .sh detection) · settings-hooks (Stop + PostToolUse exec/legacy/absent form + Bash(onebrain *) permission) · orphan-checkpoints · qmd-embeddings (3s timeout, non-fatal) · claude-settings (stale marketplace repo) · 41 unit tests + 7 Layer 2 integration + 1 Layer 3 snapshot + 1 Layer 4 parity scaffold (PR #9)
- `VaultFolders` extended from 1 (`logs`) to all 8 standard keys (inbox · projects · areas · knowledge · resources · agent · archive · logs) with per-key serde defaults matching Bun `DEFAULT_FOLDERS` (PR #9)
- `--fix` auto-repair deferred to v3.0.1 patch per spec §7.10 slip-handling — flag is parsed but emits a stub stderr message; doctor must be parity-green before GA but fix logic can ship in patch
- `orphan-scan` subcommand with Active-Session Guard (mtime-driven cross-harness live-session detection) and manual session log skip · `CheckpointPolicy { minutes: u32 }` field on `VaultConfig` drives the `max(60min, 2 * cp.minutes)` guard threshold · 38 unit tests + 3 Layer 2 integration + 1 Layer 3 snapshot + 2 Layer 4 parity (PR #3)
- New `onebrain-fs::orphan` module composes 5 internal helpers (`parse_checkpoint_filename`, `parse_frontmatter`, `has_manual_session_log`, `get_newest_mtime_ms`, `is_group_active_or_ambiguous`) with fail-safe propagation: any I/O ambiguity → group skipped rather than counted (Bun symmetry with `/wrapup`) (PR #3)
- `onebrain-core::load_vault_config_at(&Path)` helper for direct-path vault.yml loading without the `VaultRoot` invariant · used by Active-Session Guard threshold derivation (PR #3)
- `.github/workflows/release.yml` 7-platform release pipeline (darwin-{arm64,x64} · linux-{arm64,x64,musl-x64} · win-{x64,arm64}) · tar.gz / zip + sha256 · auto-detects prerelease from `-alpha`/`-beta`/`-rc` tag suffix · user-controlled inputs route through `env:` vars (PR #2)
- README clarifies `onebrain-cli` is the crate name; the produced binary is `onebrain` per `[[bin]]` in `crates/onebrain-cli/Cargo.toml` (PR #2)
- Post-merge fix-ups on PR #3: differentiate ENOENT from EACCES/EIO when reading vault.yml (silent vs stderr warning) · `frontmatter` module made `pub(crate)` to prevent visibility leak · scattered imports consolidated to top of `orphan.rs` · boundary tests added (`age == guard` counted · `minutes: 0` falls back to floor) · `.gitkeep` so empty parity fixtures survive git clone
- `CHANGELOG.md` reformatted to onebrain repo's compact style — frontmatter (`latest_version`, `released`) · conventional-commit-style per-version titles · flat detailed bullets ≤ 8 per version (PR #5 reformats PR #4's initial)
- GitHub repo metadata: description set · homepage `https://onebrain.run` · topics (rust, cli, obsidian, onebrain, ai-agent, claude-code) · main branch ruleset (5 required checks · squash-only · linear history · resolve threads · dismiss stale reviews)

## v3.0.0-alpha.0 — feat(slice-1): session-init + 4-crate workspace foundation

- 4-crate Cargo workspace: `onebrain-core` (types/config/path) · `onebrain-fs` (vault walks) · `onebrain-cache` (session token, qmd status) · `onebrain-cli` (binary · clap dispatch with all 13 subcommands scaffolded · 12 still `todo!()`) · workspace inheritance via `*.workspace = true` discipline · `publish = false` workspace-wide
- `session-init` subcommand with 8-layer session token resolution (Bun v2.3.3 parity): WT_SESSION → TMUX_PANE → TERM_SESSION_ID env vars (stripped + truncated to 8 chars) → `findClaudeAncestorPid` walk-up via `ps -o ppid=,comm=` (12-hop cap · Unix only) → `$TMPDIR/onebrain-day-YYYYMMDD.token` day-scoped cache → process ppid → PowerShell parent PID (Windows stub) → 5-digit numeric random fallback
- `qmd_unembedded` count sourced from spawning `qmd status --json` (matches Bun) instead of the originally-specced filesystem-walk approach · 2-second timeout · returns 0 on any failure · caught during PR #1 fix-up after 3-round review found 7 behavioral divergences from Bun
- Block path: BOTH `find_vault_root` returning `None` AND `load_vault_config` returning `Err` emit `{"decision":"block","reason":"onebrain-init-required"}` · session-init never exits non-zero (matches Bun contract for the Claude Code SessionStart hook)
- 4-layer test pyramid: inline unit + `assert_cmd` integration + `insta` snapshots + golden-master parity vs Bun v2.3.3 (verified byte-identical locally with `BUN_BINARY=~/projects/onebrain/dist/onebrain` · CI parity job fails until v2.3.3 release artifact is uploaded upstream)
- Error model split: `thiserror` typed errors per library crate (`CoreError` / `FsError` / `CacheError`) + `anyhow` propagation in binary with `.context()` chains · `classify_exit_code` walks `anyhow::chain()` to extract wrapped `CoreError` variants for sysexits.h-aligned exit codes (64/65/66/67)
- CI workflow: fmt + clippy + 3-platform test matrix (ubuntu/macos/windows) · `concurrency` block cancels outdated PR runs · `permissions: contents: read` hardening
- AGPL-3.0-only license · Windows ARM64 added to release matrix as the 7th platform per 2026-05-19 decision · forward-compat `tokio` scaffold (`tokio_helper::run_async` with `#[allow(dead_code)]`) ready for v3.1 server mode without restructuring main.rs · 46 tests passing

[Unreleased]: https://github.com/onebrain-ai/onebrain-cli/compare/v3.0.0...HEAD
[v3.0.0]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0
[v3.0.0-alpha.1]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.1
[v3.0.0-alpha.0]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.0
