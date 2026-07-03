# onebrain-core

## Purpose & dependencies
`onebrain-core` is the foundational crate of the OneBrain CLI workspace. It owns the canonical type system: the `onebrain.yml` config model (`VaultConfig`, `VaultFolders`, `CheckpointPolicy`), the path-resolution chain (`resolve_vault`, `find_vault_root`, `VaultRoot`), the error taxonomy (`CoreError` with stable `E_*` codes), the scheduler model (cron/at parsing → launchd plist emission, byte-for-byte parity with the legacy Bun implementation), and small shared value types (`SessionToken`, `Harness`, `DoctorResult`). It depends on **nothing in-workspace** — only external crates (`serde`, `serde_yaml`, `thiserror`, `regex`, `chrono`, `indexmap`). Config and path helpers do touch the filesystem via `std::fs::read_to_string` / `Path::is_file`, but there are no directory walks or external-tool calls beyond reading the config file. The downstream crates `onebrain-fs`, `onebrain-cache`, and `onebrain-cli` all depend on it.

## Module map
```
src/
├── lib.rs              Crate root · re-exports the public surface from every module
├── config.rs           onebrain.yml parsing (VaultConfig/VaultFolders/CheckpointPolicy) + load fns
├── error.rs            CoreError taxonomy · stable E_* codes per skill-alignment §4.8
├── path.rs             Config-file discovery, VaultRoot, vault resolution chain, legacy-warning state
├── scheduler/
│   ├── mod.rs          Scheduler library root · re-exports submodules
│   ├── types.rs        ScheduleEntry · ScheduleConfig · SkillFrontmatter · Args (untagged enum)
│   ├── cron_parse.rs   validate_cron / validate_at + conversion to launchd calendar fields
│   ├── entry.rs        is_* mode classifiers + validate_entry (structural shape check)
│   ├── launchd.rs      launchd plist string emitter (Bun byte-parity) + label/path helpers
│   ├── log_paths.rs    scheduler_log_path — per-skill stdout/stderr log path builder
│   └── error.rs        SchedulerError — verbatim error strings matched against Bun for parity
└── types/
    ├── mod.rs          Re-exports DoctorResult/DoctorStatus, Harness, SessionToken
    ├── doctor.rs       DoctorResult + DoctorStatus (diagnostic check result model)
    ├── harness.rs      Harness enum (Claude/Gemini/Direct AI runtime identifier)
    └── session.rs      SessionToken (alphanumeric session-unique id with sanitizers)
```

## `src/lib.rs`
Crate root. Declares the five top-level modules (`config`, `error`, `path`, `scheduler`, `types`) and re-exports the curated public surface so consumers can `use onebrain_core::{...}` directly.
**Connections** — re-exports from: all sibling modules; called by: every other onebrain-* crate (entry surface).

## `src/config.rs`
Parses the active vault config file into typed structs. Reads through `path::find_config_file` for dual onebrain.yml/vault.yml discovery.
**Key types**
- `VaultConfig` — top-level `onebrain.yml` shape: `qmd_collection: Option<String>`, `checkpoint: CheckpointPolicy`, `folders: VaultFolders`. All `#[serde(default)]`.
- `CheckpointPolicy` — Stop-hook thresholds: `minutes: u32` (default 30), `messages: u32` (default 15). `Default` impl matches Bun v2.3.3.
- `VaultFolders` — 8 folder-name strings (`inbox`…`logs`), each with a per-field default (`00-inbox` … `07-logs`).

**Key functions**
- `load_vault_config(root: &VaultRoot) -> Result<VaultConfig>` — read+parse the config under a validated vault root; `VaultYamlMissing` if absent, `InvalidYaml` on parse error.
- `load_vault_config_at(path: &Path) -> Result<VaultConfig>` — same dual-read semantics from an arbitrary (un-validated) directory path; used for best-effort threshold lookups.

**Connections** — calls: `find_config_file`, `serde_yaml::from_str`, `std::fs::read_to_string`; produces `CoreError::{VaultYamlMissing, InvalidYaml}`. Called by: onebrain-cli (`session init`, `doctor`, `vault current`) and the Active-Session Guard threshold derivation.
**Tests** — `#[cfg(test)]` block covers minimal/empty/malformed YAML, legacy vault.yml read, checkpoint + folders default/partial-override behavior.

## `src/error.rs`
Defines the single canonical error enum for the whole workspace, with a stable machine-readable code per variant.
**Key types**
- `CoreError` — `thiserror`-derived enum. Legacy variants (`VaultYamlMissing`, `InvalidYaml`, `NotAVault`) plus v3.1 additions (`VaultNotFound`, `FsError`, `CacheError`, `Network`, `InvalidDate`, `InvalidTarget`, `NotImplemented`, `RpcHandshake`, `AuthFailed`, `InitTargetNotEmpty`). `InvalidYaml` carries `#[from] serde_yaml::Error`.

