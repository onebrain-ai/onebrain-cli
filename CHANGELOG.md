---
latest_version: 3.4.25
released: 2026-08-26
---

# OneBrain CLI Changelog (v3.x · Rust)

All notable changes to the OneBrain CLI binary (`onebrain`) in the v3.x Rust rewrite.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

> **Versioning:** CLI version is tracked in workspace `Cargo.toml`. v3.x is the Rust port of [v2.x (TypeScript/Bun)](https://github.com/onebrain-ai/onebrain). `v3.0.0-alpha.1` is the first user-facing alpha (binary artifacts published to GitHub Releases for 7 platforms).

## [3.4.25] — 2026-08-26 — Keep Codex hooks alive

### Fixed
- Codex hooks now run through the installed CLI instead of a Python file inside a replaceable versioned plugin cache, so refreshing the plugin cannot break an active task.

### Added
- `task list --limit N` returns a deterministic bounded result while preserving the full filtered count in `data.total`, keeping startup task payloads small.

### Changed
- Claude lifecycle registration and `plugin update` migration now converge Stop and configured PostToolUse hooks on one event-dispatching `onebrain hook` runner. Historical direct checkpoint/pending and qmd/search entries are deduplicated without touching foreign hooks; direct checkpoint/search commands remain available to users and child processes.

### Upgrade notes
Restart active agent sessions after upgrading. Their already-loaded hook registrations cannot use the old `codex-hook` compatibility name, because that alias is intentionally not shipped; new registrations invoke only `onebrain hook`.

## [3.4.24] — 2026-08-07 — The gates and the surfaces tell the truth

A backlog-clearing release taken deliberately before v3.5, so the gates v3.5 leans on are
trustworthy first. No new features by design.

### Removed
- **63 verbs that only ever answered "not implemented" no longer parse.** They now fail as unknown commands (exit 2), indistinguishable from a typo, instead of advertising a surface that does not exist. `hide = true` kept them out of `--help` but the parser still accepted them, so anything that discovers verbs by trying them — a script, a doc, a user — reached exit 72 for a command that was never built ([#334](https://github.com/onebrain-ai/onebrain-cli/issues/334))

### Fixed
- `schedule register --test` now runs the same validators `register` does. A config `register` refuses outright — a control character in an argument, say — was previously accepted and executed by the one command you reach for to *check* a config ([#375](https://github.com/onebrain-ai/onebrain-cli/issues/375))
- `schedule register --dry-run` lists every bad entry instead of stopping at the first, so a config with three mistakes reports three ([#376](https://github.com/onebrain-ai/onebrain-cli/issues/376))
- `--test` against a command-mode entry says so, rather than reporting `no schedule: entry matching skill …` and sending you hunting a typo that is not there ([#375](https://github.com/onebrain-ai/onebrain-cli/issues/375))
- 15 clippy errors in `cfg(windows)`- and `cfg(unix)`-gated code that no Linux or macOS run type-checks ([#383](https://github.com/onebrain-ai/onebrain-cli/issues/383))

### Changed
- CI runs `clippy -D warnings` on `windows-latest` as a separate `clippy-windows` job. It had never passed there: CI linted on `ubuntu-latest` only, and `--all-targets` aborts at the first failing target, hiding 11 of the 15 errors behind the first 4. It is a separate job, not a matrix leg on `clippy` — adding `strategy.matrix` renames that job's status context, so the `clippy` context branch protection requires would stop reporting and every PR would block. `clippy-windows` is a required check on `main` ([#383](https://github.com/onebrain-ai/onebrain-cli/issues/383))
- A scheduler unit test no longer depends on the host's shell layout. It used a bare `sh`, which resolves on CI's Windows runner because Git Bash is on PATH and does not on a real Windows machine — so it was green in CI and red on the platform it was meant to protect ([#382](https://github.com/onebrain-ai/onebrain-cli/issues/382))
- `docs/platform-support.md` documents the onnxruntime `cpuid_info` line accurately: it affects *virtualized* ARM Linux whose hypervisor reports an invalid MIDR part number, not aarch64 Linux generally ([#332](https://github.com/onebrain-ai/onebrain-cli/issues/332))

### Upgrade notes
**If a script calls one of the removed verbs, it now gets exit 2 instead of 72.** That is the
point of the change — the verbs were never implemented — but a script that treated 72 as
"expected, skip" will now see an unknown-command failure. The full list is committed at
`crates/onebrain-cli/tests/fixtures/removed-verbs.txt`.

`plugin uninstall` is **not** among them. It is a hybrid: a real, shipped implementation for
`--harness codex`, falling through to not-implemented only for other harnesses.

This release supersedes half of [ADR 0006](docs/decisions/0006-locked-command-tree.md), which
required unbuilt verbs to be stubbed so the grammar could not drift. The `<noun> <verb>` grammar
and the hidden v3.0 aliases remain in force; only the stub-the-unbuilt-verbs half is reversed.

## [3.4.23] — 2026-08-04 — A scheduled job always runs, and always leaves a trace

### Fixed
- A vanished log directory can no longer kill a scheduled skill — launchd opens no redirect for it, so there is no path left to fail ([#372](https://github.com/onebrain-ai/onebrain-cli/issues/372))

### Added
- Every scheduled skill run appends a record to the vault — readable in Obsidian, found by vault search, whether the run succeeded or failed ([#377](https://github.com/onebrain-ai/onebrain-cli/issues/377))
- The CLI opens its own job log after it starts, so it can recreate a missing directory instead of dying before it exists ([#372](https://github.com/onebrain-ai/onebrain-cli/issues/372))

### Upgrade notes
**Run `onebrain schedule register` after upgrading.** The fix lives in the plist, not the binary —
existing plists keep their old redirect until they are re-emitted, so a vault that skips this step
still carries the #372 failure mode.

Command-mode entries (`command:` rather than `skill:`) are deliberately unchanged: launchd execs
their binary directly, so no OneBrain process exists to own a log or write a record for them.

`doctor`'s scheduler checks are unchanged in this release. Teaching them to tell a scheduled run
from a manual one needs a signal that works on systemd and Task Scheduler too, not only launchd —
that is a design question and it is deferred rather than half-shipped.

## [3.4.22] — 2026-08-01 — Make failure visible

### Fixed
- Control characters in schedule args are refused by every backend, not only the XML ones — one config now gets one verdict on all three platforms ([#355](https://github.com/onebrain-ai/onebrain-cli/issues/355))
- `schedule register --resume` can no longer delete a file outside the marker directory; a drive-relative name escaped it on Windows ([#354](https://github.com/onebrain-ai/onebrain-cli/issues/354))
- A bad entry no longer half-registers — control characters are refused before anything is written or activated ([#355](https://github.com/onebrain-ai/onebrain-cli/issues/355))

### Added
- `doctor` reports a missing scheduler log directory, the fault that kills every launchd job with no output at all ([#362](https://github.com/onebrain-ai/onebrain-cli/issues/362))
- `doctor` reports a scheduled skill that has produced nothing, and states how much of your schedule it can actually speak for ([#363](https://github.com/onebrain-ai/onebrain-cli/issues/363))

### Internal
- An escaper regression now fails CI: the corpus carries an escaped-argument case, and a non-ignored test pins the committed fixture to what the renderer emits today ([#353](https://github.com/onebrain-ai/onebrain-cli/issues/353))
- A cited corpus fixture must exist — the evidence contract is enforced rather than stated ([#359](https://github.com/onebrain-ai/onebrain-cli/issues/359))
- Docs-only PRs skip the test matrix without stranding its required checks ([#360](https://github.com/onebrain-ai/onebrain-cli/issues/360))

### Upgrade notes
Two new `doctor` checks may warn on an existing vault. `scheduler logs` is auto-fixable with `doctor --fix`.
`scheduled output` is informational — it names any scheduled skill that has produced no log, and always states
how many entries it could not check (command-mode entries and skills that write no log).

## [3.4.21] — 2026-07-31 — Scheduler polish + notifications config

### Fixed
- Schedule args may contain `"`, `$`, backtick and `\` again — each renderer escapes its own sink ([#344](https://github.com/onebrain-ai/onebrain-cli/issues/344))
- systemd: newlines refused, `;` and `'` quoted — a bare `;` split one command into two ([#344](https://github.com/onebrain-ai/onebrain-cli/issues/344))
- Windows: args quoted per `CommandLineToArgvW`; a `cmd.exe /c` payload passes through verbatim ([#344](https://github.com/onebrain-ai/onebrain-cli/issues/344))
- Command labels no longer truncate at 40 characters and collide — longer ones carry a hash suffix ([#345](https://github.com/onebrain-ai/onebrain-cli/issues/345))
- Registering no longer deletes a job that is still in your config ([#345](https://github.com/onebrain-ai/onebrain-cli/issues/345))
- Stale-label cleanup works on Linux and Windows, and runs after the replacement is installed ([#345](https://github.com/onebrain-ai/onebrain-cli/issues/345))

### Added
- `notifications.telegram_chat_id` is a known config key, placed and self-documented by `doctor --fix` ([#348](https://github.com/onebrain-ai/onebrain-cli/issues/348))

### Upgrade notes
A vault that hand-added a `notifications:` block reports config layout drift **once**; `doctor --fix` moves it into
the Automation section, preserving the value and its comment. Fresh vaults are unaffected.

**Known issue — editing a schedule entry's args strands the old job** ([#352](https://github.com/onebrain-ai/onebrain-cli/issues/352)). It stays installed and
firing, and no CLI command can reach it: `--remove` derives labels from the current config. v3.4.20 and earlier
behave identically. Remove a leftover by hand:

- macOS — `ls ~/Library/LaunchAgents/com.onebrain.*`, then `launchctl bootout gui/$(id -u)/<label>` and delete the plist
- Linux — `ls ~/.config/systemd/user/onebrain-*`, then `systemctl --user disable --now <unit>` and delete it
- Windows — `schtasks /Delete /TN "\OneBrain\<label>" /F`

A fix was built for this release and withdrawn: schedule labels are global while its bookkeeping was per-vault, so
it could delete another vault's live job. #352 redesigns it around verifying artifact ownership.

## [3.4.20] — 2026-07-29 — Cross-platform scheduling parity: `onebrain schedule` actually schedules on macOS, Windows, and Linux

Theme: the scheduler stops being macOS-only and stops lying. Every claim below is backed by a real fire or a measured corpus, not a rendered file.

- **Windows: real Task Scheduler backend** — `schedule register` compiles cron/at entries into Scheduled Tasks under `\OneBrain\` (UTF-16 XML via `schtasks /Create`; the measured 48-trigger cap, month semantics, and multi-`Repetition` behavior are pinned by a schema corpus CI re-validates against real `schtasks` on every PR), fire-proven end-to-end on `windows-latest` — register → fire → one-shot self-delete ([#310](https://github.com/onebrain-ai/onebrain-cli/issues/310))
- **Linux: real systemd user-timer backend** — units written to `~/.config/systemd/user`, activated `daemon-reload → enable → restart`; one-shots fully self-delete (units + `timers.target.wants` symlink) via `ExecStopPost`; fire-proven on a real systemd 255 user session, corpus checked by `systemd-analyze verify` on every PR ([#313](https://github.com/onebrain-ai/onebrain-cli/issues/313), [#314](https://github.com/onebrain-ai/onebrain-cli/issues/314))
- **Scheduler logs move out of the vault** into per-OS state dirs (`~/Library/Logs/onebrain` · journald · Task Scheduler history) — a cloud-synced vault can no longer make launchd fail every run with a silent exit 78, which is the incident this release exists to end ([#315](https://github.com/onebrain-ai/onebrain-cli/issues/315))
- **`schedule list` / `--status` report what the OS scheduler says**, not what's on disk: `launchctl print` / `schtasks /Query` / `systemctl --user is-active`, with `⚠` for present-but-inactive — the state the old file-existence check could not see ([#312](https://github.com/onebrain-ai/onebrain-cli/issues/312))
- **macOS activation is imperative**: register boots the job out and back in immediately (no more plists waiting for next login), and `--remove` boots out *before* deleting so a removed job actually stops firing ([#312](https://github.com/onebrain-ai/onebrain-cli/issues/312))
- `--dry-run` prints the platform's real artifact(s) headed by the entry label; remove/preview messages no longer show launchd paths on non-macOS hosts
- Help text and docs are platform-neutral, with a new `docs/platform-support.md` section on backend semantics (logged-in-only across all three; missed runs skip on Windows/Linux by design, coalesce on macOS wake)
- Test-created search collections now stamp `.onebrain-test-collection` at creation, so any future cache-root cleanup enumerates a closed set instead of guessing from names ([#305](https://github.com/onebrain-ai/onebrain-cli/issues/305))

Known: `plugin update` re-registers schedules while it runs, so an entry firing in that exact window sees a brief bootout/re-register gap (MN-6 — accepted, logged in the epic's decisions).

## [3.4.19] — 2026-07-26 — The daemon runs on Windows, so the MCP server stops holding the index lock

- `onebrain daemon` now works on Windows: process probes via `OpenProcess`/`GetExitCodeProcess`, detached spawn via `DETACHED_PROCESS`, and a `daemon-<hash>.stop` marker standing in for SIGTERM — same ask/wait/escalate shape as Unix, no new dependency ([#307](https://github.com/onebrain-ai/onebrain-cli/issues/307))
- Windows sessions therefore reach `Backend::Daemon` instead of falling back to the engine-owning path, so the MCP server holds no collection lock and `search reindex` succeeds while an editor is open — previously it was `exit 77` every time, and the Stop hook's `--pending-only` catch-up reported `detached: true` while doing nothing ([#307](https://github.com/onebrain-ai/onebrain-cli/issues/307))
- A daemon whose slot files are removed while it runs now shuts itself down rather than holding the collection lock unreachably — `daemon stop --all` enumerates registrations, not processes, so it could not see one on any platform ([#308](https://github.com/onebrain-ai/onebrain-cli/issues/308))
- `Engine::remove_doc` commits the keyword index before redb, so a crash mid-remove leaves a repairable state instead of permanently orphaning lex docs that then surface as hits on the lex-only fast paths ([#297](https://github.com/onebrain-ai/onebrain-cli/issues/297))
- `onebrain search status` reports `unknown (locked)` instead of the factually wrong `never` when the index cannot be read, and `doctor`'s lex-index hint now names the process actually holding the lock rather than a daemon that may not exist ([#307](https://github.com/onebrain-ai/onebrain-cli/issues/307))

## [3.4.18] — 2026-07-23 — Native Codex harness and managed plugin opt-in

- Add `codex` to harness detection, skill execution, ad-hoc harness runs, and per-entry scheduling.
- Invoke Codex through `codex exec` with workspace-write, ephemeral sessions, vault cwd, model, and JSON forwarding.
- Add explicit managed Codex plugin installation with an atomic vault marker and additive feature configuration.

## [3.4.17] — 2026-07-22 — Search-index health: stop writing on read paths, flag a stuck-rebuild shortfall

Theme: read paths that quietly wrote to disk or the user's config, and a doctor blind spot where a half-built keyword index reported healthy.

- Pure read paths no longer persist a generated collection name into `onebrain.yml`: `onebrain search model list`, the MCP token/status routes, and the daemon's token-cache open all resolve the collection read-only now, so a listing or status command can't rewrite (and strip the comments from) your config ([#300](https://github.com/onebrain-ai/onebrain-cli/issues/300))
- The daemon no longer creates an empty collection cache dir at boot for a never-indexed vault — it reports "no token cache" (503) instead of materializing one, which is also what had been leaking stray dirs into the cache root during the test suite ([#300](https://github.com/onebrain-ai/onebrain-cli/issues/300))
- `onebrain doctor` now flags a keyword index that holds fewer chunks than its metadata **when a rebuild is also stuck pending** — the genuinely-incomplete case — and always prints a `shortfall: N chunk(s)` detail line when the counts differ, so a partial index no longer reads as healthy ([#298](https://github.com/onebrain-ai/onebrain-cli/issues/298))
- Test-suite isolation is enforced at the source: a static guard now fails CI if any test that spawns the binary doesn't pin its cache root, closing the class of leak behind #300 rather than the one instance

## [3.4.16] — 2026-07-20 — Search recall: headings become searchable, the rerank gate stops deleting hits

Theme: two defects that were removing correct answers from every search, and the schema migration the first one forces.

- `heading_path` is now searched, not just stored — and it shares the script-aware tokenizer with `body`, so **Thai and CJK headings can match at all** for the first time. Heading-shaped recall doubles (hit@10 0.300 → 0.600 on a real 782-doc vault) with no loss on body-term queries ([#294](https://github.com/onebrain-ai/onebrain-cli/issues/294))
- **Breaking (index):** that tokenizer change alters the tantivy schema, so an index built by ≤3.4.15 is migrated on first use — rebuilt from stored chunk metadata, no files re-read and nothing re-embedded (1.2 s for 6271 chunks, then 8 ms). Crash-safe: an interrupted migration is detected and finished on the next open, and a rebuild that would erase the only surviving copy refuses instead. **Downgrading afterwards requires `onebrain search reindex --force`** ([ADR 0034](docs/decisions/0034-heading-search-schema-selfheal-rerank-gate-decouple.md))
- `search.reranker.min_score` defaults to `0.0`: the reranker now **reorders** instead of deleting rows, restoring hits it was silently dropping (heading-shaped hit@10 0.233 → 0.500, body-term 0.500 → 0.733). The confidence bands are unchanged at 0.30/0.60 — the gate asks whether to delete a row, the band asks how much to trust it ([#295](https://github.com/onebrain-ai/onebrain-cli/issues/295))
- Vaults initialized on v3.4.7–v3.4.15 have `min_score: 0.30` written into their own `onebrain.yml` and keep the old behaviour; `onebrain doctor` now flags it as a **superseded default** and `--fix` resets it
- `onebrain doctor` gains a `lex-index` check for a keyword index that is empty, duplicated or orphaned relative to its stored metadata — states that previously reported as healthy while search returned nothing; `--fix` repairs the first two from metadata
- `doctor --fix --json` no longer runs a repair for checks that have none, so a warm daemon no longer produces a spurious failure and exit 1

## [3.4.15] — 2026-07-18 — Retire the stale warm daemon on upgrade

Theme: close the v3.4 line — after a binary upgrade, a warm daemon of the OLD version no longer serves stale routes (the v3.4.14 gain-dashboard-dark class), and the fake-daemon tests are deterministic for real.

- `onebrain update` retires every warm daemon after a successful upgrade (they respawn at the new version on next use), and `onebrain plugin update` retires a version-skewed daemon for its vault — so the WebUI/mcp never keep serving an old binary's routes ([#291](https://github.com/onebrain-ai/onebrain-cli/issues/291))
- `onebrain doctor` now warns when a running daemon's version differs from the CLI, with the `onebrain daemon stop --all` hint — the safety net for an in-place `brew upgrade` ([#291](https://github.com/onebrain-ai/onebrain-cli/issues/291))
- Fake-daemon token-check tests are deterministic: verdict→exit-code mapping runs in-process (no socket timing), while the daemon-adoption/#264 regression + HTTP-branch coverage runs on a generous fixed budget — no more slow-CI-runner flakes ([#289](https://github.com/onebrain-ai/onebrain-cli/issues/289))
- CI guard asserts the README Quickstart `onebrain --version` example matches the workspace version, so it can never ship stale again

## [3.4.14] — 2026-07-17 — Human-readable CLI + gain dashboard truth

Theme: complete the v3.4 line — every failure message says what happened, why, and what to do next; the Token-Gain dashboard tells the truth. Companion plugin release 3.4.0 ships the search cascade + grep-gate hook.

- **Bind before banner**: `serve` prints its banner only after the listener actually binds — a failed bind shows only the error, never a URL+token; the URL now carries the **actual bound port** (so `--port 0` prints a clickable address). `daemon start` detects a child that died before binding (waitpid, not a zombie-fooled probe) and says so fast instead of claiming success ([#278](https://github.com/onebrain-ai/onebrain-cli/issues/278))
- **Output style contract** across every command: failures read `✗ what — why` + `💡 next step` (docs/output-style.md); JSON envelopes stay single-line and glyph-free via a typed hint mechanism, error codes and exit codes unchanged (PermissionDenied binds still exit 66) ([#279](https://github.com/onebrain-ai/onebrain-cli/issues/279))
- **Token-Gain dashboard reads the lock-free gain JSONL** — the daemon `/api/token/gain` (and `--all-time`/`--since`) no longer serve the never-auto-populated rollup DB, so the WebUI shows real numbers without a manual rebuild; a bare pre-3.4.14 client request still gets the legacy all-time view ([#281](https://github.com/onebrain-ai/onebrain-cli/issues/281))
- A gain read that races a `--reset` archive rename no longer errors (vanished file = zero events, not a 500)
- Corrupt gain timestamps bucket under the self-flagging `1970-01.jsonl` (epoch fallback), never the current month's live log
- **Breaking:** `--since` / `?since=` now validate strict `YYYY-MM-DD` — a malformed date errors (exit 70 / `E_INVALID_DATE`, route 400) instead of silently matching nothing; scripts passing non-zero-padded dates like `2026-1-1` must use `2026-01-01` (previously these silently returned zero results) ([#287](https://github.com/onebrain-ai/onebrain-cli/issues/287))
- The remaining bare `Error:` dead ends get the ✗/💡 dressing — the vault-not-found walk-up and not-a-vault-root failures (every verb's most common errors) and `token gain --rebuild`'s EngineBusy remedy (now split into what/why + hint); exit codes 64/77 unchanged ([#288](https://github.com/onebrain-ai/onebrain-cli/issues/288))

## [3.4.13] — 2026-07-17 — Ledger works in production + multi-vault daemons

Theme: make the token-optimization read-hook ledger actually gate in production (it shipped enabled-but-inert), and let one machine run a warm daemon per vault instead of one that thrashes across vaults.

- Unify the already-sent ledger key on `doc_hash` + the **vault-relative, canonicalized** path across `search get` / MCP `get` / the read-hook — so cross-surface dedup fires and `token check` gates on the absolute paths the read-hook receives, including vaults under a symlinked path (`/tmp`→`/private/tmp`) ([#255](https://github.com/onebrain-ai/onebrain-cli/issues/255), [#268](https://github.com/onebrain-ai/onebrain-cli/issues/268))
- `token check` routes to a warm same-vault daemon even across a version skew — ending the cold-open lock collision that made the gate fail open 100% of the time in the field; the round-trip budget is configurable (`token_optimization.check_timeout_ms`, default 200) for iCloud/networked vaults, a successful deny is metered (`ledger_deny`) in `token gain`, and `doctor` flags a read-hook that fails open ~always ([#264](https://github.com/onebrain-ai/onebrain-cli/issues/264))
- **Per-vault daemon slots** — each vault gets its own warm daemon (`daemon-<hash>.*` on an ephemeral port) instead of one machine-wide daemon that thrashes when two vaults are active; `daemon status` enumerates all, `daemon stop` gains `--vault`/`--all`, and `doctor` surfaces running daemons plus a lingering pre-upgrade one ([#230](https://github.com/onebrain-ai/onebrain-cli/issues/230))
- `onebrain daemon start` walks up from cwd to bind the vault like every sibling verb, instead of spawning a vault-less daemon ([#262](https://github.com/onebrain-ai/onebrain-cli/issues/262))
- Scheduler command-mode launchd plists embed `--vault` for onebrain jobs (generic binaries untouched), fixing scheduled jobs that exited 78; the legacy `run-skill` alias now walk-up-resolves ([#263](https://github.com/onebrain-ai/onebrain-cli/issues/263))
- `doctor --fix` backfills a documented `token_optimization` sub-key missing from an existing block — parse-guarded, handles spaced/quoted keys ([#270](https://github.com/onebrain-ai/onebrain-cli/issues/270))
- Hardening (Copilot findings): the `register-hooks` migration notice points at the correct command (`plugin install`); one-shot launchd `/bin/sh -c` wrappers shell-escape every interpolated value and reject shell-special chars in `args` keys — closing a demonstrated command-injection

**Breaking:** `onebrain daemon status --json` now returns `{"daemons": [...]}` (a list) instead of a single object, reflecting the multi-daemon model. Upgrading: run `onebrain daemon stop --all` once after upgrading to retire a lingering pre-v3.4.13 machine-wide daemon.

## [3.4.12] — 2026-07-12 — Self-healing: run smooth under a running daemon

Theme: no command, `serve`, or MCP call should error or fail to run because a daemon is (or isn't) holding the single-process redb lock.

- `token gain` works under a running daemon: default/`--by`/`--history`/`--reset` read the lock-free JSONL raw log; `--all-time`/`--since` route through the daemon's `/api/token/gain` — even across a **version skew**, so it works right after an upgrade without restarting the daemon ([#258](https://github.com/onebrain-ai/onebrain-cli/issues/258))
- A genuinely contended rollup open (or a daemon too old to serve the route) now reports the shared `E_ENGINE_BUSY` (exit 77) with an actionable hint, instead of a raw redb `Database already open` error at exit 1 ([#258](https://github.com/onebrain-ai/onebrain-cli/issues/258))
- `serve` now **reuses or starts** a daemon (restarting a stale/version-mismatched one) instead of an engine-less foreground standalone — so the Token-Gain dashboard is populated rather than dark; the explicit `--port`/`--dir` standalone escape hatch now also opens its token cache ([#257](https://github.com/onebrain-ai/onebrain-cli/issues/257), [#258](https://github.com/onebrain-ai/onebrain-cli/issues/258))
- `search vsearch` is daemon-routable: vector-only search routes through the daemon's new `/api/vault/search?mode=vec` instead of failing `E_ENGINE_BUSY` while an `onebrain mcp` session holds the engine ([#258](https://github.com/onebrain-ai/onebrain-cli/issues/258))
- Fixes: `doctor`'s scoped-key lookup no longer panics on empty segments; remove dead test code; migrate a `chrono` `DateTime::from_timestamp` deprecation (Copilot autoreview)

## [3.4.11] — 2026-07-12 — Token-opt seam fixes

Closes the three real seams that post-ship verification of v3.4.10 surfaced — none was caught by the epic's gates because no test exercised those exact cross-component paths — plus polish.

- Fix `search`/`query --output json` erroring (`E_INTERNAL`) whenever duplicate chunks collapse: `Signal::ChunksCollapsed` is now a struct variant, serializable under the internally-tagged enum ([#249](https://github.com/onebrain-ai/onebrain-cli/issues/249))
- Embed the Token Gain WebUI dashboard in the binary — the pinned web UI was a version behind, so v3.4.10 shipped without it; bumped to v0.1.8 ([#250](https://github.com/onebrain-ai/onebrain-cli/issues/250))
- `token check` now gates a repeat read via an in-process Direct-mode ledger check when no daemon is running, so the read-hook actually gates without a warm daemon ([#248](https://github.com/onebrain-ai/onebrain-cli/issues/248))
- `doctor --fix` backfills a missing `token_optimization` block into an existing `onebrain.yml`, byte-identical to a fresh `init` ([#247](https://github.com/onebrain-ai/onebrain-cli/issues/247))
- Docs: `ChunksCollapsed` is now correctly documented as exact-duplicate (not near-duplicate) chunk collapse ([#246](https://github.com/onebrain-ai/onebrain-cli/issues/246))

## [3.4.10] — 2026-07-12 — Token Optimization

- **Token-optimization layer** — a 4-rung level ladder (off/conservative/balanced/aggressive; lossless by default) shapes agent-facing `search`/`get` output through a transform funnel, with an honesty-signal contract: any truncation or omission is always disclosed to the agent (with a `--force` re-fetch cursor), never silent ([#237](https://github.com/onebrain-ai/onebrain-cli/issues/237), [#241](https://github.com/onebrain-ai/onebrain-cli/issues/241))
- **Two-tier cache** — query-result memoization + an already-sent ledger that turns a repeat read of an unchanged doc into a small reference receipt instead of resending the full body; `onebrain search get <path> --force` re-materializes on purpose ([#239](https://github.com/onebrain-ai/onebrain-cli/issues/239), [#241](https://github.com/onebrain-ai/onebrain-cli/issues/241))
- **`onebrain token gain`** — measures exactly what was saved: a raw per-call log plus precomputed daily/monthly/yearly rollups, `--by` pivots, and `--reset` epochs for baseline comparisons ([#240](https://github.com/onebrain-ai/onebrain-cli/issues/240))
- **`onebrain token check` + `token discover`** — a fail-open PreToolUse read-hook gate (200 ms budget, off by default) plus a field-test tool that scans Claude Code transcripts for repeat reads the ledger could have saved ([#242](https://github.com/onebrain-ai/onebrain-cli/issues/242))
- Dedicated [token-optimization guide](docs/token-optimization.md) + a Token Gain dashboard in the embedded WebUI ([#243](https://github.com/onebrain-ai/onebrain-cli/issues/243))
- **Hardening:** collection-lock acquired before migration, doctor legacy-stub detection, artifact-dedup path guard, test-support parity, `session --vault` ([#238](https://github.com/onebrain-ai/onebrain-cli/issues/238))
- Config wiring: unset `get_max_tokens`/`snippet_max_chars` now follow the per-level cap ladder (6000/4000/4000, 200/150/120) — a set value pins a fixed cap; `strip_frontmatter: auto|always|never` is fully honored ([#241](https://github.com/onebrain-ai/onebrain-cli/issues/241))

## [3.4.9] — 2026-07-11 — Cache layout split + field fixes

- **Breaking:** pinned `ONEBRAIN_TOKEN` must be ≥32 chars in `[A-Za-z0-9_-]`; violations abort serve/daemon startup (was warn-and-generate) — closes a Windows command-injection path ([#218](https://github.com/onebrain-ai/onebrain-cli/issues/218))
- `serve --open` now works on Windows via quoted `cmd /C start` ([#218](https://github.com/onebrain-ai/onebrain-cli/issues/218))
- Collection cache split into `models/` + `index/` with eager auto-migration (never re-downloads models); `search status` reports split sizes + layout state ([#225](https://github.com/onebrain-ai/onebrain-cli/issues/225))
- Fresh `onebrain.yml` emits `search.exclude` (attachments + archive); `doctor --fix` backfills existing vaults ([#220](https://github.com/onebrain-ai/onebrain-cli/issues/220))
- `session init --json` gains `recap_pending` (unrecapped session-log count) ([#219](https://github.com/onebrain-ai/onebrain-cli/issues/219))
- doctor detects legacy qmd installs with guided cleanup — destructive actions require interactive confirmation, never run headless ([#221](https://github.com/onebrain-ai/onebrain-cli/issues/221))
- Compile-time guard for `SYSTEM_SECTION`/`SECTIONS` drift ([#213](https://github.com/onebrain-ai/onebrain-cli/issues/213))

## [3.4.8] — 2026-07-10 — CLI-UX polish + self-documenting config

- **Breaking:** `serve --host` removed — localhost-only; containers use `ONEBRAIN_BIND` ([#205](https://github.com/onebrain-ai/onebrain-cli/issues/205))
- Self-documenting `onebrain.yml` template + doctor value validation/reset ([#196](https://github.com/onebrain-ai/onebrain-cli/issues/196))
- Section-banner layout + `doctor --fix` restructures existing vaults ([#203](https://github.com/onebrain-ai/onebrain-cli/issues/203))
- All config writers comment-preserving via shared `yaml_edit` ([#200](https://github.com/onebrain-ai/onebrain-cli/issues/200))
- Doctor output redesign: boxed Summary, no inline hints, daemon-routed search check ([#200](https://github.com/onebrain-ai/onebrain-cli/issues/200))
- `daemon status` full dashboard + daemon-aware `serve --open` ([#197](https://github.com/onebrain-ai/onebrain-cli/issues/197))
- `search model list`/`status` display parity + Ready row ([#195](https://github.com/onebrain-ai/onebrain-cli/issues/195))

## [3.4.7] — 2026-07-07 — Tier-2 cross-encoder reranker

- Added Tier-2 cross-encoder reranker (`onebrain-rerank-v1`, self-hosted bge-reranker-v2-m3 int8), default-on, replacing the ADR 0024 cosine gate with a calibrated 0–1 score. ([ADR 0025](docs/decisions/0025-tier2-cross-encoder-reranker.md)) ([#190](https://github.com/onebrain-ai/onebrain-cli/issues/190), [#191](https://github.com/onebrain-ai/onebrain-cli/issues/191))
- Added `search.reranker` config (`enabled`, `model`, `min_candidates` default 10, `min_score` default 0.30); model downloads + sha256-verifies during `reindex`.
- `top_k`/`min_candidates` are now settable on every surface (CLI flags, config, `/api/vault/search` query params).
- `--min-score` now filters the calibrated `rerank_score` when reranking is active (legacy raw-score meaning when off).
- MCP `query` tool now reranks and surfaces `rerank_score` like every other surface.
- Fixed: reindex previously couldn't download the reranker model (wrong accessor path), leaving it inert for every user.

## [3.4.6] — 2026-07-06 — warm daemon + honest search-engine lock contention

- Added: warm daemon (`daemon __run`) owns the native-search engine as sole redb owner; token-gated internal reindex/status/get endpoints + daemon discovery + idle-shutdown TTL. ([ADR 0023](docs/decisions/0023-warm-daemon-mcp-search.md) · [docs/daemon.md](docs/daemon.md)) ([#164](https://github.com/onebrain-ai/onebrain-cli/issues/164))
- Added: `onebrain mcp` and CLI search now route through the daemon so multiple concurrent sessions coexist; passive per-vault discovery never disrupts another vault's session. ([#168](https://github.com/onebrain-ai/onebrain-cli/issues/168), [#169](https://github.com/onebrain-ai/onebrain-cli/issues/169))
- Fixed: search-engine lock contention now surfaces honestly (`E_ENGINE_BUSY` exit 77 for user verbs, silent skip for hooks) instead of misreporting.
- Fixed: `search search` (lex) now populates `heading_path` from the stored tantivy field in a single pass.
- Fixed: auto-started daemon receives its vault via an explicit argument (no env-var mutation); reindex-path confinement now also runs in the engine, not just the HTTP layer. ([#175](https://github.com/onebrain-ai/onebrain-cli/issues/175))
- Fixed: honest `E_ENGINE_BUSY`/503 errors during the pre-daemon-to-daemon upgrade transition window instead of opaque `E_INTERNAL`/503 strings. ([#179](https://github.com/onebrain-ai/onebrain-cli/issues/179))
- Fixed: semantic search no longer silently returns nothing — per-model `vec_floor` cutoff replaced with a recall-first `keep_top_cluster` cutoff + confidence label. ([ADR 0024](docs/decisions/0024-vector-confidence-recall-first.md)) ([#183](https://github.com/onebrain-ai/onebrain-cli/issues/183))

## [3.4.5] — 2026-07-05 — native search · no dependency · auto reindex/embed · model reindex ux/ui (the qmd epic)

- **Breaking:** removed the `onebrain qmd …` command group and the external `@tobilu/qmd` dependency — native `onebrain-search` now powers webui search + the reindex hook; use `onebrain search …` instead (hooks/schedules auto-rewritten on next `plugin update`/`schedule register`; the reindex hook now runs synchronously, tracked in [#133](https://github.com/onebrain-ai/onebrain-cli/issues/133)).
- **Breaking:** native-search state (model + index + `engine.redb`) now lives in the OS data dir instead of the purgeable cache dir, after macOS cleanup wiped a ~536 MB index ([#114](https://github.com/onebrain-ai/onebrain-cli/issues/114)); existing state auto-migrates on next command. ([ADR 0021](docs/decisions/0021-search-state-persistent-data-dir.md))
- Fixed: `doctor` now flags a missing index on a configured collection as a possible OS-purge instead of "no index yet"; `search status`/MCP `query` degrade honestly with no index.
- Added: auto reindex/embed hook — `search reindex --lex-only` on PostToolUse, `--pending-only` on Stop; both auto-migrate from the old qmd hook entries. ([#133](https://github.com/onebrain-ai/onebrain-cli/issues/133))
- Added (transition): `session init --json` emits the canonical `search_unembedded` key alongside the deprecated `qmd_unembedded` alias.
- Internal: renamed `HookSpec::QMD` → `REINDEX`, removed the dead `.obsidian/` seeding path. Closes [#142](https://github.com/onebrain-ai/onebrain-cli/issues/142).
- Fixed: `plugin update` on a vault with no `update_channel` no longer 404s — absent/unknown channel now defaults to `main` instead of the nonexistent `next` branch.

## [3.4.4] — 2026-07-03 — scheduler runs actually fire

- Fixed: scheduled cron skills no longer exit 78 (EX_CONFIG) — `skill run` now prepends its own binary dir to the headless `claude` child's PATH. ([#124](https://github.com/onebrain-ai/onebrain-cli/issues/124))
- Fixed: generated plists use the current `skill run` subcommand instead of the deprecated `run-skill` alias, so scheduled runs stop logging a deprecation notice. ([#125](https://github.com/onebrain-ai/onebrain-cli/issues/125))

## [3.4.3] — 2026-07-03 — scheduler fixes + housekeeping

- Scheduler cron now accepts step (`*/N`), list (`a,b,c`), and range (`a-b`) syntax per field, emitted as launchd `StartCalendarInterval` arrays. ([#116](https://github.com/onebrain-ai/onebrain-cli/issues/116))
- Scheduler command-mode plists are now disambiguated by their args so two entries for the same binary no longer collide; `schedule register` auto-migrates stale pre-[#116](https://github.com/onebrain-ai/onebrain-cli/issues/116) plists. ([#116](https://github.com/onebrain-ai/onebrain-cli/issues/116))
- Scheduler cron `weekday` now accepts the standard `0`-`7` range (both mean Sunday), normalizing `7`→`0`.
- Scheduler cron now rejects strings that restrict both day-of-month AND day-of-week (cron ORs them, launchd ANDs them) — use two `schedule:` entries instead.
- Scheduler cron combination cap raised from 366 to 1000, accepting the "every day of every month" idiom while still rejecting `*/1 */1 * * *`.
- `onebrain schedule list` is now implemented (was a stub), reusing the existing status view. ([#116](https://github.com/onebrain-ai/onebrain-cli/issues/116))
- CI now runs the lex-only (`--no-default-features`) test suite alongside clippy. ([#119](https://github.com/onebrain-ai/onebrain-cli/issues/119))
- Polish ([#120](https://github.com/onebrain-ai/onebrain-cli/issues/120)): `SearchMcpServer` renamed to `McpServer`; `get` tool documents line clamping; `QueryParams` dead-code allowance tightened.

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

## [3.4.0] — 2026-07-03 — native search engine (`onebrain-search`)

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

## [3.3.25] — 2026-07-02 — webview preflight route

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

## [3.3.18] — 2026-06-30 — coverage phase 2 (command modules)

- test(cli): closes coverage gaps in the command-module layer — `doctor.rs` 87.55%→94.20%, `register_schedule.rs` 72.08%→91.30%, `vault_ctx.rs` 51.35%→100%, `run_skill.rs` +110 tests.
- Core line coverage 92.59% → 93.62%; residuals documented in `docs/coverage.md`.
- Test isolation hardening: plugin-cache/qmd-embeddings fix-path tests now run via subprocess with a tempdir `$HOME`/`PATH`.
- No behavior change — tests only.

## [3.3.17] — 2026-06-30 — fix `onebrain update` hang on Homebrew + tighter --help indent

- Fixed: `onebrain update` no longer hangs on Homebrew — Homebrew 4.4+'s "proceed? [y/n]" prompt was corrupted by the install spinner redrawing the TTY; `HOMEBREW_NO_ASK=1` fixes it.
- style(cli): tighter `--help` layout — category headings flush left, commands indent 2 spaces.

## [3.3.16] — 2026-06-30 — coverage foundation + dispatch tests

- test(cli): adds `scripts/coverage.sh` + `docs/coverage.md` (excluded-files list + rationale + baselines); targets 100% line coverage on testable core code.
- test(cli): covers `v31/dispatch.rs` stub + verb arms — 76.94% → 86.70% line.
- Measured baselines: whole-workspace 89.58% line; core (exclusions applied) 92.59% line. No behavior change.

## [3.3.15] — 2026-06-30 — categorized root --help

- feat(cli): groups root `--help` commands into 4 named category sections (⚙️ System Management, 🧠 Vault Management, 🔄 Session Management, 🚀 Launch Management).
- Category headings show emoji on a terminal, render plain when piped, so `onebrain --help | cat` stays clean.
- Descriptions pulled live from clap `about` annotations — can't drift from source of truth.
- Subcommand help (`onebrain note --help`, etc.) is unchanged.
- Drift-guard test: CI fails if any visible root subcommand is missing from CATEGORIES or a category entry is stale.
- Options section keeps its compact format, unaffected by the categorized block injection.
- Fixed `is_root_help_request` to not intercept `--version`/`-V`.

## [3.3.14] — 2026-06-30 — surface note + task in --help

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

## [3.3.10] — 2026-06-28 — serve: qmd-backed vault search

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

## [3.3.1] — 2026-06-25 — daemon write / media / chat surface

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
- Internal: unified the two `plugin update` text renderers into one, removing a trait that existed only for test doubles (PR [#57](https://github.com/onebrain-ai/onebrain-cli/issues/57)).
- Internal: renamed `vault_sync::run_silent` + `register_schedule::run_quiet` → both `run_embedded` for naming consistency (PR [#57](https://github.com/onebrain-ai/onebrain-cli/issues/57)).

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

## [3.2.0] — 2026-05-26 — `note` resource group (11 verbs)

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

## [3.1.0] — 2026-05-26 — Consistency Standard · locked command tree · canonical JSON envelope

- Feat: R1 branded banner — 5-line FIGlet "Slant" `OneBrain` wordmark + tagline on interactive sessions and every `--help` screen, gated on a 6-rule TTY chain.
- Feat: locked 27-entry command tree — 3 root verbs + 24 resource groups, singular-noun 2-level `onebrain <noun> <verb>`; other 200+ verbs stubbed with `E_NOT_IMPLEMENTED` (exit 72).
- Feat: `--vault` global flag + walk-up resolver + `ONEBRAIN_VAULT` env, documented priority order, surfaced by new `onebrain vault current`.
- Feat: `plugin update` semantic swap — now self-updates the CLI binary (was `onebrain update`); the legacy plugin-overlay behavior moves under `plugin update`'s vault-side step.
- Fix: `onebrain init` now registers the plugin with Claude Code AND prompts before initializing in a non-empty directory.
- Feat: canonical `Envelope<T>` JSON shape + partial-failure contract (`E_PLUGIN_UPDATE_PARTIAL`); `BrokenPipe` on stdout now exits 0 instead of panicking.
- Feat: output-format compliance — interactive commands default to text and honor `--json`/`--json --pretty`/`--yaml`/`--output` consistently via one canonical dispatcher.
- **Breaking:** config file renamed `vault.yml` → `onebrain.yml` — CLI v3.1+ dual-reads for back-compat (one-time deprecation warning on legacy); `doctor --fix` migrates via atomic rename; v4.0.0 drops `vault.yml` support entirely.

## [3.0.x] — 2026-05-26 — post-GA follow-ups

These shipped under the v3.0.x patch line after the 2026-05-22 GA and are not part of v3.1.0 itself.

- npm wrapper source recovered and landed in-repo at `npm-wrapper/` after the original tarball-only source was lost; `engines.node` raised to `>=20`.
- CI auto-publishes the npm wrapper on each stable tag via npm Trusted Publishers (OIDC + `--provenance`, no long-lived token).
- postinstall verifies SHA256 against the published `.sha256` before extracting, closing the gap between attested publish and binary integrity.
- bin shim re-raises signal terminations (`128 + signum`) so Ctrl-C/SIGTERM is distinguishable from a real error in CI.
- README + CONTRIBUTING signpost the new `npm-wrapper/` layout; install table promotes npm + Homebrew out of "planned" (both live since v3.0.0 GA).
- postinstall hardening: retry-with-backoff on HTTP 404, Alpine/musl detection, Windows tar fallback to PowerShell `Expand-Archive`, post-install smoke test, escape-hatch env overrides. (PR [#29](https://github.com/onebrain-ai/onebrain-cli/issues/29))
- Raspberry Pi + 32-bit ARM Linux support — release matrix adds `armv7`/`arm-unknown-linux-gnueabihf`; every Pi from 1 to 5 now has a published binary.

## [3.0.0] — 2026-05-22 — Rust rewrite GA · 7-platform release pipeline · stable JSON contracts

- Complete Rust rewrite of OneBrain CLI replacing v2.x TypeScript/Bun — 4-crate workspace, ~10× less memory, 92% smaller binary, startup within 10ms of Bun on warm cache.
- 7-platform release pipeline (macOS Apple Silicon + Intel, Linux ARM64 + x86_64 glibc/musl, Windows ARM64 + x86_64), `cargo-binstall`-ready.
- `onebrain update` fetches binaries directly from GitHub Releases over HTTPS and atomically swaps the running binary — no npm/bun shell-out anywhere.
- Stable JSON output contracts for v3.x (`doctor --json`, `update --check --json`, `update --plan`) — frozen schemas, stability covers v3.x, v4 may break.
- Trust model: downloaded binaries authenticated solely by GitHub's TLS chain — no SHA-256/cosign verification at GA (matches rustup/deno/bun baseline).
- Skill + scheduler ecosystem wired end-to-end — `register-hooks`/`register-schedule`/`run-skill` round-trip with the plugin's hooks; plist generation verified byte-identical to Bun v2.3.3.
- `doctor` ships 8 read-only checks and 5 `--fix` recipes; remaining recipes + Windows zip extraction deferred to v3.0.1.
- Distribution at GA: GitHub Releases + `onebrain update` is the primary path; npm-wrapper and Homebrew tap are planned for the v3.0.x window, not published at GA.

## [3.0.0-alpha.9] — 2026-05-20 — GA candidate: fix `onebrain update` install path · TTY spinner · direct harness · real `--test` · Windows pin

- `onebrain update` install path rewritten to fetch directly from GitHub Releases (alpha.1–alpha.8 shelled out to `bun`/`npm install -g`, which never had the Rust binary published — every real update failed). Downloads over HTTPS (rustls TLS, no checksum verification yet — trust model matches rustup/deno/bun), atomically swaps via tmp + rename. Windows zip extraction intentionally stubbed for v3.0.0.
- TTY spinner + colorized output for `onebrain update`; non-TTY output stays plain-text byte-for-byte; `--json` suppresses all log output.
- `direct` harness lands in `register-hooks` as a first-class no-op — vaults without `.claude/` print "direct mode · no hooks to register" instead of a gemini-only error message.
- `register-schedule --test <skill>` is now a real implementation — builds the same argv launchd would emit, spawns it synchronously, and propagates the exit code.
- `update --plan` JSON now includes `binary_targets[]` enumerating the six published `(triple, ext)` pairs.
- New `UpdateError::Install(String)` variant replaces `UpdateError::Network` for filesystem/OS errors during install, so failures no longer misleadingly blame the network.
- `--vault-dir` flag pattern audited across all subcommands (Reviewer C-I4) — user-visible flag name is consistent everywhere; no code change.
- Defense-in-depth: `extract_tar_gz` now guards on `entry_type().is_file()` so a malicious tar can't promote a symlink/dir to "the binary"; deleted the dead bun/npm install-command code path.

## [3.0.0-alpha.8] — 2026-05-20 — feat: JSON output modes for `doctor` + `update` · cosmetic

- feat: `doctor --json` emits a single JSON document (`{ok, summary, checks[]}`); combines with `--fix` for a post-fix `fix[]` array; schema stable for v3.x.
- `doctor --json` outside a vault now emits a JSON failure envelope on stdout with exit code 1, instead of an anyhow plain-text error.
- `update --check --json` emits `{ok, current, latest, update_available, released_at?}`; `update_available` is `null` (not a guessed false) when the remote fetch failed.
- `update --plan` is `--check --json` plus `release_url`/`binary_url_template`, designed for the `/update` plugin skill; implies dry-run.
- `vault-sync --vault-dir <path>` flag-form alternative to the positional argument.
- `register-schedule` resolves `folders.logs` from `vault.yml` instead of hardcoding `07-logs/scheduler/...`, with path-traversal guards.
- 3-round review consensus fix-pass: `version_at_least` promoted to `pub`; `progress_writer` option added to `VaultSyncOptions`; +5 unit tests.

## [3.0.0-alpha.7] — 2026-05-20 — feat(doctor): four new `--fix` recipes (settings-hooks · plugin-files · vault.yml-keys · claude-settings)

- `doctor --fix` now repairs four more check types: `settings-hooks`, `plugin-files`, `vault.yml-keys` (backfills keys, strips deprecated ones, repairs non-positive checkpoint values), `claude-settings`.
- Dispatch widened to Warn AND Error so previously-bypassed failure modes are now repaired by `--fix`.
- Atomic writes everywhere — `vault.yml`/`settings.json` mutations go through `.tmp + rename`.
- `fix_plugin_files` now respects the same `refuse_dangerous_vault_path` guard as `onebrain vault-sync`.
- `orphan-checkpoints` routes to Manual with a clearer hint pointing at `/wrapup` — auto-deletion intentionally off the table.
- Five recipes total ship with the auto-fix flow; the `vault.yml-keys` message notes YAML comments aren't preserved yet.

## [3.0.0-alpha.6] — 2026-05-20 — fix(update): target CLI repo + prerelease-safe · ci: GHA Node 24 · docs: README hero + badges

- `onebrain update` now targets the CLI repo (`onebrain-ai/onebrain-cli`) instead of the plugin repo, fixing a bug where the non-`--check` form could downgrade users to the plugin repo's last Bun release.
- Semver-aware version comparison via the `semver` crate replaces the string-equality check, preventing silent downgrades.
- GitHub Actions Node 24 bump across `ci.yml`/`release.yml`, clearing deprecation warnings ahead of the forced cutover.
- README hero/banner + CLI-only badges aligned with the plugin repo's presentation; license badge updated to AGPL-3.0.

## [3.0.0-alpha.5] — 2026-05-20 — feat: doctor --fix lands · cleaner --help output

- `doctor --fix` now actually attempts repair instead of a stub — first recipe is `qmd-embeddings`, re-running all checks after the fix pass.
- Removed `(Slice N)` internal porting markers from every subcommand description shown in `--help`.
- New `FixOutcome { Fixed, Failed, Manual }` enum + summary block so the user can quickly read what changed.

## [3.0.0-alpha.4] — 2026-05-20 — perf: faster doctor + warm-cache update --check

- `update --check` warm-path 480ms → 10ms (~48× faster) via an on-disk JSON cache with a 1-hour TTL; `--fresh` bypasses it.
- `doctor` wall time ~980ms → ~890ms by running the `qmd-embeddings` probe on a background thread while the other 7 checks run serially.
- `qmd-embeddings` probe jitter eliminated by replacing a 100ms poll loop with `wait-timeout`'s blocking `wait_timeout`.
- `onebrain update` no longer spawns a subprocess for the current version, using `env!("CARGO_PKG_VERSION")` instead.
- New unit/integration tests cover the cache hit/miss/staleness paths and the in-process version constant.

## [3.0.0-alpha.3] — 2026-05-20 — fix(parity): close all 6 Bun-CLI argv gaps + init becomes one-step + safety + friendlier release notes

- `init` now runs `vault-sync` automatically, collapsing the previous 2-step bootstrap into one; `--no-sync` skips it for offline/CI use.
- Closes 6 Bun-CLI argv gaps the Rust port had dropped (`vault-sync --branch`, positional args on `session-init`/`checkpoint`/`register-schedule`/`init`, `migrate` positional).
- Unifies the flag surface — every `--vault` flag now also accepts `--vault-dir` as a visible clap alias.
- `vault-sync` refuses to write at filesystem root or the literal `$HOME` — a defensive guard against foot-cannons.
- `migrate <name>` rejects supplying both the positional `[cutoff_date]` and `--cutoff <date>` together.
- GitHub Release body now renders a friendly platform table so non-Rust users can pick the right download.
- README rewritten with the platform table + one-step quickstart; CONTRIBUTING.md added.
- Adds 9 new integration tests; suite now at 634 passing.

## [3.0.0-alpha.2] — 2026-05-20 — fix(release): Windows TARGET expansion in release pipeline

- fix(release): adds `shell: bash` to Build/Strip steps so `$TARGET` expands correctly on Windows runners; unblocks 7/7 platform builds. (PR [#20](https://github.com/onebrain-ai/onebrain-cli/issues/20))

## [3.0.0-alpha.1] — 2026-05-20 — feat(slices-7-13): Bun parity port + 2 v3.0.1 fixes

- Ports the full Bun CLI parity surface (slices 7–13): `init` (vault bootstrap + schedule presets + `register-hooks`), `vault-sync` (9-step release-overlay flow), `register-hooks`, `register-schedule` (launchd plists, skill/command mode, one-shot `at:`), `update` (GitHub releases fetch + atomic swap), `run-skill`, `migrate`, `doctor` (8 read-only checks), and `orphan-scan` (Active-Session Guard). (PR [#2](https://github.com/onebrain-ai/onebrain-cli/issues/2), [#3](https://github.com/onebrain-ai/onebrain-cli/issues/3), [#9](https://github.com/onebrain-ai/onebrain-cli/issues/9)–[#16](https://github.com/onebrain-ai/onebrain-cli/issues/16))
- Fixes 2 parity regressions found during the port: `init` reporting `hooks: ok` while `.claude/settings.json` was never written (slice 10); `vault-sync` silently exiting 0 on a caught error with no message (slice 13).
- New core modules: `onebrain-core::scheduler` (cron/launchd, ports Bun 1:1), `onebrain-fs::init`/`orphan` (injectable IO closures for offline/TTY-free tests), `load_vault_config_at` for direct-path config loading.
- `VaultFolders` extended from 1 key (`logs`) to all 8 standard PARA keys, matching Bun's `DEFAULT_FOLDERS`.
- `doctor --fix` auto-repair deferred to v3.0.1 per spec §7.10 — flag is parsed but emits a stub message; doctor itself is parity-green.
- New workspace deps: `regex`, `dirs`, `libc`, `indexmap`, `inquire` (interactive prompts).
- `.github/workflows/release.yml` 7-platform release pipeline (tar.gz/zip + sha256); `CHANGELOG.md` reformatted to the repo's compact style (PR [#5](https://github.com/onebrain-ai/onebrain-cli/issues/5)).
- Post-merge hardening on PR [#3](https://github.com/onebrain-ai/onebrain-cli/issues/3) (ENOENT vs EACCES differentiation, `frontmatter` visibility fix, boundary tests) plus repo metadata (description, homepage, topics, branch ruleset).

## [3.0.0-alpha.0] — 2026-05-19 — feat(slice-1): session-init + 4-crate workspace foundation

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
