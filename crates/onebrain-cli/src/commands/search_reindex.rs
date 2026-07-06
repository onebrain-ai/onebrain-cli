//! `onebrain search reindex` — reindex the whole vault, or specific doc
//! paths.
//!
//! Vault-required (exit 64 outside a vault). This is the expected
//! first-model-download point: `Engine::index_doc` embeds every new/changed
//! chunk, so the lazy embedder (engine.rs) constructs here on first call.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::SearchReindexArgs;
use crate::commands::daemon_client::DaemonHandle;
use crate::commands::search_common::{
    collection_cache_dir, collection_for, collection_name_readonly, index_size_bytes,
    map_daemon_error, model_not_chosen, open_engine, read_reindex_progress,
    reconcile_missing_model, reindex_progress_path, route_to_daemon,
};
use crate::output::{emit, item, section, Envelope, OutputMode};
use onebrain_search::engine::{ReindexProgress, ReindexStats};

#[derive(Debug, Serialize)]
struct ReindexData {
    /// Active embedding model for this run (from `search.embed_model`, or the
    /// default). Always present so an all-unchanged run names the model just
    /// like a run that embedded (the load notice only fires when the engine
    /// actually loads the model).
    embed_model: String,
    added: usize,
    updated: usize,
    removed: usize,
    unchanged: usize,
    failed: usize,
    /// Total on-disk size in bytes of the index (the `tantivy/` and `vectors/`
    /// dirs plus `engine.redb`) AFTER this run. `None` if the size couldn't be
    /// measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    index_size_bytes: Option<u64>,
    /// Change in index size (bytes) vs. before this run: positive = grew,
    /// negative = shrank. `None` if either measurement was unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    index_size_delta_bytes: Option<i64>,
}