**Key functions**
- `CoreError::error_code(&self) -> &'static str` — maps each variant to its stable `E_*` code (e.g. `E_VAULT_NOT_FOUND`, `E_FS_ERROR`) for the JSON error envelope.
- `type Result<T> = std::result::Result<T, CoreError>` — crate-wide result alias.

**Connections** — produced by: `config.rs`, `path.rs` (and re-thrown by downstream crates); consumed by: onebrain-cli error-envelope renderer + `exit_code_for` helper, plus library-crate pattern matches.
**Tests** — `#[cfg(test)]` locks the `error_code` string surface (any change is a breaking release).

## `src/path.rs`
Config-file discovery, the `VaultRoot` newtype, and the full vault-resolution priority chain. Pure filename checks — does not parse the config (resolution ≠ validation).
**Key types**
- `VaultRoot` — newtype wrapping a validated vault-root `PathBuf`; methods `as_path`, `join`, `name`, `config_path`, and `from_path` (validates onebrain.yml/vault.yml presence).
- `VaultSource` — enum `Flag | Env | WalkUp` recording how a vault was resolved; `label()` gives a human string.
- `ResolvedVault` — `{ root: VaultRoot, source: VaultSource }` returned by the resolvers.
- `VaultResolveInputs` — `{ flag, env, cwd }` inputs (injectable for deterministic tests).
- Consts `CONFIG_FILENAME = "onebrain.yml"`, `LEGACY_CONFIG_FILENAME = "vault.yml"`.

**Key functions**
- `find_config_file(dir: &Path) -> Option<PathBuf>` — prefer onebrain.yml, fall back to vault.yml (firing the one-time deprecation warning), else `None`.
- `find_vault_root(start: &Path) -> Option<VaultRoot>` — walk up from `start` to the nearest dir containing a config file.
- `resolve_vault(inputs) -> Result<Option<ResolvedVault>>` — priority chain flag → env → walk-up; `Ok(None)` if nothing found; `NotAVault` if an explicit flag/env path lacks a config.
- `require_vault(inputs) -> Result<ResolvedVault>` — like `resolve_vault` but maps `None` → `CoreError::VaultNotFound`.
- `emit_legacy_deprecation_warning_once()` / `legacy_warning_was_emitted() -> bool` — process-global (`AtomicBool`) one-shot stderr warning for legacy vault.yml; structured-output callers can poll the flag. (`reset_legacy_warning_for_test`, `#[doc(hidden)]`, exists for tests only.)

**Connections** — calls: nothing in-workspace (uses `std::fs`, `std::env`); produces `CoreError::{NotAVault, VaultNotFound}`. Called by: `config.rs` (`find_config_file`), and onebrain-cli vault-resolution entry (`--vault` flag / `ONEBRAIN_VAULT` env / cwd walk-up, `vault current`, `session init`).
**Tests** — extensive `#[cfg(test)]` block: walk-up, basename naming, resolver priority chain, none-found, dual-read onebrain.yml-over-vault.yml preference, and the fire-once deprecation contract.

## scheduler/ submodules

### `src/scheduler/mod.rs`
Scheduler library root — pure-Rust port of the Bun `src/lib/scheduler/`. Declares submodules and re-exports their public surface.
**Connections** — re-exports from: `cron_parse`, `entry`, `error`, `launchd`, `log_paths`, `types`. Called by: onebrain-cli `register-schedule` (a.k.a. `schedule register`) command and the scheduled-skill runtime.

### `src/scheduler/types.rs`
The data model a user writes in the `onebrain.yml` `schedule:` block.
**Key types**
- `ScheduleEntry` — one schedule row: `cron`/`at` (exactly one), `skill`/`command` (exactly one), optional `args`. `Default`; serde skips `None` fields.
- `Args` — `#[serde(untagged)]` enum: `Map(IndexMap<String,String>)` for skill-mode `--arg key=value` flags, `List(Vec<String>)` for command-mode positional argv. `IndexMap` preserves insertion order for byte-stable plist emission.
- `ScheduleConfig` — `{ schedule: Vec<ScheduleEntry> }` — the parsed top-level config slice.
- `SkillFrontmatter` — SKILL.md frontmatter: `name`, `schedulable`, `schedulable_with_args`, `required_args` (all optional) — drives schedulability validation.

**Connections** — called by: `entry::validate_entry`, `launchd` (reads `entry.args`/`skill`/`command`), and onebrain-cli config loading. Calls: nothing (pure data + serde).
**Tests** — `#[cfg(test)]` covers untagged map vs list deserialization, insertion-order preservation, minimal/command entries, and frontmatter defaults.

