# onebrain-fs

## Purpose & dependencies
`onebrain-fs` is the filesystem-effects crate of the OneBrain CLI workspace — everything that reads, writes, walks, downloads, or mutates files on disk lives here. It owns: vault frontmatter parsing, orphan-checkpoint scanning, harness detection, timestamped config backups, the `onebrain init` bootstrap, the `doctor` health-check set (the `Box<dyn Check>` collection), the self-update install path (direct GitHub-Release tarball fetch + atomic binary swap — explicitly **not** npm/bun), `register-hooks` settings.json wiring, the `run-skill` prompt/binary resolver, idempotent structure migrations, and the multi-step `vault-sync` plugin-tarball overlay. It depends only on **`onebrain-core`** in-workspace (for `VaultConfig`, `CoreError`/`FsError` chaining, `Harness`, `DoctorResult`, config-file discovery, vault-root helpers). External crates: `serde`/`serde_json`/`serde_yaml`, `thiserror`, `chrono`, `walkdir`, `tar`/`flate2`, `ureq` (blocking, sync), `indicatif`, `inquire`, `which`, `wait-timeout`, `tempfile`, `dirs`, `semver`, `indexmap`, `is-terminal`, `filetime`. The binary crate `onebrain-cli` depends on it and drives every public entry point.

## Module map
```
src/
├── lib.rs                Crate root · curated re-exports (scan_orphans, run_init, run_update, run_vault_sync, …)
├── backup.rs             Timestamped config backups (.onebrain-backups/) — hard precondition before any config overwrite
├── error.rs              FsError enum (Io + #[from] CoreError) + crate Result alias
├── frontmatter.rs        parse_frontmatter — CRLF-aware YAML-mapping frontmatter extractor (crate-private)
├── harness.rs            detect_harnesses / detect_harness — Claude/Gemini/Direct runtime detection
├── migrate.rs            run_backfill_recapped — idempotent session-log `recapped:` backfill migration
├── orphan.rs             scan_orphans — unmerged-checkpoint scan with Active-Session Guard
├── run_skill.rs          build_prompt + resolve_claude_bin/resolve_gemini_bin — pure helpers for `onebrain skill run`
├── doctor/               health checks (Box<dyn Check> set)
│   ├── mod.rs            Check trait + run_all_checks orchestrator (qmd probe on bg thread)
│   ├── folders.rs        FoldersCheck — 8 PARA folders present
│   ├── marketplace.rs    ClaudeSettingsCheck — stale marketplace repo in settings.json
│   ├── orphans.rs        OrphanCheckpointsCheck — count unmerged checkpoint files
│   ├── plugin.rs         PluginFilesCheck — required plugin files/dirs + stale bash scripts
│   ├── qmd.rs            QmdEmbeddingsCheck — non-fatal `qmd status` probe
│   ├── settings_hooks.rs SettingsHooksCheck — Stop/qmd hooks + permission validation
│   ├── vault_config_migration.rs  VaultConfigMigrationCheck — legacy vault.yml → onebrain.yml
│   ├── vault_yml.rs      VaultYmlCheck — config file present + valid YAML
│   └── vault_yml_keys.rs VaultYmlKeysCheck — config schema (required keys, enums, deprecated)
├── init/                 vault scaffolding
│   ├── mod.rs            run_init orchestrator + InitOptions/InitResult + injectable IO closures
│   ├── enable_plugin.rs  enabledPlugins.onebrain@onebrain merge into settings.json
│   ├── folders.rs        create_folders — 8 PARA dirs + 00-inbox/imports/
│   ├── marketplace.rs    marketplace.json writer (canonical-shape validate/repair on --force)
│   ├── onebrain_yml.rs   render/write onebrain.yml from a SchedulePreset
│   ├── presets.rs        SchedulePreset (Minimal/Essentials/MaintenancePlus/Skip) + ScheduleEntry
│   ├── safety.rs         classify — target-directory safety check (DirState)
│   └── wizard.rs         inquire-backed interactive prompts (default impls)
├── note/                 vault note operations (v3.2.0 · `onebrain note <verb>`)
│   ├── mod.rs            module root · re-exports the 11 verb fns + Options/Result types
│   ├── walker.rs         walk_notes (skips tooling+archive) · walk_all · walk_notes_with_archive
│   ├── search.rs         search_notes — substring / `--mode regex` line scan (size-capped regex)
│   ├── list.rs           list_notes — metadata listing sorted by name/mtime/created
│   ├── find.rs           find_notes — glob (basename or path) + --type + Unix --mtime
│   ├── read.rs           read_note — --section / --frontmatter-only / --tasks-only / body(--limit)
│   ├── stat.rs           stat_note — line/word/char/link/task/heading counts
│   ├── backlinks.rs      backlinks — incoming [[wikilink]] scan (+ --include-archive)
│   ├── orphans.rs        orphans — notes with zero incoming wikilinks (exclusion set)
│   ├── append.rs         append_note — section-aware append (atomic tmp+rename)
│   ├── new.rs            new_note — --template + inline --frontmatter (atomic write)
│   ├── archive.rs        archive_note — move to <root>/YYYY/MM/ (date-injectable)
│   └── move.rs           move_note — transactional vault-wide wikilink rewrite + rollback
├── register_hooks/       harness hook wiring
│   ├── mod.rs            run + RegisterHooksOptions/Result + HookStatus
│   ├── hooks.rs          HookSpec + Stop-hook apply/migrate/strip primitives
│   ├── permissions.rs    14-entry permissions.allow apply/strip
│   ├── qmd.rs            qmd PostToolUse hook apply/migrate/strip + legacy `qmd update` dedupe
│   └── settings.rs       settings.json read + atomic 4-space write
├── update/               self-update install path
│   ├── mod.rs            run_update orchestrator + GitHub release fetch/cache + version helpers
│   └── install.rs        AssetInfo triple detection + download/extract/atomic binary swap (NOT npm)
└── vault_sync/           plugin tarball overlay
    ├── mod.rs            module root · re-exports run_vault_sync + helpers
    ├── types.rs          VaultSyncOptions/VaultSyncResult + FetchFn/UnlinkFn/NowFn aliases
    ├── orchestrate.rs    run_vault_sync — stitches steps 1–9
    ├── branch.rs         resolve_branch — update_channel → main/next
    ├── download.rs       download_tarball + extract_tarball + build_tar_spawn_overrides
    ├── walker.rs         list_files_recursive — best-effort recursive file listing
    ├── sync.rs           sync_plugin_files / sync_gemini_config / sync_obsidian (overlay + stale removal)
    ├── docs.rs           copy_root_docs — CONTRIBUTING/CHANGELOG/PLUGIN-CHANGELOG
    ├── harness_merge.rs  merge_harness_files — merge @-import lines into CLAUDE/GEMINI/AGENTS.md
    ├── vault_yml.rs      update_vault_yml — write update_channel back (atomic, backup-first)
    ├── pin.rs            pin_to_vault — refresh installed_plugins.json entries + normalize_path
    ├── cache_clean.rs    clean_plugin_cache — rm stale cache/*/onebrain/* version dirs
    └── progress.rs       Progress trait + TtyProgress (indicatif) / PlainProgress
```

