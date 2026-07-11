use crate::commands::search_common::{collection_cache_dir, is_indexed};
use crate::legacy_output::{serialize_for_mode, SessionInitBlock, SessionInitOutput};
use crate::output::OutputMode;
use anyhow::{Context, Result};
use onebrain_cache::{clean_stale_state_file, resolve_session_token, ResolveInputs};
use onebrain_core::{load_vault_config, resolve_vault, CoreError, VaultResolveInputs};
use onebrain_search::engine::Engine;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Result of `session init` — either the happy-path metadata or one of two
/// block variants. Kept private; rendering goes through `format_output`.
enum SessionInitResult {
    Ok(SessionInitOutput),
    Block(SessionInitBlock),
}

pub fn run(
    vault_dir: Option<PathBuf>,
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
) -> Result<()> {
    // Bun parity: `--vault-dir <path>` overrides the cwd-based auto-detect.
    // Kept as the walk-up STARTING POINT (soft — a directory to search
    // upward from), distinct from the global `--vault`/`ONEBRAIN_VAULT`
    // resolution below, which is a HARD override (must already be a vault).
    let start = match vault_dir {
        Some(dir) => dir,
        None => env::current_dir().context("read current directory")?,
    };
    let line = build_output(&start, vault_flag, mode, native_pending_bounded)?;
    println!("{line}");
    Ok(())
}

/// `qmd_count` is injected so the unembedded figure is deterministic in tests
/// (production passes [`native_pending`], which probes the native search
/// index). It is only invoked on the happy path when the vault actually has a
/// search collection configured; `collection` is that already-resolved name
/// (never `None` at the call site — see [`compute_result`]).
fn build_output(
    cwd: &Path,
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    qmd_count: impl Fn(&onebrain_core::VaultRoot, &str) -> Option<usize>,
) -> Result<String> {
    Ok(format_output(
        &compute_result(cwd, vault_flag, qmd_count)?,
        mode,
    ))
}

fn compute_result(
    cwd: &Path,
    vault_flag: Option<PathBuf>,
    qmd_count: impl Fn(&onebrain_core::VaultRoot, &str) -> Option<usize>,
) -> Result<SessionInitResult> {
    // Vault resolution follows the same flag > ONEBRAIN_VAULT env >
    // cwd-walk-up chain every other vault-aware command uses
    // (`vault_ctx::require`) — session init previously ignored both the
    // `--vault` flag and the env var and ALWAYS walked up from `cwd`,
    // silently discarding an explicit override (#229). `cwd` here is the
    // walk-up starting point (either the real process cwd or the legacy
    // `--vault-dir`, resolved by the caller); it's only consulted when
    // neither the flag nor the env var is set.
    //
    // Distinct block reasons for missing-vault vs malformed-yaml — the
    // SessionStart hook routes each to a different recovery path. A
    // flag/env value that doesn't resolve to a real vault
    // (`CoreError::NotAVault`) degrades to the same "not found" block a
    // plain walk-up miss would produce — hook consumers only care "is
    // there a vault", not why resolution failed.
    let inputs = VaultResolveInputs {
        flag: vault_flag,
        env: std::env::var_os("ONEBRAIN_VAULT").map(PathBuf::from),
        cwd: cwd.to_path_buf(),
    };
    let vault_root = match resolve_vault(&inputs) {
        Ok(Some(resolved)) => resolved.root,
        Ok(None) | Err(_) => {
            return Ok(SessionInitResult::Block(SessionInitBlock::init_required()));
        }
    };

    let config = match load_vault_config(&vault_root) {
        Ok(c) => c,
        Err(err) => {
            let block = match &err {
                // YAML present but unparseable → distinct reason so the
                // SessionStart consumer routes to "fix your onebrain.yml".
                CoreError::InvalidYaml(_) => SessionInitBlock::vault_malformed(err.to_string()),
                // Anything else (file gone between walk-up + read, EACCES,
                // NotAVault) keeps the legacy reason for back-compat with
                // the SessionStart hook's current handling.
                _ => SessionInitBlock::init_required(),
            };
            return Ok(SessionInitResult::Block(block));
        }
    };

    // Approximate the process start time before any subprocess work.
    let process_start = SystemTime::now();

    let inputs = ResolveInputs::from_env();
    let token = resolve_session_token(&inputs).context("resolve session token")?;

    // Best-effort cleanup of an orphaned state file from a prior process —
    // mirrors Bun `cleanStaleStateFile`. Failures emit a stderr warning only;
    // they never block session-init.
    clean_stale_state_file(&token, &std::env::temp_dir(), process_start);

    // Unembedded-doc count · queried only when THIS vault actually has a
    // search collection configured (`search.collection`, falling back to the
    // legacy `qmd_collection` — both already folded into
    // `config.search.collection` by `load_vault_config`). Vaults with no
    // collection report a genuine `Some(0)` rather than leaking some other
    // vault's pending count into this one's startup. When a collection IS
    // configured, the probe may still return `None` (index dir missing /
    // engine open or status failed) — surfaced as `null` so a probe failure
    // is distinguishable from a true zero instead of silently hiding pending
    // embeddings at startup.
    let unembedded = match &config.search.collection {
        Some(collection) => qmd_count(&vault_root, collection),
        None => Some(0),
    };

    // Recap-pending count · always computed (unlike the search probe, this
    // isn't gated behind a configured collection — it's a plain filesystem
    // scan of the session-log directory that every vault has).
    let recap_pending = recap_pending_bounded(vault_root.as_path(), &config.folders.logs);

    let datetime = chrono::Local::now()
        .format("%a · %d %b %Y · %H:%M")
        .to_string();

    // Set by `onebrain skill run` on the headless harness child. When true,
    // INSTRUCTIONS.md skips the interactive startup ceremony and runs only the
    // requested skill. Accept "1" or "true" (case-insensitive); anything else
    // (incl. unset) is false.
    let headless = std::env::var("ONEBRAIN_HEADLESS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    Ok(SessionInitResult::Ok(SessionInitOutput {
        datetime,
        session_token: token.to_string(),
        // Transition: emit the canonical `search_unembedded` and the
        // deprecated `qmd_unembedded` alias with the same value (see
        // `SessionInitOutput`).
        search_unembedded: unembedded,
        qmd_unembedded: unembedded,
        recap_pending,
        headless,
    }))
}

