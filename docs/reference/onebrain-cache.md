# onebrain-cache

## Purpose & dependencies
`onebrain-cache` owns host/runtime state that lives outside the vault — session-token resolution, checkpoint cadence state (the `$TMPDIR/onebrain-{token}.state` file), checkpoint-NN derivation from on-disk checkpoint files, qmd index/embedding status detection, and detached qmd-reindex spawning. It depends only on `onebrain-core` (for `SessionToken`, `CoreError`, and `load_vault_config_at`). It is consumed by `onebrain-cli`, which reaches into it for `session init` (token resolve + stale-state cleanup), the Stop/reset checkpoint hooks, and the qmd `status`/`reindex` commands. All public surface is re-exported from `lib.rs`.

## Module map
```
src/
  lib.rs            crate root · re-exports the public API from every module
  error.rs          CacheError enum (CacheIo + transparent Core wrapper) + Result alias
  session_token.rs  8-layer session-token resolution chain + stale .state cleanup
  checkpoint.rs     Stop/reset hook logic · threshold checks · checkpoint-NN derivation
  state.rs          CheckpointState type + atomic read/write of the 3-field .state file
  qmd.rs            qmd status query + text parsing (QmdStatus) + unembedded count
  qmd_reindex.rs    detached `qmd update -c <collection>` spawn (cross-platform args)
```

## `src/lib.rs`
Crate root. No logic — declares the six modules and re-exports their public items so callers use `onebrain_cache::{...}` directly.
**Connections** — calls: every module; called by: `onebrain-cli`.

## `src/error.rs`
Crate error type, `thiserror`-derived.
**Key types** — `CacheError` — enum with `CacheIo { path, source }` (filesystem failure at a path) and `Core(#[from] onebrain_core::CoreError)` (transparent passthrough). `Result<T>` — alias for `std::result::Result<T, CacheError>`.
**Connections** — calls: `onebrain_core::CoreError`; called by: all other modules in this crate (return type) and `onebrain-cli`.

## `src/session_token.rs`
Resolves a stable per-session token via an 8-layer priority chain mirroring Bun v2.3.3 `resolveSessionToken`. Also cleans up stale state files.

**Token-resolution priority chain** (first match wins, in `resolve_session_token`):

| # | Layer | Source | Treatment |
|---|-------|--------|-----------|
| 1 | `WT_SESSION` | Windows Terminal env | sanitize + truncate to 8 chars |
| 2 | `TMUX_PANE` | tmux pane id env | sanitize + truncate to 8 chars |
| 3 | `TERM_SESSION_ID` | macOS Terminal env | sanitize + truncate to 8 chars |
| 4 | `find_claude_ancestor_pid` | Unix process-tree walk-up (12 hops) | returns claude PID, sanitized · **no cache write** |
| 5 | day-scoped cache | `$TMPDIR/onebrain-day-YYYYMMDD.token` | read-only; accepts only numeric value > 1 |
| 6 | `ppid` | parent PID (numeric) | sanitize · **write to cache** · return |
| 7 | PowerShell parent PID | Windows only · 3000ms timeout subprocess | sanitize · write to cache · return |
| 8 | random 5-digit | `[10000, 99999]` LCG-mixed | write to cache · return |

**Key types**
- `ProcInfo` — `{ ppid: u32, comm_basename: String }`; a process's parent PID + path-stripped, `.exe`-trimmed command name (compared directly against `"claude"`).
- `ProcLookup` — `Arc<dyn Fn(u32) -> Option<ProcInfo> + Send + Sync>`; injectable PID-lookup fn (tests bypass real `ps`).
- `ResolveInputs` — all resolution inputs (env vars, `ppid`, plus `today_override` / `tmp_dir_override` / `proc_lookup` test seams); built in prod via `from_env`.

**Key functions**
- `resolve_session_token(inputs: &ResolveInputs) -> Result<SessionToken>` — runs the 8-layer chain above.
- `ResolveInputs::from_env() -> Self` — snapshots real env + parent PID for production callers.
- `find_claude_ancestor_pid<F>(start_pid: u32, lookup: F, max_depth: u32) -> Option<u32>` — walks the process tree (capped at 12 hops) for a `claude`-named ancestor.
- `clean_stale_state_file(token: &SessionToken, tmp_dir: &Path, process_start: SystemTime)` — deletes `$TMPDIR/onebrain-{token}.state` if its mtime predates `process_start`; quiet on ENOENT.
- `default_proc_lookup(pid) -> Option<ProcInfo>` (private, `#[cfg(unix)]`) — backs `ProcLookup` via `ps -o ppid=,comm= -p <pid>`; `#[cfg(not(unix))]` stub returns `None`.

