use crate::commands::search_common::{collection_cache_dir, is_indexed};
use crate::legacy_output::{serialize_for_mode, SessionInitBlock, SessionInitOutput};
use crate::output::OutputMode;
use anyhow::{Context, Result};
use onebrain_cache::{clean_stale_state_file, resolve_session_token, ResolveInputs};
use onebrain_core::{find_vault_root, load_vault_config, CoreError};
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

pub fn run(vault_dir: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    // Bun parity: `--vault-dir <path>` overrides the cwd-based auto-detect.
    let start = match vault_dir {
        Some(dir) => dir,
        None => env::current_dir().context("read current directory")?,
    };
    let line = build_output(&start, mode, native_pending_bounded)?;
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
    mode: &OutputMode,
    qmd_count: impl Fn(&onebrain_core::VaultRoot, &str) -> Option<usize>,
) -> Result<String> {
    Ok(format_output(&compute_result(cwd, qmd_count)?, mode))
}

fn compute_result(
    cwd: &Path,
    qmd_count: impl Fn(&onebrain_core::VaultRoot, &str) -> Option<usize>,
) -> Result<SessionInitResult> {
    // Distinct block reasons for missing-vault vs malformed-yaml — the
    // SessionStart hook routes each to a different recovery path.
    let Some(vault_root) = find_vault_root(cwd) else {
        return Ok(SessionInitResult::Block(SessionInitBlock::init_required()));
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
fn native_pending(vault_root: &onebrain_core::VaultRoot, collection: &str) -> Option<usize> {
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
            format!(
                "Session ready · token={token} · datetime={datetime}\nsearch index: {unembedded}",
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

        let line = build_output(dir.path(), &json_mode(), |_, _| Some(0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert!(v.get("datetime").and_then(|d| d.as_str()).is_some());
        assert!(v.get("session_token").and_then(|s| s.as_str()).is_some());
        // v3.4.5 transition: canonical `search_unembedded` + deprecated
        // `qmd_unembedded` alias both emitted with the same value.
        assert_eq!(v.get("search_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert_eq!(v.get("qmd_unembedded").and_then(|n| n.as_u64()), Some(0));
        assert!(
            v.get("decision").is_none(),
            "happy path must not include decision field"
        );
    }

    #[test]
    fn block_path_when_no_vault_yml_found() {
        let dir = tempdir().unwrap();
        // No vault.yml anywhere.
        let line = build_output(dir.path(), &json_mode(), |_, _| Some(0)).unwrap();
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

        let line = build_output(dir.path(), &json_mode(), |_, _| Some(0)).unwrap();
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
        let line = build_output(dir.path(), &json_mode(), |_, _| Some(0)).unwrap();
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

    #[test]
    fn collection_absent_reports_zero_without_querying_qmd() {
        // Gating guard: a vault with no `qmd_collection` reports 0 and must NOT
        // leak the global qmd index's pending count. The injected closure
        // returns 99, yet the field stays 0 because the query is skipped.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "# no qmd_collection\n").unwrap();

        let line = build_output(dir.path(), &json_mode(), |_, _| Some(99)).unwrap();
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

        let line = build_output(dir.path(), &json_mode(), |_, _| Some(7)).unwrap();
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

        let line = build_output(dir.path(), &json_mode(), |_, _| None).unwrap();
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

        let line = build_output(dir.path(), &text_mode(), |_, _| None).unwrap();
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

        let line = build_output(dir.path(), &text_mode(), |_, _| Some(7)).unwrap();
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
        let line = build_output(dir.path(), &OutputMode::Yaml, |_, _| Some(0)).unwrap();
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
        let line = build_output(dir.path(), &OutputMode::Yaml, |_, _| Some(0)).unwrap();
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
        let line = build_output(dir.path(), &text_mode(), |_, _| Some(0)).unwrap();
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
        let line = build_output(dir.path(), &text_mode(), |_, _| Some(0)).unwrap();
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
        let line = build_output(dir.path(), &text_mode(), |_, _| Some(0)).unwrap();
        assert!(!line.trim_start().starts_with('{'), "got: {line}");
        assert!(line.contains("malformed"), "got: {line}");
        assert!(line.contains("onebrain doctor"), "got: {line}");
    }

    #[test]
    fn json_pretty_emits_indented_multiline() {
        let dir = tempdir().unwrap();
        // Block path is simplest — no volatile fields to assert against.
        let line = build_output(dir.path(), &OutputMode::Json { pretty: true }, |_, _| {
            Some(0)
        })
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
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());

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
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());

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