### `src/scheduler/cron_parse.rs`
Cron + at-string validation and conversion to launchd `StartCalendarInterval` fields. Each field accepts `*`, a single integer, a step (`*/N`), a list (`a,b,c`), or a range (`a-b`) within the field's valid bounds. Single-value fields map to one launchd dict (`CronFields`); when any field expands to multiple values, the entry is emitted as a launchd `StartCalendarInterval` **array of dicts** (launchd ORs across the array).
**Key types**
- `CronFields` — `{ minute, hour, day, month, weekday: Option<u32> }`; `None` = launchd wildcard.
- `AtFields` — `{ year, month, day, hour, minute: u32 }`; all five required (omitting any would re-fire a one-shot).

**Key functions**
- `validate_cron(cron: &str) -> Result<(), SchedulerError>` — enforce 5 fields, each `*`/integer/step/list/range within bounds; reason strings for invalid syntax.
- `cron_fields_to_launchd(cron: &str) -> CronFields` — convert a *validated* single-value cron string; **panics** on unvalidated input. Multi-value fields go through the expander (a set of value-combinations, one launchd dict each).
- `validate_at(at: &str) -> Result<(), SchedulerError>` — validate `YYYY-MM-DD HH:MM` form + month/day/hour/minute ranges.
- `at_to_launchd(at: &str) -> AtFields` — convert a *validated* at-string; **panics** on unvalidated input.

**Connections** — uses `regex` (`OnceLock`-cached); produces `SchedulerError::{InvalidCron, InvalidAt}`. Called by: `launchd::calendar_block` and onebrain-cli schedule registration (validates before emit).
**Tests** — `#[cfg(test)]` covers accepted/rejected cron syntax (step/range/list/field-count/chars), at-format + out-of-range fields, and conversion round-trips.

### `src/scheduler/entry.rs`
Schedule-entry mode classifiers and a structural shape validator (no field-format checks).
**Key functions**
- `is_one_shot(&ScheduleEntry) -> bool` — has `at:`.
- `is_skill_mode(&ScheduleEntry) -> bool` — has `skill:`.
- `is_command_mode(&ScheduleEntry) -> bool` — has `command:`.
- `validate_entry(&ScheduleEntry) -> Result<(), SchedulerError>` — enforces exactly-one-of cron/at, exactly-one-of skill/command, non-empty skill/command, and args-type-matches-mode (map↔skill, list↔command).

**Connections** — reads `types::{Args, ScheduleEntry}`; produces `SchedulerError::InvalidEntry`. Called by: `launchd` (mode classifiers steer block selection) and onebrain-cli schedule loader (`validate_entry` surfaces friendly errors).
**Tests** — `#[cfg(test)]` covers each classifier and every `validate_entry` rejection/acceptance path.

### `src/scheduler/launchd.rs`
launchd `.plist` emitter — **string templating, not quick-xml**, to guarantee byte-for-byte parity with Bun v2.3.3 (Layer-4 parity test).
**Key types**
- `LaunchdContext` — emit inputs: `vault_path`, `skill_cli_path`, `log_base_path`, `homedir`, `uid`.

**Key functions**
- `generate_plist(entry, ctx) -> String` — full plist; dispatches on `(is_one_shot, is_command_mode)` to one of four `<ProgramArguments>` blocks (recurring/one-shot × skill/command).
- `label_for_entry(entry) -> String` — derive label suffix (command basename, or skill with leading `/` stripped; non-`[a-zA-Z0-9-]` → `-`).
- `plist_path(skill_or_label, homedir) -> PathBuf` — `~/Library/LaunchAgents/com.onebrain.<label>.plist`; accepts `/daily` or `daily`.
- `xml_escape(s) -> String` — `&`-first escape chain mirroring Bun (avoids double-escaping `&amp;`).
- (private block builders: `recurring_skill_block`, `recurring_command_block`, `one_shot_skill_block`, `one_shot_command_block`, `calendar_block`, `sanitize_label`.)

**Connections** — calls: `cron_parse::{at_to_launchd, cron_fields_to_launchd}`, `entry::{is_command_mode, is_one_shot}`, `types::{Args, ScheduleEntry}`. Called by: onebrain-cli schedule registration (writes the emitted string to disk + `launchctl`).
**Tests** — large `#[cfg(test)]` block plus an `insta` snapshot (`generate_plist_snapshot_recurring_skill`) asserting label, calendar fields, escaping, self-delete shell wrapper, and command/skill argv parity.

### `src/scheduler/log_paths.rs`
Builds the relative log path for a scheduled-skill invocation.
**Key functions**
- `scheduler_log_path(logs_folder, date: NaiveDate, skill, is_error) -> String` — `<logs_folder>/scheduler/YYYY/MM/YYYY-MM-DD-<skill>{.md|.err.md}`; strips leading `/` from skill name.