**Connections** — calls: `onebrain_core::SessionToken` (`sanitize`, `sanitize_truncated`, `from_clean`), `chrono::Local`, `ps`/`powershell.exe` subprocesses; called by: `onebrain-cli` `session init` (resolve + stale cleanup).
**Tests** — heavy in-module coverage: each layer, walk-up depth cap/init-termination, cache filename format, numeric-only cache contract, stale-file keep/remove/ENOENT.

## `src/checkpoint.rs`
Stop-hook cadence logic: increments a message counter, checks vault-configured thresholds, and emits the `decision:"block"` JSON that drives checkpoint creation. Also resets state after `/wrapup`.

Constants: `SKIP_WINDOW = 60` (silent post-reset window, seconds), `MIN_ACTIVITY = 2` (floor to suppress blocks on trivial sessions).

**Key functions**
- `handle_stop(token, vault_root, now: u64, tmp_dir, stdout: impl Write)` — reads state; honors the 60s post-reset skip window (signed-subtraction semantics, no skip on backward clock skew); increments count; loads `checkpoint.messages`/`checkpoint.minutes`/`folders.logs` (defaults `15`/`30`/`07-logs`); if `count >= messages || elapsed >= minutes*60` and `count >= MIN_ACTIVITY`, derives the next NN and writes `{"decision":"block","reason":"NN since <start|checkpoint-NN>"}` to `stdout`, then resets state to `{count:0, last_ts:now, last_stop_nn:NN}`.
- `handle_reset(token, now: u64, tmp_dir)` — writes `{count:0, last_ts:now, last_stop_nn:"00"}` (on-disk `0:<now>:00`); called by the agent after `/wrapup`.
- `max_checkpoint_nn(vault_root, logs_folder, date, token) -> u32` (`pub(crate)`) — scans `<vault>/<logs>/checkpoint/` for `{date}-{token}-checkpoint-NN.md`, returns the highest NN (0 if none/dir missing).

**Connections** — calls: `state::{read_state, write_state, CheckpointState}`, `onebrain_core::load_vault_config_at`, `chrono` (Local-TZ date formatting); called by: `onebrain-cli` checkpoint/Stop hook + reset command. The block JSON is consumed by the Claude Code Stop hook.
**Tests** — extensive: skip-window boundaries (exact 60s, backward skew), message/minute threshold firing, min-activity floor, fresh-state elapsed handling, NN derivation, custom logs folder.

## `src/state.rs`
Owns the on-disk checkpoint state file `$TMPDIR/onebrain-{token}.state` in 3-field `count:last_ts:last_stop_nn` format.

**Key types** — `CheckpointState` — `{ count: u32, last_ts: u64, last_stop_nn: String }`; `CheckpointState::fresh()` yields `{0, 0, "00"}` (disk form `0:0:00`).
**Key functions**
- `read_state(token, tmp_dir) -> CheckpointState` — parses the file; on missing/malformed input returns `fresh()` and eagerly rewrites the file to fresh-on-disk so later reads short-circuit.
- `write_state(token, state, tmp_dir)` — atomic write-then-rename via a pid-suffixed temp file; errors logged to stderr, never propagated.
- `state_file_path(token, tmp_dir) -> PathBuf` (`pub(crate)`), `parse_state(raw) -> Option<CheckpointState>` (private, strict 3-field + 2-digit-NN validation).
**Connections** — calls: `std::fs`; called by: `checkpoint.rs` (every state read/write) and re-exported for `onebrain-cli`.
**Tests** — roundtrip, fresh-on-missing/malformed/wrong-field-count, atomic temp-file cleanup.

## `src/qmd.rs`
Queries `qmd status`, parses its human-readable text output, and reports index/embedding health. Designed for silent fallback — a missing or hung qmd never blocks the caller.

