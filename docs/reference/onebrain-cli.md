# onebrain-cli

## Purpose & dependencies
`onebrain-cli` is the binary crate at the top of the OneBrain CLI workspace — it owns everything between `argv` and a process exit code. It parses the locked v3.1 `<noun> <verb>` command tree (`clap`), dispatches each verb to a handler, renders results through the canonical `Envelope<T>` + `serialize_for_mode` output stack across five output modes (text/json/yaml/table/tsv), prints the TTY-only branded banner, resolves the active vault, and centralises `CoreError → exit code` mapping. It also carries the v3.0→v3.1 migration layer: hidden aliases that emit a one-time rename notice before dispatching, a `.claude/settings.json` hook-path rewriter, and `E_NOT_IMPLEMENTED` stubs for the forward-declared command surface. Depends on **all three** in-workspace library crates — `onebrain-core` (config, `CoreError`, vault resolution, scheduler model), `onebrain-fs` (init, vault-sync, register-hooks, doctor checks, update, harness detection), `onebrain-cache` (session token, checkpoint stop/reset) — plus `clap`, `anyhow`, `serde`, `serde_json`, `serde_yaml`, `chrono`, `indicatif`, `dirs`. Nothing in-workspace depends on it.

## Module map
```
src/
├── main.rs              argv → clap → dispatch; help-banner pre-pass; structured-mode error renderer
├── cli.rs               clap command tree (Cli + Cmd) — 3 root verbs + 24 groups + 8 hidden v3.0 aliases
├── banner.rs            TTY-only branded wordmark banner + help-banner gating
├── exit.rs              CoreError → stable i32 exit-code mapping (walks anyhow chain)
├── vault_ctx.rs         CLI-side wiring for onebrain_core vault resolution (resolve/require/hook)
├── safety.rs            refuse_dangerous_vault_path guard for FS-mutating commands
├── migration.rs         v3.0→v3.1 one-time rename notice (state file + suppression env)
├── legacy_output.rs     SessionInitOutput/Block byte-stable shapes + serialize_for_mode
├── output/
│   ├── mod.rs           output stack root · re-exports Envelope/emit/OutputMode/TtyInputs
│   ├── envelope.rs      canonical Envelope<T> {version,command,ok,vault,data,warnings,error}
│   ├── mode.rs          OutputMode enum + 6-rule TTY resolver (resolve_output_mode)
│   └── dispatcher.rs    emit() — picks the serializer per OutputMode
├── commands/
│   ├── mod.rs           declares the 15 v3.0/legacy command-handler submodules
│   ├── session_init.rs  session init — hook-protocol session metadata JSON (incl. headless flag)
│   ├── checkpoint.rs    checkpoint stop/reset — auto-save cadence
│   ├── orphan_scan.rs   checkpoint orphans — orphan-checkpoint count
│   ├── harness.rs       harness detect — runtime detection
│   ├── harness_run.rs   harness run — ad-hoc prompt through claude/gemini (--mode with-context | ad-hoc)
│   ├── init.rs          init — vault scaffold wizard wiring
│   ├── update.rs        update — CLI self-update (json/tty/plain modes)
│   ├── doctor.rs        doctor — vault health checks (+ --fix recipes)
│   ├── migrate.rs       plugin migrate — one-shot vault migrations
│   ├── vault_sync.rs    vault sync — pull plugin tarball + overlay
│   ├── register_hooks.rs plugin install — .claude/settings.json hook wiring
│   ├── register_schedule.rs schedule register — launchd plist emission
│   └── run_skill.rs     skill run — headless claude/gemini spawn (--harness/--model · ONEBRAIN_HEADLESS)
└── v31/
    ├── mod.rs           v3.1 module root
    ├── dispatch.rs      central Cmd → handler dispatcher (the routing brain)
    ├── hook_rewriter.rs settings.json v3.0→v3.1 hook-arg rewriter + --json injection
    ├── plugin_update.rs plugin update — CLI-self-update-adjacent vault overlay workflow
    ├── vault_current.rs vault current — active-vault + resolution-source report
    └── stubs.rs         not_implemented / not_implemented_vault_required (exit 72 / 64)
```

## Top-level modules