/// Probe the native search index for pending (unembedded/changed/removed)
/// docs. `None` = could not determine.
///
/// Cheap and read-only by construction: mirrors `search status`'s
/// `status_data` ordering (see that module's doc comment) rather than opening
/// the engine unconditionally. `Engine::open` creates the cache dir skeleton
/// as a side effect (`create_dir_all` + opens/creates the tantivy, vector and
/// redb stores) — harmless once a collection is genuinely indexed, but on a
/// vault that has a `search.collection` configured yet was NEVER indexed
/// (fresh `search.collection` in `onebrain.yml`, no `onebrain search reindex`
/// run yet), calling `Engine::open` here would silently create an empty index
/// skeleton on every session start, just to answer "how many are pending?".
/// So: check the cache dir's existence first (pure fs, via the shared
/// `is_indexed` helper) and only open the engine when it already exists. A
/// configured-but-never-indexed vault
/// reports `None` (unknown) without touching disk — consistent with "probe
/// couldn't determine" rather than a misleading `Some(0)`.
///
/// **Daemon-first, like `search status`:** when a live warm daemon already
/// serves THIS vault (typically an `onebrain mcp` session), it is the sole
/// redb owner, so a local `Engine::open` here would (a) hit the lock and
/// (b) — worse — eagerly migrate the cache layout out from under a process
/// that may hold the legacy paths open (rename-while-open is an untested
/// hazard on Windows). In that case the pending count is read from the
/// daemon's `/api/internal/status` with the NO-retry/NO-respawn probe; any
/// probe failure (engine-less daemon 503s, transport error) reports `None`
/// (unknown) — it never falls through to a local open while the daemon is
/// live.
fn native_pending(vault_root: &onebrain_core::VaultRoot, collection: &str) -> Option<usize> {
    use crate::commands::daemon_client;

    if !crate::commands::search_common::daemon_routing_disabled() {
        if let Ok(Some(handle)) = daemon_client::discover_matching(Some(vault_root.as_path())) {
            return handle
                .probe_status_no_retry()
                .and_then(|v| v.get("pending_total")?.as_u64())
                .map(|n| n as usize);
        }
        // No matching live daemon (or discovery failed) → the direct local
        // probe below is contention-free.
    }

    let cache_dir = collection_cache_dir(collection);
    if !is_indexed(&cache_dir) {
        return None;
    }

    let config = load_vault_config(vault_root).ok()?;
    let mut engine = Engine::open(&cache_dir, &config.search.embed_model).ok()?;
    engine.set_exclude_patterns(config.search.exclude);
    engine
        .status(vault_root.as_path())
        .ok()
        .map(|s| s.pending_total())
}

/// Bounded probe: the drift scan walks + hashes every note, which is too
/// slow for the SessionStart hot path on large vaults (~1s at 10k notes).
/// Cap it — timing out yields `None` (wire `null` = could not determine),
/// mirroring the old qmd subprocess probe's 5s cap philosophy at in-process
/// scale. See task-6 review.
const NATIVE_PENDING_CAP: Duration = Duration::from_millis(300);

/// Production wiring: runs [`native_pending`] on a background thread and
/// waits at most [`NATIVE_PENDING_CAP`]. The `is_indexed` filesystem
/// pre-check inside `native_pending` (which avoids `Engine::open`'s
/// dir-creation side effect) still runs before/inside the spawned thread —
/// read-only either way, so a slow probe finishing after the deadline is
/// harmless; its result is simply discarded.
fn native_pending_bounded(
    vault_root: &onebrain_core::VaultRoot,
    collection: &str,
) -> Option<usize> {
    let vault_root = vault_root.clone();
    let collection = collection.to_string();
    bounded(
        move || native_pending(&vault_root, &collection),
        NATIVE_PENDING_CAP,
    )
}

/// Generic time-boxed executor: runs `f` on a spawned thread and waits at
/// most `cap` for a result via an `mpsc` channel. Returns `None` on timeout
/// (the spawned thread is left to finish on its own; harmless for read-only
/// probes). Kept generic + thin so it's directly unit-testable without
/// touching the filesystem or a real search index.
fn bounded<F, T>(f: F, cap: Duration) -> Option<T>
where
    F: FnOnce() -> Option<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(cap).unwrap_or(None)
}