impl ReindexData {
    fn from_stats(
        s: ReindexStats,
        embed_model: String,
        before: Option<u64>,
        after: Option<u64>,
    ) -> Self {
        let delta = match (before, after) {
            (Some(b), Some(a)) => Some(a as i64 - b as i64),
            _ => None,
        };
        Self {
            embed_model,
            added: s.added,
            updated: s.updated,
            removed: s.removed,
            unchanged: s.unchanged,
            failed: s.failed,
            index_size_bytes: after,
            index_size_delta_bytes: delta,
        }
    }
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &SearchReindexArgs) -> Result<()> {
    // `--lex-only` / `--pending-only` are hook paths: dispatch to their own
    // entry point BEFORE any of the normal reindex machinery below (the
    // first-model-choice prompt, `reconcile_missing_model`, `--force` wipe)
    // — hook paths must never prompt, never mutate config, and never fail
    // the calling turn. See `run_hook_path`'s doc comment for the full
    // safety-gate contract.
    if args.lex_only || args.pending_only {
        return run_hook_path(vault_flag, mode, args);
    }

    // Warm-daemon path (v3.4.6 Track 2c): a manual `search reindex` (full, or
    // over explicit paths) while a daemon holds the engine would otherwise hit
    // the redb lock. Route it to the daemon (the sole owner) so it runs
    // in-session: full → daemon `all`, explicit paths → daemon `paths`. NOT for
    // `--force` — that wipes the index files the daemon has open, so it needs a
    // daemon-free direct rebuild (falls through to the local path, honest
    // `EngineBusy` if a lock is genuinely held). Only when the daemon serves
    // THIS vault; else the local path below.
    if !args.force {
        if let Ok(resolved) = crate::vault_ctx::require(vault_flag.clone()) {
            if let Some(handle) = route_to_daemon(&resolved) {
                return run_reindex_via_daemon(&handle, &resolved, mode, args);
            }
        }
    }

    // A purged model download should re-open the first-run pick: clear the
    // stale key so `model_not_chosen` becomes true and the TTY-prompt /
    // headless-default split below fires correctly. Runs unconditionally
    // (before the TTY/structured gate in `maybe_prompt_first_model`) so a
    // headless reindex also drops the stale key and falls back to the
    // `multilingual-e5-small` default instead of trying to load a model
    // that's no longer on disk.
    if let Ok(resolved) = crate::vault_ctx::require(vault_flag.clone()) {
        if let Ok(collection) = collection_for(&resolved) {
            let cache_dir = collection_cache_dir(&collection);
            let embed_model = onebrain_core::load_vault_config(&resolved.root)
                .map(|c| c.search.embed_model)
                .unwrap_or_default();
            reconcile_missing_model(resolved.root.as_path(), &cache_dir, &embed_model);
        }
    }

    // First-run model choice: if no model has been chosen yet (no persisted
    // `search.embed_model` key AND nothing downloaded), prompt the user to pick
    // one BEFORE `open_engine` triggers a download — but only on a real TTY in
    // text mode. Non-TTY / structured runs (pipes, agents, hooks, scheduled
    // reindex) silently keep the `multilingual-e5-small` default so a headless
    // reindex never blocks on a prompt. If a model is already chosen/downloaded,
    // no prompt fires.
    maybe_prompt_first_model(vault_flag.clone(), mode)?;

    // --force: wipe the index files (NOT the downloaded models) before
    // opening, so everything re-chunks + re-embeds from scratch. Needed
    // when stored vectors go stale in ways a hash diff can't see (e.g.
    // embedding-prefix changes).
    if args.force {
        anyhow::ensure!(
            args.paths.is_empty(),
            "--force rebuilds the whole index; it can't be combined with specific paths"
        );
        wipe_index_files(vault_flag.clone())?;
    }

    let (mut engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    // Measure the index size before the run so we can report the delta after.
    // Best-effort: a collection-resolution hiccup degrades to "no delta"
    // rather than failing the reindex.
    let cache_dir = collection_for(&resolved)
        .ok()
        .map(|c| collection_cache_dir(&c));
    let size_before = cache_dir.as_deref().and_then(index_size_bytes);

    // Live progress goes to STDERR (never stdout — stdout carries only the
    // envelope). Rendered only when stderr is a real TTY AND we're in text mode
    // (not `--json` / `--yaml`, which must stay silent except the final
    // envelope). Piped / agent / hook runs get a couple of plain milestone
    // lines instead of a cursor-controlled bar.
    let mut reporter = ProgressReporter::new(mode, model_load_notice_for(&resolved));

    // Live progress marker: lets a `search status` in ANOTHER process see
    // this run's (done, total) while it works. Removed on drop (any exit).
    let live = LiveProgressFile::new(&resolved)?;
    let mut on_progress = |p: ReindexProgress| {
        match &p {
            ReindexProgress::Walked { total } => live.record(0, *total),
            ReindexProgress::Indexing { done, total, .. } => live.record(*done, *total),
            ReindexProgress::LoadingModel | ReindexProgress::LoadingReranker => {}
        }
        reporter.handle(p);
    };

    let stats = if args.paths.is_empty() {
        engine.reindex_all_with_progress(resolved.root.as_path(), &mut on_progress)?
    } else {
        engine.reindex_paths_with_progress(
            resolved.root.as_path(),
            &args.paths,
            &mut on_progress,
        )?
    };
    reporter.finish();
    drop(live); // remove the marker before printing the final summary

    // Re-measure after indexing so the summary can show the resulting size and
    // its delta vs. before the run.
    let size_after = cache_dir.as_deref().and_then(index_size_bytes);
    // Active model for the summary's 🧠  Model section — read from vault
    // config (same source `model_load_notice_for` uses); serde fills the
    // default when the key is absent.
    let embed_model = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.search.embed_model)
        .unwrap_or_default();
    let data = ReindexData::from_stats(stats, embed_model, size_before, size_after);

    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// `--lex-only` / `--pending-only` — hook paths
// ─────────────────────────────────────────────────────────────────────────
//
// Both flags exist so an automation hook (a Claude Code `PostToolUse`/`Stop`
// hook, a cron job, etc.) can keep the index warm without ever risking the
// calling turn: every failure mode here — an unresolved vault, a missing
// index, an undownloaded model, a reindex already running, or any error
// during the actual reindex — degrades to a "skip" envelope on stdout and
// exit code 0. Nothing in this path prompts (no TTY interaction), persists
// `search.collection` (uses the READONLY resolver), or calls
// `reconcile_missing_model` (that mutates `onebrain.yml`).

/// Payload for a skipped hook-path run: `{"skipped":true,"reason":"..."}`.
/// Deliberately has NO `decision` field — a completely different hook
/// contract (the plugin's Stop checkpoint flow) parses `decision:block` out
/// of another hook's JSON output, and this envelope must stay inert against
/// that parser.
#[derive(Debug, Serialize)]
struct ReindexSkipData {
    skipped: bool,
    reason: String,
}

fn render_skip_text(env: &Envelope<ReindexSkipData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    format!("⏭  Skipped — {}", d.reason)
}

/// Payload for a `--pending-only --json` run that detached a background
/// child instead of embedding in the foreground: `{"detached":true}`. A
/// dedicated struct (rather than reusing `ReindexSkipData`) keeps the two
/// shapes independent — this is NOT a skip, the work is happening, just not
/// in this process.
///
/// Detach only exists in `semantic` builds — a lex-only build has nothing to
/// embed in the background, so it falls straight to `run_pending_only`'s
/// `semantic-unavailable` skip.
#[cfg(feature = "semantic")]
#[derive(Debug, Serialize)]
struct ReindexDetachData {
    detached: bool,
}

#[cfg(feature = "semantic")]
fn render_detach_text(env: &Envelope<ReindexDetachData>) -> String {
    let _ = env.data.as_ref().expect("ok envelope always has data");
    "🚀  Embedding pending docs in the background".to_string()
}

/// Emit the "detached" envelope and return `Ok(())`.
#[cfg(feature = "semantic")]
fn emit_detached(mode: &OutputMode, vault_info: crate::output::VaultInfo) -> Result<()> {
    let data = ReindexDetachData { detached: true };
    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(
        &envelope,
        mode,
        std::io::stdout().lock(),
        render_detach_text,
    )?;
    Ok(())
}

/// Re-exec the current binary as a detached background child that performs
/// the real `--pending-only` embed, so the parent (the calling hook) can
/// return immediately. Args are rebuilt explicitly (not forwarded from
/// `argv`) so the child always runs exactly `search reindex --pending-only
/// --json [--vault <path>]` regardless of how the parent was invoked. Stdin
/// / stdout / stderr are all nulled — that (not `setsid`) is what lets the
/// parent return without the hook waiting on the child. The child sets
/// `ONEBRAIN_EMBED_FOREGROUND` so it runs the Task-3 foreground path instead
/// of detaching again.
#[cfg(feature = "semantic")]
fn spawn_detached_pending_embed(vault_flag: Option<&PathBuf>) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["search", "reindex", "--pending-only", "--json"]);
    if let Some(vault) = vault_flag {
        cmd.arg("--vault").arg(vault);
    }
    cmd.env("ONEBRAIN_EMBED_FOREGROUND", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()?;
    Ok(())
}