`QMD_TIMEOUT_MS = 2000` caps every subprocess call.

**Key types** — `QmdStatus` (`Serialize`, all `Option`) — `total_files: Option<u64>`, `embedded_vectors: Option<u64>`, `pending_embedding: Option<u64>`, `index_size: Option<String>`, `last_updated: Option<String>`.

**How status is parsed** — `query_status()` spawns `qmd status` (platform-wrapped: direct on Unix, `powershell.exe -Command "qmd ..."` on Windows) via `capture_qmd_stdout`, then `parse_status` line-matches prefixes — `Total:` → `total_files`, `Vectors:` → `embedded_vectors`, `Pending:` → `pending_embedding`, `Size:` → `index_size`, `Updated:` → `last_updated`. Numeric fields take the first `u64` token on the line; unmatched lines leave their field `None`. (Text parsing rather than `--json` because qmd ≤ 2.1.0 ignores `--json`.)

**Key functions**
- `query_status() -> Option<QmdStatus>` — runs `qmd status`; `None` when qmd is unavailable/empty, else best-effort parsed struct.
- `query_unembedded_count() -> usize` — `query_status().and_then(|s| s.pending_embedding).unwrap_or(0)`; the count surfaced at session-init.
- `capture_qmd_stdout(args, timeout_ms) -> Option<String>` (private) — spawns, reads stdout on a worker thread with `recv_timeout`, kills child on timeout; `None` on any failure.

**Connections** — calls: `qmd` subprocess (via `powershell.exe` on Windows); called by: `onebrain-cli` session-init (unembedded count) and qmd `status` command.
**Tests** — verbatim qmd-2.1.0 sample parse, missing-field `None`, zero-pending vs none, all-garbage → all-`None`, no-panic when qmd absent.

## `src/qmd_reindex.rs`
Fire-and-forget reindex: spawns a detached `qmd update -c <collection>` background process. Always returns `Ok(())` (matches Bun's exit-0 contract).

**Spawn mechanism** — `qmd_reindex` loads vault config; silently returns `Ok(())` if config is missing/malformed or `qmd_collection` is absent/empty (Bun JS-truthiness parity). Otherwise it builds args via `build_qmd_spawn_args` and hands them to an injected `spawn_fn` closure (production passes a closure doing `Command::spawn` with platform detach flags; tests pass a recorder). Spawn failure writes `qmd-reindex: <error>` to stderr but still returns `Ok(())`.

**Key types** — `SpawnOs` — `enum { Unix, Windows }`; `SpawnOs::from_env()` maps `std::env::consts::OS` (everything non-Windows → `Unix`).
**Key functions**
- `build_qmd_spawn_args(collection: &str, os: SpawnOs) -> Vec<String>` — Unix: `["qmd","update","-c",collection]`; Windows: `["powershell.exe","-NoProfile","-Command","qmd update -c '<collection>'"]` with embedded single quotes doubled (`''`).
- `qmd_reindex<F>(vault_root: &Path, os: SpawnOs, spawn_fn: F) -> std::io::Result<()>` where `F: FnOnce(&[String]) -> std::io::Result<()>` — the entry point above.
**Connections** — calls: `onebrain_core::load_vault_config_at`, the injected spawn closure; called by: `onebrain-cli` qmd `reindex` command.
**Tests** — Unix/Windows arg shapes, single-quote doubling, no-spawn on missing/empty/malformed config, `Ok(())` on spawn failure.

## Entry points
- `resolve_session_token(&ResolveInputs)` + `ResolveInputs::from_env()` — get the session token (CLI `session init`).
- `clean_stale_state_file(&SessionToken, &Path, SystemTime)` — drop a stale prior-session state file at startup.
- `handle_stop(token, vault_root, now, tmp_dir, stdout)` / `handle_reset(token, now, tmp_dir)` — Stop-hook cadence and post-wrapup reset.
- `read_state` / `write_state` + `CheckpointState` — direct checkpoint-state access.
- `query_status() -> Option<QmdStatus>` / `query_unembedded_count() -> usize` — qmd index health.
- `qmd_reindex(vault_root, os, spawn_fn)` + `build_qmd_spawn_args` + `SpawnOs` — trigger a background reindex.
- `CacheError` / `Result<T>` — error surface for all of the above.