/// Count session logs whose frontmatter lacks a `recapped:` key — the same
/// criterion the `/recap` skill uses for discovery (`skills/recap/SKILL.md`:
/// "filter to files WITHOUT `recapped:` frontmatter field"). `logs_folder` is
/// the vault-config-resolved `folders.logs` value (e.g. `07-logs`); the
/// session logs live under `<logs_folder>/session/**/*-session-*.md`. A vault
/// with no `session/` directory yet (fresh vault, never wrapped up) reports a
/// genuine `Some(0)` rather than `None`. Any walk error (e.g. a
/// permission-denied subtree) aborts the probe with `None` — a partial count
/// would look like a determined figure while silently missing files, and
/// `null` = "couldn't determine" is exactly the contract `search_unembedded`
/// already established.
fn recap_pending(vault_root: &Path, logs_folder: &str) -> Option<u64> {
    let session_dir = vault_root.join(logs_folder).join("session");
    if !session_dir.is_dir() {
        return Some(0);
    }
    let mut pending = 0u64;
    for entry in walkdir::WalkDir::new(&session_dir) {
        let entry = entry.ok()?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !(name.ends_with(".md") && name.contains("-session-")) {
            continue;
        }
        if !frontmatter_has_key(entry.path(), "recapped") {
            pending += 1;
        }
    }
    Some(pending)
}

/// True when `path` starts with a *closed* `---` frontmatter block containing
/// a top-level `{key}:` line (no leading whitespace — an indented `{key}:` is
/// a nested mapping entry, not a frontmatter field). A leading UTF-8 BOM is
/// tolerated before the opening `---`, as are leading single-line HTML
/// comments (`<!-- ... -->`) — orphan recovery prepends a
/// `<!-- recovery-of: <token>:<date> -->` marker line above the frontmatter
/// of auto-recovered session logs, and those files must still be recognised
/// as recapped once `/recap` stamps them. Only complete single-line comments
/// are skipped; no multi-line comment parsing. Files with no frontmatter, an
/// unterminated block (no closing `---` before EOF), or an unreadable file
/// all return `false`; content after the closing `---` (the note body) is
/// never inspected.
fn frontmatter_has_key(path: &Path, key: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    let mut lines = text
        .lines()
        .skip_while(|line| line.trim().starts_with("<!--") && line.trim().ends_with("-->"));
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    let prefix = format!("{key}:");
    let mut seen = false;
    for line in lines {
        if line.trim() == "---" {
            // Key only counts when the block is actually closed.
            return seen;
        }
        if line.starts_with(&prefix) {
            seen = true;
        }
    }
    // EOF without a closing `---` → not a valid frontmatter block.
    false
}

/// Bounded wrapper mirroring [`native_pending_bounded`]: `recap_pending` is
/// also a SessionStart hot-path filesystem scan, so it shares the same time
/// budget as the search probe rather than a separate tunable.
fn recap_pending_bounded(vault_root: &Path, logs_folder: &str) -> Option<u64> {
    let vault_root = vault_root.to_path_buf();
    let logs_folder = logs_folder.to_string();
    bounded(
        move || recap_pending(&vault_root, &logs_folder),
        NATIVE_PENDING_CAP,
    )
}

/// Render `result` for the requested output mode.
///
/// v3.1: text is the default. Machine consumers (Claude Code SessionStart
/// hook) must pass `--json` (or `--yaml` / `--output <fmt>`) explicitly to
/// get the structured envelope. The hook rewriter + init scaffold both add
/// `--json` so existing installs migrate automatically.
fn format_output(result: &SessionInitResult, mode: &OutputMode) -> String {
    if let OutputMode::Text { .. } = mode {
        return render_text(result);
    }
    match result {
        SessionInitResult::Ok(out) => serialize_for_mode(out, mode),
        SessionInitResult::Block(block) => serialize_for_mode(block, mode),
    }
}