/// Emit the skip envelope for `reason` and return `Ok(())` — the hook-path
/// contract's single exit point for every gate failure / run error.
/// `vault_info` is `None` when the vault itself couldn't be resolved (gate
/// reason 1); present otherwise.
fn emit_skip(
    mode: &OutputMode,
    vault_info: Option<crate::output::VaultInfo>,
    reason: &str,
) -> Result<()> {
    let data = ReindexSkipData {
        skipped: true,
        reason: reason.to_string(),
    };
    let envelope = Envelope::ok("search.reindex", vault_info, data);
    emit(&envelope, mode, std::io::stdout().lock(), render_skip_text)?;
    Ok(())
}

/// Skip `reason` for a hook-path `Engine::open` failure: `"engine-busy"` when
/// the index is locked by another process (redb single-process lock — bug C,
/// v3.4.6), else the generic `"error"`. Keeps the hook path honest without
/// ever failing the calling turn (still exit 0 via `emit_skip`).
fn engine_open_skip_reason(err: &anyhow::Error) -> &'static str {
    if onebrain_search::error::is_engine_busy(err) {
        "engine-busy"
    } else {
        "error"
    }
}

/// Entry point for `--lex-only` / `--pending-only`. Runs the shared safety
/// gate first (pure fs probes, no engine, no prompt); on any gate failure
/// (or any error during the run itself) emits a skip envelope and returns
/// `Ok(())` — this function's return type promises the caller (main.rs) that
/// a hook invocation NEVER fails the process.
fn run_hook_path(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchReindexArgs,
) -> Result<()> {
    // Gate reason 1: vault / collection unresolvable. Uses the READONLY
    // collection resolver (`collection_name_readonly`) — a hook must never
    // persist an auto-generated `search.collection` into `onebrain.yml`.
    let resolved = match crate::vault_ctx::require(vault_flag.clone()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("onebrain search reindex (hook path): {e:#}");
            return emit_skip(mode, None, "no-collection");
        }
    };
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let collection = match collection_name_readonly(resolved.root.as_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("onebrain search reindex (hook path): {e:#}");
            return emit_skip(mode, Some(vault_info), "no-collection");
        }
    };
    let cache_dir = collection_cache_dir(&collection);

    // Warm-daemon path (v3.4.6 Track 2c) — THE headline fix. When a daemon
    // holds the engine (an active `onebrain mcp` session), the auto-reindex
    // hooks previously skipped with `engine-busy` and the write never got
    // indexed in-session. Route the reindex to the daemon instead so it lands
    // NOW: `--lex-only` → daemon `lex` (keyword-only, no embed), `--pending-only`
    // → daemon `pending` (embed the drifted docs). Only when the daemon serves
    // THIS vault; any daemon-side failure degrades to a skip (still exit 0), and
    // no daemon → the local direct-open gates below (unchanged, honest skip).
    if let Some(handle) = route_to_daemon(&resolved) {
        return run_hook_via_daemon(&handle, mode, vault_info, args);
    }

    // Gate reason 2: no index yet.
    if !cache_dir.join("tantivy").is_dir() {
        return emit_skip(mode, Some(vault_info), "no-index");
    }

    // Gate reason 3: the configured model isn't downloaded. Never
    // auto-downloads from a hook — applies to BOTH flags by design (even
    // `--lex-only`, which itself never touches the embedder) so a hook never
    // races a foreground first-run download.
    let embed_model = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.search.embed_model)
        .unwrap_or_default();
    let model_downloaded = {
        use onebrain_search::embed::{model_download_status, model_registry};
        model_registry()
            .iter()
            .find(|m| m.name == embed_model)
            .is_some_and(|info| model_download_status(info, &cache_dir).downloaded)
    };
    if !model_downloaded {
        return emit_skip(mode, Some(vault_info), "model-not-downloaded");
    }

    // Gate reason 4: a reindex is already running (live marker).
    if read_reindex_progress(&cache_dir).is_some() {
        return emit_skip(mode, Some(vault_info), "reindex-in-flight");
    }

    if args.lex_only {
        return run_lex_only(mode, &resolved, vault_info, &cache_dir, args);
    }
    debug_assert!(args.pending_only);

    // Detach: a structured-mode `--pending-only` run (the Stop hook's call
    // shape) must never let model load / embed delay the calling turn. Text
    // mode stays foreground (manual debugging UX), and
    // `ONEBRAIN_EMBED_FOREGROUND` — set by the detached child itself, and by
    // tests — always forces the foreground path below. Only relevant when
    // this build actually has an embedder to run in the background — a
    // `--no-default-features` (lex-only) build has nothing to detach, so it
    // falls straight through to `run_pending_only`'s `semantic-unavailable`
    // skip.
    #[cfg(feature = "semantic")]
    if mode.is_structured() && std::env::var_os("ONEBRAIN_EMBED_FOREGROUND").is_none() {
        return match spawn_detached_pending_embed(vault_flag.as_ref()) {
            Ok(()) => emit_detached(mode, vault_info),
            Err(e) => {
                eprintln!("onebrain search reindex --pending-only: failed to detach: {e:#}");
                emit_skip(mode, Some(vault_info), "detach-failed")
            }
        };
    }
    run_pending_only(mode, &resolved, vault_info, &cache_dir)
}