## Top-level modules

### `src/lib.rs`
Crate root — declares the modules and re-exports the curated public surface so consumers can `use onebrain_fs::{...}`. `frontmatter` is `pub(crate)`; everything else is `pub`.
**Connections** — re-exports from all sibling modules; called by: `onebrain-cli` (entry surface).

### `src/backup.rs`
Timestamped config backups — copies the current config file into `<vault>/.onebrain-backups/<name>.<YYYYMMDD-HHMMSS>.bak` before any overwrite/migration. Backup is a hard precondition: callers propagate a failure rather than write.
**Key types** — `const BACKUP_DIR: &str = ".onebrain-backups"`; `const MAX_BACKUP_ATTEMPTS: u32 = 10_000`.
**Key functions** — `backup_config_file(config_path: &Path) -> Result<Option<PathBuf>>` — `Ok(None)` only when genuinely absent (`symlink_metadata` ENOENT); any other stat error propagates; per-second `-N` uniquifier so a same-second backup never clobbers.
**Connections** — calls: `chrono::Local`, `fs::{symlink_metadata,copy,create_dir_all}`; called by: `vault_sync::vault_yml::update_vault_yml` (and any config-overwrite path).
**Tests** — covers no-op-on-absent, copy-into-dir, same-second non-clobber, legacy-filename preservation, present-but-uncopyable (broken symlink) → error.

### `src/error.rs`
The crate's error enum.
**Key types** — `FsError` (`thiserror`): `Core(#[from] CoreError)` (transparent) and `Io { path: PathBuf, source: std::io::Error }`; `type Result<T> = Result<T, FsError>`.
**Connections** — wraps `onebrain_core::CoreError`; produced throughout the crate; consumed by `onebrain-cli` exit-code mapping.

### `src/frontmatter.rs`
CRLF-aware markdown frontmatter parser (crate-private).
**Key functions** — `parse_frontmatter(raw_text: &str) -> Option<serde_yaml::Value>` — recognises `---\n…\n---`; returns `Some` only for a top-level YAML mapping, `None` for any other shape / missing delimiter / parse error (callers treat `None` as "no frontmatter").
**Connections** — called by: `orphan::has_manual_session_log`. (`migrate.rs` reimplements a body-preserving variant locally rather than reuse this.)
**Tests** — LF/CRLF, missing delimiters, non-mapping, invalid YAML.

### `src/harness.rs`
Which AI runtime(s) a vault targets.
**Key functions**
- `detect_harnesses(vault_root: &Path) -> Vec<Harness>` — priority: `ONEBRAIN_HARNESS` env override (`claude`/`claude-code`→Claude, `gemini`→Gemini, `direct`→Direct, garbage→stderr warn + fall through) → `.claude/`/`.gemini/` dir presence (both possible) → `[Direct]` fallback. Always ≥1.
- `detect_harness(vault_root: &Path) -> Harness` — back-compat shim, first detected (default Direct).
- `detect_harnesses_with_env(..)` (crate-private) — env-injectable inner driver for tests.
**Connections** — calls: `onebrain_core::Harness`; called by: `init::run_init`, `register_hooks::run`, `vault_sync::orchestrate`.
**Tests** — full matrix of dir/env combinations.

