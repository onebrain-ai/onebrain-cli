# Test Coverage

The CLI workspace targets **maximum testable line coverage on "core" code** — command
handlers, parsers, pure helpers, and dispatch — while explicitly excluding code that
cannot be meaningfully exercised in tests (network installs, blocking servers, TTY
wizards, OS-specific probes).

> **Realistic ceiling (verified 2026-06-30):** literal 100% is **not** reachable on the
> stable toolchain. cargo-llvm-cov has no inline `// coverage:ignore` support, and
> `#[coverage(off)]` is nightly-only — still gated behind the experimental
> `coverage_attribute` feature (rust-lang/rust#84605, open); on rustc 1.96.0 stable it errors
> `E0658`. (Even if stabilized, annotating a whole arm/closure would suppress the surrounding
> reachable code too — documenting residuals with reasons is the better policy regardless.)
> So genuinely-unreachable lines — defensive `match` arms, `spawn_blocking` panic-only
> `JoinError` closures, post-`canonicalize` TOCTOU I/O errors, body-limit-middleware dead
> branches, platform-defensive arms — can be neither covered nor ignored. The target is
> therefore **≈99% core + a documented residual list (below) + a ratcheting CI gate set
> conservatively below the achieved % and raised over time**. (Decided over switching to nightly `#[coverage(off)]` or grcov, to keep
> the stable toolchain.)

Measure with:

```sh
scripts/coverage.sh          # summary, with the documented exclusions applied
scripts/coverage.sh --html   # detailed HTML report under target/llvm-cov/html
```

(`scripts/coverage.sh` wraps `cargo llvm-cov --workspace` with the `--ignore-filename-regex`
below. A clean run is required — `--no-clean -p <pkg>` reuses non-instrumented artifacts
and silently drops the binary crate from the report.)

## Excluded files (documented allowlist)

These are excluded from the coverage target because exercising them in a unit/integration
test would require mocking the network, spawning real subprocesses, running a blocking HTTP
server, or driving a TTY — out of proportion to the value. Each is listed with its reason so
the exclusion is an explicit decision, not a silent skip.

| File | Reason |
|---|---|
| `crates/onebrain-cli/src/main.rs` | Process bootstrap, panic-hook install, `std::process::exit` |
| `crates/onebrain-cli/src/commands/serve.rs` | Foreground blocking HTTP listener — never returns in a test |
| `crates/onebrain-cli/src/commands/update.rs` | GitHub releases API + binary self-replace (network) |
| `crates/onebrain-cli/src/commands/harness_run.rs` | Spawns `claude` / `gemini` |
| `crates/onebrain-cli/src/commands/search_query.rs` | Embedding-backed query verb — the `run_query`/`run_vsearch`/`run_lex` bodies open the real engine (model download). Still majority-embedding-shell (≈62% line with the file un-ignored, 2026-07-05), so it stays excluded. The v3.4.6 Track 2c daemon-routing LOGIC it added does NOT download a model and IS tested: the pure daemon-response mapping (`daemon_hits`, `file_stem`) is unit-tested in-file, and `run_query_via_daemon`'s HTTP round-trip is covered by the `daemon_client` live-server tests |
| `crates/onebrain-cli/src/commands/search_reindex.rs` | Embedding-backed reindex verb — model download in the CLI binary path (embed reindex, `ProgressReporter`, first-run model prompt, `--force` wipe, `run_lex_only`/`run_pending_only`). Still majority-embedding-shell (≈69% line un-ignored, 2026-07-05), so it stays excluded. The v3.4.6 Track 2c daemon-routing it added (`run_hook_via_daemon`/`run_reindex_via_daemon`) is download-free and covered end-to-end by the daemon-side `internal.rs` reindex-mode tests (`lex`/`all`/`pending`/`paths`) + the `daemon_client` live-server round-trip |
| `crates/onebrain-cli/src/commands/search_model_tui.rs` | Raw-mode ratatui event loop (`run`/`event_loop`/`render`/`perform_switch`) — needs a live TTY. The pure pieces it drives (`build_rows`, `sort_rows`, `next_sort`, `footer_text`, and every `AppState` transition incl. `perform_delete`) ARE unit-tested in-file; only the terminal-coupled shell is untestable, so the whole file is excluded per the "most of it is untestable" rule |
| `crates/onebrain-cli/src/server/chat.rs` | SSE streaming proxy to a live AI subprocess |
| `crates/onebrain-search/src/embed.rs` | fastembed model download + ONNX inference (multi-GB model, network) — logic exercised via the `Embed` trait + `FakeEmbedder`; real path validated by embed-gated tests |
| `crates/onebrain-fs/src/update/install.rs` | npm/bun/brew install subprocess + network |
| `crates/onebrain-fs/src/init/wizard.rs` | Interactive TTY prompts |
| `crates/onebrain-fs/src/vault_sync/progress.rs` | Terminal progress-bar rendering |
| `crates/onebrain-cli/src/output/progress.rs` | Terminal progress-bar rendering |
| `crates/onebrain-cache/src/session_token.rs` | Platform-specific PPID / terminal-session probes |

Partial residuals (a few genuinely-unreachable lines inside an otherwise-core file) **cannot**
be inline-ignored on the stable toolchain (see the ceiling note above) — they are instead
listed in the **Residual unreachable lines** section below with a per-line reason, and the
file stays just under 100%. A whole file only joins the exclusion table above when *most* of
it is untestable.

### Residual unreachable lines (documented, keep file <100%)

`crates/onebrain-cli/src/server/api.rs` (~80 lines, after phase 3b):
- `spawn_blocking` `JoinError` closures (fire only if the blocking closure panics): tree walk, task scan, file/raw read, upload, post/put/delete write, move, folder create/delete.
- Post-`canonicalize` I/O errors (TOCTOU vanish / EACCES on stat or read of an already-resolved path): file stat/read, raw stat/read.
- FS write failure after pre-validation (`write`/`create_dir_all` fails post-check): upload.
- `get_config` non-missing/non-YAML config-load error arm.
- Body-limit middleware rejects before the handler (`DefaultBodyLimit::max(MAX_RAW_BYTES)`): upload over-cap branch is dead.
- Canonicalize / server-fault arms (vault root canonicalize fails though root existed at bind).
- Platform-defensive `RootDir`/`Prefix` path-component arms (unreachable on unix — caught by `is_absolute()`; platform-defensive on Windows, where a drive-relative `C:foo` carries a `Prefix` component yet `is_absolute()` is false).
- Defensive "can't happen" arms (WalkDir entry `Err`/`strip_prefix`, `HeaderValue::from_str` on a sanitized filename, ancestor-loop past canonical root) + llvm-cov closing-brace region artifacts.

#### Warm daemon (high-assurance policy — NOT whole-file-excluded)

**Policy (2026-07-05):** security/filesystem-touching daemon code is held to the coverage target; only the irreducible OS/network/embed shell is residual. The extracted LOGIC — concurrent-start orchestration + lock reclaim (`acquire_start_lock`/`lock_owner_state`/`read_lock_pid`), path confinement (`confine_reindex_path`), `is_live`/health decision, discovery + version-skew (`version_decision`/`classify_last_state`), the retry policy (`retry_once`/`is_transport_error`), and the idle-shutdown predicate (`should_idle_shutdown`) — is unit-tested, plus live-server HTTP round-trips (`DaemonHandle` status/search/reindex/`is_live`, `discover`) and a real-binary concurrent-start orchestration test. Residual lines:

`crates/onebrain-cli/src/commands/daemon.rs`:
- `run_internal` (the detached daemon body: tokio runtime + blocking server + `$ONEBRAIN_*` env parsing + on-exit cleanup) — never returns in a test.
- `spawn_detached_run` (fork/exec + `setsid` `pre_exec`), `terminate` (SIGTERM + poll), `init_tracing` (global subscriber), `sigterm_future` (async signal handler) — OS-shell.
- Error-context arms in `write_pid`/`remove_pid`/`ensure_private_run_dir` (EACCES etc. on an already-created private dir) and llvm-cov closing-brace artifacts.

`crates/onebrain-cli/src/commands/daemon_client.rs`:
- `ensure_running` poll-after-spawn loop and `spawn_daemon_start` — spawn the real `onebrain` binary; the FAST path (`discover` succeeds) IS tested against a live server. (Per-vault slots, #230: `discover`'s version-skew restart now calls the IN-PROCESS `daemon::stop_slot`, not a binary spawn.)
- `discover`'s same-slot version-skew branch's `stop_slot` call and `with_retry`'s live `ensure_running` reconnect closure (a real transport failure mid-call) — the `retry_once` policy and `version_decision` are unit-tested with fakes; slot resolution (`resolve_slot`, `record_is_our_vault`, `slot_stem`, `all_slots`) and the same-vault reuse / different-vault-declines invariants are unit-tested (`discover_reuses_only_when_vault_matches`, `resolve_slot_maps_none_vault_and_unresolvable`, `record_is_our_vault_matches_and_rejects`) against a live server.
- The Track 2c PASSIVE `discover_matching` (CLI path) — its same-vault-routes + wrong-vault-declines-untouched arms are tested against a live server (`discover_matching_routes_same_vault_and_declines_other_untouched`); the `get` method (incl. its `404 → Ok(None)` arm + same-vault transport-retry) is tested against the live server too. Only `discover_matching`'s live-liveness-probe-fails-then-reclaim arm on a dead SAME-vault record is residual (a bound-then-vanished daemon).
- Error-context arms (`.context(...)` on `read`/`write`/`remove` I/O failures) + the test-only live-server helper's own error paths.

`crates/onebrain-cli/src/commands/mcp.rs`:
- `run()`'s backend-SELECTION arm on daemon-spawn failure (the `Err(daemon_err) => open_engine(...)` branch that constructs a `Direct` backend) — reaching it requires `ensure_running` to actually fail to spawn a real daemon in-process, which the test harness can't do without the binary. The fallback backend's BEHAVIOUR is fully tested (`direct_engine_fallback_answers_query_and_status`, `with_engine` on both backends); only the in-process selection-on-spawn-failure is residual OS shell.

`crates/onebrain-cli/src/commands/search_common.rs` — the Track 2c routing helpers are core (not excluded): `daemon_routing_disabled` and `route_to_daemon`'s disabled / no-daemon / wrong-vault-goes-direct-and-leaves-daemon-untouched arms are unit-tested (the vault-match itself is now per-vault slot resolution — `resolve_slot`/`canonical_vault_id`/`record_is_our_vault`, unit-tested in `daemon_client`). Only `route_to_daemon`'s same-vault-live routing needs a running daemon and is covered via `discover_matching`'s live-server test.

`crates/onebrain-cli/src/server/search.rs`:
- `get_vault_search` async error/timeout arms (`spawn_blocking` `JoinError`, `run_search` `Err`, 30s timeout) — fire only on a panic/hang.
- Semantic `run_hybrid` / `run_hybrid_held` hit-mapping AFTER a non-empty index — requires a real embedding model (`Engine::query`); forbidden by the no-download test rule. The empty-index early-returns + the lex-only degrade paths ARE tested; engine query correctness is covered by `onebrain-search`'s `FakeEmbedder` tests.

`crates/onebrain-cli/src/server/internal.rs`:
- A few `spawn_blocking` panic-mapper arms + the reindex/status engine-error `.map_err` arms (fire only on an engine fault).

## Status (2026-07-05 · v3.4.6)

- **Core (this initiative's target surface, exclusions applied): ~94.99% line** (`scripts/coverage.sh`,
  gate `CORE_LINE_THRESHOLD=94`). The v3.4.6 warm-daemon epic **un-excluded** `daemon.rs`,
  `daemon_client.rs`, and `server/search.rs` from the measured surface (high-assurance policy — see
  ADR 0023: security/filesystem-touching code must be covered, not whole-file-excluded). Including that
  harder-to-cover daemon code pulled the aggregate down from the prior ~95.5% (that number **excluded**
  the daemon code) while genuinely **raising** assurance; the ratchet was reset 95 → 94 to lock in the
  new, broader measured floor (see ratchet history below).
- Prior (2026-06-30, pre-daemon-un-exclusion): 95.59% macOS / ~95.5% Linux with the daemon files excluded.

Closed so far:
- Phase 1 — `v31/dispatch.rs` 76.94% → 86.70% (stub/verb exit-code tests).
- Phase 2 — `commands/doctor.rs` 87.55% → 94.20%, `commands/register_schedule.rs` 72.08% →
  91.30%, `vault_ctx.rs` 51.35% → 100%, `commands/run_skill.rs` 78.82% → 79.17%.
- Phase 3 (fs cluster, +94 tests) — `note/archive.rs` 80.25% → 94.20%, `note/move.rs` → 95.49%,
  `init/mod.rs` 89.11% → 94.14%, `init/marketplace.rs` → 90.62%, `vault_sync/pin.rs` 93.43% → 97.16%,
  `vault_sync/orchestrate.rs` → 93.61%, `vault_sync/sync.rs` → 98.13%, `register_hooks/settings.rs`
  78.26% → 85.71%, `register_hooks/hooks.rs` → 97.83%, `migrate.rs` → 95.34%, `doctor/vault_yml_keys.rs`
  → 97.45%, `v31/hook_rewriter.rs` → 97.98%, `output/dispatcher.rs` → 95.60%. Every target improved;
  none reached 100% — residuals are hard error/`EXDEV`/edge paths and defensive test-guard arms.
- Phase 3b (server/api.rs, +28 oneshot/unit tests) — `server/api.rs` 69.56% → 87.06%
  (covered no-vault 503s for all handlers, byte-range 206, forced-attachment, upload, move/folder
  conflict + 404/409, method 405, `If-Match` overwrite, error-mapping unit tests). The remaining
  ~80 lines are the documented **Residual unreachable lines** above (genuinely untestable on stable).
- Phase 3c (command-layer residuals, +47 tests) — `v31/dispatch.rs` 88.69% → 91.08%,
  `onebrain-fs/src/update/mod.rs` 89.62% → 92.62%, `commands/register_schedule.rs` 91.30% → 93.09%,
  `commands/doctor.rs` → 94.21%. Diminishing returns: these are earlier-phase residuals, so the
  remaining lines are mostly hard. Newly-documented residuals:
  - `v31/dispatch.rs` — the `dispatch()` body (~90 lines) is `process::exit(code)` arms, reachable
    ONLY via integration tests (assert_cmd spawning the binary), plus TTY-only animated-render paths.
    **Closing these is the clearest next win** (one exit-code integration test per remaining verb).
  - `onebrain-fs/src/update/mod.rs` — `windows_shell`, the `ureq` HTTP body of `default_fetch_latest_release`,
    `default_install_binary` (delegates to the already-excluded `install.rs`), `default_validate_binary`,
    `spawn_version_command` (real subprocess). Kept in-file (the orchestrator/cache/parse logic is testable).
  - `commands/register_schedule.rs` — `test_run` (spawns `onebrain run-skill`), non-unix `current_uid`,
    `home_dir()`-None / `create_dir_all` fault paths.
  - `commands/doctor.rs` — `fix_qmd_embeddings` qmd-not-on-PATH, `fix_plugin_cache` no-home,
    `fix_plugin_files` network sync, backup-I/O-fault, `emit_structured` debug-assert fallback.
- Phase 3d (dispatch exit-code integration tests, +9 assert_cmd tests) — `v31/dispatch.rs` 91.08% →
  **95.64%**. Covered the `process::exit` arms for `qmd reindex`, `plugin install`/`migrate`,
  `skill show`/`info`, and the early vault/arg guards of `serve`/`harness run`/`skill run` (exit
  before any subprocess). Remaining `dispatch.rs` residuals (real network/subprocess/TTY, not
  safely invocable): `vault sync` (git clone/pull), `skill run`/`harness run` real path (claude/gemini
  spawn — note onebrain probes absolute harness paths so stripping `PATH` can't force a fail), `daemon
  start`/`run` (process fork / blocking server), `plugin update` TTY-animated render.
- Long-tail mop-up (+80 unit tests) — core 95.21% → **95.59%**. Swept the many small-gap files:
  `note/{folder,delete,list,walker,stat,new}.rs`, `init/{safety,folders,enable_plugin}.rs`,
  `backup.rs`, `doctor/{settings_hooks,qmd,plugin}.rs`, `register_hooks/qmd.rs`, `orphan.rs`,
  `v31/{vault_current,plugin_update}.rs`, `commands/skill_inspect.rs`, `exit.rs`, `cli.rs`,
  `migration.rs`, `onebrain-cache/{state,qmd_reindex}.rs`. Gate raised 94 → 95.

**Ratcheting CI coverage gate — ACTIVE (Phase 4).** The `coverage` job in
`.github/workflows/ci.yml` runs `scripts/coverage.sh --ci-gate` on Linux and fails the build if
core line coverage drops below `CORE_LINE_THRESHOLD` (in `scripts/coverage.sh`). It is
**ratcheted UP, never down**, as coverage climbs (raise the threshold in a follow-up PR whenever a
new floor is comfortably held): started at **94** (v3.3.21), raised to **95** after the long-tail
mop-up gave ~0.5% Linux headroom (achieved ~95.5% Linux / 95.59% macOS), then **reset 95 → 94
(v3.4.6, 2026-07-05)** when the warm-daemon epic **un-excluded** `daemon.rs` / `daemon_client.rs` /
`server/search.rs` per the ADR 0023 high-assurance policy — including that harder-to-cover code
lowered the aggregate to ~94.99%, so the floor was reset to 94 to lock in the new (broader, more
honest) measured set. This is the one intentional exception to "ratchet up, never down": the drop
reflects *more* code being measured, not a coverage regression. Literal 100% is not the
bar (see the ceiling note up top); the gate locks in whatever % the documented-residual set leaves.

**No dead code inflates the residuals.** An audit traced every logically-unreachable "residual"
to its call sites: all are legitimate — compiler-required exhaustiveness arms, panic-avoiding
`unwrap_or_else` fallbacks guarding runtime-enforced invariants, and real OS-fault / network /
subprocess / `cfg(windows)` paths that execute at runtime but are impractical to trigger in a unit
test. None is removable dead code (deleting would break `match` totality or reintroduce panic risk).

Remaining core gaps to close (tracked, by reachable missed lines):
- The fs-cluster residuals not yet maxed (`register_hooks/settings.rs` ~86%, `init/{mod,marketplace}.rs`
  ~94/91%, `vault_sync/orchestrate.rs` ~94%) and the long tail of 1–6-missed-line files
  (`note/{folder,walker,delete,stat,write}.rs`, `init/{safety,folders}.rs`, `doctor/*`, `backup.rs`).
- A few unit-coverable render-helper edges in `v31/dispatch.rs` left after 3d
  (`render_plugin_update_text` empty-data guard, color/version-bump reload-hint branch, a writeln
  error edge) — small, unit-testable in a later sweep.
- `commands/run_skill.rs` interactive-spinner paths (needs a pty harness — likely promoted to the
  exclusion list).
- The documented residuals above (`server/api.rs`, `dispatch.rs` network/subprocess/TTY,
  `update/mod.rs` network, `register_schedule.rs` subprocess, `doctor.rs` network/fault) are at
  their testable ceiling.

`commands/doctor.rs`, `v31/dispatch.rs` (95.64%), `onebrain-fs/src/update/mod.rs`,
`commands/register_schedule.rs`, and `server/api.rs` were advanced in phases 3b–3d and are now near
their testable ceilings. **Phase 4 (the ratcheting CI gate) is done** — the initiative is locked at
core ~95.2%; further phases are optional fs-cluster/long-tail mop-up, each free to raise the gate.
The full phase history is the "Closed so far" list above; the detailed design lives in the
maintainer's project notes (not tracked in this repo).