/// Route a manual `search reindex` (full, or over explicit paths) to the warm
/// daemon so it runs while the daemon holds the engine. Unlike the hook path,
/// this is a foreground command: a daemon error PROPAGATES (the user asked for a
/// reindex and should hear if it failed), not degrade to a silent skip. Full →
/// daemon `all`; explicit paths → daemon `paths`. The daemon reports counts +
/// its active model; on-disk size deltas aren't available over the wire, so the
/// summary omits the 📊  Index section (the counts are what a reindex reports).
fn run_reindex_via_daemon(
    handle: &DaemonHandle,
    resolved: &onebrain_core::ResolvedVault,
    mode: &OutputMode,
    args: &SearchReindexArgs,
) -> Result<()> {
    let vault_info = crate::vault_ctx::info_from(resolved);
    let (daemon_mode, paths) = if args.paths.is_empty() {
        ("all", Vec::new())
    } else {
        ("paths", args.paths.clone())
    };
    let resp = handle
        .reindex(daemon_mode, &paths)
        .map_err(|e| map_daemon_error(e, "route `search reindex` through warm daemon"))?;

    let embed_model = resp
        .get("embed_model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let field = |k: &str| resp.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    let stats = ReindexStats {
        added: field("added"),
        updated: field("updated"),
        removed: field("removed"),
        unchanged: field("unchanged"),
        failed: field("failed"),
    };
    let data = ReindexData::from_stats(stats, embed_model, None, None);
    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Route a hook-path reindex (`--lex-only` / `--pending-only`) to the warm
/// daemon (the sole redb owner), so the write is indexed IN-SESSION instead of
/// skipping on the lock. `--lex-only` → daemon `lex` (keyword-only, no embed);
/// `--pending-only` → daemon `pending` (embed the drifted docs).
///
/// Honesty + exit-0 contract preserved: any daemon-side failure degrades to a
/// skip envelope (`daemon-error`) and returns `Ok(())`, exactly like the local
/// gate failures — a hook invocation NEVER fails the calling turn. On success it
/// emits the normal reindex summary so the hook's output shape is unchanged from
/// the direct path. The daemon caps this at the vault's collection; the size
/// fields stay `None` (the daemon doesn't report on-disk sizes — the counts are
/// what matter to a hook).
fn run_hook_via_daemon(
    handle: &DaemonHandle,
    mode: &OutputMode,
    vault_info: crate::output::VaultInfo,
    args: &SearchReindexArgs,
) -> Result<()> {
    let daemon_mode = if args.lex_only { "lex" } else { "pending" };
    let resp = match handle.reindex(daemon_mode, &[]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("onebrain search reindex (hook path, daemon): {e:#}");
            return emit_skip(mode, Some(vault_info), "daemon-error");
        }
    };

    let embed_model = resp
        .get("embed_model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let field = |k: &str| resp.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    let stats = ReindexStats {
        added: field("added"),
        updated: field("updated"),
        removed: field("removed"),
        unchanged: field("unchanged"),
        failed: field("failed"),
    };
    // The daemon doesn't report on-disk index sizes (`None` → the summary omits
    // the 📊  Index section), which is fine for a hook.
    let data = ReindexData::from_stats(stats, embed_model, None, None);
    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// `--lex-only`, gate already passed: lex/tantivy pass over the whole vault
/// (or `args.paths`, though clap's `conflicts_with_all` makes that
/// combination unreachable today). Any error degrades to a skip envelope
/// rather than failing the hook's calling turn. `cache_dir` is the
/// already-resolved collection cache dir from the gate — re-derived here
/// would just repeat `run_hook_path`'s `collection_name_readonly` call.
fn run_lex_only(
    mode: &OutputMode,
    resolved: &onebrain_core::ResolvedVault,
    vault_info: crate::output::VaultInfo,
    cache_dir: &std::path::Path,
    args: &SearchReindexArgs,
) -> Result<()> {
    let config = match onebrain_core::load_vault_config(&resolved.root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("onebrain search reindex --lex-only: {e:#}");
            return emit_skip(mode, Some(vault_info), "no-collection");
        }
    };
    let mut engine =
        match onebrain_search::engine::Engine::open(cache_dir, &config.search.embed_model) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("onebrain search reindex --lex-only: {e:#}");
                return emit_skip(mode, Some(vault_info), engine_open_skip_reason(&e));
            }
        };
    engine.set_exclude_patterns(config.search.exclude.clone());

    let size_before = index_size_bytes(cache_dir);
    // Structured mode (hooks always pass --json) keeps the reporter Silent;
    // same live-progress marker as the normal path so `search status` from
    // another process can see this run too.
    let mut reporter = ProgressReporter::new(mode, String::new());
    let live = LiveProgressFile::in_cache_dir(cache_dir);
    let mut on_progress = |p: ReindexProgress| {
        match &p {
            ReindexProgress::Walked { total } => live.record(0, *total),
            ReindexProgress::Indexing { done, total, .. } => live.record(*done, *total),
            ReindexProgress::LoadingModel | ReindexProgress::LoadingReranker => {}
        }
        reporter.handle(p);
    };

    let result = if args.paths.is_empty() {
        engine.reindex_all_lex_only_with_progress(resolved.root.as_path(), &mut on_progress)
    } else {
        engine.reindex_paths_lex_only_with_progress(
            resolved.root.as_path(),
            &args.paths,
            &mut on_progress,
        )
    };
    reporter.finish();
    drop(live);

    let stats = match result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("onebrain search reindex --lex-only: {e:#}");
            return emit_skip(mode, Some(vault_info), "error");
        }
    };

    let size_after = index_size_bytes(cache_dir);
    let data = ReindexData::from_stats(stats, config.search.embed_model, size_before, size_after);
    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// `--pending-only`, gate already passed: embed exactly the docs
/// `Engine::pending_vector_paths` reports as drifted. Foreground only — the
/// background-detach wrapper is a separate follow-up (not built here).
///
/// In a `--no-default-features` (lex-only) build there is no embedder at
/// all, so this always skips `semantic-unavailable` without even resolving
/// the collection further.
#[cfg(not(feature = "semantic"))]
fn run_pending_only(
    mode: &OutputMode,
    _resolved: &onebrain_core::ResolvedVault,
    vault_info: crate::output::VaultInfo,
    _cache_dir: &std::path::Path,
) -> Result<()> {
    emit_skip(mode, Some(vault_info), "semantic-unavailable")
}

