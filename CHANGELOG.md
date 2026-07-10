---
latest_version: 3.4.7
released: 2026-07-06
---

# OneBrain CLI Changelog (v3.x · Rust)

All notable changes to the OneBrain CLI binary (`onebrain`) in the v3.x Rust rewrite.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

> **Versioning:** CLI version is tracked in workspace `Cargo.toml`. v3.x is the Rust port of [v2.x (TypeScript/Bun)](https://github.com/onebrain-ai/onebrain). `v3.0.0-alpha.1` is the first user-facing alpha (binary artifacts published to GitHub Releases for 7 platforms).

## [3.4.8] — Unreleased

### Breaking
- **Removed `serve --host`.** Every listener binds `127.0.0.1` only, same as the daemon — remote access goes through an encrypted tunnel (docs/daemon.md § Remote access). Containers use the `ONEBRAIN_BIND` env var instead (invalid value = hard error; non-loopback prints the plaintext-HTTP warning). Maintainer-approved breaking change in a patch: zero known users of the flag. (#205)

### Added
- **Self-documenting `onebrain.yml`.** `init` now scaffolds a hand-authored commented template — every key preceded by `# <what it is> · default: <value>`, values interpolated from the runtime's own default fns so template and binary can't drift. The full `search:` block (incl. the v3.4.7 reranker keys) ships active on day one; `collection` stays a commented placeholder (absent = search disabled). ([ADR 0026](docs/decisions/0026-config-self-documentation.md) · [docs/configuration.md](docs/configuration.md)) (#196)
- **doctor `config-values` check (12th check).** Validates every present config value per key against the runtime defaults + model/reranker registries — `update_channel`, `checkpoint.*`, `search.default_top_k`/`embed_model`, `search.reranker.*`, plus non-empty `folders.*`/`search.collection` — one finding per violation, each naming its documented default.
- **`doctor --fix` resets out-of-range tunables to their defaults** through a comment-preserving line editor (comments, key order, inline `# …` notes, and CRLF all survive; every reset itemised in the fix footer as `key → default`). `search.embed_model` resets print a reindex-required warning; `folders.*` + `search.collection` are report-only, never auto-reset.
- **Section layout for `onebrain.yml`.** The template now groups keys under Style-A banners (General → Vault layout → Agent behavior → Search → Automation → System), documents the plugin `recap.min_sessions`/`min_frequency` keys (defaults 6/2), the `schedule:` entry shape, and the `search.exclude`/`search.embed.*` keys (backfill-only), and puts the system-managed `stats:` block last. `doctor` reports layout drift read-only; `doctor --fix` restructures an existing vault into this order — moving each top-level block as opaque bytes so every value and comment survives — and is idempotent. A completeness test walks the config structs so a new key can't ship without its doc entry. ([ADR 0026](docs/decisions/0026-config-self-documentation.md) · [docs/configuration.md](docs/configuration.md)) (#203)
- `onebrain daemon status` is now a full dashboard (#197): process/bind/webui/engine/models sections incl. the clickable `http://127.0.0.1:PORT/?token=TOKEN`; probe failures degrade to absent fields (exit stays 0); JSON gains the same optional fields
- `onebrain serve` is daemon-aware (#197): with a daemon already serving the vault it prints the daemon's webui URL (+ `--open` opens it) instead of binding a second listener; explicit `--port`/`--dir`/`$ONEBRAIN_BIND` still means standalone
- `GET /api/health` reports `dist_dir` (webui source); `GET /api/internal/status` reports `embed_model` (both additive)

### Changed
- **doctor is now strictly read-only outside `--fix`:** the search check resolves the collection without persisting a generated name on BOTH the no-index and index-exists paths (previously a `doctor` run could re-serialize the config and strip its comments), and the `onebrain.yml-keys` recipe no longer serde-rewrites the file for out-of-range checkpoint values — the comment-preserving `config-values` recipe owns value repair.
- **`doctor --fix` gains an honest `partial` outcome** (JSON `fix[].outcome`, text glyph `◐`) for mixed runs where some values reset while others sit in unsupported YAML shapes; value findings now carry a `doctor --fix` hint (checkpoint-only warnings previously had none). Remaining comment-dropping structural writers tracked in #200.
- **Existing vaults get the self-documentation via `doctor --fix`:** the `config-values` check reports template-known keys lacking a comment; `--fix` inserts the template's own `# <what> · default: <value>` line above each (user comments always win, missing keys never added, idempotent) — sourced from the same table the fresh template renders from.
- **vault-sync no longer rewrites `onebrain.yml` on every run:** its `update_channel` step is change-detecting (already-correct config → file untouched — the default `init` and re-sync cases) and, when a change is needed, comment-preserving via the shared `yaml_edit` line editor — the template survives the default install path, plugin updates, and `doctor --fix` plugin repairs.
- **All config writers are now comment-preserving (#200).** The last four whole-file re-serializing writers — `fix_vault_yml_keys`, `fix_legacy_qmd_collection`, and `onebrain-fs`'s `persist_search_key`/`remove_search_key` (first `search reindex`/`model set` + missing-model reconcile) — migrated onto `yaml_edit`: a read-only serde parse classifies the change, then `append_top_level`/`upsert_child`/the new `delete_key` primitive apply it as line edits. User comments survive a full `doctor --fix` on a legacy vault regardless of recipe order; the old "comments not preserved" disclosures are gone; `reconcile_missing_model` now logs the config mutation instead of discarding it. The only remaining `serde_yaml::to_string` writes are template creation and the documented degenerate-root fallback, and `stamp_doctor_run` now declines flow-style roots too.
- **`doctor` search check routes through the warm daemon (#200).** When an `onebrain mcp` session holds the engine, the check reads doc/pending counts from the daemon's `/api/internal/status` (passive discovery — never starts one) instead of opening a second engine and reporting a misleading "index locked"; it falls back to a direct open only when no daemon serves the vault.
- **`doctor` text output redesigned (#200).** A `🩺 Doctor · <vault> · onebrain <version>` header, aligned check rows with NO inline hints, and a bottom boxed `Summary` (shared with `search model list`): tally line, the non-ok findings (fails before warnings), then deduplicated `💡 command → outcome` action lines. The two legacy-migration checks fold into one `migration` row; counts are carried into the `/wrapup`/`search reindex` outcomes. JSON output is unchanged.

### Fixed
- **`search model list`: the Rerankers box can no longer break.** Both tables now share one boxed-table renderer with a 100-column width cap; an over-long registry NOTE is truncated with an ellipsis (unicode-width-aware) instead of blowing the box border past the terminal. (#195)
- **`search status`: Embedding/Reranker section parity.** The "🧠 Model" section header is renamed "🧠 Embedding" (parallels "🎯 Reranker"; emoji unchanged — the `search reindex` summary's matching section is renamed too), and the Reranker section gains a `Downloaded <local date>` row backed by a new `reranker_downloaded_at` field (epoch-seconds dir mtime, `null` when not downloaded) on the status payload — mirroring the embedder's `model_downloaded_at` across all three status builders (direct, held-engine/MCP, daemon). Both sections now render a `Ready` row as their LAST row (Name · Size · Downloaded · Ready); the Reranker section's existing `Ready` row moved from 2nd to last, and the Embedding section gains a display-only `Ready` row (semantic build + active model downloaded) with no new JSON field. `search model list` drops its `📁 Cache dir:` footer from the text render (`search status`'s Cache section owns that path; `cache_dir` stays in the JSON payload). (#195)
- Retired the stale "API-only" wording: the daemon has always served the embedded webui without `$ONEBRAIN_DIST` (which remains a dev/plugin override); a regression test now pins the embedded fallback (#197)

## [3.4.7] — Tier-2 cross-encoder reranker

- Added Tier-2 cross-encoder reranker (`onebrain-rerank-v1`, self-hosted bge-reranker-v2-m3 int8), default-on, replacing the ADR 0024 cosine gate with a calibrated 0–1 score. ([ADR 0025](docs/decisions/0025-tier2-cross-encoder-reranker.md)) (#190, #191)
- Added `search.reranker` config (`enabled`, `model`, `min_candidates` default 10, `min_score` default 0.30); model downloads + sha256-verifies during `reindex`.
- `top_k`/`min_candidates` are now settable on every surface (CLI flags, config, `/api/vault/search` query params).
- `--min-score` now filters the calibrated `rerank_score` when reranking is active (legacy raw-score meaning when off).
- MCP `query` tool now reranks and surfaces `rerank_score` like every other surface.
- Fixed: reindex previously couldn't download the reranker model (wrong accessor path), leaving it inert for every user.

## [3.4.6] — warm daemon + honest search-engine lock contention

- Added: warm daemon (`daemon __run`) owns the native-search engine as sole redb owner; token-gated internal reindex/status/get endpoints + daemon discovery + idle-shutdown TTL. ([ADR 0023](docs/decisions/0023-warm-daemon-mcp-search.md) · [docs/daemon.md](docs/daemon.md)) (#164)
- Added: `onebrain mcp` and CLI search now route through the daemon so multiple concurrent sessions coexist; passive per-vault discovery never disrupts another vault's session. (#168, #169)
- Fixed: search-engine lock contention now surfaces honestly (`E_ENGINE_BUSY` exit 77 for user verbs, silent skip for hooks) instead of misreporting.
- Fixed: `search search` (lex) now populates `heading_path` from the stored tantivy field in a single pass.
- Fixed: auto-started daemon receives its vault via an explicit argument (no env-var mutation); reindex-path confinement now also runs in the engine, not just the HTTP layer. (#175)
- Fixed: honest `E_ENGINE_BUSY`/503 errors during the pre-daemon-to-daemon upgrade transition window instead of opaque `E_INTERNAL`/503 strings. (#179)
- Fixed: semantic search no longer silently returns nothing — per-model `vec_floor` cutoff replaced with a recall-first `keep_top_cluster` cutoff + confidence label. ([ADR 0024](docs/decisions/0024-vector-confidence-recall-first.md)) (#183)

## [3.4.5] — native search · no dependency · auto reindex/embed · model reindex ux/ui (the qmd epic)

- **Breaking:** removed the `onebrain qmd …` command group and the external `@tobilu/qmd` dependency — native `onebrain-search` now powers webui search + the reindex hook; use `onebrain search …` instead (hooks/schedules auto-rewritten on next `plugin update`/`schedule register`; the reindex hook now runs synchronously, tracked in #133).
- **Breaking:** native-search state (model + index + `engine.redb`) now lives in the OS data dir instead of the purgeable cache dir, after macOS cleanup wiped a ~536 MB index (#114); existing state auto-migrates on next command. ([ADR 0021](docs/decisions/0021-search-state-persistent-data-dir.md))
- Fixed: `doctor` now flags a missing index on a configured collection as a possible OS-purge instead of "no index yet"; `search status`/MCP `query` degrade honestly with no index.
- Added: auto reindex/embed hook — `search reindex --lex-only` on PostToolUse, `--pending-only` on Stop; both auto-migrate from the old qmd hook entries. (#133)
- Added (transition): `session init --json` emits the canonical `search_unembedded` key alongside the deprecated `qmd_unembedded` alias.
- Internal: renamed `HookSpec::QMD` → `REINDEX`, removed the dead `.obsidian/` seeding path. Closes onebrain-ai/onebrain-cli#142.
- Fixed: `plugin update` on a vault with no `update_channel` no longer 404s — absent/unknown channel now defaults to `main` instead of the nonexistent `next` branch.

## [3.4.4] — 2026-07-03 — scheduler runs actually fire

- Fixed: scheduled cron skills no longer exit 78 (EX_CONFIG) — `skill run` now prepends its own binary dir to the headless `claude` child's PATH. (#124)
- Fixed: generated plists use the current `skill run` subcommand instead of the deprecated `run-skill` alias, so scheduled runs stop logging a deprecation notice. (#125)

## [3.4.3] — 2026-07-03 — scheduler fixes + housekeeping

- Scheduler cron now accepts step (`*/N`), list (`a,b,c`), and range (`a-b`) syntax per field, emitted as launchd `StartCalendarInterval` arrays. (#116)
- Scheduler command-mode plists are now disambiguated by their args so two entries for the same binary no longer collide; `schedule register` auto-migrates stale pre-#116 plists. (#116)
- Scheduler cron `weekday` now accepts the standard `0`-`7` range (both mean Sunday), normalizing `7`→`0`.
- Scheduler cron now rejects strings that restrict both day-of-month AND day-of-week (cron ORs them, launchd ANDs them) — use two `schedule:` entries instead.
- Scheduler cron combination cap raised from 366 to 1000, accepting the "every day of every month" idiom while still rejecting `*/1 */1 * * *`.
- `onebrain schedule list` is now implemented (was a stub), reusing the existing status view. (#116)
- CI now runs the lex-only (`--no-default-features`) test suite alongside clippy. (#119)
- Polish (#120): `SearchMcpServer` renamed to `McpServer`; `get` tool documents line clamping; `QueryParams` dead-code allowance tightened.

## [3.4.2] — 2026-07-03 — fix: weak server auth token on Windows

- Security fix: `serve`/daemon auth token now comes from the OS CSPRNG (`getrandom`) on every platform instead of a time-seeded fallback that made every Windows token (and any failed-read Unix run) guessable; no fallback remains, an unavailable OS RNG now panics rather than emit a predictable token.
- `getrandom` promoted from a transitive to a direct dependency (already in the graph — no new crate).
- `query`'s camelCase wire test now covers all three `lex`/`vec`/`hyde` sub-query variants (would have caught a `rename_all` typo before it shipped).
- `search status` now opens the engine at the already-resolved cache dir instead of re-resolving the vault + collection.
- Test fixtures write the canonical `onebrain.yml` instead of the legacy `vault.yml`, avoiding a spurious deprecation warning.

## [3.4.1] — 2026-07-03 — native search MCP server

- Added `onebrain mcp` — MCP stdio server (rmcp) over the native engine: `query` (lex/vec/hyde, RRF-fused), `get`, `multi_get`, `status`.
- `session init` now probes the native index for `qmd_unembedded` directly (no qmd subprocess), same JSON contract.
- Model picker: pressing Enter on an active model with missing files (e.g. OS-purged cache) now re-downloads without re-embedding.
- `search status` reports the active model's on-disk size only (was summing every `models--*` dir).
- `dot_scalar` gains a debug-build equal-length assertion; simsimd fallback logs before returning `NEG_INFINITY`.
- ADR 0018 polish: sysroot typo fixed, win-arm64 decision restructured into sub-bullets.

## [3.4.0] — 2026-07-01 — native search engine (`onebrain-search`)

- Added native Rust search engine: tantivy BM25 + fastembed embeddings + flat mmap vector store + RRF hybrid ranking — no Node/Python runtime.
- Added `onebrain search query/search/vsearch/get/status/reindex` (`--json`) plus `search model list/set` and an interactive TTY model picker.
- Multilingual: ~100-language semantic search (default `multilingual-e5-small`, swappable) + no-space-script keyword bigrams for Thai/CJK/Lao/Khmer/Myanmar.
- Swappable embedding model via `search model set` (rebuilds vector store, re-embeds); `bge-m3` is the best-accuracy upgrade path.
- Platform-tiered semantic search (rustls): targets with no ONNX Runtime prebuilt ship a lex-only binary, gated by the `semantic` cargo feature. ([ADR 0017](docs/decisions/0017-platform-tiered-semantic-search.md))
- Runs alongside qmd (engine milestone only) — MCP swap and qmd removal land in follow-up milestones.
- Release cross-toolchains fixed so all 9 targets build (aarch64-linux-gnu g++, arm64 Windows MSVC toolset), plus a main-branch review sweep (webview redirect off-by-one, translate error logging, gzip robustness/hardening).

## [3.3.27] — 2026-07-02 — translate bridge for select-to-lookup

- Added `POST /api/translate` — server-side bridge to Google's free gtx endpoint, powering the WebUI select-to-lookup Translate action (5,000-char cap, 8s timeout, fixed host).
- Fixed: webview preflight now resolves scheme-relative and absolute-path redirect `Location`s (RFC 3986) — th.wikipedia's `Special:Search` redirect was wrongly reported unframeable.

## [3.3.26] — 2026-07-02 — release embeds the prebuilt webui dist

- Release workflow now downloads the prebuilt webui dist (from onebrain-webui's own GH Release tarball, sha256-verified) instead of rebuilding it — releases are minutes faster and reproducible.
- Fail-closed: missing/malformed pin metadata, missing asset, or hash mismatch aborts the release loudly.

## [3.3.25] — 2026-07-01 — webview preflight route

- Added `GET /api/webview/preflight?url=` — inspects `X-Frame-Options`/CSP `frame-ancestors` so the web UI can decide iframe-embed vs new-tab.
- Fail-safe: any probe failure (bad scheme, network error, timeout) degrades to `frameable:false`, never an HTTP error.

## [3.3.24] — 2026-07-01 — serve robots.txt (the one unauthenticated route)

- Added `GET /robots.txt` served without a token (private-instance `Disallow: /`) — the one exemption to the whole-surface token gate; fixes Lighthouse SEO 91 → 100.
- Verb-restricted to GET/HEAD only so the exemption never widens the CSRF surface.

## [3.3.23] — 2026-07-01 — gzip-precompress the embedded web UI

- Precompressed web UI assets (gzip at build time); `serve` detects the gzip magic and serves with `Content-Encoding: gzip` — release binary ~16.2 MB → ~9.3 MB (−43%).
- Zero new dependencies — pure-Rust `flate2` fallback only for clients without `Accept-Encoding: gzip`.
- No effect on non-`serve` commands or non-`assets/` files; detection is by gzip magic bytes.

## [3.3.22] — 2026-07-01 — serve banner + embedded web UI version

- `onebrain serve` now reports the bundled web UI version + release date from `version.json`/`changelog.json`.
- Prettier startup banner — framed, emoji-prefixed layout mirroring the session-greeting look.
- `server::{webui_version, webui_released}` + pure `parse_*` helpers added, unit-tested; dist's `version.json`/`changelog.json` served as static assets too.
- No behavior change to routing/auth — startup output only.

## [3.3.21] — 2026-06-30 — coverage phase 3d (dispatch.rs exit-code integration tests)

- test(cli): +9 assert_cmd tests cover `dispatch()` `process::exit` arms — `v31/dispatch.rs` 91.08% → 95.64%.
- Core line coverage 95.03% → 95.21%.
- Residual `dispatch()` arms (real network/subprocess/TTY paths) documented in `docs/coverage.md`.
- No behavior change — tests + docs only.

## [3.3.20] — 2026-06-30 — coverage phase 3b + 3c (server/api.rs + command residuals)

- test(server): +28 oneshot/unit tests cover the JSON API handlers — `server/api.rs` 69.56% → 87.06%.
- test(cli/fs): +47 tests close residual command-layer branches — `dispatch.rs` 88.69%→91.08%, `onebrain-fs/update` 89.62%→92.62%, `register_schedule.rs` 91.30%→93.09%, `doctor.rs`→94.21%.
- Core line coverage 94.28% → 95.03%.
- Documented the realistic coverage ceiling in `docs/coverage.md` (100% unreachable on stable; genuinely-unreachable lines listed as residuals).
- No behavior change — tests + docs only.

## [3.3.19] — 2026-06-30 — coverage phase 3 (fs cluster)

- test(fs): +94 tests close coverage gaps across the onebrain-fs cluster (`note/archive.rs`, `init/mod.rs`, `vault_sync/pin.rs`, `register_hooks/*`, `doctor/vault_yml_keys.rs`, `v31/hook_rewriter.rs`, and more).
- Tests target real error/edge paths with meaningful assertions; permission-denial tests are `#[cfg(unix)]`-gated.
- Core line coverage 93.62% → 94.28%; residuals tracked in `docs/coverage.md`.
- No behavior change — tests only.

## [3.3.18] — 2026-06-29 — coverage phase 2 (command modules)

- test(cli): closes coverage gaps in the command-module layer — `doctor.rs` 87.55%→94.20%, `register_schedule.rs` 72.08%→91.30%, `vault_ctx.rs` 51.35%→100%, `run_skill.rs` +110 tests.
- Core line coverage 92.59% → 93.62%; residuals documented in `docs/coverage.md`.
- Test isolation hardening: plugin-cache/qmd-embeddings fix-path tests now run via subprocess with a tempdir `$HOME`/`PATH`.
- No behavior change — tests only.

## [3.3.17] — 2026-06-29 — fix `onebrain update` hang on Homebrew + tighter --help indent

- Fixed: `onebrain update` no longer hangs on Homebrew — Homebrew 4.4+'s "proceed? [y/n]" prompt was corrupted by the install spinner redrawing the TTY; `HOMEBREW_NO_ASK=1` fixes it.
- style(cli): tighter `--help` layout — category headings flush left, commands indent 2 spaces.

## [3.3.16] — 2026-06-29 — coverage foundation + dispatch tests

- test(cli): adds `scripts/coverage.sh` + `docs/coverage.md` (excluded-files list + rationale + baselines); targets 100% line coverage on testable core code.
- test(cli): covers `v31/dispatch.rs` stub + verb arms — 76.94% → 86.70% line.
- Measured baselines: whole-workspace 89.58% line; core (exclusions applied) 92.59% line. No behavior change.

## [3.3.15] — 2026-06-29 — categorized root --help

- feat(cli): groups root `--help` commands into 4 named category sections (⚙️ System Management, 🧠 Vault Management, 🔄 Session Management, 🚀 Launch Management).
- Category headings show emoji on a terminal, render plain when piped, so `onebrain --help | cat` stays clean.
- Descriptions pulled live from clap `about` annotations — can't drift from source of truth.
- Subcommand help (`onebrain note --help`, etc.) is unchanged.
- Drift-guard test: CI fails if any visible root subcommand is missing from CATEGORIES or a category entry is stale.
- Options section keeps its compact format, unaffected by the categorized block injection.
- Fixed `is_root_help_request` to not intercept `--version`/`-V`.

## [3.3.14] — 2026-06-29 — surface note + task in --help

- feat(cli): surfaces the `note` and `task` command groups in `onebrain --help` — all 14 `note` verbs + `task list` were implemented but hidden.
- Stub verbs `task add`/`task done` stay hidden until implemented; all-stub groups and v3.0 legacy aliases remain hidden.
- Added tests asserting `note`/`task` visibility and stub-group hiding.

## [3.3.13] — 2026-06-29 — fence-aware task scan + task list verb

- fix(fs): `scan_tasks` now skips checkbox lines inside fenced code blocks — demo/fixture tasks no longer pollute task scans (also fixes `/api/vault/tasks`).
- feat(cli): implements `onebrain task list` — fence-aware dated-task listing with `--due-by`, repeatable `--folder`, `--all`.

## [3.3.12] — 2026-06-28 — serve: --dir help matches the embedded UI

- docs(serve): `--dir` help text updated from stale "API-only" wording to "serve the embedded UI" (matching the v3.3.10 embed).

## [3.3.11] — 2026-06-28 — serve: embedded-UI banner + API hardening

- fix(serve): startup banner now correctly reports `dist: (embedded web UI)` for a no-`--dir` run.
- fix(serve): OWASP A03 — `GET /api/vault/file`/`/raw` now refuse vault tooling dirs (`.git`/`.obsidian`/`.claude`/`.trash`/`node_modules`), matching the write paths.
- fix(serve): OWASP A03 — the `claude` chat subprocess argv ends options with `--` so a message starting with `-`/`--` can't be smuggled as a flag.

## [3.3.10] — 2026-06-27 — serve: qmd-backed vault search

- feat(serve): new `GET /api/vault/search?q=&mode=lex|hybrid` shells out to the `qmd` index for the web UI's search panel.
- fix(serve): the endpoint returns 503 when `qmd_collection`/`qmd` binary is missing, falling back to filename/path search.

## [3.3.9] — 2026-06-27 — serve: web UI preview support (framing, media)

- fix(serve): security headers relaxed to `SAMEORIGIN`/`frame-ancestors 'self'` so the web UI can frame its own `/api/vault/raw` to preview PDFs.
- fix(serve): CSP `img-src` now allows `blob:` so pptx-preview embedded media can load.
- feat(serve): `/api/vault/raw` sends audio/video content-types and honors `Range` requests for native `<audio>`/`<video>` streaming.
- fix(serve): hardened `/api/vault/raw` against stored XSS now that same-origin framing is allowed — script-carrying types served as `application/octet-stream` + attachment disposition.
- fix(serve): OWASP hardening — pinned `ONEBRAIN_TOKEN` now requires ≥32 chars; the `claude` subprocess no longer inherits it.

## [3.3.8] — 2026-06-27 — serve: download keeps the original filename

- fix(serve): `GET /api/vault/raw?download=1` now sends the file's real name via RFC 5987 `filename*`, preserving spaces/non-ASCII names on download.

## [3.3.7] — 2026-06-26 — serve: allow data: fonts for the Office-doc preview

- fix(serve): CSP now allows `data:` fonts so the Office-document preview can render embedded slide/text fonts.

## [3.3.6] — 2026-06-26 — serve: security hardening (token gating · CSP · stable token)

- feat(serve): the whole router is now token-gated (every route/method) via header, bearer, query param (GET/HEAD only), or cookie.
- feat(serve): a security-headers middleware sets CSP, `X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`, COOP, and HSTS on https.
- feat(serve): `resolve_token` honors `$ONEBRAIN_TOKEN` (≥16 chars) so the token can stay stable across restarts.
- fix(serve): chat request bodies are capped; `serve` warns when binding a non-loopback address over plain HTTP.

## [3.3.5] — 2026-06-26 — tasks: scan projects + areas only

- fix(tasks): `GET /api/vault/tasks` now scans only the configured project + area folders instead of the whole vault.

## [3.3.4] — 2026-06-26 — doctor qmd: unknown-not-zero parity

- fix(doctor): the qmd-embeddings check now reports "qmd status unavailable" on incomplete/corrupted probe output instead of inventing "0 unembedded".

## [3.3.3] — 2026-06-26 — qmd probe: one shared source of truth · 15 s timeout · null-not-zero

- fix(qmd): session-init's unembedded count and `qmd status` no longer report a false `0` when `qmd status` is slow — shared probe timeout bumped 2s → 15s.
- perf(session-init): startup probe keeps a tighter 5s cap so a hung qmd can't freeze the greeting; degrades to `null` on timeout.
- feat(session-init): `qmd_unembedded` is now `null` (not `0`) when the probe can't determine the count, distinguishing unknown from a genuine zero.
- fix(qmd): robust `qmd` resolution — probe now looks in the bun-global dir so a restricted-PATH launcher (hook/launchd/Obsidian terminal) can find it.
- refactor(qmd): unified the duplicated qmd-status probes into one shared `onebrain-cache::qmd` source of truth.
- `serve`/`daemon` default port changed from `4317` to `6789` (collided with OpenTelemetry OTLP); override with `--port` as before.
- chore(license): relicensed from `AGPL-3.0-only` to `MIT OR Apache-2.0`.

## [3.3.2] — 2026-06-25 — note edit / delete / mkdir CLI verbs

- feat(note): `onebrain note edit <path> <content>` — verbatim overwrite/create via shared `write_note` primitive.
- feat(note): `onebrain note delete <path>` — move a note to `.trash`.
- feat(note): `onebrain note mkdir <path>` — create a folder.
- These are the CLI counterparts to the v3.3.1 daemon write endpoints — both surfaces now share one implementation.

## [3.3.1] — 2026-06-24 — daemon write / media / chat surface

- feat(daemon): note write surface — `POST/PUT/DELETE /api/vault/file`, `POST /api/vault/move` (rewrites incoming wikilinks), `POST`+`DELETE /api/vault/folder`.
- feat(daemon): `GET /api/vault/raw` (image/PDF preview) and `POST /api/vault/upload` (binary attachments), behind a body-size limit.
- feat(daemon): `GET /api/vault/tasks` — vault-wide dated Obsidian-Tasks scan.
- feat(daemon): `POST /api/chat` — SSE stream over a `claude -p` agent turn (concurrency-capped, process-group kill on disconnect).
- feat(auth): per-session token accepted via `?token=` query on GET/HEAD only; writes stay header-only.
- refactor(core): handlers are thin veneers over shared `onebrain_fs` primitives — CLI and daemon share one implementation per vault operation.

## [3.3.0] — 2026-06-05 — daemon foundation + HTTP surface

- feat(daemon): `onebrain daemon start|stop|status` — self-respawning detached process tracked by `daemon.pid`.
- feat(serve): `onebrain serve [--dir] [--port] [--host] [--open]` brings up one local HTTP surface (static SPA + read-only vault JSON API); per-session token gates `/api/*`.
- deps: net-new compiled crates are `axum 0.8` + `tower` + `tower-http`, `tracing`, `nix`.

## [3.2.21] — 2026-05-30 — cache-clean hardening

- fix(cache-clean): orphan cache dirs under an unregistered marketplace are now swept even when a registered marketplace exists.
- fix(cache-clean): `remove_dir_all` failures are now surfaced (counted + stderr warning) instead of silently dropped.
- Verified the Step 9 sweep runs unconditionally on every real Claude update.

## [3.2.20] — 2026-05-29 — completions: exclude hidden commands

- fix(cli): shell completions no longer list hidden/internal/legacy subcommands — generated from a recursively hidden-filtered command tree.

## [3.2.19] — 2026-05-29 — shell completions

- feat(cli): `onebrain completions <SHELL>` — hidden subcommand emitting a shell completion script (bash/zsh/fish/powershell/elvish).
- feat(cli): optional shell-aware hint after interactive `onebrain init`; enables Homebrew formula completion auto-install.

## [3.2.18] — 2026-05-29 — dependency + size cleanup (reqwest→ureq · serde_yaml_ng · async-stack drop)

- perf/size: `reqwest` → `ureq` (blocking sync HTTP) removes the entire async stack from the release binary — −342 KB, −54 crates, ~12% faster clean build.
- Internal: removed the dead `tokio_helper` runtime shim (zero callers); the daemon (v3.3) re-introduces `tokio` deliberately.
- Dep: `serde_yaml` (archived upstream) → `serde_yaml_ng`, an actively-maintained drop-in.
- Internal: dropped the unused `clap` `env` feature.
- Internal: unified the two `plugin update` text renderers into one, removing a trait that existed only for test doubles (PR #57).
- Internal: renamed `vault_sync::run_silent` + `register_schedule::run_quiet` → both `run_embedded` for naming consistency (PR #57).

## [3.2.17] — 2026-05-29 — `onebrain update`: refresh Homebrew tap before upgrade + dedicated npm channel

- Fix: `onebrain update` on a Homebrew install now refreshes the `onebrain-ai/onebrain` tap before `brew upgrade`, so a fresh formula is visible immediately after a release.
- Feat: `onebrain update` now has a dedicated npm channel — an npm-installed binary updates via `npm install -g @onebrain-ai/cli@<version>` instead of the Direct swap path.

## [3.2.16] — 2026-05-29 — plugin-cache doctor check + orphan cleanup + post-update reload hint

- Fix: stale plugin-cache orphans no longer silently shadow the vault-local plugin — `doctor` now detects orphans created outside an update.
- Feat: new `doctor` `plugin-cache` check reports stale cached plugin versions; `--fix` prunes them.
- Feat: `plugin update` prints a post-update reload next-step (`↻ /reload-plugins …`) whenever a real version change lands.

## [3.2.15] — 2026-05-28 — `--help` compact-with-wrap · plugin update polish · per-command emoji · version tracking · `--json` minified

- **Breaking:** `--help` reverts to compact layout (command + description on one line) — `next_line_help` no longer forces every arg into long format; args with `[default]`+`[possible values]` still wrap the value block to an indented line.
- Polish: per-command framed-header emoji differentiated — `doctor` → 🔬, `update` → 🚀, `plugin update` → 🔄 (was all 🧠, competing with the brand glyph).
- Polish: `plugin update` no longer leaks the orchestrator's per-step `▸ <label>` lines above its framed report — routes through `vault_sync::run_silent`.
- Feat: `plugin update` now reports current + latest version explicitly (`vX → vY` / `vX · up-to-date` / `installed vY`); JSON envelope gains `version_before`/`version_after`.
- **Breaking:** `--json` (and `--output json`) now emits minified single-line JSON by default — pass `--json --pretty` for indented output.
- **Breaking:** `--output table` and `--output tsv` removed (both silently fell through to the JSON encoder unchanged); remaining set is `text`/`json`/`yaml`.
- Polish: `skill run --help`/`harness run --help` reverted to the compact one-line style — Options section stays compact, `[default]`+`[possible values]` still wrap onto an indented line.
- Polish: positional `<NAME>` args on `skill info`/`show`/`bootstrap` (and hidden `bundle` verbs) now carry a description in the Arguments section.

## [3.2.14] — 2026-05-28 — `plugin update` animated spinner pacing (doctor/update parity)

- Polish: `plugin update` now animates its three step rows with the same braille spinner + random 800–2000ms pacing that `doctor`/`update` use.
- Internal: new `render_plugin_update_animated`/`_to` pair with an injectable `Write` + step-delay override for deterministic spinner tests.

## [3.2.13] — 2026-05-28 — `plugin update` UX polish: framed report (doctor-style) · silenced sub-output

- Polish: `plugin update` now renders a framed doctor-style report instead of a key:value summary.
- Polish: removed "OneBrain Vault Sync" intro/outro frame leakage via a new `vault_sync::run_embedded` helper.
- Polish: silenced `register_schedule`'s per-plist `✓ Wrote …` chatter when invoked from `plugin update`.
- Polish: non-TTY (CI/scheduler/piped) sub-output is now silenced too via `PlainProgress::with_embedded`.
- Fix: partial-failure path no longer paints the failing step with `✓` — now renders `✗ … failed` matching the footer glyph.

## [3.2.12] — 2026-05-28 — `--help` long-format · `[default]` / `[possible values]` wrap onto their own line

- Polish: every `--help` screen now uses the long format (description below the option name, `[default]`/`[possible values]` on their own lines) via `next_line_help = true`.
- Restored: `HarnessMode::WithContext`/`AdHoc` variant docs (stripped in v3.2.11 to keep help compact — no longer needed with the long format).

## [3.2.11] — 2026-05-28 — help cleanup: `--help` only · `skill help` → `skill show` · `harness run --help` compact · banner consistency

- **Breaking:** `<group> help` subcommand removed across the tree — use `onebrain <group> --help` everywhere.
- Rename: `skill help <NAME>` → `skill show <NAME>` (distinguishes SKILL.md body from clap `--help`); same rename for the hidden `bundle help` → `bundle show`.
- Fix: bare `onebrain harness` now emits the brand banner before showing help (missed `arg_required_else_help` group hops).
- Fix: `onebrain skill show <NAME>` no longer prints the banner twice.
- Polish: `harness run --help` rewritten to the compact one-line style used by `skill run --help`.
- Polish: no more banner above "unrecognized subcommand 'help'" errors; `MissingSubcommand` wired into the banner-gate interception path.

## [3.2.10] — 2026-05-28 — `skill info` / `skill help`, harness/skill UX polish, `--json` passthrough

- Feat: `onebrain skill info <NAME>` — prints a skill's frontmatter (name/description/schedulable/required_args); JSON/YAML supported.
- Feat: `onebrain skill help <NAME>` — prints the SKILL.md body; text dumps markdown verbatim, `--json` wraps as `{name, body}`.
- Feat: `--json` on `skill run`/`harness run` now passes through to the harness (`--output-format json`) so captured stdout is the harness's native structured response.
- Polish: bare `onebrain harness` now prints help instead of silently running `detect`.
- Polish: `harness run`/`skill run` descriptions rewritten to surface `--harness`/`--model`/`--mode` inline at the group-help level.

## [3.2.9] — 2026-05-28 — `harness run` polish: spinner subject + true ad-hoc

- Fix: `--mode ad-hoc` now actually skips vault context — forces `cwd = $TMPDIR` so `claude`/`gemini` can't auto-walk-up and silently reload OneBrain's `CLAUDE.md`.
- Polish: `harness run`'s watched spinner now says "on the prompt" instead of "on the skill" (copy-paste leak from `skill run`).

## [3.2.8] — 2026-05-28 — `onebrain harness run` (ad-hoc prompts through claude / gemini)

- Feat: `onebrain harness run [PROMPT]` — send an ad-hoc prompt to the chosen agent harness (`--harness {claude,gemini}`, `--model`); reads stdin if `[PROMPT]` is omitted.
- Two modes via `--mode {with-context,ad-hoc}`: with-context loads the vault's CLAUDE.md/INSTRUCTIONS.md (vault required); ad-hoc skips the vault flag entirely (`cwd = $PWD`).
- Internal: refactored the shared spawn path (`harness_argv`) so both `skill run` and `harness run` reuse `spawn_harness`, the in-place spinner, and output capture.

## [3.2.7] — 2026-05-28 — `skill run` in-place spinner (no more heartbeat scrollback)

- UX: `skill run` shows an in-place `indicatif` spinner on a watched run, replacing the per-10s heartbeat that flooded scrollback during long runs.
- Internal: pipes the harness's stdout/stderr into in-process buffers via two reader threads while `child.wait()` blocks, so a `wait()` error can still kill the harness instead of leaking an orphan.

## [3.2.6] — 2026-05-28 — `skill run` harness + model selection · faster headless runs

- Feat: `skill run --harness {claude,gemini}` (default `claude`) — run a OneBrain skill through either agent runtime; gemini uses `--approval-mode yolo` to match `claude -p`'s trust model.
- Feat: `skill run --model <m>` — passed through to the harness; the biggest raw-speed lever for headless runs.
- Perf: headless runs skip the interactive startup ceremony — `skill run` sets `ONEBRAIN_HEADLESS=1`, `session init` reports `headless: true`.
- Internal: generalized claude-only binary resolution to `resolve_claude_bin`/`resolve_gemini_bin` over a shared `resolve_bin`.

## [3.2.5] — 2026-05-27 — checkpoint hook actually fires now

- Fix: the auto-checkpoint safety net never fired — two compounding root causes left `07-logs/checkpoint/` empty across every session.
- Root cause 1: session token churned (terminal env vars unset in Obsidian/Desktop) so the message counter never accumulated across restarts.
- Fix: `CLAUDE_CODE_SESSION_ID` is now the top-priority token source — stable across PID churn and distinct sessions sharing one terminal.
- Root cause 2: the 30-minute time threshold was dead for a session's first checkpoint (`last_ts` stayed 0).
- Fix: anchor `last_ts` on the first stop so the minutes threshold starts ticking immediately.

## [3.2.4] — 2026-05-27 — doctor `--fix` UX overhaul · qmd timeout · skill-run feedback

- `doctor --fix` is now one pass with a confirmation step: report shown once, planned fixes previewed, then a `[y/N]` prompt confirms before anything changes.
- Feat: `doctor --fix` creates missing vault folders via a new `folders` recipe, named from `onebrain.yml`.
- Fix: `doctor` qmd check timeout raised 3s → 15s — a real index could take ~10s for `qmd status`, causing spurious timeouts.
- Polish: `doctor` frame rules now span the longest line instead of stopping short.
- Feat: `skill run` shows progress on an interactive TTY (start line + elapsed heartbeat) while `claude -p` runs.
- Feat: `skill run` accepts `--skill <name>` as an alias for the positional name.
- Polish: `--vault` is the single documented vault flag; `--vault-dir` becomes a hidden back-compat alias everywhere.
- Chore: removed the dead `.ci-trigger` scaffold file.

## [3.2.3] — 2026-05-27 — `skill run` fixes · `--vault` everywhere · doctor stamps last-run

- Fix: `onebrain skill run` now resolves the vault through the canonical chain (`--vault` → `ONEBRAIN_VAULT` → walk-up from cwd) instead of demanding an explicit path.
- Hardening: `skill run` gives the spawned `claude -p` a null stdin so it can't block reading an inherited interactive TTY.
- Fix: global `--vault` accepted on every command — `skill run`/`schedule register`/`plugin migrate` renamed their local field to `vault_dir` to stop colliding with the global arg id.
- Feat: `onebrain doctor` stamps `stats.last_doctor_run`/`last_doctor_fix` in `onebrain.yml` on every run.

## [3.2.2] — 2026-05-27 — animated `update` + banner/doctor polish

- Feat: `onebrain update` gets an animated TTY — framed header + braille spinner on the `fetch`/`install` phases (matching `doctor`).
- Polish: banner vertical gradient — a top-lit shade layered on the horizontal cyan→purple→pink hue; non-truecolor fallback is now a vertical-only gray ramp.
- Polish: `doctor` spinner now visibly rotates and paces 800–2000ms per check; summary-footer rule widened to span the verdict line.
- Internal: the framed header, braille spinner frames, and pacing band extracted into `output::` so `doctor`/`update` share one look.

## [3.2.1] — 2026-05-27 — doctor grouped UX + braille spinner · qmd-hook fix · logo banner gradient

- Feat: `onebrain doctor` redesign — 9 checks grouped into 4 sections under a `🧠 OneBrain Doctor · <vault>` header, via a new reusable braille-spinner progress primitive.
- Fix: `doctor` qmd-hook false "missing" — the detector now recognizes both the canonical `qmd reindex` form and the legacy `qmd-reindex` alias; `--fix` migrates + dedups.
- Feat: banner wordmark gradient — continuous horizontal cyan→purple→pink gradient across `ONEBRAIN` in truecolor.
- Polish: `onebrain update`'s post-update hint now names the direct `onebrain plugin update` path alongside `/update`.

## [3.2.0] — 2026-05-27 — `note` resource group (11 verbs)

- Feat: `onebrain note <verb>` — 11 native vault note operations (`search`/`list`/`find`/`read`/`stat`/`backlinks`/`orphans`/`append`/`new`/`archive`/`move`) replacing ad-hoc `grep`/`ls`/`find`/`cat`.
- All verbs emit the canonical `Envelope<T>` (text/json/yaml), vault-required, backed by 100+ fs-layer + CLI unit tests plus a 22-case fixture-vault integration suite.

## [3.1.5] — 2026-05-26 — fix: `onebrain update` false-negative binary validation

- Fix: `onebrain update` no longer reports "Binary validation failed" after a successful upgrade — the post-install validator expected Bun's `v`-prefixed version shape, not the Rust/clap `onebrain 3.1.4` output.
- Hardening: the post-install gate now confirms the PATH-resolved `onebrain` actually reports the just-installed version (`>= expected`), surfacing the specific failure cause.

## [3.1.4] — 2026-05-26 — self-update hardening: SHA-256 verification + Homebrew-aware update

- Feat: `onebrain update` verifies the downloaded binary's SHA-256 against the published `<archive>.sha256` before the swap — an unverifiable asset is now a hard failure.
- Feat: Homebrew-aware `onebrain update` — a brew-managed install now delegates to `brew upgrade onebrain` instead of swapping the Cellar binary in place.

## [3.1.3] — 2026-05-26 — `schedule register` reads `onebrain.yml`

- Fix: `onebrain schedule register` now dual-reads the config (canonical `onebrain.yml` preferred, legacy `vault.yml` fallback) — it hardcoded `vault.yml` and silently found zero schedule entries on a v3.1 vault.

## [3.1.2] — 2026-05-26 — implement `qmd embed`

- Feat: `onebrain qmd embed` implemented (was a stub) — runs `qmd embed` in the foreground with inherited stdio, surfacing a non-zero exit as an error.

## [3.1.1] — 2026-05-26 — config-loss fix + backups · doctor label rename + animated TTY · `qmd status`

- Fix (data loss): `onebrain init --force` no longer clobbers an existing config — re-init now preserves `onebrain.yml` verbatim; missing keys are repaired by `doctor --fix` instead.
- Feat: timestamped config backups — every config-overwriting operation first copies to `.onebrain-backups/<file>.<timestamp>.bak`, refusing the write if the backup fails.
- Fix: doctor check labels renamed `vault.yml` → `onebrain.yml`/`onebrain.yml-keys` to match the canonical filename.
- Fix: stale `vault.yml` references in user-facing output (help text, error messages) updated to `onebrain.yml`.
- Feat: `onebrain qmd status` — reports index + embedding health (collection/indexed/embedded/pending/size/updated) in text/json/yaml.
- Fix: `session init` unembedded count now works and is vault-aware — parses the text form instead of `--json` (which qmd ignores).
- Feat: animated `doctor` on an interactive TTY — checks reveal one at a time with a short per-step delay.

## [3.1.0] — 2026-05-25 — Consistency Standard · locked command tree · canonical JSON envelope

- Feat: R1 branded banner — 5-line FIGlet "Slant" `OneBrain` wordmark + tagline on interactive sessions and every `--help` screen, gated on a 6-rule TTY chain.
- Feat: locked 27-entry command tree — 3 root verbs + 24 resource groups, singular-noun 2-level `onebrain <noun> <verb>`; other 200+ verbs stubbed with `E_NOT_IMPLEMENTED` (exit 72).
- Feat: `--vault` global flag + walk-up resolver + `ONEBRAIN_VAULT` env, documented priority order, surfaced by new `onebrain vault current`.
- Feat: `plugin update` semantic swap — now self-updates the CLI binary (was `onebrain update`); the legacy plugin-overlay behavior moves under `plugin update`'s vault-side step.
- Fix: `onebrain init` now registers the plugin with Claude Code AND prompts before initializing in a non-empty directory.
- Feat: canonical `Envelope<T>` JSON shape + partial-failure contract (`E_PLUGIN_UPDATE_PARTIAL`); `BrokenPipe` on stdout now exits 0 instead of panicking.
- Feat: output-format compliance — interactive commands default to text and honor `--json`/`--json --pretty`/`--yaml`/`--output` consistently via one canonical dispatcher.
- **Breaking:** config file renamed `vault.yml` → `onebrain.yml` — CLI v3.1+ dual-reads for back-compat (one-time deprecation warning on legacy); `doctor --fix` migrates via atomic rename; v4.0.0 drops `vault.yml` support entirely.

## [3.0.x] — post-GA follow-ups

These shipped under the v3.0.x patch line after the 2026-05-22 GA and are not part of v3.1.0 itself.

- npm wrapper source recovered and landed in-repo at `npm-wrapper/` after the original tarball-only source was lost; `engines.node` raised to `>=20`.
- CI auto-publishes the npm wrapper on each stable tag via npm Trusted Publishers (OIDC + `--provenance`, no long-lived token).
- postinstall verifies SHA256 against the published `.sha256` before extracting, closing the gap between attested publish and binary integrity.
- bin shim re-raises signal terminations (`128 + signum`) so Ctrl-C/SIGTERM is distinguishable from a real error in CI.
- README + CONTRIBUTING signpost the new `npm-wrapper/` layout; install table promotes npm + Homebrew out of "planned" (both live since v3.0.0 GA).
- postinstall hardening: retry-with-backoff on HTTP 404, Alpine/musl detection, Windows tar fallback to PowerShell `Expand-Archive`, post-install smoke test, escape-hatch env overrides. (PR #29)
- Raspberry Pi + 32-bit ARM Linux support — release matrix adds `armv7`/`arm-unknown-linux-gnueabihf`; every Pi from 1 to 5 now has a published binary.

## v3.0.0 — Rust rewrite GA · 7-platform release pipeline · stable JSON contracts

- Complete Rust rewrite of OneBrain CLI replacing v2.x TypeScript/Bun — 4-crate workspace, ~10× less memory, 92% smaller binary, startup within 10ms of Bun on warm cache.
- 7-platform release pipeline (macOS Apple Silicon + Intel, Linux ARM64 + x86_64 glibc/musl, Windows ARM64 + x86_64), `cargo-binstall`-ready.
- `onebrain update` fetches binaries directly from GitHub Releases over HTTPS and atomically swaps the running binary — no npm/bun shell-out anywhere.
- Stable JSON output contracts for v3.x (`doctor --json`, `update --check --json`, `update --plan`) — frozen schemas, stability covers v3.x, v4 may break.
- Trust model: downloaded binaries authenticated solely by GitHub's TLS chain — no SHA-256/cosign verification at GA (matches rustup/deno/bun baseline).
- Skill + scheduler ecosystem wired end-to-end — `register-hooks`/`register-schedule`/`run-skill` round-trip with the plugin's hooks; plist generation verified byte-identical to Bun v2.3.3.
- `doctor` ships 8 read-only checks and 5 `--fix` recipes; remaining recipes + Windows zip extraction deferred to v3.0.1.
- Distribution at GA: GitHub Releases + `onebrain update` is the primary path; npm-wrapper and Homebrew tap are planned for the v3.0.x window, not published at GA.

## v3.0.0-alpha.9 — GA candidate: fix `onebrain update` install path · TTY spinner · direct harness · real `--test` · Windows pin

- `onebrain update` install path rewritten to fetch directly from GitHub Releases (alpha.1–alpha.8 shelled out to `bun`/`npm install -g`, which never had the Rust binary published — every real update failed). Downloads over HTTPS (rustls TLS, no checksum verification yet — trust model matches rustup/deno/bun), atomically swaps via tmp + rename. Windows zip extraction intentionally stubbed for v3.0.0.
- TTY spinner + colorized output for `onebrain update`; non-TTY output stays plain-text byte-for-byte; `--json` suppresses all log output.
- `direct` harness lands in `register-hooks` as a first-class no-op — vaults without `.claude/` print "direct mode · no hooks to register" instead of a gemini-only error message.
- `register-schedule --test <skill>` is now a real implementation — builds the same argv launchd would emit, spawns it synchronously, and propagates the exit code.
- `update --plan` JSON now includes `binary_targets[]` enumerating the six published `(triple, ext)` pairs.
- New `UpdateError::Install(String)` variant replaces `UpdateError::Network` for filesystem/OS errors during install, so failures no longer misleadingly blame the network.
- `--vault-dir` flag pattern audited across all subcommands (Reviewer C-I4) — user-visible flag name is consistent everywhere; no code change.
- Defense-in-depth: `extract_tar_gz` now guards on `entry_type().is_file()` so a malicious tar can't promote a symlink/dir to "the binary"; deleted the dead bun/npm install-command code path.

## v3.0.0-alpha.8 — feat: JSON output modes for `doctor` + `update` · cosmetic

- feat: `doctor --json` emits a single JSON document (`{ok, summary, checks[]}`); combines with `--fix` for a post-fix `fix[]` array; schema stable for v3.x.
- `doctor --json` outside a vault now emits a JSON failure envelope on stdout with exit code 1, instead of an anyhow plain-text error.
- `update --check --json` emits `{ok, current, latest, update_available, released_at?}`; `update_available` is `null` (not a guessed false) when the remote fetch failed.
- `update --plan` is `--check --json` plus `release_url`/`binary_url_template`, designed for the `/update` plugin skill; implies dry-run.
- `vault-sync --vault-dir <path>` flag-form alternative to the positional argument.
- `register-schedule` resolves `folders.logs` from `vault.yml` instead of hardcoding `07-logs/scheduler/...`, with path-traversal guards.
- 3-round review consensus fix-pass: `version_at_least` promoted to `pub`; `progress_writer` option added to `VaultSyncOptions`; +5 unit tests.

## v3.0.0-alpha.7 — feat(doctor): four new `--fix` recipes (settings-hooks · plugin-files · vault.yml-keys · claude-settings)

- `doctor --fix` now repairs four more check types: `settings-hooks`, `plugin-files`, `vault.yml-keys` (backfills keys, strips deprecated ones, repairs non-positive checkpoint values), `claude-settings`.
- Dispatch widened to Warn AND Error so previously-bypassed failure modes are now repaired by `--fix`.
- Atomic writes everywhere — `vault.yml`/`settings.json` mutations go through `.tmp + rename`.
- `fix_plugin_files` now respects the same `refuse_dangerous_vault_path` guard as `onebrain vault-sync`.
- `orphan-checkpoints` routes to Manual with a clearer hint pointing at `/wrapup` — auto-deletion intentionally off the table.
- Five recipes total ship with the auto-fix flow; the `vault.yml-keys` message notes YAML comments aren't preserved yet.

## v3.0.0-alpha.6 — fix(update): target CLI repo + prerelease-safe · ci: GHA Node 24 · docs: README hero + badges

- `onebrain update` now targets the CLI repo (`onebrain-ai/onebrain-cli`) instead of the plugin repo, fixing a bug where the non-`--check` form could downgrade users to the plugin repo's last Bun release.
- Semver-aware version comparison via the `semver` crate replaces the string-equality check, preventing silent downgrades.
- GitHub Actions Node 24 bump across `ci.yml`/`release.yml`, clearing deprecation warnings ahead of the forced cutover.
- README hero/banner + CLI-only badges aligned with the plugin repo's presentation; license badge updated to AGPL-3.0.

## v3.0.0-alpha.5 — feat: doctor --fix lands · cleaner --help output

- `doctor --fix` now actually attempts repair instead of a stub — first recipe is `qmd-embeddings`, re-running all checks after the fix pass.
- Removed `(Slice N)` internal porting markers from every subcommand description shown in `--help`.
- New `FixOutcome { Fixed, Failed, Manual }` enum + summary block so the user can quickly read what changed.

## v3.0.0-alpha.4 — perf: faster doctor + warm-cache update --check

- `update --check` warm-path 480ms → 10ms (~48× faster) via an on-disk JSON cache with a 1-hour TTL; `--fresh` bypasses it.
- `doctor` wall time ~980ms → ~890ms by running the `qmd-embeddings` probe on a background thread while the other 7 checks run serially.
- `qmd-embeddings` probe jitter eliminated by replacing a 100ms poll loop with `wait-timeout`'s blocking `wait_timeout`.
- `onebrain update` no longer spawns a subprocess for the current version, using `env!("CARGO_PKG_VERSION")` instead.
- New unit/integration tests cover the cache hit/miss/staleness paths and the in-process version constant.

## v3.0.0-alpha.3 — fix(parity): close all 6 Bun-CLI argv gaps + init becomes one-step + safety + friendlier release notes

- `init` now runs `vault-sync` automatically, collapsing the previous 2-step bootstrap into one; `--no-sync` skips it for offline/CI use.
- Closes 6 Bun-CLI argv gaps the Rust port had dropped (`vault-sync --branch`, positional args on `session-init`/`checkpoint`/`register-schedule`/`init`, `migrate` positional).
- Unifies the flag surface — every `--vault` flag now also accepts `--vault-dir` as a visible clap alias.
- `vault-sync` refuses to write at filesystem root or the literal `$HOME` — a defensive guard against foot-cannons.
- `migrate <name>` rejects supplying both the positional `[cutoff_date]` and `--cutoff <date>` together.
- GitHub Release body now renders a friendly platform table so non-Rust users can pick the right download.
- README rewritten with the platform table + one-step quickstart; CONTRIBUTING.md added.
- Adds 9 new integration tests; suite now at 634 passing.

## v3.0.0-alpha.2 — fix(release): Windows TARGET expansion in release pipeline

- fix(release): adds `shell: bash` to Build/Strip steps so `$TARGET` expands correctly on Windows runners; unblocks 7/7 platform builds. (PR #20)

## v3.0.0-alpha.1 — feat(slices-7-13): Bun parity port + 2 v3.0.1 fixes

- Ports the full Bun CLI parity surface (slices 7–13): `init` (vault bootstrap + schedule presets + `register-hooks`), `vault-sync` (9-step release-overlay flow), `register-hooks`, `register-schedule` (launchd plists, skill/command mode, one-shot `at:`), `update` (GitHub releases fetch + atomic swap), `run-skill`, `migrate`, `doctor` (8 read-only checks), and `orphan-scan` (Active-Session Guard). (PR #2, #3, #9–#16)
- Fixes 2 parity regressions found during the port: `init` reporting `hooks: ok` while `.claude/settings.json` was never written (slice 10); `vault-sync` silently exiting 0 on a caught error with no message (slice 13).
- New core modules: `onebrain-core::scheduler` (cron/launchd, ports Bun 1:1), `onebrain-fs::init`/`orphan` (injectable IO closures for offline/TTY-free tests), `load_vault_config_at` for direct-path config loading.
- `VaultFolders` extended from 1 key (`logs`) to all 8 standard PARA keys, matching Bun's `DEFAULT_FOLDERS`.
- `doctor --fix` auto-repair deferred to v3.0.1 per spec §7.10 — flag is parsed but emits a stub message; doctor itself is parity-green.
- New workspace deps: `regex`, `dirs`, `libc`, `indexmap`, `inquire` (interactive prompts).
- `.github/workflows/release.yml` 7-platform release pipeline (tar.gz/zip + sha256); `CHANGELOG.md` reformatted to the repo's compact style (PR #5).
- Post-merge hardening on PR #3 (ENOENT vs EACCES differentiation, `frontmatter` visibility fix, boundary tests) plus repo metadata (description, homepage, topics, branch ruleset).

## v3.0.0-alpha.0 — feat(slice-1): session-init + 4-crate workspace foundation

- 4-crate Cargo workspace (`onebrain-core`/`onebrain-fs`/`onebrain-cache`/`onebrain-cli`) scaffolding all 13 subcommands (12 still `todo!()`).
- `session-init` subcommand with 8-layer session token resolution (Bun v2.3.3 parity): env vars → process ancestor walk-up → day-scoped cache → PID fallback.
- `qmd_unembedded` count sourced from spawning `qmd status --json` (2s timeout, returns 0 on any failure) — matches Bun.
- Block path: vault-not-found OR config-load-error both emit `{"decision":"block","reason":"onebrain-init-required"}`; `session-init` never exits non-zero.
- 4-layer test pyramid: inline unit + `assert_cmd` integration + `insta` snapshots + golden-master parity vs Bun v2.3.3.
- Error model split: `thiserror` typed errors per library crate + `anyhow` propagation in the binary, mapped to sysexits.h-aligned exit codes.
- CI workflow: fmt + clippy + 3-platform test matrix (ubuntu/macos/windows).
- AGPL-3.0-only license; Windows ARM64 added as the 7th release-matrix platform; 46 tests passing.

[Unreleased]: https://github.com/onebrain-ai/onebrain-cli/compare/v3.0.0...HEAD
[v3.0.0]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0
[v3.0.0-alpha.1]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.1
[v3.0.0-alpha.0]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.0