fn render_text(result: &SessionInitResult) -> String {
    match result {
        SessionInitResult::Ok(out) => {
            // Single-line happy path → keep tight; multi-line metadata risks
            // pushing useful info offscreen on narrow terminals. `None` ⟹ the
            // search probe couldn't determine the count (missing / timed out);
            // say so rather than printing a misleading "0 unembedded".
            let unembedded = match out.search_unembedded {
                Some(n) => format!("{n} unembedded"),
                None => "unknown (search index unavailable)".to_string(),
            };
            let recap_pending = match out.recap_pending {
                Some(n) => format!("{n}"),
                None => "unknown (couldn't scan)".to_string(),
            };
            format!(
                "Session ready · token={token} · datetime={datetime}\nsearch index: {unembedded}\nRecap pending: {recap_pending}",
                token = out.session_token,
                datetime = out.datetime,
            )
        }
        SessionInitResult::Block(block) => match block.reason {
            "onebrain-vault-not-found" => {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<cwd>".to_string());
                format!("⚠ No OneBrain vault found at {cwd}\n→ Run `onebrain init` to create one")
            }
            "onebrain-vault-malformed" => {
                let detail = block.error_detail.as_deref().unwrap_or("(no detail)");
                format!(
                    "⚠ OneBrain vault config is malformed: {detail}\n→ Run `onebrain doctor --fix` to attempt auto-repair, or edit onebrain.yml manually"
                )
            }
            other => format!("⚠ Session init blocked: {other}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Explicit JSON mode — what hook consumers see when they pass `--json`
    /// (or v3.0 callers via the auto-migrated settings.json).
    fn json_mode() -> OutputMode {
        OutputMode::Json { pretty: false }
    }

    fn text_mode() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: false,
        }
    }

    #[test]
    fn happy_path_emits_required_fields() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), None, &json_mode(), |_, _| Some(0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert!(v.get("datetime").and_then(|d| d.as_str()).is_some());
        assert!(v.get("session_token").and_then(|s| s.as_str()).is_some());
        // v3.4.5 transition: canonical `search_unembedded` + deprecated
        // `qmd_unembedded` alias both emitted with the same value.
        assert_eq!(v.get("search_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
        // Fresh tempdir has no `07-logs/session/` — genuine Some(0), not null.
        assert_eq!(v.get("recap_pending").and_then(|n| n.as_u64()), Some(0));
        assert!(
            v.get("decision").is_none(),
            "happy path must not include decision field"
        );
    }

    #[test]
    fn block_path_when_no_vault_yml_found() {
        let dir = tempdir().unwrap();
        // No vault.yml anywhere.
        let line = build_output(dir.path(), None, &json_mode(), |_, _| Some(0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found")
        );
        assert!(v.get("datetime").is_none());
        assert!(v.get("session_token").is_none());
    }

    #[test]
    fn block_path_when_vault_yml_malformed() {
        // Malformed YAML reports `onebrain-vault-malformed` so SessionStart
        // consumers route to "fix your onebrain.yml" instead of "/onboarding".
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid\n").unwrap();

        let line = build_output(dir.path(), None, &json_mode(), |_, _| Some(0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-malformed")
        );
        // error_detail surfaces the parse-error message.
        assert!(
            v.get("error_detail")
                .and_then(|d| d.as_str())
                .is_some_and(|s| !s.is_empty()),
            "expected non-empty error_detail; got {v:?}"
        );
    }

    #[test]
    fn block_path_when_no_vault_yml_omits_error_detail() {
        // Counterpart to the previous test: missing vault.yml emits the
        // `onebrain-vault-not-found` reason (renamed from `init-required`
        // in v3.1) and skips the `error_detail` field.
        let dir = tempdir().unwrap();
        let line = build_output(dir.path(), None, &json_mode(), |_, _| Some(0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found")
        );
        assert!(
            v.get("error_detail").is_none(),
            "vault-not-found block must not carry error_detail"
        );
    }

    // ── #229: --vault flag / ONEBRAIN_VAULT env resolution ──────────────

    #[test]
    fn vault_flag_overrides_cwd_walk_up() {
        // `cwd` here is an isolated tempdir with no onebrain.yml anywhere in
        // its ancestry (no vault via walk-up), while `--vault` points at a
        // real, separate vault. Before #229 the flag was silently ignored
        // and this resolved to a "not found" block; after the fix it must
        // resolve to the flagged vault (proven by the injected qmd_count
        // closure observing THAT vault's configured collection name).
        let cwd_dir = tempdir().unwrap();
        let flag_dir = tempdir().unwrap();
        std::fs::write(
            flag_dir.path().join("onebrain.yml"),
            "search:\n  collection: flag-vault-collection\n",
        )
        .unwrap();

        let seen = std::cell::RefCell::new(None);
        let line = build_output(
            cwd_dir.path(),
            Some(flag_dir.path().to_path_buf()),
            &json_mode(),
            |_, collection| {
                *seen.borrow_mut() = Some(collection.to_string());
                Some(0)
            },
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(
            v.get("decision").is_none(),
            "must resolve via --vault flag, not block; got {v:?}"
        );
        assert_eq!(
            seen.borrow().as_deref(),
            Some("flag-vault-collection"),
            "must have queried the flagged vault's collection, not cwd's"
        );
    }

    #[test]
    fn onebrain_vault_env_overrides_cwd_walk_up_when_flag_absent() {
        let cwd_dir = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        std::fs::write(
            env_dir.path().join("onebrain.yml"),
            "search:\n  collection: env-vault-collection\n",
        )
        .unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_VAULT", env_dir.path());

        let seen = std::cell::RefCell::new(None);
        let line = build_output(cwd_dir.path(), None, &json_mode(), |_, collection| {
            *seen.borrow_mut() = Some(collection.to_string());
            Some(0)
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(
            v.get("decision").is_none(),
            "must resolve via ONEBRAIN_VAULT env, not block; got {v:?}"
        );
        assert_eq!(seen.borrow().as_deref(), Some("env-vault-collection"));
    }

    #[test]
    fn vault_flag_wins_over_onebrain_vault_env() {
        let cwd_dir = tempdir().unwrap();
        let flag_dir = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        std::fs::write(
            flag_dir.path().join("onebrain.yml"),
            "search:\n  collection: flag-wins-collection\n",
        )
        .unwrap();
        std::fs::write(
            env_dir.path().join("onebrain.yml"),
            "search:\n  collection: env-loses-collection\n",
        )
        .unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_VAULT", env_dir.path());

        let seen = std::cell::RefCell::new(None);
        let line = build_output(
            cwd_dir.path(),
            Some(flag_dir.path().to_path_buf()),
            &json_mode(),
            |_, collection| {
                *seen.borrow_mut() = Some(collection.to_string());
                Some(0)
            },
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("decision").is_none(), "got {v:?}");
        assert_eq!(
            seen.borrow().as_deref(),
            Some("flag-wins-collection"),
            "--vault must outrank ONEBRAIN_VAULT"
        );
    }

    #[test]
    fn vault_flag_pointing_at_non_vault_degrades_to_not_found_block() {
        // An explicit --vault whose path has no onebrain.yml is a resolver
        // Err (CoreError::NotAVault), not a panic or a raw process error —
        // the hook-protocol contract still gets a clean block JSON.
        let cwd_dir = tempdir().unwrap();
        let not_a_vault = tempdir().unwrap();

        let line = build_output(
            cwd_dir.path(),
            Some(not_a_vault.path().to_path_buf()),
            &json_mode(),
            |_, _| Some(0),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found")
        );
    }

    #[test]
    fn collection_absent_reports_zero_without_querying_qmd() {
        // Gating guard: a vault with no `qmd_collection` reports 0 and must NOT
        // leak the global qmd index's pending count. The injected closure
        // returns 99, yet the field stays 0 because the query is skipped.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "# no qmd_collection\n").unwrap();

        let line = build_output(dir.path(), None, &json_mode(), |_, _| Some(99)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("search_unembedded").and_then(|n| n.as_u64()),
            Some(0),
            "collection-absent vault must not query the search index"
        );
        assert_eq!(
            v.get("qmd_unembedded").and_then(|n| n.as_u64()),
            Some(0),
            "deprecated alias mirrors search_unembedded"
        );
    }

    #[test]
    fn collection_set_surfaces_the_queried_count() {
        // When the vault uses qmd, the queried unembedded count flows through
        // verbatim. Injected so the test is deterministic regardless of whether
        // a real qmd is installed on the dev machine.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), None, &json_mode(), |_, _| Some(7)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v.get("search_unembedded").and_then(|n| n.as_u64()),
            Some(7),
            "collection-set vault must surface the queried count"
        );
        assert_eq!(
            v.get("qmd_unembedded").and_then(|n| n.as_u64()),
            Some(7),
            "deprecated alias mirrors search_unembedded"
        );
    }

    #[test]
    fn collection_set_probe_failure_reports_null_not_zero() {
        // The core contract change: when the vault uses qmd but the probe can't
        // determine the count (`None` — missing binary / timeout), the field is
        // `null`, NOT a misleading `0` that hides pending embeddings. The field
        // is still present (key required by the hook contract).
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), None, &json_mode(), |_, _| None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let field = v
            .get("search_unembedded")
            .expect("canonical key must be present");
        assert!(
            field.is_null(),
            "probe failure must report null (unknown), not a false zero; got {field:?}"
        );
        // Deprecated alias mirrors the null.
        assert!(v
            .get("qmd_unembedded")
            .expect("alias key must be present")
            .is_null());
    }

    #[test]
    fn text_mode_reports_unknown_when_probe_unavailable() {
        // Counterpart of the JSON `null`: the human-readable line must say the
        // count is unknown rather than printing "0 unembedded".
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), None, &text_mode(), |_, _| None).unwrap();
        assert!(
            line.contains("search index: unknown"),
            "expected unknown marker for unavailable search index; got: {line}"
        );
        assert!(
            !line.contains("0 unembedded"),
            "must not print a false zero; got: {line}"
        );
    }

    #[test]
    fn text_mode_reports_count_when_probe_succeeds() {
        // Positive text path: a determined count renders as "N unembedded"
        // (the same figure the JSON `search_unembedded` carries — both render
        // from the one `SessionInitOutput` field, so text and --json agree).
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();

        let line = build_output(dir.path(), None, &text_mode(), |_, _| Some(7)).unwrap();
        assert!(
            line.contains("search index: 7 unembedded"),
            "expected the determined count in text mode; got: {line}"
        );
    }

    #[test]
    fn block_path_emits_yaml_when_mode_is_yaml() {
        // v3.1: --yaml / --output yaml flips the hook-protocol block to
        // YAML. Default stays JSON (verified above).
        let dir = tempdir().unwrap();
        let line = build_output(dir.path(), None, &OutputMode::Yaml, |_, _| Some(0)).unwrap();
        // Parse the YAML to assert structure rather than string-matching
        // (serde_yaml's emitter formatting is implementation-defined).
        let v: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
        assert_eq!(
            v.get("decision").and_then(|d| d.as_str()),
            Some("block"),
            "yaml block missing decision; got: {line}"
        );
        assert_eq!(
            v.get("reason").and_then(|r| r.as_str()),
            Some("onebrain-vault-not-found"),
            "yaml block missing reason; got: {line}"
        );
        // Defensive: must NOT look like JSON (no leading `{`).
        assert!(
            !line.trim_start().starts_with('{'),
            "expected YAML, got JSON-shaped output: {line}"
        );
    }

    #[test]
    fn happy_path_emits_yaml_when_mode_is_yaml() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();
        let line = build_output(dir.path(), None, &OutputMode::Yaml, |_, _| Some(0)).unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
        assert!(v.get("datetime").and_then(|d| d.as_str()).is_some());
        assert!(v.get("session_token").and_then(|s| s.as_str()).is_some());
        assert_eq!(v.get("search_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert!(
            !line.trim_start().starts_with('{'),
            "expected YAML, got JSON-shaped output: {line}"
        );
    }

    // ── v3.1: text is the new default ────────────────────────────────────

    #[test]
    fn default_outside_vault_emits_text_not_json() {
        let dir = tempdir().unwrap();
        let line = build_output(dir.path(), None, &text_mode(), |_, _| Some(0)).unwrap();
        assert!(
            !line.trim_start().starts_with('{'),
            "default mode must NOT emit JSON braces; got: {line}"
        );
        assert!(
            line.contains("No OneBrain vault found"),
            "expected human-readable not-found marker; got: {line}"
        );
        assert!(
            line.contains("onebrain init"),
            "expected init suggestion; got: {line}"
        );
    }

    #[test]
    fn default_inside_vault_emits_text_success() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "qmd_collection: ob-1\n").unwrap();
        let line = build_output(dir.path(), None, &text_mode(), |_, _| Some(0)).unwrap();
        assert!(
            !line.trim_start().starts_with('{'),
            "default mode must NOT emit JSON braces; got: {line}"
        );
        assert!(
            line.contains("Session ready"),
            "expected `Session ready` marker; got: {line}"
        );
        assert!(line.contains("token="), "expected token field; got: {line}");
    }

    #[test]
    fn default_on_malformed_vault_emits_text() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "not: : valid\n").unwrap();
        let line = build_output(dir.path(), None, &text_mode(), |_, _| Some(0)).unwrap();
        assert!(!line.trim_start().starts_with('{'), "got: {line}");
        assert!(line.contains("malformed"), "got: {line}");
        assert!(line.contains("onebrain doctor"), "got: {line}");
    }

    #[test]
    fn json_pretty_emits_indented_multiline() {
        let dir = tempdir().unwrap();
        // Block path is simplest — no volatile fields to assert against.
        let line = build_output(
            dir.path(),
            None,
            &OutputMode::Json { pretty: true },
            |_, _| Some(0),
        )
        .unwrap();
        // Pretty JSON contains newlines + 2-space indent.
        assert!(
            line.contains('\n'),
            "expected multi-line indented JSON; got: {line}"
        );
        assert!(
            line.contains("  \"decision\""),
            "expected 2-space indent on `decision`; got: {line}"
        );
        // Still parseable.
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["reason"], "onebrain-vault-not-found");
    }

    // ── native_pending: real probe against the native search index ──────
    //
    // These exercise the production `native_pending` function directly
    // (not the injected-closure seam above), isolating the process-global
    // `ONEBRAIN_CACHE_DIR` override behind the crate-wide `test_env` lock so
    // parallel test threads — including `server::search`'s tests, which
    // mutate the same variable — can't race each other.

    #[test]
    fn session_init_reports_null_when_collection_configured_but_never_indexed() {
        // Hard constraint (task-6 brief): a vault with a configured
        // `search.collection` but no native index yet must yield a JSON-null
        // `qmd_unembedded`, WITHOUT `Engine::open`'s side effect of creating
        // the cache dir skeleton. `search_common::status_data` avoids this by
        // checking the cache dir's existence before opening the engine;
        // `native_pending` mirrors that ordering.
        let cache = tempdir().unwrap();
        // `ONEBRAIN_NO_DAEMON`: pin these local-path tests to the direct
        // probe — without it, a live daemon.json on the developer's machine
        // could route the probe over HTTP instead.
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);

        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: never-indexed-collection\n",
        )
        .unwrap();
        let vault_root = onebrain_core::VaultRoot::from_path(vault.path()).unwrap();

        let result = native_pending(&vault_root, "never-indexed-collection");

        assert_eq!(
            result, None,
            "never-indexed collection must report None (unknown), not a false Some(0)"
        );
        assert!(
            !cache
                .path()
                .join("search")
                .join("never-indexed-collection")
                .exists(),
            "native_pending must NOT create the cache dir skeleton for a vault \
             that has never been indexed (Engine::open's side effect must be avoided)"
        );
    }

    #[test]
    fn session_init_reports_native_pending_when_collection_configured_and_indexed() {
        // Counterpart: once the collection's cache dir already exists (i.e.
        // `onebrain search reindex` has run at least once), `native_pending`
        // opens the engine and reports the real pending count via
        // `Engine::status(..).pending_total()` — 0 here since the vault has
        // no markdown files and the (empty) index was never touched.
        let cache = tempdir().unwrap();
        // Direct local path (see the never-indexed test above for why).
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
        ]);

        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: already-indexed-collection\n",
        )
        .unwrap();
        let vault_root = onebrain_core::VaultRoot::from_path(vault.path()).unwrap();

        // Simulate a prior `reindex` having created the cache dir skeleton.
        std::fs::create_dir_all(
            cache
                .path()
                .join("search")
                .join("already-indexed-collection"),
        )
        .unwrap();

        let result = native_pending(&vault_root, "already-indexed-collection");

        assert_eq!(
            result,
            Some(0),
            "an indexed-but-empty vault must report a determined Some(0), not None"
        );
    }

    /// A live warm daemon serving THIS vault must be consulted INSTEAD of a
    /// local `Engine::open`: the daemon owns the redb lock, and a local open
    /// would also eagerly migrate the cache layout out from under a process
    /// that may hold the legacy paths open (rename-while-open is an untested
    /// hazard on Windows). The stub daemon here answers `/api/health` (so
    /// discovery routes to it) but 503s `/api/internal/status` (an
    /// engine-less daemon), so the probe reports `None` — and, crucially,
    /// the legacy on-disk layout stays untouched: no `index/`/`models/`
    /// split dirs appear and the artifacts stay at the collection root,
    /// proving no local engine open (and thus no silent SessionStart
    /// migration) happened.
    #[cfg(unix)] // discovery lives under $HOME; dirs::home_dir ignores $HOME on Windows
    #[test]
    fn native_pending_routes_to_live_daemon_without_local_open_or_migration() {
        use std::io::{Read as _, Write as _};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let cache = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("HOME", home.path().as_os_str()),
        ]);

        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: daemon-guard-collection\n",
        )
        .unwrap();
        let vault_root = onebrain_core::VaultRoot::from_path(vault.path()).unwrap();

        // Legacy flat fixture: index artifacts at the collection root, no
        // split dirs. (Names via a variable so the repo-wide "no literal
        // artifact joins" sweep stays clean.)
        let artifacts = ["tantivy", "engine.redb"];
        let collection_dir = cache.path().join("search").join("daemon-guard-collection");
        let legacy_lex_dir = collection_dir.join(artifacts[0]);
        std::fs::create_dir_all(&legacy_lex_dir).unwrap();
        let legacy_redb = collection_dir.join(artifacts[1]);
        std::fs::write(&legacy_redb, b"legacy-bytes").unwrap();

        // Minimal stub daemon: 200 on /api/health (liveness), 503 on
        // everything else (engine-less daemon shape). Plain std TCP — no
        // engine, no axum, so the stub itself can't migrate anything.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let server = std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = conn else { continue };
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => buf.push(byte[0]),
                        _ => break,
                    }
                }
                let request = String::from_utf8_lossy(&buf);
                let response: &[u8] = if request.starts_with("GET /api/health") {
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}"
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n"
                };
                let _ = stream.write_all(response);
            }
        });

        // Publish a matching discovery record: our version, THIS vault.
        let info = crate::commands::daemon_client::DaemonInfo {
            port,
            token: "session-init-daemon-guard-token".to_string(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vault: crate::commands::daemon_client::canonical_vault_id(vault.path()),
        };
        let discovery = crate::commands::daemon_client::discovery_path().unwrap();
        info.write(&discovery).unwrap();

        let result = native_pending(&vault_root, "daemon-guard-collection");

        // Engine-less daemon 503s the status probe → unknown, NOT a fall
        // through to a local open.
        assert_eq!(
            result, None,
            "a live daemon that can't answer status must yield None, never a local open"
        );
        // The load-bearing assertion: the legacy layout is byte-for-byte
        // untouched — no local Engine::open ran, so no silent migration.
        assert!(
            !collection_dir.join("index").exists() && !collection_dir.join("models").exists(),
            "no split-layout dirs may appear while a live daemon owns the collection"
        );
        assert!(legacy_lex_dir.is_dir(), "legacy lex dir must stay in place");
        assert_eq!(
            std::fs::read(&legacy_redb).unwrap(),
            b"legacy-bytes",
            "legacy redb file must stay in place, untouched"
        );

        // Unblock the accept loop and shut the stub down.
        stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        let _ = server.join();
    }

    // ── recap_pending: session logs missing `recapped:` frontmatter ──────

    #[test]
    fn recap_pending_counts_only_unrecapped_session_logs() {
        let vault = tempdir().unwrap();
        let session_dir = vault
            .path()
            .join("07-logs")
            .join("session")
            .join("2026")
            .join("07");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Already recapped — excluded.
        std::fs::write(
            session_dir.join("2026-07-01-session-01.md"),
            "---\nrecapped: 2026-07-05\n---\n# session\n",
        )
        .unwrap();
        // Has frontmatter, but no `recapped:` key — counted.
        std::fs::write(
            session_dir.join("2026-07-08-session-01.md"),
            "---\ntags: [foo]\n---\n# session\n",
        )
        .unwrap();
        // No frontmatter at all — counted.
        std::fs::write(
            session_dir.join("2026-07-09-session-02.md"),
            "# session, no frontmatter\n",
        )
        .unwrap();

        let result = recap_pending(vault.path(), "07-logs");

        assert_eq!(
            result,
            Some(2),
            "expected the two non-recapped session logs, got {result:?}"
        );
    }

    #[test]
    fn frontmatter_unclosed_block_is_not_recapped() {
        // No closing `---` before EOF → not a valid frontmatter block, even
        // though a body line happens to start with `recapped:` — the file
        // must count as pending.
        let dir = tempdir().unwrap();
        let file = dir.path().join("2026-07-08-session-01.md");
        std::fs::write(
            &file,
            "---\ntags: [foo]\n# discussing\nrecapped: 2026-07-05\n",
        )
        .unwrap();
        assert!(
            !frontmatter_has_key(&file, "recapped"),
            "an unterminated frontmatter block must not match"
        );
    }

    #[test]
    fn frontmatter_nested_key_is_not_recapped() {
        // `recapped:` indented under another mapping is a nested entry, not a
        // top-level frontmatter field — must count as pending.
        let dir = tempdir().unwrap();
        let file = dir.path().join("2026-07-08-session-01.md");
        std::fs::write(&file, "---\nmeta:\n  recapped: true\n---\n# session\n").unwrap();
        assert!(
            !frontmatter_has_key(&file, "recapped"),
            "a nested (indented) key must not match"
        );
    }

    #[test]
    fn frontmatter_bom_prefixed_recapped_is_detected() {
        // A UTF-8 BOM before the opening `---` must not hide a genuine
        // top-level `recapped:` key (would overcount pending).
        let dir = tempdir().unwrap();
        let file = dir.path().join("2026-07-01-session-01.md");
        std::fs::write(&file, "\u{FEFF}---\nrecapped: 2026-07-05\n---\n").unwrap();
        assert!(
            frontmatter_has_key(&file, "recapped"),
            "a BOM-prefixed frontmatter block must still match"
        );
    }

    #[test]
    fn frontmatter_recapped_before_close_matches_despite_body_content() {
        // Regression guard for the scan-to-completion rewrite: a key seen
        // BEFORE the closing `---` still matches (stays not-pending), and the
        // body after the close is never inspected.
        let dir = tempdir().unwrap();
        let file = dir.path().join("2026-07-01-session-01.md");
        std::fs::write(
            &file,
            "---\nrecapped: 2026-07-05\n---\n# body mentioning recapped: again\n",
        )
        .unwrap();
        assert!(
            frontmatter_has_key(&file, "recapped"),
            "key inside a closed block must match regardless of body content"
        );

        // Converse: body-only mention after a closed, key-less block must NOT
        // match — the file stays pending.
        let body_only = dir.path().join("2026-07-09-session-01.md");
        std::fs::write(&body_only, "---\ntags: [foo]\n---\nrecapped: 2026-07-05\n").unwrap();
        assert!(
            !frontmatter_has_key(&body_only, "recapped"),
            "a body-only mention after the closing --- must not match"
        );
    }

    #[test]
    fn frontmatter_behind_recovery_comment_is_detected() {
        // Orphan recovery prepends `<!-- recovery-of: ... -->` above the
        // frontmatter of auto-recovered session logs; a `recapped:` key in the
        // actual frontmatter must still be found (real-vault audit: 11 files
        // misclassified as pending forever without this).
        let dir = tempdir().unwrap();
        let file = dir.path().join("2026-07-01-session-01.md");
        std::fs::write(
            &file,
            "<!-- recovery-of: abc12:2026-07-01 -->\n---\ntags: [session-log]\nrecapped: 2026-07-05\n---\n",
        )
        .unwrap();
        assert!(
            frontmatter_has_key(&file, "recapped"),
            "frontmatter behind a leading recovery comment must still match"
        );
    }

    #[test]
    fn recovery_comment_without_recapped_is_still_pending() {
        // Converse: the leading comment is skipped, but a frontmatter block
        // without the key still counts as pending.
        let dir = tempdir().unwrap();
        let file = dir.path().join("2026-07-08-session-01.md");
        std::fs::write(
            &file,
            "<!-- recovery-of: abc12:2026-07-08 -->\n---\ntags: [session-log]\n---\n# body\n",
        )
        .unwrap();
        assert!(
            !frontmatter_has_key(&file, "recapped"),
            "a recovered log without `recapped:` must stay pending"
        );
    }

    #[test]
    fn recap_pending_zero_when_session_dir_absent() {
        let vault = tempdir().unwrap();
        // Fresh vault — no `07-logs/session/` directory at all.

        let result = recap_pending(vault.path(), "07-logs");

        assert_eq!(
            result,
            Some(0),
            "a fresh vault with no session logs must report a genuine Some(0), not None"
        );
    }

    // ── bounded: generic time-boxed executor ─────────────────────────────

    #[test]
    fn bounded_returns_fast_probe_result() {
        let result = bounded(|| Some(42), Duration::from_millis(300));
        assert_eq!(result, Some(42));
    }

    #[test]
    fn bounded_returns_none_when_probe_exceeds_cap() {
        // The probe blocks on a channel this test releases only AFTER the
        // assertion, so it is guaranteed to outlive the cap. A sleep-based
        // fixture raced the runner instead: `bounded`'s gap between spawning
        // the probe and starting its timed wait is unbounded, so a stall
        // there longer than the fixture's sleep let the "slow" probe land in
        // the channel first → Some(42) on a stalled macOS runner (#144).
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let result = bounded(
            move || {
                let _ = release_rx.recv();
                Some(42)
            },
            Duration::from_millis(50),
        );
        assert_eq!(
            result, None,
            "a probe that outlives the cap must yield None, not block startup"
        );
        // Unblock the detached probe thread so it exits promptly.
        drop(release_tx);
    }
}