/// `cache_dir` is the already-resolved collection cache dir from the gate —
/// see `run_lex_only`'s doc comment for why this isn't re-derived here.
#[cfg(feature = "semantic")]
fn run_pending_only(
    mode: &OutputMode,
    resolved: &onebrain_core::ResolvedVault,
    vault_info: crate::output::VaultInfo,
    cache_dir: &std::path::Path,
) -> Result<()> {
    let config = match onebrain_core::load_vault_config(&resolved.root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("onebrain search reindex --pending-only: {e:#}");
            return emit_skip(mode, Some(vault_info), "no-collection");
        }
    };
    let mut engine =
        match onebrain_search::engine::Engine::open(cache_dir, &config.search.embed_model) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("onebrain search reindex --pending-only: {e:#}");
                return emit_skip(mode, Some(vault_info), engine_open_skip_reason(&e));
            }
        };
    engine.set_exclude_patterns(config.search.exclude.clone());

    // Fast path: `pending_vector_paths` never constructs the embedder, so an
    // empty worklist skips before anything expensive happens.
    let pending = match engine.pending_vector_paths(resolved.root.as_path()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onebrain search reindex --pending-only: {e:#}");
            return emit_skip(mode, Some(vault_info), "error");
        }
    };
    if pending.is_empty() {
        return emit_skip(mode, Some(vault_info), "no-pending");
    }

    let size_before = index_size_bytes(cache_dir);
    let mut reporter = ProgressReporter::new(mode, model_load_notice_for(resolved));
    let live = LiveProgressFile::in_cache_dir(cache_dir);
    let mut on_progress = |p: ReindexProgress| {
        match &p {
            ReindexProgress::Walked { total } => live.record(0, *total),
            ReindexProgress::Indexing { done, total, .. } => live.record(*done, *total),
            ReindexProgress::LoadingModel | ReindexProgress::LoadingReranker => {}
        }
        reporter.handle(p);
    };

    let result =
        engine.reindex_paths_with_progress(resolved.root.as_path(), &pending, &mut on_progress);
    reporter.finish();
    drop(live);

    let stats = match result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("onebrain search reindex --pending-only: {e:#}");
            return emit_skip(mode, Some(vault_info), "error");
        }
    };

    let size_after = index_size_bytes(cache_dir);
    let data = ReindexData::from_stats(stats, config.search.embed_model, size_before, size_after);
    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// RAII live-progress marker (see `search_common::reindex_progress_path`):
/// `record` rewrites the tiny JSON atomically; dropping removes the file so
/// a finished (or failed) reindex never leaves a "running" marker behind.
struct LiveProgressFile {
    path: PathBuf,
}

impl LiveProgressFile {
    /// Normal reindex path: resolves the collection via the persisting
    /// `collection_for` (may auto-generate + write `search.collection`).
    fn new(resolved: &onebrain_core::ResolvedVault) -> Result<Self> {
        let collection = collection_for(resolved)?;
        Ok(Self::in_cache_dir(&collection_cache_dir(&collection)))
    }

    /// Hook paths (`--lex-only` / `--pending-only`): build from the gate's
    /// already-resolved cache dir. Re-resolving via `collection_for` here
    /// would persist an auto-generated `search.collection` when unset,
    /// breaking `run_hook_path`'s "never mutates config" contract.
    fn in_cache_dir(cache_dir: &std::path::Path) -> Self {
        Self {
            path: reindex_progress_path(cache_dir),
        }
    }

    fn record(&self, done: usize, total: usize) {
        let _ = onebrain_fs::atomic_write_text(
            &self.path,
            &format!("{{\"done\":{done},\"total\":{total}}}"),
        );
    }
}