**Connections** — uses `chrono::{Datelike, NaiveDate}`. Called by: the onebrain-cli scheduler runtime (routes stdout to `.md`, stderr to `.err.md`).
**Tests** — `#[cfg(test)]` checks success (`.md`) and error (`.err.md`) suffix paths.

### `src/scheduler/error.rs`
Typed scheduler errors with verbatim Bun-matched strings (parity tests assert on exact substrings).
**Key types**
- `SchedulerError` — `thiserror` enum: `InvalidCron`, `InvalidAt`, `InvalidEntry`, `SkillNotFound`, `SkillNoFrontmatter`, `SkillNotSchedulable`, `SkillMissingArgs`, `SkillSchedulableMissing`, `ShellSpecialInArg`, `ShellSpecialInOneShotArg`, `CommandNotFound{Absolute,Relative,InPath}`, `Conflict`, plus `Io`/`Yaml` `#[from]` bridges.

**Connections** — produced by: `cron_parse`, `entry`, and onebrain-cli schedule-registration validation (skill/command existence, shell-special-char, plist-path conflict). Consumed by: onebrain-cli error rendering.

## types/ submodules

### `src/types/mod.rs`
Re-exports the shared value types: `DoctorResult`, `DoctorStatus`, `Harness`, `SessionToken`.

### `src/types/doctor.rs`
Diagnostic check-result model serialized into the `doctor` command's JSON output.
**Key types**
- `DoctorStatus` — `Ok | Warn | Error`, serialized lowercase.
- `DoctorResult` — `{ check, status, message, hint: Option, details: Vec }`; empty hint/details skipped in serialization.

**Key functions**
- `DoctorResult::ok / warn / error(check, message)` — status-specific constructors.
- `.with_hint(hint)` / `.with_details(details)` — builder-style fluent setters.

**Connections** — uses `serde`. Called by: onebrain-cli `doctor` checks (each emits a `DoctorResult`).
**Tests** — `#[cfg(test)]` checks constructors, hint round-trip, lowercase status serialization.

### `src/types/harness.rs`
Identifies which AI runtime is in use.
**Key types**
- `Harness` — `Claude | Gemini | Direct`, serialized lowercase.

**Key functions**
- `Harness::as_str(&self) -> &'static str` — lowercase string form.

**Connections** — uses `serde`. Called by: onebrain-cli / onebrain-cache when recording or branching on the active harness.
**Tests** — `#[cfg(test)]` asserts the lowercase `as_str` mapping.

### `src/types/session.rs`
The session-unique identifier type — alphanumeric only, stable within a calendar day (resolution chain documented in INSTRUCTIONS.md Auto Checkpoint).
**Key types**
- `SessionToken` — newtype over a sanitized alphanumeric `String`; `Serialize` + `Display`.

**Key functions**
- `SessionToken::sanitize(raw) -> Option<Self>` — strip non-alphanumerics; `None` if empty.
- `SessionToken::sanitize_truncated(raw, max_len) -> Option<Self>` — strip then cap to `max_len` (mirrors Bun `.replace(...).slice(0,8)`).
- `SessionToken::from_clean(s) -> Self` — wrap an already-clean literal; **panics** if non-alphanumeric/empty (test + cache random-fallback use only).
- `as_str(&self) -> &str` — borrow the inner string.

**Connections** — uses `serde`. Called by: onebrain-cache (session-token resolution + random fallback) and onebrain-cli (`session init`, checkpoint paths).
**Tests** — `#[cfg(test)]` covers sanitize/sanitize_truncated edge cases, the `from_clean` panic contract, and `Display`.

## Entry points
The public functions/types other crates reach for first:
- **Config** — `load_vault_config`, `load_vault_config_at`, `VaultConfig`, `VaultFolders`, `CheckpointPolicy`
- **Path / vault resolution** — `resolve_vault`, `require_vault`, `find_vault_root`, `find_config_file`, `VaultRoot`, `ResolvedVault`, `VaultResolveInputs`, `VaultSource`, `CONFIG_FILENAME`, `LEGACY_CONFIG_FILENAME`
- **Errors** — `CoreError`, `CoreError::error_code`, `Result`, `SchedulerError`
- **Scheduler** — `ScheduleEntry`, `ScheduleConfig`, `Args`, `SkillFrontmatter`, `validate_entry`, `validate_cron`, `validate_at`, `generate_plist`, `LaunchdContext`, `plist_path`, `label_for_entry`, `scheduler_log_path`
- **Shared value types** — `SessionToken`, `Harness`, `DoctorResult`, `DoctorStatus`