### `src/migrate.rs`
Idempotent vault-structure migrations — currently one: `backfill-recapped`. Byte-for-byte parity with the Bun reference.
**Key types** — `MigrateResult { backfilled: usize, skipped: usize }`.
**Key functions**
- `run_backfill_recapped(logs_folder, cutoff_date: Option<&str>, today: &str, warn: impl FnMut(&str)) -> MigrateResult` — walks `logs/session/YYYY/MM/*.md` whitelisted on `-session-`, adds `recapped: <today>` when missing; optional ISO `cutoff_date` skips strictly-newer files (boundary inclusive); `today` injected for testability; malformed/unreadable/unwritable → `skipped` + warn line.
- `parse_frontmatter_with_rest(raw) -> Option<(Mapping, String)>` (private) — body-preserving variant of `frontmatter::parse_frontmatter` (matches Bun's `/\n---(\n|$)/`, skips false `---some-text` closers).
**Connections** — calls: `fs::{read_dir,read_to_string,write}`, `serde_yaml`; called by: `onebrain-cli` migrate command. Pure FS — no network.
**Tests** — extensive (CRLF, EOF closer, false separators, cutoff boundary, EACCES write failure, idempotent rerun).

### `src/orphan.rs`
Orphan-checkpoint scan (port of Bun's `runOrphanScan`). An orphan is a checkpoint whose date ≠ today, token ≠ current session, date has no manual session log, AND whose group's newest mtime exceeds the Active-Session Guard.
**Key types** — `OrphanScanResult { orphan_count: usize }` (Serialize); `const MIN_GUARD_MINUTES = 60`, `DEFAULT_ACTIVE_SESSION_GUARD_MS`.
**Key functions**
- `scan_orphans(logs_folder, session_token: &str, now: DateTime<Local>, vault_root) -> Result<OrphanScanResult>` — always `Ok` (errors fold into safe defaults; the CLI never exits non-zero).
- `parse_checkpoint_filename(name) -> Option<(&str,&str)>` (crate-private) — `(date, token)` from `YYYY-MM-DD-{token}-checkpoint-NN.md`.
- `has_manual_session_log(month_dir, date) -> bool` — whitelist `-session-` infix; manual = no frontmatter OR `auto-saved` not truthy (bool `true` or string `"true"`).
- `collect_candidate_groups(..)` — per-token grouping with today/current-token/manual-log filters + per-date cache.
- `active_session_guard_ms(vault_root) -> u64` — `max(60min, 2×checkpoint.minutes)`; fail-safe 60 min on missing/malformed config (silent on ENOENT, stderr warn on real errors).
- `is_group_active_or_ambiguous(paths, now_ms, guard_ms) -> bool` — fail-safe `true` on empty/stat-fail/future-mtime/zero-guard; `age_ms < guard_ms` is strict.
- `get_mtime_ms` / `get_newest_mtime_ms` — mtime helpers (None on any stat failure → group ambiguous).
**Connections** — calls: `frontmatter::parse_frontmatter`, `onebrain_core::load_vault_config_at`, `chrono::Local`; called by: `onebrain-cli` `checkpoint orphans` command.
**Tests** — very thorough: filename parsing, manual-log detection, guard thresholds, predicate edge cases, full scan scenarios, group collection.

### `src/run_skill.rs`
Pure helpers for `onebrain run-skill` (the scheduler's headless skill launcher). All side effects (spawn, env reads) live in the CLI handler.
**Key types** — `RunSkillError::EmptySkill`; `HarnessBinResolution { path: PathBuf, warning: Option<String> }` (was `ClaudeBinResolution` pre-v3.2.6).
**Key functions**
- `build_prompt(skill: &str, args: &[(String,String)]) -> Result<String, RunSkillError>` — strips leading `/`, namespaces bare names with `onebrain:` (keeps explicit `plugin:` form), appends `k=v` tokens preserving order. Same form works for both harnesses (gemini exposes the namespaced `/onebrain:<skill>` custom command).
- `resolve_bin(bin, env_var, override_path, env_lookup, path_exists, home) -> HarnessBinResolution` — shared resolver: explicit override → `{env_var}` (if exists) → `$HOME/.local/bin/{bin}` → `/opt/homebrew/bin/{bin}` → `/usr/local/bin/{bin}` → bare `{bin}`. A missing `{env_var}` emits a warning string and falls through. Closures keep it deterministic/test-injectable.
- `resolve_claude_bin(..)` / `resolve_gemini_bin(..)` — thin wrappers over `resolve_bin` for `claude`/`CLAUDE_BIN` and `gemini`/`GEMINI_BIN` (v3.2.6 adds gemini).
**Connections** — called by: `onebrain-cli` `skill run` handler (which picks the resolver per `--harness`, builds the per-harness argv, and spawns).
**Tests** — full prompt-build matrix + claude/gemini bin-resolution priority.

## `doctor/` — health checks
Each check is a zero-sized unit struct implementing the sync `Check` trait. `run_all_checks` builds a `Vec<Box<dyn Check>>`, runs them serially, and splices in the qmd result. The plugin `/doctor` skill matches on the reported `check` name strings; `--fix` recipes (in the CLI) key off the hints these checks emit.

`trait Check { fn name(&self) -> &'static str; fn run(&self, vault_root: &Path, config: &VaultConfig) -> DoctorResult; }`

### `src/doctor/mod.rs`
Defines `Check` and `run_all_checks`. Canonical order: `onebrain.yml` · `onebrain.yml-keys` · `vault-config-migration` · `folders` · `plugin-files` · `settings-hooks` · `orphan-checkpoints` · `qmd-embeddings` · `claude-settings`.
**Key functions** — `run_all_checks(vault_root, config) -> Vec<DoctorResult>` — spawns `QmdEmbeddingsCheck` on a background thread first (it dominates wall time via `qmd status`), runs the 8 serial checks, then splices the qmd row at `QMD_SPLICE_POSITION = 7` (`debug_assert_eq!` guards order drift). A panicked qmd worker becomes a `warn` row, not `ok`.
**Connections** — re-exports every check struct; called by: `onebrain-cli` `doctor` command.

### `src/doctor/folders.rs`
**Reports `check` = `"folders"`.** Verifies the 8 PARA folders (from `config.folders`) exist. `ok` → "8/8 present"; missing → `error` "N/8 present" + hint listing them. **No `--fix`** (folders are recreated by `init`/`vault-sync`).
**Connections** — calls: `Path::is_dir`; reads `config.folders`.

### `src/doctor/marketplace.rs`
**Reports `check` = `"claude-settings"`.** Reads `.claude/settings.json`; warns when `extraKnownMarketplaces.onebrain.source.repo` is the stale `kengio/onebrain` (canonical is `onebrain-ai/onebrain`; the plugin now loads via `enabledPlugins`). Missing file → `ok`; invalid JSON → `warn`. **Has `--fix`** (hint: "remove the stale marketplace entry").
**Connections** — calls: `serde_json`, `Value::pointer`.

### `src/doctor/orphans.rs`
**Reports `check` = `"orphan-checkpoints"`.** Recursively walks the logs folder (`walkdir`) counting `*-checkpoint-*.md` (covers both flat post-v2.4.0 and nested legacy layouts). `warn` when >0 with hint "Run /wrapup to synthesize and merge them". **No CLI `--fix`** — remediation is the `/wrapup` skill. (Distinct from `orphan.rs::scan_orphans`, which is the guard-aware startup count.)
**Connections** — calls: `walkdir::WalkDir`; reads `config.folders.logs`.

### `src/doctor/plugin.rs`
**Reports `check` = `"plugin-files"`.** Checks `.claude/plugins/onebrain/` for required files (`INSTRUCTIONS.md`, `.claude-plugin/plugin.json`) and non-empty dirs (`agents/`, `skills/`); also flags 7 stale bash scripts (`session-init.sh`, `orphan-scan.sh`, …). Missing → `error` (takes precedence); stale → `warn`; clean → `ok` with "N skills · M agents · INSTRUCTIONS.md ✓". Hint points at `onebrain update`. **Remediated by `onebrain update`/`vault-sync`, not `doctor --fix`.**
**Connections** — calls: `walkdir`, `fs::read_dir`.

### `src/doctor/qmd.rs`
**Reports `check` = `"qmd-embeddings"`.** Non-fatal `qmd status` probe (15-second `wait-timeout` — a real multi-MB index can take ~10 s); parses `Total: N` / `Pending: M`. Missing `qmd_collection` → `warn` ("Run /qmd to set up search index"); pending>0 → `warn` with advisory hint ("run /qmd embed … or onebrain doctor --fix"); pending 0 → `ok`; any spawn/timeout/parse failure → `ok` (never blocks doctor). **Has `--fix`** for the pending case (advisory). `run_with(probe, config)` injects a stub `QmdProbe { NotFound, Timeout, Stdout, Error }` for tests; `real_qmd_probe` resolves `qmd` via `which` (Unix; falls back to `$HOME/.bun/bin`) or `powershell.exe` (Windows).
**Connections** — calls: `which`, `wait_timeout::ChildExt`, `std::process::Command`.

### `src/doctor/settings_hooks.rs`
**Reports `check` = `"settings-hooks"`.** Validates `.claude/settings.json`: Stop hook present in canonical exec form, qmd PostToolUse hook present when `qmd_collection` set, no onebrain commands under disallowed events, no stale `*.sh` wrapper references, and `Bash(onebrain *)` permission granted. Missing file → `warn`; invalid JSON → `error`; any issues → `warn` ("N issue(s)"); clean → `ok`. **Has `--fix`** (hint: "repair/register hooks"). Internals: `HookForm { Exec, Legacy, Absent }`, `effective_command`, `is_canonical`, `detect_hook_form` (canonical beats a legacy duplicate).
**Connections** — calls: `serde_json`; reads `config.qmd_collection`.

### `src/doctor/vault_config_migration.rs`
**Reports `check` = `"vault-config-migration"`.** Warns when on legacy `vault.yml`: legacy-only or both-present → `warn`; canonical-only / neither → `ok` (silent, `VaultYmlCheck` owns the missing-config error). **Has `--fix`** (single atomic `fs::rename`; hint: "migrate vault.yml to onebrain.yml" / "remove the stale vault.yml").
**Connections** — calls: `Path::is_file` with `CONFIG_FILENAME`/`LEGACY_CONFIG_FILENAME`; `const CHECK_NAME`.

### `src/doctor/vault_yml.rs`
**Reports `check` = `CONFIG_FILENAME` (`"onebrain.yml"`).** Reads whichever config exists (canonical preferred, legacy fallback via `find_config_file`). Missing/unreadable → `error` ("Run onebrain init"); invalid YAML → `error`; valid → `ok` "valid" + detail lines for `update_channel`/`qmd_collection`. **No `--fix`** (creation is `init`).
**Connections** — calls: `onebrain_core::find_config_file`, `serde_yaml`.

### `src/doctor/vault_yml_keys.rs`
**Reports `check` = `"onebrain.yml-keys"`.** Schema validation (port of Bun `checkVaultYmlKeys`): required `folders` + 8 sub-keys (errors), soft-required `update_channel` (warn), `update_channel` enum `stable|next` (error), `checkpoint.messages`/`.minutes` numeric>0 (warn), deprecated `onebrain_version`/`method`/`runtime.harness` (warn). Hint string varies by which combination fired. **Has `--fix`** (backfill defaults / remove deprecated / repair). Helpers: `is_positive_number`, `yaml_to_display`.
**Connections** — calls: `find_config_file`, `serde_yaml`.

## `init/` — vault scaffolding
`run_init` is the `onebrain init` core (port of Bun `init.ts`). Steps: target-dir safety check → config-file guard → interactive confirm → schedule preset → write `onebrain.yml` → create folders → register-hooks (best-effort) → write marketplace.json + enable plugin (Claude harness only) → embedded vault-sync (best-effort). All IO surfaces (confirm/preset/register-hooks/vault-sync prompts + stdout/stderr sinks) are injectable closures on `InitOptions`, so the whole flow is unit-testable offline.

### `src/init/mod.rs`
Orchestrator + public types.
**Key types**
- `InitOptions` — `vault_dir`, `force`, `yes`, `structured_output`, `skip_vault_sync`, plus injectable `confirm_fn`/`preset_fn`/`register_hooks_fn`/`vault_sync_fn` and `stdout_lines`/`stderr_lines` line sinks.
- `InitResult` — `ok`, `exit_code`, `message`, `folders_created`, `vault_yml_written`, `preset_installed`, `hooks_registered`, `vault_sync_ok`, `marketplace_written`, `plugin_enabled`, `aborted`.
- closure aliases `ConfirmFn`/`PresetFn`/`RegisterHooksFn`/`VaultSyncFn`.
**Key functions** — `run_init(opts: InitOptions) -> Result<InitResult, FsError>`. Critical safety: re-init (`--force` or confirmed overwrite) re-registers the plugin + completes the scaffold but **never rewrites an existing config** (preserves `qmd_collection`/custom keys; missing keys are repaired by `doctor --fix`). `--force` suppresses the prompt but `safety::classify` always runs. Re-exports `default_register_hooks` / `default_vault_sync` and the four wizard fns.
**Connections** — calls: `safety::classify`, `onebrain_yml::write_onebrain_yml`, `folders::create_folders`, `register_hooks::run`, `marketplace::write_marketplace_json_with_force`, `enable_plugin::enable_onebrain_plugin_with_force`, `vault_sync::run_vault_sync`, `harness::detect_harnesses`; produces `CoreError::InitTargetNotEmpty` (exit 75). Called by: `onebrain-cli` `init`.
**Tests** — large `#[cfg(test)]` block (fresh vault, existing-config guard, legacy-vault skip, force preserves config, preset selection, abort paths, EACCES/regular-file classify under --force).

### `src/init/enable_plugin.rs`
Merges `enabledPlugins.onebrain@onebrain = true` into `.claude/settings.json` (without it Claude Code never loads the plugin).
**Key types** — `const PLUGIN_KEY = "onebrain@onebrain"`; `EnableOutcome { wrote: bool, warning: Option<String> }`.
**Key functions** — `enable_onebrain_plugin_with_force(vault_dir, force) -> Result<EnableOutcome>` — no-op if already `true`; non-bool existing value → refuse (`CoreError::InitTargetNotEmpty`) unless `--force` (then overwrite + warning). Malformed JSON surfaces as a hard error. Helpers `already_enabled`/`set_enabled` (defensive object promotion); test-only `enable_onebrain_plugin`.
**Connections** — calls: `register_hooks::settings::{read_settings,write_settings,settings_path}`; called by: `init::run_init`.

### `src/init/folders.rs`
**Key types** — `STANDARD_FOLDERS: [&str; 8]` (`00-inbox`…`07-logs`), `INBOX_IMPORTS_SUBDIR = "imports"`.
**Key functions** — `create_folders(vault_dir) -> Result<usize, FsError>` — idempotent; creates each folder + `00-inbox/imports/` if absent; returns count of newly-created paths (9 on a fresh vault).
**Connections** — calls: `fs::create_dir_all`; called by: `init::run_init`.

### `src/init/marketplace.rs`
Writes `<vault>/.claude-plugin/marketplace.json` (required for Claude Code to discover the bundled plugin). Atomic tmp+rename, 4-space indent.
**Key types** — `MarketplaceOutcome { Written, Skipped, Repaired }`.
**Key functions** — `write_marketplace_json_with_force(vault_dir, force) -> Result<MarketplaceOutcome>` — default skips existing files; `--force` validates canonical shape (`plugins[0].name == "onebrain"` && `source == "./.claude/plugins/onebrain"`) and rewrites malformed/wrong-shape → `Repaired`. A write failure propagates as `Err` (not swallowed). Helpers `is_canonical_existing`, `atomic_write_canonical`, `pretty_4_space`.
**Connections** — calls: `serde_json`, `fs::rename`; called by: `init::run_init`.

### `src/init/onebrain_yml.rs`
Generates `onebrain.yml` (v3.1 canonical filename) with `update_channel`, default folders, default checkpoint (15 msgs / 30 min), and the preset's `schedule:` block (omitted when empty).
**Key functions** — `render_onebrain_yml(preset) -> Result<String, FsError>` (pure); `write_onebrain_yml(vault_dir, preset) -> Result<(), FsError>` (overwrites unconditionally — guard belongs in `run_init`).
**Connections** — calls: `serde_yaml`, `presets::SchedulePreset::entries`, `onebrain_core::CONFIG_FILENAME`; called by: `init::run_init`.

### `src/init/presets.rs`
Schedule preset definitions (mirror of `_shared/schedule-presets.md`).
**Key types** — `SchedulePreset { Minimal, Essentials (default), MaintenancePlus, Skip }`; `ScheduleEntry` (untagged serde enum: `Skill { cron, skill, args }` / `Command { cron, command, args }`).
**Key functions** — `SchedulePreset::{label, entries, all}` (Minimal=1, Essentials=3, MaintenancePlus=6-with-1-command, Skip=0); `ScheduleEntry::{skill, command}` constructors.
**Connections** — calls: `indexmap::IndexMap`, `serde`; consumed by `onebrain_yml` + re-exported via `init` and crate root.

### `src/init/safety.rs`
Target-directory safety check (runs before any write, even under `--force`).
**Key types** — `DirState { Missing, Empty, OneBrainVault, NonEmptyNonVault { summary } }`.
**Key functions** — `classify(vault_dir) -> Result<DirState>` — ENOENT→Missing, regular file→NonEmptyNonVault, config present→OneBrainVault, zero entries→Empty, else NonEmptyNonVault with a count+sample summary; permission errors propagate as `FsError::Io`. Non-recursive; hidden files count.
**Connections** — calls: `fs::{symlink_metadata,read_dir}`, `CONFIG_FILENAME`/`LEGACY_CONFIG_FILENAME`; called by: `init::run_init`.

### `src/init/wizard.rs`
`inquire`-backed default prompts (replaced by injected closures in tests/headless).
**Key functions** — `default_confirm` (non-TTY stdin → `false`), `ask_initialize_here`, `ask_overwrite_vault_yml`, `ask_continue_nonempty` (prints context to stderr), `ask_schedule_preset` (defaults to Essentials cursor; non-TTY → Skip).
**Connections** — calls: `inquire::{Confirm,Select}`, `IsTerminal`; re-exported via `init::mod` for the CLI binary.

## `note/` — vault note operations (v3.2.0)
The `note` resource group: 11 `onebrain note <verb>` commands operating over an already-resolved vault root. This module is the pure fs-layer (vault-walking + text/IO); the CLI layer (`onebrain-cli/commands/note_*`) wraps each result in the canonical `Envelope<T>`. `mod.rs` re-exports every verb's fn plus its `Options`/`Result` types.
**Key functions** — `walk_notes` (shared `.md` enumeration, skips tooling + `06-archive`; `walk_all` / `walk_notes_with_archive` variants serve `find` / `--include-archive`) feeds: `search_notes` (substring/regex), `list_notes` (sorted metadata), `find_notes` (glob + `--type` + `--mtime`), `read_note` (section/frontmatter/tasks/body), `stat_note` (counts), `backlinks`, `orphans`, and the write verbs `append_note` / `new_note` / `archive_note` / `move_note` — the last is transactional with a vault-wide `[[wikilink]]` rewrite + rollback + `--dry-run`. Bad regex/glob input → `CoreError::InvalidTarget` (exit 71).
**Connections** — uses: `walkdir`, `regex`, `globset`, `crate::frontmatter::parse_frontmatter`, `chrono`; atomic writes via tmp+rename; called by: `onebrain-cli` `commands/note_*` handlers.

## `register_hooks/` — harness hook wiring
Idempotent mutation of `.claude/settings.json` for the Claude harness: registers the Stop hook + (conditionally) the qmd PostToolUse hook + 14 permissions, migrates legacy shapes in place, and strips everything on `--remove`. Gemini/Direct harnesses are no-ops. All JSON goes through `serde_json::Value` so unknown keys survive round-trips.

### `src/register_hooks/mod.rs`
Entry point + result types.
**Key types** — `RegisterHooksOptions { vault_dir, dry_run, remove }`; `RegisterHooksResult` (`#[non_exhaustive]`: `ok`, `stop`, `qmd`, `permissions_added/removed`, `wrote`, `vault_dir`, `claude_harness`, `remove_mode`, `direct_mode`); `HookStatus`.
**Key functions** — `run(opts) -> Result<RegisterHooksResult>` — detects harness (`[Direct]`→`direct_mode` no-op; Gemini-only→no-op; Claude→proceed), reads qmd_collection best-effort, then applies/strips hooks + permissions and atomic-writes unless `dry_run`.
**Connections** — calls: `harness::detect_harnesses`, `onebrain_core::{find_vault_root,load_vault_config}`, `settings`/`hooks`/`qmd`/`permissions` submodules; called by: `init::run_init` and `onebrain-cli` `register-hooks`.
**Tests** — fresh/idempotent/dry-run/remove/qmd-toggle/legacy-migration/unknown-key preservation.

### `src/register_hooks/hooks.rs`
Stop-hook primitives (port of Bun `register-hooks.ts`).
**Key types** — `HookSpec { command, args }` with `STOP = ["checkpoint","stop","--json"]` and `QMD = ["qmd-reindex","--json"]`; `Presence { Found, Migrate, Missing }`; `HookStatus { Added, Migrated, Ok }`.
**Key functions** — `matches_spec` (v3.1 canonical + v3.0 pre-json + both shell forms), `matches_spec_pre_json`, `append_json_if_needed` (v3.0→v3.1 flag append), `rewrite_if_shell_form` (shell→exec), `check_hook_presence`, `apply_hooks` (stale-event sweep + Stop register/migrate), `strip_onebrain_hooks`.
**Connections** — calls: `serde_json`; called by: `register_hooks::mod`, `register_hooks::qmd` (shared `HookSpec`/`rewrite`).
**Tests** — exhaustive match/rewrite/migrate/strip + stale-event sweep.

### `src/register_hooks/permissions.rs`
**Key types** — `PERMISSIONS_TO_ADD: &[&str]` (14 entries: `Read`/`Write`/`Edit`/`Glob`/`Grep`, several `Bash(...)`, `WebFetch`/`WebSearch`).
**Key functions** — `apply_permissions(settings) -> Vec<String>` (append missing only; preserves order/user entries); `strip_permissions(settings) -> Vec<String>` (remove only the 14 managed entries).
**Connections** — calls: `serde_json`; called by: `register_hooks::mod`.

### `src/register_hooks/qmd.rs`
qmd PostToolUse hook + legacy `qmd update …` migration/dedupe (matcher `Write|Edit`).
**Key functions** — `apply_qmd_hook(settings) -> HookStatus`, `strip_qmd_hook(settings) -> bool`, `migrate_legacy_qmd_entries(groups, keep_canonical) -> bool` (4 passes: rewrite/strip legacy → shell→exec → append `--json` → dedupe → splice empties), `is_legacy_qmd_cmd` (word-bounded `\bqmd\s+update\b`, matches powershell/bash-wrapped forms).
**Connections** — calls: `hooks::{HookSpec, matches_spec*, rewrite_if_shell_form, append_json_if_needed}`; called by: `register_hooks::mod`.
**Tests** — legacy/canonical/shell/wrapped forms, dedupe, strip, user-hook preservation.

### `src/register_hooks/settings.rs`
settings.json read + atomic write.
**Key functions** — `settings_path(vault_root) -> PathBuf`, `read_settings(path) -> Result<Value>` (missing→empty object, invalid JSON→`FsError::Io` InvalidData), `write_settings(path, value) -> Result<()>` (4-space pretty matching Bun `JSON.stringify(v,null,4)`, no trailing newline, atomic tmp+rename, creates parent).
**Connections** — calls: `serde_json` PrettyFormatter; called by: `register_hooks::mod`, `init::enable_plugin` (re-exported).

## `update/` — self-update install path
`run_update` self-updates the `onebrain` binary: read current version → fetch the latest GitHub release (cache-aware, semver-comparison to refuse downgrades) → install → validate (atomic gate). The install path fetches the GitHub-Release tarball directly and atomically swaps the running binary — it explicitly does **not** use npm or bun (the v3 Rust binary was never published to npm). All four IO surfaces are injectable for offline tests.

### `src/update/mod.rs`
Orchestrator + GitHub fetch/cache + pure version helpers.
**Key types** — `UpdateResult { ok, exit_code, latest_version, current_version, error, latest_published_at }`; `ReleaseInfo`; `CurrentVersion`; `UpdateOptions { check, fresh, fetch_fn, install_fn, validate_fn, current_version_fn, stdout/stderr_lines }`; `UpdateError { GithubStatus, MissingTag, Network, Decode, Install, InstallBinary, Spawn }`.
**Key functions** — `run_update(opts) -> UpdateResult`; `default_fetch_latest_release(fresh)` (1 h on-disk cache, `ONEBRAIN_GITHUB_RELEASES_URL` override; endpoint is `/releases?per_page=1` on the **CLI** repo); `parse_release_payload`; `default_install_binary(version)` → `install::fetch_and_swap_binary`; `default_validate_binary(expected)` (spawn `onebrain --version`, parse via `extract_version_token`, require `>= expected` semver — pure core split out as `validate_reported`); `default_current_version` (compile-time `CARGO_PKG_VERSION`); `version_at_least(current, candidate)` (semver-aware downgrade guard, string-eq fallback); helpers `extract_version_token`/`version_regex_matches`/`extract_version_prefix`/`extract_release_date`/`format_release_date`/`windows_shell`.
**Connections** — calls: `ureq`, `semver`, `dirs::cache_dir`, `chrono`, `install::fetch_and_swap_binary`; called by: `onebrain-cli` `update`.
**Tests** — full orchestrator paths (upgrade/check/up-to-date/fetch-fail/install-fail/validate-fail) + cache round-trip/stale/corrupt + semver comparison matrix.

### `src/update/install.rs`
Direct GitHub-Release fetch + atomic binary swap (replaces the broken npm/bun path).
**Key types** — `AssetInfo { triple, extension, binary_name }`; `const RELEASES_DOWNLOAD_BASE`, `DOWNLOAD_ENV_OVERRIDE` (`ONEBRAIN_GITHUB_RELEASES_DOWNLOAD_URL`), `DOWNLOAD_TIMEOUT_SECS = 90`.
**Key functions**
- `fetch_and_swap_binary(version, current_exe) -> Result<(), UpdateError>` — resolve asset → build `{base}/v{version}/onebrain-{triple}.{ext}` → download → extract → swap.
- `AssetInfo::for_running_target()` — resolves the running target triple via `cfg!(all(target_arch, target_os, target_env))` (8 triples incl. linux musl vs gnu; macOS/linux→`tar.gz`, windows→`zip`); unsupported triple → `UpdateError::Install` with arch/os/env hint.
- `AssetInfo::extract_binary` — delegates to `extract_tar_gz` (Windows zip path intentionally unwired for v3.0.0 GA → error).
- `download_archive(url)` — blocking ureq GET (90 s `timeout_global`); non-2xx → `GithubStatus`.
- `extract_tar_gz(bytes, target_name)` — gzip+tar; skips non-regular-file entries (symlink/dir guard); matches the binary by file_name at root or one-level prefix.
- `swap_binary(current_exe, new_bytes)` — writes `<exe>.new` + `set_executable` (chmod 0755 Unix; no-op Windows), then atomic `rename` over the live binary on Unix; on Windows renames live exe → `.old` first, moves new into place, and **rolls back** `.old`→live if the second rename fails (surfacing rollback outcome to stderr).
**Connections** — calls: `ureq`, `flate2`, `tar`, `fs::{rename,File,set_permissions}`; called by: `update::mod::default_install_binary`.
**Tests** — tar extraction (found/skip/missing) + unix swap (replace + chmod + tmp cleanup) + running-target smoke.

## `vault_sync/` — plugin tarball overlay
`run_vault_sync` pulls the upstream `onebrain-ai/onebrain` release tarball and overlays the bundled plugin, harness configs, and root docs onto a vault. Steps 1–9 (download+extract → plugin/.gemini sync → .obsidian (init) → root docs → harness merge → onebrain.yml update → pin → cache clean), with a TTY/non-TTY progress UI. Step ordering and the `VaultSyncResult` shape mirror the Bun source for parity diffs. Network and clock are injectable.

### `src/vault_sync/mod.rs`
Module root — declares step submodules and re-exports `run_vault_sync`, `resolve_branch`, `build_tar_spawn_overrides`, `normalize_path`, `VaultSyncOptions`, `VaultSyncResult`.

### `src/vault_sync/types.rs`
**Key types** — `VaultSyncOptions` (`branch`, `fetch_fn`, `installed_plugins_path`/`_cache_dir`, `is_tty`, `unlink_fn`, `include_obsidian`, `embedded`, `now_fn`, `progress_writer`); `VaultSyncResult` (Serialize with Bun camelCase rename: `ok`, `version`, `branch`, `filesAdded`, `filesRemoved`, `importsAdded`, `pinSkipped`, `cacheRemoved`, `error`); closure aliases `FetchFn`/`UnlinkFn`/`NowFn`. `VaultSyncResult::pending(branch)` (crate-private initial state).
**Connections** — consumed by every vault_sync submodule + `init::default_vault_sync` + `onebrain-cli`.

### `src/vault_sync/orchestrate.rs`
**Key functions** — `run_vault_sync(vault_root, opts) -> VaultSyncResult` — resolves options/branch/harness, builds progress, downloads+extracts into a tempdir, reads version from extracted plugin.json, runs plugin/.gemini sync (critical), optional .obsidian, root docs (non-fatal), harness merge (critical), `update_vault_yml` (critical), then Claude-only pin + cache-clean (non-fatal). Always returns a result; `!ok` carries `error`. Private helpers `read_update_channel`/`read_plugin_version`/`download_and_extract` + env overrides for installed-plugins path/cache.
**Connections** — calls: every vault_sync submodule, `harness::detect_harness`, `onebrain_core::find_config_file`, `tempfile`; called by: `init::default_vault_sync` + `onebrain-cli` `vault-sync`.
**Tests** — fresh sync, stale removal, download/tarball/HTTP-error paths, update_channel preservation, harness-merge injection.

### `src/vault_sync/branch.rs`
**Key functions** — `resolve_branch(update_channel: Option<&str>) -> &'static str` — `Some("stable")`→`"main"`, anything else/None→`"next"`.
**Connections** — called by: `orchestrate::run_vault_sync`.

### `src/vault_sync/download.rs`
Step 1 — tarball download + extract (pure-Rust `tar`+`flate2`, no system `tar` spawn).
**Key functions** — `default_fetch_fn()` (blocking ureq, no timeout; `ONEBRAIN_VAULT_SYNC_FIXTURE` reads a local file for tests); `download_tarball(branch, fetch)`; `extract_tarball(bytes, dest_dir) -> io::Result<PathBuf>` (returns the single top-level dir); `tarball_url`/`format_http_error` (403/404/429 hints); `build_tar_spawn_overrides(platform, parent_env)` (kept for parity — empty on non-Windows, `TAR_OPTIONS=--force-local` on Windows).
**Connections** — calls: `flate2`, `tar`, `ureq`; called by: `orchestrate`.

### `src/vault_sync/walker.rs`
**Key functions** — `list_files_recursive(dir) -> Vec<PathBuf>` — best-effort recursive regular-file listing (missing/unreadable→empty, no symlink follow).
**Connections** — calls: `walkdir`; called by: `sync.rs`.

### `src/vault_sync/sync.rs`
Steps 2–4 — overlay directories.
**Key functions** — `sync_plugin_files` (`.claude/plugins/onebrain/`, copies + removes stale), `sync_gemini_config` (`.gemini/`, source-absent no-op), `sync_obsidian` (init-only, no stale removal), `default_unlink_fn`; private `overlay_directory(source, dest, unlink_fn, _strict) -> io::Result<(u64,u64)>` (returns `(files_added, files_removed)`; counts only successful unlinks).
**Connections** — calls: `walker::list_files_recursive`; called by: `orchestrate`.

### `src/vault_sync/docs.rs`
**Key types** — `ROOT_DOCS: [&str; 3]` (`CONTRIBUTING.md`, `CHANGELOG.md`, `PLUGIN-CHANGELOG.md`).
**Key functions** — `copy_root_docs(extracted_dir, vault_root) -> u64` — copies each present doc; missing silently skipped (non-fatal).
**Connections** — called by: `orchestrate` (step 5).

### `src/vault_sync/harness_merge.rs`
Step 6 — merge `@`-import lines into harness files.
**Key types** — `HARNESS_FILES: [&str; 3]` (`CLAUDE.md`, `GEMINI.md`, `AGENTS.md`).
**Key functions** — `merge_harness_files(extracted_dir, vault_root) -> io::Result<u64>` (sum); `merge_harness_file(.., filename)` — source-absent→0, vault-absent→write verbatim, else insert new `@`-lines before the last existing one (or append after a blank line); LF-only split (mirrors Bun, no CRLF normalize). Helper `count_at_lines`.
**Connections** — called by: `orchestrate`.

### `src/vault_sync/vault_yml.rs`
Step 7 — write `update_channel` back to the config (filename-agnostic).
**Key functions** — `update_vault_yml(vault_root, update_channel) -> io::Result<()>` — reads the present config (canonical preferred, legacy fallback; fresh vault → seeds canonical `onebrain.yml`, never resurrects `vault.yml`), sets `update_channel`, **backs up first** (`backup::backup_config_file`, hard precondition), atomic-writes to the same path. `atomic_write(path, bytes)` (crate-private, tmp+rename with direct-write fallback) is reused by `pin.rs`.
**Connections** — calls: `onebrain_core::find_config_file`, `backup::backup_config_file`, `serde_yaml`; called by: `orchestrate`.

### `src/vault_sync/pin.rs`
Step 8 — refresh `installed_plugins.json` entries for this vault (port of Bun `pinToVault`, the gnarliest function).
**Key types** — `PinResult { skipped: bool }`; `PluginMeta { version, last_updated }`.
**Key functions** — `pin_to_vault(vault_root, installed_plugins_path, cache_dir, now_fn) -> io::Result<PinResult>` (and `pin_to_vault_inner` with injected stderr): marketplace short-circuit (any `source: marketplace`→skip), `onebrain@onebrain` orphan dedup (drop ENOENT projectPath, preserve+warn on other stat errors), per-entry installPath canonicalize + version/lastUpdated refresh (lastUpdated bumps only on version change; cross-vault isolation), non-string path → warn+skip, atomic write only on change. `read_plugin_metadata`; `normalize_path(p) -> PathBuf` (absolutize + strip trailing separator, no realpath); `serialize_pretty` (4-space).
**Connections** — calls: `vault_yml::atomic_write`, `serde_json`, `chrono`; called by: `orchestrate` (Claude harness only). `normalize_path` re-exported at crate root.
**Tests** — very thorough (marketplace skip, canonicalize, orphan dedup, cross-vault isolation, idempotency, trailing slash, malformed entry, invalid JSON).

### `src/vault_sync/cache_clean.rs`
Step 9 — remove obsolete cached OneBrain versions.
**Key functions** — `clean_plugin_cache(installed_plugins_path, cache_dir) -> u64` — discovers `cache/<marketplace>/onebrain/` dirs (via installed_plugins.json keys, falling back to a glob), removes each version subdir, returns count. Non-fatal.
**Connections** — calls: `serde_json`, `fs::remove_dir_all`; called by: `orchestrate` (Claude harness only).

### `src/vault_sync/progress.rs`
Progress UI.
**Key types** — `trait Progress { intro, start, stop, outro }`; `PlainProgress<W: Write>` (non-TTY `vault-sync: <label>` lines; intro/stop no-ops; `outro`→`vault-sync: done`); `TtyProgress` (indicatif spinner; `embedded` mode prefixes `▸ `).
**Key functions** — `build_progress(is_tty, embedded) -> Box<dyn Progress>`; `plain_progress_to(out)` (snapshot-test sink).
**Connections** — calls: `indicatif`; called by: `orchestrate` (and `doctor --json` forces `PlainProgress` via `progress_writer`).

## Entry points
Top public functions other crates (chiefly `onebrain-cli`) call:
- `run_init(InitOptions) -> Result<InitResult, FsError>` — `onebrain init`.
- `run_update(UpdateOptions) -> UpdateResult` — `onebrain update` (self-update binary swap).
- `run_vault_sync(&Path, VaultSyncOptions) -> VaultSyncResult` — `onebrain vault-sync` (plugin overlay).
- `register_hooks::run(RegisterHooksOptions) -> Result<RegisterHooksResult>` — `onebrain register-hooks`.
- `doctor::run_all_checks(&Path, &VaultConfig) -> Vec<DoctorResult>` — `onebrain doctor`.
- `scan_orphans(&Path, &str, DateTime<Local>, &Path) -> Result<OrphanScanResult>` — `onebrain checkpoint orphans`.
- `run_backfill_recapped(&Path, Option<&str>, &str, impl FnMut(&str)) -> MigrateResult` — migration command.
- `build_prompt(&str, &[(String,String)])` + `resolve_claude_bin(..)` / `resolve_gemini_bin(..)` (→ `HarnessBinResolution`) — `onebrain skill run`.
- `detect_harnesses(&Path)` / `detect_harness(&Path)` — harness detection (used by init/register-hooks/vault-sync).
- `backup_config_file(&Path) -> Result<Option<PathBuf>>` — pre-write config backup.
- `resolve_branch` / `build_tar_spawn_overrides` / `normalize_path` — vault-sync parity/helper re-exports.