impl Drop for LiveProgressFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Delete the collection's index files — `tantivy/`, `vectors/`, and
/// `engine.redb` — while keeping the downloaded `models--*` dirs. The next
/// `Engine::open` recreates everything empty.
fn wipe_index_files(vault_flag: Option<PathBuf>) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag)?;
    let collection = collection_for(&resolved)?;
    let cache_dir = collection_cache_dir(&collection);
    for sub in ["tantivy", "vectors"] {
        match std::fs::remove_dir_all(cache_dir.join(sub)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    match std::fs::remove_file(cache_dir.join("engine.redb")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// First-run model prompt (Feature B). On a real TTY in text mode, when no
/// model has been chosen yet, ask the user to pick one via the shared
/// `inquire::Select` picker and persist the choice to `onebrain.yml` BEFORE the
/// reindex downloads anything. Any other case (already chosen, non-TTY,
/// structured output) is a silent no-op — the caller proceeds with the
/// configured/default model.
///
/// Persists the choice directly (not via `apply_model_change`, which would open
/// the old index + rebuild — pointless on a first run with nothing indexed
/// yet): the subsequent normal reindex downloads + embeds the chosen model
/// exactly once.
fn maybe_prompt_first_model(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    use std::io::IsTerminal;

    // Structured output or non-TTY → never prompt (headless-safe default).
    if mode.is_structured() || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }

    let resolved = crate::vault_ctx::require(vault_flag)?;
    let collection = collection_for(&resolved)?;
    let cache_dir = collection_cache_dir(&collection);

    if !model_not_chosen(resolved.root.as_path(), &cache_dir) {
        return Ok(()); // A model is already chosen or downloaded.
    }

    // Prompt starting on the default model; Esc/cancel keeps the default.
    let config = onebrain_core::load_vault_config(&resolved.root)?;
    let current = config.search.embed_model.clone();
    if let Some(chosen) = crate::commands::search_model::prompt_pick_model(&current) {
        onebrain_fs::persist_search_key(resolved.root.as_path(), "embed_model", chosen)?;
        eprintln!("✅  Using {chosen} — indexing now…");
    }
    Ok(())
}

/// How live reindex progress is surfaced, chosen once from the output mode +
/// stderr TTY state:
/// - `Bar` — an in-place `indicatif` progress bar on a real TTY in text mode.
/// - `PlainLines` — a few plain milestone lines to stderr on a non-TTY text run
///   (piped / agent / hook): the model-load notice plus a ~10%-throttled counter
///   and a final "indexed N" line, so a log never gets thousands of lines.
/// - `Silent` — nothing (structured `--json` / `--yaml`: only the final
///   envelope is emitted).
enum ProgressReporter {
    Bar {
        pb: indicatif::ProgressBar,
        load_notice: String,
        /// Whether anything was drawn/printed (bar or notice) — drives the
        /// blank spacer line before the final summary in [`Self::finish`].
        printed: bool,
    },
    PlainLines {
        last_pct_bucket: Option<usize>,
        load_notice: String,
        /// Same spacer flag as `Bar` for the plain milestone lines.
        printed: bool,
    },
    Silent,
}

impl ProgressReporter {
    fn new(mode: &OutputMode, load_notice: String) -> Self {
        use is_terminal::IsTerminal;
        // Structured modes stay silent regardless of TTY.
        if mode.is_structured() {
            return Self::Silent;
        }
        if std::io::stderr().is_terminal() {
            let pb = indicatif::ProgressBar::new(0);
            // Bar while total is known; the template degrades gracefully before
            // the first `set_length` (spinner-ish) too.
            pb.set_style(
                indicatif::ProgressStyle::with_template(
                    "📇  Indexing  {pos}/{len}  ({percent}%)  {wide_msg}",
                )
                .expect("static template is valid")
                .progress_chars("=>-"),
            );
            Self::Bar {
                pb,
                load_notice,
                printed: false,
            }
        } else {
            Self::PlainLines {
                last_pct_bucket: None,
                load_notice,
                printed: false,
            }
        }
    }

    fn handle(&mut self, p: ReindexProgress) {
        match p {
            // The walk finished: the total is known before any (slow) model
            // load / embed starts, so the bar reads 0/N instead of 0/0.
            ReindexProgress::Walked { total } => match self {
                Self::Bar { pb, printed, .. } => {
                    pb.set_length(total as u64);
                    pb.set_position(0);
                    *printed = true;
                }
                Self::PlainLines { printed, .. } => {
                    eprintln!("📇  Indexing {total} doc(s)…");
                    *printed = true;
                }
                Self::Silent => {}
            },
            ReindexProgress::LoadingModel => match self {
                Self::Bar {
                    pb,
                    load_notice,
                    printed,
                } => {
                    // fastembed prints its own download bar; suspend ours so the
                    // notice + that bar aren't clobbered by our redraw.
                    pb.suspend(|| {
                        eprintln!("{load_notice}");
                    });
                    *printed = true;
                }
                Self::PlainLines {
                    load_notice,
                    printed,
                    ..
                } => {
                    eprintln!("{load_notice}");
                    *printed = true;
                }
                Self::Silent => {}
            },
            // Fires at most once, at the end of a full reindex, only when
            // reranking is enabled and the model isn't downloaded yet —
            // hf-hub prints its own download progress, so this is just a
            // one-line heads-up (no embed-model-specific `load_notice`: the
            // reranker is a distinct model with its own name/size).
            ReindexProgress::LoadingReranker => match self {
                Self::Bar { pb, printed, .. } => {
                    pb.suspend(|| {
                        eprintln!("🧠  Fetching reranker model…");
                    });
                    *printed = true;
                }
                Self::PlainLines { printed, .. } => {
                    eprintln!("🧠  Fetching reranker model…");
                    *printed = true;
                }
                Self::Silent => {}
            },
            ReindexProgress::Indexing {
                done,
                total,
                doc_path,
            } => match self {
                Self::Bar { pb, printed, .. } => {
                    if pb.length() != Some(total as u64) {
                        pb.set_length(total as u64);
                    }
                    pb.set_position(done as u64);
                    pb.set_message(doc_path);
                    *printed = true;
                }
                Self::PlainLines {
                    last_pct_bucket,
                    printed,
                    ..
                } => {
                    // Throttle to ~every 10% so a piped log stays readable.
                    // `checked_div` guards total == 0 (an empty vault emits no
                    // Indexing events anyway, so this branch is really total>0).
                    if let Some(bucket) = (done * 10).checked_div(total) {
                        if *last_pct_bucket != Some(bucket) {
                            *last_pct_bucket = Some(bucket);
                            let pct = (done * 100) / total;
                            eprintln!("📇  Indexing {done}/{total} ({pct}%)");
                            *printed = true;
                        }
                    }
                }
                Self::Silent => {}
            },
        }
    }

    fn finish(&mut self) {
        // One blank stderr line between the progress output (bar / notice /
        // milestone lines) and the final summary, so the ✅  headline doesn't
        // sit flush against it. Nothing printed (Silent, or a run that emitted
        // no progress) → no spacer.
        match self {
            Self::Bar { pb, printed, .. } => {
                // Clear the bar so the final ✅  summary prints on a clean line.
                pb.finish_and_clear();
                if *printed {
                    eprintln!();
                }
            }
            Self::PlainLines { printed, .. } => {
                if *printed {
                    eprintln!();
                }
            }
            Self::Silent => {}
        }
    }
}

/// What to announce when the engine signals `LoadingModel`: honest about
/// whether this is a real first-run download or just loading an
/// already-downloaded model into memory (both stall the bar; only one
/// costs bandwidth).
pub(crate) fn model_load_notice(model: &str, downloaded: bool, approx_size: &str) -> String {
    if downloaded {
        format!("🧠  Loading {model} model…")
    } else {
        format!("⏬  Downloading {model} model ({approx_size}, first run)…")
    }
}

/// Best-effort wiring for [`model_load_notice`]: reads the active model from
/// vault config and checks its on-disk download status under the collection
/// cache dir. Any lookup failure degrades to the generic "loading" wording
/// (never blocks the reindex over a status probe).
fn model_load_notice_for(resolved: &onebrain_core::ResolvedVault) -> String {
    use onebrain_search::embed::{model_download_status, model_registry};

    let model = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.search.embed_model)
        .unwrap_or_default();
    let info = model_registry().iter().find(|m| m.name == model);
    match info {
        Some(i) => {
            let downloaded = collection_for(resolved)
                .map(|c| model_download_status(i, &collection_cache_dir(&c)).downloaded)
                .unwrap_or(false);
            model_load_notice(&model, downloaded, i.approx_size)
        }
        None => format!("🧠  Loading {model} model…"),
    }
}

/// Human-readable byte size with ONE-DECIMAL MB (`16.2 MB`) — deliberately
/// different from `search_common::format_size` (whole-number MB): this file's
/// size DELTAS (`+1.2 MB`) need the precision.
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The `<size> (<delta>)` value for the Index section's Size row when the
/// index size is known. A zero delta renders without the parenthetical; a
/// negative delta uses a minus sign (`−0.8 MB`). Returns `None` when the size
/// couldn't be measured (so the summary just omits the section).
fn index_size_suffix(size: Option<u64>, delta: Option<i64>) -> Option<String> {
    let size = size?;
    let paren = match delta {
        Some(0) | None => String::new(),
        Some(d) if d > 0 => format!(" (+{})", format_size(d as u64)),
        Some(d) => format!(" (−{})", format_size(d.unsigned_abs())),
    };
    Some(format!("{}{paren}", format_size(size)))
}

fn render_text(env: &Envelope<ReindexData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    // Grouped convention, exactly like `search status`: a ✅  confirm line,
    // then emoji-headed sections of indented label/value rows. The 🧠  Model
    // section always renders (an all-unchanged run names the model exactly
    // like a run that embedded); zero doc categories are omitted (a section
    // with no rows is omitted entirely); `failed` carries a ⚠️  so it stands
    // out.
    let mut doc_rows: Vec<String> = Vec::new();
    if d.added > 0 {
        doc_rows.push(item("Added", &d.added.to_string()));
    }
    if d.updated > 0 {
        doc_rows.push(item("Updated", &d.updated.to_string()));
    }
    if d.removed > 0 {
        doc_rows.push(item("Removed", &d.removed.to_string()));
    }
    if d.unchanged > 0 {
        doc_rows.push(item("Unchanged", &d.unchanged.to_string()));
    }
    if d.failed > 0 {
        doc_rows.push(item("Failed", &format!("⚠️  {}", d.failed)));
    }

    let mut out = if doc_rows.is_empty() {
        // Nothing to do — an already-current index reindexed to a no-op.
        "✅  Reindexed — nothing to update".to_string()
    } else {
        "✅  Reindexed".to_string()
    };

    out.push_str(&format!("\n\n{}", section("🧠", "Model")));
    out.push_str(&format!("\n{}", item("Name", &d.embed_model)));

    if !doc_rows.is_empty() {
        out.push_str(&format!("\n\n{}", section("📄", "Docs")));
        for row in doc_rows {
            out.push_str(&format!("\n{row}"));
        }
    }

    if let Some(size) = index_size_suffix(d.index_size_bytes, d.index_size_delta_bytes) {
        out.push_str(&format!("\n\n{}", section("📊", "Index")));
        out.push_str(&format!("\n{}", item("Size", &size)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_progress_file_records_json_and_removes_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        // The hook-path constructor: pure path construction from the cache
        // dir — no vault, no config, so it can never persist anything.
        let path = reindex_progress_path(dir.path());
        {
            let live = LiveProgressFile::in_cache_dir(dir.path());
            live.record(457, 761);
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(body.contains("\"done\":457"), "{body}");
            assert!(body.contains("\"total\":761"), "{body}");
        }
        assert!(!path.exists(), "marker removed on drop");
    }

    /// Build `ReindexData` from counts only (no index-size fields) — the size
    /// suffix is covered by its own dedicated tests.
    fn data(
        added: usize,
        updated: usize,
        removed: usize,
        unchanged: usize,
        failed: usize,
    ) -> ReindexData {
        ReindexData {
            embed_model: "multilingual-e5-small".to_string(),
            added,
            updated,
            removed,
            unchanged,
            failed,
            index_size_bytes: None,
            index_size_delta_bytes: None,
        }
    }

    #[test]
    fn load_notice_distinguishes_download_from_load() {
        let dl = model_load_notice("bge-m3", false, "~2.2 GB");
        assert!(dl.contains("Downloading bge-m3"), "{dl}");
        assert!(dl.contains("~2.2 GB"), "{dl}");
        let ld = model_load_notice("bge-m3", true, "~2.2 GB");
        assert!(ld.contains("Loading bge-m3"), "{ld}");
        assert!(
            !ld.contains("Downloading"),
            "already-downloaded must not claim a download: {ld}"
        );
    }

    #[test]
    fn text_summarizes_all_four_counts() {
        let e = Envelope::ok("search.reindex", None, data(1, 2, 3, 4, 0));
        let s = render_text(&e);
        assert!(s.starts_with("✅  Reindexed\n"), "{s}");
        // The model is always named, between the headline and the Docs section.
        assert!(
            s.contains("\n\n🧠  Model\n    Name          multilingual-e5-small"),
            "{s}"
        );
        // Counts sit under the 📄  Docs section header.
        assert!(s.contains("\n\n📄  Docs\n"), "{s}");
        assert!(s.contains("    Added         1"), "{s}");
        assert!(s.contains("    Updated       2"), "{s}");
        assert!(s.contains("    Removed       3"), "{s}");
        assert!(s.contains("    Unchanged     4"), "{s}");
        // failed == 0 is omitted from the summary.
        assert!(!s.contains("Failed"));
        // No index size known → no 📊  Index section.
        assert!(!s.contains("📊  Index"), "{s}");
    }

    #[test]
    fn text_omits_zero_categories() {
        let e = Envelope::ok("search.reindex", None, data(5, 0, 0, 700, 0));
        let s = render_text(&e);
        assert!(s.contains("    Added         5"), "{s}");
        assert!(s.contains("    Unchanged     700"), "{s}");
        // Zero categories are not shown.
        assert!(!s.contains("Updated"));
        assert!(!s.contains("Removed"));
    }

    #[test]
    fn text_reports_noop_when_all_zero() {
        // No counts and no size → the confirm line + the always-present
        // Model section, no Docs / Index sections.
        let e = Envelope::ok("search.reindex", None, data(0, 0, 0, 0, 0));
        assert_eq!(
            render_text(&e),
            "✅  Reindexed — nothing to update\n\n🧠  Model\n    Name          multilingual-e5-small"
        );
    }

    #[test]
    fn text_noop_still_shows_model_and_index_sections_when_size_known() {
        let mut d = data(0, 0, 0, 0, 0);
        d.index_size_bytes = Some(5 * 1024 * 1024);
        let e = Envelope::ok("search.reindex", None, d);
        let s = render_text(&e);
        assert!(s.starts_with("✅  Reindexed — nothing to update"), "{s}");
        // The Model section names the model even on a no-op run; the Docs
        // section stays omitted (no rows); the Index section still reports
        // the size.
        assert!(s.contains("\n\n🧠  Model\n    Name          "), "{s}");
        assert!(!s.contains("📄  Docs"), "{s}");
        assert!(s.contains("\n\n📊  Index\n    Size          5.0 MB"), "{s}");
    }

    #[test]
    fn text_appends_failed_count_when_nonzero() {
        let e = Envelope::ok("search.reindex", None, data(0, 0, 0, 0, 2));
        assert!(render_text(&e).contains("    Failed        ⚠️  2"));
    }

    #[test]
    fn text_appends_index_size_and_positive_delta() {
        let mut d = data(23, 3, 19, 744, 0);
        d.index_size_bytes = Some(16 * 1024 * 1024 + 200 * 1024); // ~16.2 MB
        d.index_size_delta_bytes = Some(1_300_000); // +~1.2 MB
        let e = Envelope::ok("search.reindex", None, d);
        let s = render_text(&e);
        // Size lands in its own 📊  Index section, after the Docs section.
        assert!(s.contains("\n\n📊  Index\n"), "{s}");
        assert!(s.contains("    Size          16.2 MB"), "{s}");
        assert!(s.contains("(+1.2 MB)"), "{s}");
        let docs_at = s.find("📄  Docs").expect("Docs section");
        let index_at = s.find("📊  Index").expect("Index section");
        assert!(docs_at < index_at, "Docs before Index: {s}");
    }

    #[test]
    fn text_renders_negative_delta_with_minus_sign() {
        let s = index_size_suffix(Some(10 * 1024 * 1024), Some(-800 * 1024)).unwrap();
        assert!(s.contains("10.0 MB"), "{s}");
        assert!(s.contains("(−800 KB)"), "{s}");
    }

    #[test]
    fn text_omits_parenthetical_on_zero_delta() {
        let s = index_size_suffix(Some(5 * 1024 * 1024), Some(0)).unwrap();
        assert!(s.contains("5.0 MB"), "{s}");
        assert!(
            !s.contains('('),
            "zero delta must render no parenthetical: {s}"
        );
    }

    #[test]
    fn text_omits_index_suffix_when_size_unknown() {
        assert!(index_size_suffix(None, Some(100)).is_none());
    }

    #[test]
    fn from_stats_computes_delta_only_when_both_known() {
        let stats = || ReindexStats {
            added: 1,
            ..Default::default()
        };
        let model = || "bge-m3".to_string();
        let d = ReindexData::from_stats(stats(), model(), Some(100), Some(180));
        assert_eq!(d.embed_model, "bge-m3");
        assert_eq!(d.index_size_bytes, Some(180));
        assert_eq!(d.index_size_delta_bytes, Some(80));

        let d2 = ReindexData::from_stats(stats(), model(), None, Some(180));
        assert_eq!(d2.index_size_bytes, Some(180));
        assert_eq!(d2.index_size_delta_bytes, None);

        let d3 = ReindexData::from_stats(stats(), model(), Some(200), Some(120));
        assert_eq!(d3.index_size_delta_bytes, Some(-80));
    }

    #[test]
    fn json_includes_embed_model_field() {
        // Machine consumers can read the active model from the envelope even
        // on an all-unchanged run.
        let v = serde_json::to_value(data(0, 0, 0, 5, 0)).unwrap();
        assert_eq!(v["embed_model"], "multilingual-e5-small");
        assert_eq!(v["unchanged"], 5);
    }
}