### `src/main.rs`
Binary entry point. `argv → help-banner pre-pass → Cli::parse() → dispatch → exit`. Captures the resolved `OutputMode` before dispatch so the error path can render a canonical envelope.
**Key functions**
- `fn main()` — runs the pre-parse help-banner pass, parses `Cli`, dispatches, maps the result to an exit code via `exit::exit_code_for`, calls `std::process::exit`.
- `fn render_error(e: &anyhow::Error, mode: &OutputMode)` — text mode writes `Error: {e:#}` to stderr; structured modes build `Envelope::err` and force-emit it as JSON to stdout (R2-H2). Skips emission when the chain carries `AlreadyReported` (R2-H3).
- `fn error_code_and_message(e: &anyhow::Error) -> (&'static str, String)` — walks the anyhow chain for a `CoreError` (or `FsError::Core`) to extract its stable `E_*` code; falls back to `E_FS_ERROR`/`E_CACHE_ERROR`/`E_INTERNAL`.
**Connections** — calls: `banner::{argv_requests_help, emit_help_banner}`, `v31::dispatch::{dispatch, output_mode}`, `exit::exit_code_for`, `output::emit`; called by: OS process entry.

### `src/cli.rs`
The entire clap surface, locked at v3.1 per spec §2.4. `Cli` (global flags + `command: Cmd`) and `Cmd` (3 root verbs + 24 resource groups + 8 hidden v3.0 aliases). Every group's verb list is a `Subcommand` enum even when unimplemented — the tree shape itself is the v3.1 deliverable.
**Key types**
- `Cli` — global flags `--vault`, `--output {text,json,yaml,table,tsv}`, `--json`, `--yaml` (conflict), `--pretty`, `--no-color`, `--quiet`; all `global = true`.
- `Cmd` — root: `Init/Update/Doctor`; visible groups: `Checkpoint/Harness/Plugin/Schedule/Session/Skill/Vault`; `hide = true` stub-only groups: `Avatar/Bookmark/Bundle/Config/Daemon/Date/Dream/Frontmatter/Gateway/Inbox/Log/Memory/Note/Pause/Serve/Task`; hidden aliases: `SessionInitAlias/OrphanScanAlias/QmdReindexAlias/RegisterHooksAlias/RegisterScheduleAlias/MigrateAlias/VaultSyncAlias/RunSkillAlias`; `Qmd` (removed v3.4.5) is now a hidden catch-all that emits a migration error rather than a visible group.
- Per-group `*Cmd { verb: *Verb }` arg structs + `Legacy*Args` structs holding the verbatim v3.0 arg surface for back-compat.
**Connections** — parsed by: `main`; matched by: `v31::dispatch::dispatch`, `banner::is_hook_protocol`. Tests assert root help advertises exactly the 3 verbs + 8 visible groups, hidden aliases parse but don't surface in help, and new `<noun> <verb>` paths parse.

### `src/banner.rs`
TTY-only branded `OneBrain` block-art wordmark + dim `Your AI Thinking Partner · vX.Y.Z` tagline, emitted to **stderr**. Truecolor terminals get a continuous horizontal cyan→purple→pink hue gradient (matching the brain logo) with a top-lit vertical shade layered on; non-truecolor terminals fall back to a vertical-only xterm-256 gray ramp. Suppressed for `--quiet`, non-color/structured modes, and all hook-protocol commands.
**Key functions**
- `should_show_banner(cli, mode) -> bool` — pure gate: only `Text{color:true,..}` and non-hook-protocol commands qualify.
- `is_hook_protocol(cmd) -> bool` — true for `session init`, all `checkpoint *`, and their hidden aliases (incl. the legacy `qmd-reindex` alias, which now dispatches to native `search reindex` but stays banner-suppressed for un-migrated hooks); plain `search reindex` is not part of this gate.
- `render_banner() -> String` / `emit_banner<W>(w, cli, mode)` — build + gated stderr write.
- `argv_requests_help` / `argv_requests_version` — pre-parse argv scanners (clap prints help/version in-process before dispatch).
- `should_show_banner_for_help(mode, args, env: &HelpBannerEnv)` + `HelpBannerEnv::from_env` — pure help-path gate mirroring the color-suppression subset of the 6-rule chain.
**Connections** — calls: `OutputMode`; called by: `main` (help pre-pass) and `v31::dispatch::dispatch` (per-command).

### `src/exit.rs`
Single source of truth for `CoreError → i32` exit codes (skill-alignment §4.8). Stable for v3.x.
**Key items**
- Named constants: `EXIT_OK=0`, `EXIT_GENERIC=1`, `EXIT_INVALID_ARGS=2`, `EXIT_VAULT_NOT_FOUND=64`, `EXIT_INVALID_YAML=65`, `EXIT_FS_ERROR=66`, `EXIT_CACHE_ERROR=67`, `EXIT_NETWORK=68`, `EXIT_INVALID_DATE=70`, `EXIT_INVALID_TARGET=71`, `EXIT_NOT_IMPLEMENTED=72`, `EXIT_RPC_HANDSHAKE=73`, `EXIT_AUTH_FAILED=74`, `EXIT_INIT_TARGET_NOT_EMPTY=75`, `EXIT_ROLLBACK_INCOMPLETE=76`, `EXIT_ENGINE_BUSY=77` (native-search engine locked by another process — `E_ENGINE_BUSY`; a dedicated transient-failure code, semantically `EX_TEMPFAIL`, but 75 was already taken; see [ADR 0022](../decisions/0022-honest-search-lock-errors.md)).
- `exit_code_for_core(&CoreError) -> i32` — direct variant→code map.
- `exit_code_for(&anyhow::Error) -> i32` — walks the chain for `CoreError` (and the `FsError::Core(_)` passthrough that thiserror's `transparent` hides from `chain()`); then `FsError`/`CacheError` wrappers; then bare/`serde_json`-wrapped `io::Error` (BrokenPipe→0, PermissionDenied→66); else `EXIT_GENERIC`.
**Connections** — called by: `main` (drives `process::exit`) and `v31::dispatch` tests. Tests lock every variant→code mapping plus the broken-pipe/wrapped-error edge cases.

### `src/vault_ctx.rs`
CLI-side wiring for `onebrain_core::resolve_vault` — snapshots `--vault` flag + `ONEBRAIN_VAULT` env + cwd, then delegates to the pure core resolver (priority: flag > env > walk-up). Three vault-dependency classes.
**Key functions**
- `snapshot_inputs(flag) -> Result<VaultResolveInputs>` — builds the resolver input from live env+cwd.
- `resolve(flag) -> Result<Option<ResolvedVault>>` — vault-free/informational, never errors.
- `require(flag) -> Result<ResolvedVault>` — vault-required, errors `E_VAULT_NOT_FOUND` (exit 64). Dresses the error in the ✗/💡 `HintedError` contract via `dress_vault_not_found` (#288): the walk-up `VaultNotFound` gets a "run inside a vault / pass `--vault`" hint, and `NotAVault` (an explicit path that isn't a vault root) gets its own "check the path" hint — exit 64 preserved in both (the original `CoreError` stays in the chain under the `.context(..)` wrapper).
- `resolve_for_hook` / `info_from` / `print_vault_not_found_help` — reserved for v3.2+ hook-protocol and vault-required handlers (dead-code-allowed in v3.1).
**Connections** — calls: `onebrain_core::{resolve_vault, require_vault}`; called by: `stubs::not_implemented_vault_required`, `plugin_update`, `vault_current`, `doctor`.

### `src/safety.rs`
Shared filesystem-mutation guard.
**Key function** — `refuse_dangerous_vault_path(p: &Path) -> Result<()>` — bails on a filesystem root (`parent().is_none()`) or the literal `$HOME`/`%USERPROFILE%`; any dedicated subdirectory is allowed.
**Connections** — called by: `commands::vault_sync::run` and `commands::doctor` (`--fix` plugin-files recipe). Tests cover root, home, and subdirectory cases.

### `src/migration.rs`
Prints the v3.0→v3.1 rename notice once per alias, persisting shown aliases to `migration-shown.txt` under the platform cache dir. Suppressed via `ONEBRAIN_QUIET_MIGRATION=1`.
**Key functions** — `should_show(state_dir, old, env_suppress) -> bool` (pure), `record(state_dir, old) -> bool`, `write_notice<W>(w, old, new_path)`, `default_state_dir() -> PathBuf` (`ONEBRAIN_CACHE_DIR` test override), `print_once(old, new_path)` (the wired one-shot helper).
**Connections** — called by: every hidden-alias arm in `v31::dispatch::dispatch`. One-shot stderr persist-failure warning gated by an `AtomicBool`.

### `src/legacy_output.rs`
Byte-stable v3.0 output shapes + the structured-mode serializer shared by the hook-protocol commands.
**Key types** — `SessionInitOutput {datetime, session_token, search_unembedded, qmd_unembedded, recap_pending, headless}` — extends the Bun v2.3.3 shape; `search_unembedded` is `Option<usize>` (canonical since v3.4.5; `qmd_unembedded` is its deprecated same-value alias kept for wire back-compat) — reports the **native** search index's pending/unembedded count (no qmd subprocess involved); `recap_pending` is `Option<u64>` — session logs without a top-level `recapped:` frontmatter key (the `/recap` discovery criterion); for all three, `null` when the probe can't determine the count (index missing / timed out / walk error) — distinct from a genuine `0` — the keys are always present; `SessionInitBlock {decision, reason, error_detail}` with `init_required()` (`onebrain-vault-not-found`) and `vault_malformed(detail)` (`onebrain-vault-malformed`) constructors.
**Key functions** — `serialize_for_mode<T>(value, mode) -> String` — JSON (compact/pretty), YAML, Table/Tsv→compact-JSON fallback; Text-mode arrival is a caller bug (`debug_assert!`, compact-JSON fallback in release). Serde failures are loud on stderr (avoids the v3.0 empty-stdout regression).
**Connections** — called by: `session_init`, `orphan_scan`, `harness`, `update`, `doctor`.

## `output/` — rendering & the Envelope
Every v3.1 command body builds typed `data`, wraps it in an `Envelope<T>`, and hands it to `dispatcher::emit`, which picks the serializer from the `OutputMode` resolved from CLI flags + env + TTY state. The envelope is the canonical machine contract; the legacy hook-protocol commands instead use `legacy_output::serialize_for_mode` for their byte-stable shapes.

**Envelope shape** (`version`, `command`, `ok`, `vault?`, `data?`, `warnings[]`, `error?`): `version` is `"1"` for the whole v3.x line; `command` is the dotted name (`"vault.current"`, `"plugin.update"`); `ok` is the success bit; `vault`/`error` are `skip_serializing_if None`; `data` serializes `null` (not skipped) on errors; `warnings` is always `[]` (never `null`).

### `src/output/mod.rs`
Output-stack root. Re-exports `emit`, `Envelope`, `ErrorInfo`, `VaultInfo`, `Warning`, `resolve_output_mode`, `OutputMode`, `TtyInputs`.
**Connections** — used by: `main`, `v31::dispatch`, `vault_current`, and every envelope-building handler.

### `src/output/envelope.rs`
The canonical envelope type and its construction discipline (R1 C1).
**Key types** — `Envelope<T: Serialize>` (fields `pub(crate)`, forcing construction through the constructors); `VaultInfo {name, path}`; `Warning {code, message}`; `ErrorInfo {code, message}`; `const ENVELOPE_VERSION = "1"`.
**Key functions** — `Envelope::ok(command, vault, data)`, `Envelope::err(command, vault, error)` (data=None), `Envelope::partial(command, vault, data, error)` (ok=false + both data & error, for mid-flight bails), `with_warning(self, code, msg) -> Self`, `ErrorInfo::new(code, msg)`. Each constructor carries a `debug_assert!` enforcing the ok/error/data invariant.
**Connections** — built by: `vault_current`, `plugin_update` summary, `main::render_error`; serialized by: `dispatcher::emit`. Tests lock field presence/skip behavior and the partial-failure shape.

### `src/output/mode.rs`
`OutputMode` + the pure 6-rule TTY resolver.
**Key types** — `OutputMode` = `Text{color,pretty} | Json{pretty} | Yaml | Table | Tsv`; `is_structured(&self) -> bool` (everything but `Text`). `TtyInputs` (output_flag, json/yaml shortcuts, pretty, no_color, stdout_is_tty, no_color_env, term_env, ci_env) with `TtyInputs::from_env(...)`.
**Key functions** — `resolve_output_mode(&TtyInputs) -> OutputMode`: precedence `--json` > `--yaml` > `--output <fmt>`; text mode applies the 6-rule monochrome chain (`--no-color` / `NO_COLOR` / `TERM=dumb` / truthy `CI` / piped stdout). JSON pretty-prints only on a TTY (or `--pretty`).
**Connections** — `from_env` called by `v31::dispatch::output_mode` + `main` help pre-pass; consumed everywhere. Extensive `#[cfg(test)]` matrix over each rule.

### `src/output/dispatcher.rs`
Single emit point for envelopes.
**Key function** — `emit<T,W,F>(envelope, mode, writer, text_render) -> Result<()>`: JSON (compact/pretty + trailing newline), YAML, TSV (generic `command\tok` header+row fallback), and Table/Text (invokes the lazy `text_render` closure, ensures trailing newline). Structured modes never invoke `text_render`.
**Connections** — called by: `vault_current::run`, `main::render_error`, `dispatch::emit_plugin_update_summary_to`. Tests cover each mode + double-newline avoidance.

## `commands/` — command handlers
One handler module per working v3.0/legacy verb; `commands/mod.rs` just declares them. The clap subcommand enum is **not** matched here — `v31::dispatch::dispatch` maps each `Cmd` variant to the right `commands::*::run`, passing the resolved `OutputMode` and vault flag. These handlers predate the envelope; the hook-protocol ones (`session_init`, `orphan_scan`, `harness`) render via `legacy_output::serialize_for_mode` and own per-command text renderers, while lifecycle handlers (`init`, `update`, `doctor`, `vault_sync`, `register_*`, `run_skill`) return an `i32` exit code that the dispatcher passes straight to `process::exit`.

### `src/commands/session_init.rs`
Implements `session init` (hook-protocol; SessionStart hook). Walks up for the vault, loads config, resolves the session token, and — only when the vault has a `search.collection` configured (falling back to the legacy `qmd_collection` key) — probes the **native** search index for the pending/unembedded doc count via `native_pending_bounded` (a time-boxed wrapper over `native_pending`, which checks the collection's cache dir exists via `commands::search_common::{collection_cache_dir, is_indexed}` before opening `onebrain_search::engine::Engine` and calling `.status(..).pending_total()`); no `qmd` subprocess is spawned. Emits `SessionInitOutput` (still carrying the `qmd_unembedded` field name for wire back-compat) or a `SessionInitBlock`. Library calls: `onebrain_core::{find_vault_root, load_vault_config}`, `onebrain_cache::{resolve_session_token, clean_stale_state_file}`, `onebrain_search::engine::Engine`. Output: text default, `--json`/`--yaml` structured (machine consumers must pass `--json`).

### `src/commands/checkpoint.rs`
Implements `checkpoint stop` and `checkpoint reset`. Resolves the session token, then delegates to `onebrain_cache::{handle_stop, handle_reset}` against `std::env::temp_dir()` cache. `stop` writes checkpoint metadata to stdout; `reset` clears the cadence counter. Library: `onebrain_cache`. Output: handled inside the cache crate (hook-protocol).

### `src/commands/orphan_scan.rs`
Implements `checkpoint orphans` (and the `orphan-scan` alias; SessionStart hook). Calls `onebrain_fs::scan_orphans(logs_folder, session_token, now, vault_root) -> OrphanScanResult`. Output: text (`N orphan checkpoints found …`) default, structured via `serialize_for_mode` (`{"orphan_count":N}`).

### `src/commands/harness.rs`
Implements `harness detect` (default verb when none given). Calls `onebrain_fs::detect_harnesses(&cwd)`, emits `HarnessOutput {harnesses}`. Library: `onebrain_fs`. Output: text (`Detected harnesses: …`) default, structured via `serialize_for_mode`.

### `src/commands/init.rs`
Implements `init`. Thin wiring around `onebrain_fs::init::run_init(InitOptions)`; `--yes` skips prompts + installs the Essentials preset, otherwise supplies real `inquire`-backed confirm/preset closures (`ask_overwrite_vault_yml`, `ask_continue_nonempty`, `ask_initialize_here`, `ask_schedule_preset`). Returns `i32` exit code. Library: `onebrain_fs::init`.

### `src/commands/update.rs`
Implements `update` (CLI binary self-update). Three output modes — structured (`--json`/`--plan`/global format flag → one document, orchestrator logs suppressed), TTY (framed `🧠 OneBrain Update` header + colorized phase lines + a braille `indicatif` spinner — matching `doctor` — on the fetch and install phases; the version check is padded to a deliberate beat so it reads as real work even on a warm cache), plain. Calls `onebrain_fs::update::run_update(UpdateOptions)`; `--check`/`--plan` force dry-run. Returns `result.exit_code`. Library: `onebrain_fs::update`. **Post-upgrade daemon retire (#291):** on the real-install path only, wires `UpdateOptions::post_update_fn` to `daemon::stop_all_slots` so every now-stale warm daemon is retired after the validate gate (never on `--check`/no-op/failure); the count surfaces as `↻ retired {n} warm daemon(s)` (text) / `daemons_retired` (JSON).

### `src/commands/doctor.rs`
Implements `doctor`. Resolves vault via `vault_ctx::resolve` (errors / failure-envelope when absent), loads config (best-effort), runs `onebrain_fs::doctor::run_all_checks`, prints the grouped report once (sequential-reveal on TTY, instant when piped/structured). Under `--fix` (text mode) the report's footer is deferred; `planned_action` classifies each issue (auto-fixable vs manual), the auto-fixable plan is previewed, `confirm_fix` gates on a `[y/N]` prompt (auto-yes under `--json`/`--yes`/non-interactive), then the auto recipes (`FixOutcome::{Fixed,Failed,Manual}`, incl. the new `folders` mkdir recipe) run and a single verdict footer is emitted. `--json` keeps the no-prompt "run every recipe" path. `doctor_rule_width` widens the header/footer rules to span the longest rendered line; the footer's `--fix` hint shows only when something is auto-fixable. After checks/fixes settle, `stamp_doctor_run` upserts `stats.last_doctor_run` (+ `last_doctor_fix` with `--fix`) via `upsert_doctor_stats` (comment-preserving line edit, no-ops when already today; best-effort). Returns 0 / 1. Library: `onebrain_fs::doctor`, `onebrain_core`, `safety`, `vault_ctx`. Output: text default, `--json`/`--yaml` structured. **Daemon version-skew warn (#291):** the local `daemon_status_check` flags a LIVE per-slot daemon whose stamped version differs from `own_version()` as a `warn` (naming both versions + the `onebrain daemon stop --all` hint) — diagnostic only, it never stops anything; the safety net for a user who `brew upgrade`d directly and never ran `onebrain update`/`plugin update`.

### `src/commands/migrate.rs`
Implements `plugin migrate` (and the `migrate` alias). Resolves vault root, loads config for the logs folder, dispatches by migration name — currently `backfill-recapped` via `onebrain_fs::run_backfill_recapped`. Always exits 0 (failures→stderr only) per the internal-command contract. Library: `onebrain_core::{find_vault_root, load_vault_config_at}`, `onebrain_fs`.

### `src/commands/vault_sync.rs`
Implements `vault sync` (and the `vault-sync` alias). Resolves vault root (positional/flag/walk-up), passes through `safety::refuse_dangerous_vault_path`, then `onebrain_fs::run_vault_sync(root, VaultSyncOptions{branch,..})` overlays the upstream release tarball. Returns `Ok(0)` / `Ok(1)`. Library: `onebrain_fs`, `onebrain_core::find_vault_root`, `safety`.

### `src/commands/register_hooks.rs`
Implements `plugin install` (and the `register-hooks` alias). Idempotent `.claude/settings.json` wiring via `onebrain_fs::register_hooks::run(RegisterHooksOptions{vault_dir,dry_run,remove})` — adds the Stop hook, an optional PostToolUse reindex hook (registered when `search.collection`/legacy `qmd_collection` is configured; internally still called the "qmd hook" for legacy naming, but its `HookSpec` now dispatches to native `onebrain search reindex --json`), and the OneBrain permission set. Prints a summary; returns 0 / 1. Library: `onebrain_fs::register_hooks`.

### `src/commands/register_schedule.rs`
Implements `schedule register` (and the `register-schedule` alias). Six flags route at the top (`--remove`/`--status`/`--test`/`--resume`/`--refresh`/`--dry-run`); otherwise validates each `schedule:` entry and emits launchd plists. Library: `onebrain_core::scheduler::*` (validate/generate/label/plist helpers + `LaunchdContext`). Output: plain status lines. Also invoked internally by `plugin_update` (`--refresh`).

### `src/commands/run_skill.rs`
Implements `skill run` (and the `run-skill` alias). The skill name is positional (`skill run daily`) or `--skill <name>` (parity with `run-skill`; clap `conflicts_with` rejects both). `--harness {claude,gemini,codex}` (default claude) picks the runtime and `--model <m>` the model. Prompts use `/onebrain:<name>` for Claude/Gemini and `$onebrain:<name>` for Codex. Codex runs `codex exec --sandbox workspace-write --skip-git-repo-check --ephemeral -C <vault>`; a managed unattended installation also enables trusted hooks. All harnesses inherit the environment, receive null stdin, and propagate their exit code.

## `v31/` — command-tree migration & v3.1 verbs
The v3.1 layer: `dispatch.rs` is the routing brain mapping every `Cmd` variant to a handler; hidden v3.0 aliases call the corresponding new-path handler **after** `migration::print_once`; `hook_rewriter` migrates on-disk `settings.json` hook args; `plugin_update` is the plugin-side overlay workflow (distinct from `commands::update`, which is the CLI binary self-update); `vault_current` is a new informational verb; `stubs` turns the forward-declared command surface into clean `E_NOT_IMPLEMENTED` (exit 72) or `E_VAULT_NOT_FOUND` (exit 64) responses.

### `src/v31/mod.rs`
Declares `dispatch`, `hook_rewriter`, `plugin_update`, `stubs`, `vault_current`.

### `src/v31/dispatch.rs`
Central dispatcher.
**Key items**
- `AlreadyReported` — sentinel `Error` attached as anyhow context by commands that already emitted their envelope (so `main::render_error` skips a duplicate while the exit code still propagates; R2-H3).
- `output_mode(&Cli) -> OutputMode` — wraps `TtyInputs::from_env` + `resolve_output_mode`.
- `dispatch(cli: Cli) -> Result<()>` — emits the banner, then the giant `match` over `Cmd`: root verbs → `commands::{init,update,doctor}` (which `process::exit`); hook-protocol verbs → legacy handlers; new verbs → `vault_current`/`plugin_update`; everything else → `stubs::{not_implemented, not_implemented_vault_required}`; hidden aliases → `migration::print_once` then the new handler. `harness` with no verb defaults to `Detect`.
- `emit_plugin_update_summary_to<W>(report, mode, writer) -> Result<()>` — renders the `plugin update` envelope (`Envelope::ok`/`partial` + plumbed rewriter warnings), with an injectable writer for the broken-pipe test.
**Connections** — calls: every `commands::*` handler, `vault_current`, `plugin_update`, `stubs`, `banner`, `migration`; called by: `main`. Tests cover the `AlreadyReported` downcast quirk + broken-pipe→exit-0 classification.

### `src/v31/hook_rewriter.rs`
Rewrites v3.0 hook entries in `.claude/settings.json` to v3.1 paths during `plugin update`. Strictly additive + fully idempotent; only touches entries whose `command == "onebrain"`.
**Key items** — `REWRITES` (`session-init`→`session init`, `orphan-scan`→`checkpoint orphans`, `qmd-reindex`→`search reindex`, `qmd reindex`→`search reindex`, preserving trailing args); `JSON_REQUIRED_PREFIXES` (`session init`, `checkpoint orphans`, `checkpoint stop`, `search reindex` must carry `--json` since the v3.1 default is now text); `RewriteReport {rewrites, total, json_flag_added, warnings}`; `RewriteWarning {code, message}`. `rewrite_hooks(&mut Value) -> RewriteReport`, `rewrite_settings_file(path, dry_run) -> Result<RewriteReport>` (load→rewrite→write-back, no-op on missing file). Malformed entries (non-string `command` / non-array `args`) emit `W_MALFORMED_HOOK_ENTRY` and are skipped; `--json` injection is gated by `has_explicit_format_flag` so an explicit `--yaml`/`--output` is never clobbered.
**Connections** — called by: `plugin_update::run`. Extensive `#[cfg(test)]` over path rewrites, flag injection, idempotency, and malformed-entry warnings.

### `src/v31/plugin_update.rs`
Implements `plugin update` — the plugin-side workflow (CLI self-update lives in `commands::update`). Idempotent 4-step flow.
**Key items** — `PluginUpdateReport {vault_synced, hooks_rewritten, plists_rewritten, plists_count, version_before, version_after, dry_run, partial_failure, warnings, daemon_retired}`. `run(vault_dir, branch, dry_run) -> Result<PluginUpdateReport>`: (1) `vault_ctx::require` vault, (2) `commands::vault_sync::run` tarball pull, (3) `hook_rewriter::rewrite_settings_file`, (4) `commands::register_schedule::run(refresh=true)` to rebind launchd plists, (5) **conditional skewed-daemon retire (#291)** — `retire_skewed_daemon(vault_root)` resolves this vault's slot, reads its `DaemonInfo`, and `daemon::stop_slot`s it ONLY when `version_decision(info.version, own_version()) == Restart` (a no-op when versions match, so a normal plugin update pays no cold-start penalty); sets `daemon_retired` and never bubbles `Err` (partial-failure convention). Covers the `brew upgrade` + `plugin update` flow that leaves a stale-version daemon serving the old wire shape. Steps 1–2 failures bubble as `Err`; a step-4 failure after step-3 success sets `partial_failure` and returns `Ok(report)` so the dispatcher renders an `E_PLUGIN_UPDATE_PARTIAL` partial envelope. `plists_count` (`Some(N)`/`Some(0)`/`None`) and `version_before`/`version_after` drive the framed renderer's per-step detail + `vX → vY` verdict; `daemon_retired` renders a `↻ retired warm daemon` line + a JSON field (omitted when false).
**Connections** — calls: `vault_ctx`, `commands::{vault_sync, register_schedule}`, `hook_rewriter`, `settings_path`; called by: `dispatch` (`Plugin::Update`).

### `src/v31/vault_current.rs`
Implements `vault current` (new in v3.1) — reports the active vault and **how** it resolved (`--vault flag` / `ONEBRAIN_VAULT env` / `walk-up`). Soft-fails (never `Err`) but validates `vault.yml`.
**Key items** — `VaultCurrentData {detected, name?, path?, source?, cwd, error?}`. `run(flag, mode) -> Result<()>`: resolves via `vault_ctx::resolve`, gates `detected` on a successful `load_vault_config` (a resolved-but-malformed vault sets `detected=false` + populates `error`), then `output::emit`s an `Envelope::ok("vault.current", …)`. `render_text` shows the 3-line detected block, an invalid-vault block, or a quickfix block.
**Connections** — calls: `vault_ctx::resolve`, `onebrain_core::load_vault_config`, `output::emit`; called by: `dispatch` (`Vault::Current`). Tests cover detected/not-detected/invalid-yaml renders + JSON field-skip.

### `src/v31/stubs.rs`
Turns the forward-declared command surface into clean exits.
**Key functions** — `not_implemented(path: &str) -> Result<()>` (→ `CoreError::NotImplemented`, exit 72); `not_implemented_vault_required(vault_flag, path) -> Result<()>` (`vault_ctx::require` first so outside-vault returns exit 64, else exit 72; R1 C3).
**Connections** — called by: most arms of `dispatch`. Returns plumb through `main`'s envelope/exit-code path rather than panicking.

## Command → handler map

| `onebrain <noun> <verb>` | Handler file | Library crate it drives |
|---|---|---|
| `session init` | `commands/session_init.rs` | onebrain-core (config, vault) · onebrain-cache (token) · onebrain-search (native pending-count probe) |
| `checkpoint stop` | `commands/checkpoint.rs` | onebrain-cache (`handle_stop`) |
| `checkpoint reset` | `commands/checkpoint.rs` | onebrain-cache (`handle_reset`) |
| `checkpoint orphans` | `commands/orphan_scan.rs` | onebrain-fs (`scan_orphans`) |
| `harness detect` | `commands/harness.rs` | onebrain-fs (`detect_harnesses`) |
| `vault sync` | `commands/vault_sync.rs` | onebrain-fs (`run_vault_sync`) · onebrain-core (`find_vault_root`) |
| `vault current` | `v31/vault_current.rs` | onebrain-core (`load_vault_config`) + vault_ctx |
| `plugin install` | `commands/register_hooks.rs` | onebrain-fs (`register_hooks::run`) |
| `plugin update` | `v31/plugin_update.rs` | onebrain-fs (vault-sync, register-hooks settings) + hook_rewriter; drives vault_sync + register_schedule |
| `plugin migrate` | `commands/migrate.rs` | onebrain-fs (`run_backfill_recapped`) · onebrain-core (config) |
| `schedule register` | `commands/register_schedule.rs` | onebrain-core (`scheduler::*`, launchd plists) |
| `skill run` | `commands/run_skill.rs` | onebrain-core (`find_config_file`) · onebrain-fs (`build_prompt`, `resolve_claude_bin`, `resolve_gemini_bin`) |
| `init` | `commands/init.rs` | onebrain-fs (`init::run_init`) |
| `update` | `commands/update.rs` | onebrain-fs (`update::run_update`) |
| `doctor` | `commands/doctor.rs` | onebrain-fs (`doctor::run_all_checks`) · onebrain-core (config) |
| all other `<noun> <verb>` | `v31/stubs.rs` | none (exit 72, or 64 outside a vault) |

Hidden v3.0 aliases route through the same handlers after a one-time migration notice: `session-init`→`session_init`, `orphan-scan`→`orphan_scan`, `qmd-reindex`→`search_reindex` (dispatches to the native `search reindex` handler, kept for un-migrated hooks), `register-hooks`→`register_hooks`, `register-schedule`→`register_schedule`, `migrate`→`migrate`, `vault-sync`→`vault_sync`, `run-skill`→`run_skill`.

## Entry points
- `fn main()` (`src/main.rs`) — process entry; help pre-pass → `Cli::parse()` → dispatch → `process::exit`.
- The clap tree `Cli` / `Cmd` (`src/cli.rs`) — the entire parsed command surface (3 root verbs + 24 groups + 8 hidden aliases).
- `v31::dispatch::dispatch(cli: Cli) -> Result<()>` (`src/v31/dispatch.rs`) — the command dispatcher that fans every `Cmd` variant out to its handler.
