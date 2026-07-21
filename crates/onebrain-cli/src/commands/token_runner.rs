//! Surface-side wiring for the token-optimization funnel (design §2a/§5c,
//! Track 4). Every agent-facing read surface — MCP `query`/`get`/`multi_get`,
//! the CLI `search` verbs' structured output, and the daemon's search
//! responses — shapes its payload through [`onebrain_token::run_funnel`] here,
//! so the "no surface escapes metering" rule (design §5d) holds by
//! construction: one funnel call, one recorded gain event.
//!
//! This module owns the plumbing the pure crate deliberately doesn't:
//! - **Level resolution** with the design precedence per-call > config >
//!   default ([`resolve_level`]).
//! - **`TransformCtx` construction** from `token_optimization` config
//!   ([`ctx_for`]).
//! - **Model-family resolution** from the config `model` hint
//!   ([`model_family_for`]).
//! - **The gain sink**: an append-only JSONL writer under
//!   `<collection_cache>/token/gain/` ([`gain_writer`]) — a plain file append
//!   that works in BOTH daemon and `Backend::Direct` mode, since it never
//!   touches the daemon-owned `token.redb` (design §5d: "Direct mode appends
//!   events locally; metering never depends on the daemon being up"). The
//!   Tier-2 rollup tables are derived state, rebuilt from this JSONL via
//!   `onebrain token gain --rebuild`.

use std::path::PathBuf;

use onebrain_core::config::{StripFrontmatter, TokenOptimizationConfig};
use onebrain_core::path::ResolvedVault;
use onebrain_token::{
    estimate::ModelFamily,
    gain::{GainSink, JsonlGainWriter, Surface},
    run_funnel, FrontmatterPolicy, GainEvent, LedgerVerdict, OptLevel, Payload, ReferenceEnvelope,
    Signal, TokenCache, TransformCtx,
};

use super::daemon_client::DaemonHandle;
use super::search_common::{collection_cache_dir, collection_name_readonly};

/// Best-effort session-token resolution for the ledger (design §3b) — the same
/// resolution the daemon client uses. `None` when no token is resolvable
/// (headless / odd host): the ledger is then silently inactive for the call and
/// content inlines, never guessed.
pub fn resolve_session_token_opt() -> Option<String> {
    let inputs = onebrain_cache::ResolveInputs::from_env();
    onebrain_cache::resolve_session_token(&inputs)
        .ok()
        .map(|t| t.to_string())
}

/// `<collection_cache>/token/token.redb` for the Direct-mode memo + ledger (the
/// daemon owns this file in daemon mode). **Pure path resolution: creates
/// nothing, writes nothing.** `None` when the collection can't be resolved.
///
/// Two deliberate properties, both of them the defect in issue #300:
///
/// 1. The collection resolves through the NON-persisting
///    [`collection_name_readonly`], not `collection_for`. A read tool
///    (`memo_get`, a ledger probe) must never rewrite the user's
///    `onebrain.yml`: `collection_for` auto-generates `<dir>-<hash>` and
///    persists it, re-serializing the whole config through serde and stripping
///    a commented template's comments. The server side already resolves
///    read-only everywhere (`server/token_api.rs`) — this follows it.
/// 2. It does NOT create the directory. Creation is gated on the CALLER's
///    intent via [`token_db_path_for_write`] rather than deferred into the
///    first `TokenCache` write, because `TokenCache::open` creates the redb
///    file itself and needs its parent to exist; pushing the `mkdir` down into
///    the token crate would buy nothing. Gating at the call site is what makes
///    the read paths provably side-effect-free — `onebrain serve` reached this
///    function at startup with no engine open and materialized
///    `<cache_root>/<generated>/token/` in the developer's REAL cache root on
///    every test-suite run (125 leaked dirs / 124 MB before the fix).
pub fn token_db_path(resolved: &ResolvedVault) -> Option<PathBuf> {
    let collection = collection_name_readonly(resolved.root.as_path()).ok()?;
    Some(
        collection_cache_dir(&collection)
            .join("token")
            .join("token.redb"),
    )
}

/// [`token_db_path`] for callers that are about to WRITE (a memo put, a ledger
/// record): the same path, with the parent dir created first because
/// `TokenCache::open` creates the redb file and needs it. `None` when the
/// collection can't be resolved or the dir can't be created.
///
/// Every caller must genuinely intend a write — the point of the split is that
/// a pure read never materializes a cache dir.
pub fn token_db_path_for_write(resolved: &ResolvedVault) -> Option<PathBuf> {
    let path = token_db_path(resolved)?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    Some(path)
}

/// Resolve the effective optimization level with the design precedence:
/// per-call override (`--opt-level` / MCP `opt_level`) > `onebrain.yml`
/// `token_optimization.level` > default (already folded into the config type,
/// which defaults `level` to `conservative`). An unparseable per-call string
/// is an error surfaced to the caller — never silently ignored.
pub fn resolve_level(
    per_call: Option<&str>,
    cfg: &TokenOptimizationConfig,
) -> anyhow::Result<OptLevel> {
    match per_call.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .parse::<OptLevel>()
            .map_err(|e| anyhow::anyhow!("invalid opt-level {s:?}: {e}")),
        None => Ok(cfg.level),
    }
}

/// Map the config `model` hint to a coarse [`ModelFamily`] for token
/// estimation. `auto` and any Claude-ish name resolve to the Claude family
/// (the product's own default); an OpenAI/GPT name resolves to the GPT family.
/// Unknown → Claude family (the safe default calibration).
pub fn model_family_for(model: &str) -> ModelFamily {
    let m = model.trim().to_ascii_lowercase();
    if m.starts_with("gpt") || m.contains("openai") || m.starts_with("o1") || m.starts_with("o3") {
        ModelFamily::Gpt4Family
    } else {
        // auto | claude* | anything else
        ModelFamily::ClaudeGeneric
    }
}

/// Build a [`TransformCtx`] from a resolved level + the vault's
/// `token_optimization` config. [`TransformCtx::for_level`] supplies the
/// per-level cap ladder (design §2) as the baseline; the config only
/// *overrides* a cap when the operator set one explicitly:
///
/// - `None` (cap unset) → leave the per-level ladder value in place (the fix
///   for gap 2a — unset caps now tighten as you climb the ladder).
/// - `Some(0)` → unlimited (the long-standing `0 = unlimited` meaning).
/// - `Some(n)` → a fixed cap `n` pinned at every active level.
///
/// `strip_frontmatter` maps the config enum onto the ctx policy so
/// `always`/`never` are honored (the fix for gap 2b). `force` bypasses the
/// ledger and the size cap (design §3c).
pub fn ctx_for(
    level: OptLevel,
    cfg: &TokenOptimizationConfig,
    doc_path: Option<String>,
    force: bool,
    session_token: Option<String>,
) -> TransformCtx {
    let mut ctx = TransformCtx::for_level(level);
    ctx.model = model_family_for(&cfg.model);
    // Only an explicitly-set config cap overrides the per-level ladder. `0`
    // stays "unlimited" (→ None); any other value pins a fixed cap. When the
    // config leaves a cap unset (None), the `for_level` per-level value stands.
    // At `off` no transform runs, so caps are inert either way.
    if level != OptLevel::Off {
        if let Some(cap) = cfg.get_max_tokens {
            ctx.get_max_tokens = (cap != 0).then_some(cap);
        }
        if let Some(cap) = cfg.snippet_max_chars {
            ctx.snippet_max_chars = (cap != 0).then_some(cap);
        }
    }
    ctx.strip_frontmatter = match cfg.strip_frontmatter {
        StripFrontmatter::Auto => FrontmatterPolicy::Auto,
        StripFrontmatter::Always => FrontmatterPolicy::Always,
        StripFrontmatter::Never => FrontmatterPolicy::Never,
    };
    ctx.doc_path = doc_path;
    ctx.force = force;
    ctx.session_token = session_token;
    ctx
}

/// `<collection_cache>/token/gain/` for this vault, or `None` when the
/// collection can't be resolved (no search configured) — in which case there's
/// nothing to meter against and the surface simply skips gain recording.
///
/// Resolves read-only for the same reason as [`token_db_path`] (#300): metering
/// is a side channel and must not rewrite `onebrain.yml`. The directory itself
/// is still created lazily by `JsonlGainWriter` on its first actual append
/// (`gain/writer.rs` `create_dir_all`), so a vault that never records a gain
/// event never gets a dir — no `mkdir` belongs here.
pub fn gain_dir(resolved: &ResolvedVault) -> Option<PathBuf> {
    let collection = collection_name_readonly(resolved.root.as_path()).ok()?;
    Some(collection_cache_dir(&collection).join("token").join("gain"))
}

/// An append-only JSONL gain sink under `<collection_cache>/token/gain/`.
/// `None` when the collection can't be resolved.
pub fn gain_writer(resolved: &ResolvedVault) -> Option<JsonlGainWriter> {
    gain_dir(resolved).map(JsonlGainWriter::new)
}

/// A gain sink that discards events — used when the collection can't be
/// resolved, so the funnel still runs (shaping is independent of metering)
/// without a place to write. Keeps [`shape`] infallible.
#[derive(Default)]
struct NullGainSink;
impl GainSink for NullGainSink {
    fn record(&mut self, _event: onebrain_token::GainEvent) {}
}

/// Run one payload through the funnel for `surface`, recording a gain event to
/// the vault's JSONL sink (or discarding it when no collection is resolvable).
/// The single choke-point every surface calls — returns the shaped [`Payload`]
/// so callers can read both the text and the honesty signals.
pub fn shape(
    resolved: &ResolvedVault,
    surface: Surface,
    input: Payload,
    ctx: &TransformCtx,
) -> Payload {
    match gain_writer(resolved) {
        Some(mut sink) => run_funnel(input, ctx, surface, &mut sink),
        None => run_funnel(input, ctx, surface, &mut NullGainSink),
    }
}

/// Record a single pre-built gain event (the cache-hit paths — a memo hit or a
/// ledger reference — bypass the funnel, so they record their own event with
/// the right [`onebrain_token::CacheKind`]). Best-effort: no collection → no
/// sink → the event is dropped, exactly like the funnel path.
pub fn record_gain(resolved: &ResolvedVault, event: GainEvent) {
    if let Some(mut sink) = gain_writer(resolved) {
        sink.record(event);
    }
}

/// Shape a plain document body (`get`/`multi_get`): returns the shaped text AND
/// the honesty signals the funnel produced (design §4 — no silent drops). The
/// caller renders the signals for the client (e.g. a truncation marker on the
/// body) rather than discarding them.
pub fn shape_document(
    resolved: &ResolvedVault,
    surface: Surface,
    text: String,
    ctx: &TransformCtx,
) -> (String, Vec<Signal>) {
    let shaped = shape(resolved, surface, Payload::new(text), ctx);
    (shaped.text, shaped.signals)
}

/// Shape a JSON hit list (query surfaces): serialize the canonical hits to a
/// JSON array string, run the funnel, and parse the result back. Returns the
/// shaped hits AND the honesty signals (design §4). Callers map their own hit
/// struct to/from canonical `serde_json::Value` objects keyed on `doc_path` and
/// `text` (the keys the query transforms understand — design §1: the transform
/// layer is shared, the field names are not). On any serialize error the hits
/// pass through unshaped (metering still fires) — shaping never loses the set.
pub fn shape_query_hits(
    resolved: &ResolvedVault,
    surface: Surface,
    hits: Vec<serde_json::Value>,
    ctx: &TransformCtx,
) -> (Vec<serde_json::Value>, Vec<Signal>) {
    let Ok(json) = serde_json::to_string(&hits) else {
        return (hits, Vec::new());
    };
    let shaped = shape(resolved, surface, Payload::new(json), ctx);
    let signals = shaped.signals.clone();
    let out = serde_json::from_str::<Vec<serde_json::Value>>(&shaped.text).unwrap_or(hits);
    (out, signals)
}

/// Like [`shape_query_hits`] but records NO gain event — for the memoization
/// hit path (design §3a), where the caller records its own `MemoHit` event so
/// the call still meters exactly once. Same canonical-hit contract; also
/// returns signals.
pub fn shape_query_hits_unmetered(
    surface: Surface,
    hits: Vec<serde_json::Value>,
    ctx: &TransformCtx,
) -> (Vec<serde_json::Value>, Vec<Signal>) {
    let Ok(json) = serde_json::to_string(&hits) else {
        return (hits, Vec::new());
    };
    let shaped = run_funnel(Payload::new(json), ctx, surface, &mut NullGainSink);
    let signals = shaped.signals.clone();
    let out = serde_json::from_str::<Vec<serde_json::Value>>(&shaped.text).unwrap_or(hits);
    (out, signals)
}

/// Render a plain-text honesty marker for a doc-body surface (`get`/
/// `multi_get`) from the funnel's signals — appended to the body so a truncated
/// doc is NEVER silently short (design §4). `None` when there's nothing to
/// disclose. The `Truncated{next}` cursor from `get_cap` is a line index; the
/// marker tells the agent how to see the rest.
pub fn doc_signal_marker(signals: &[Signal], doc_path: &str) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    for sig in signals {
        match sig {
            Signal::Truncated { next } if next == "body" => notes.push(
                "[truncated: only the head of each document is shown — \
                 re-fetch a full document with `onebrain search get <path> --force`]"
                    .to_string(),
            ),
            Signal::Truncated { next } => notes.push(format!(
                "[truncated at line {next}: continue with `fromLine: {next}`, \
                 or get the full document with `onebrain search get {doc_path} --force`]"
            )),
            Signal::SnippetOmitted => notes.push("[snippet omitted]".to_string()),
            Signal::ChunksCollapsed { count } => {
                notes.push(format!("[{count} duplicate chunk(s) collapsed]"))
            }
            Signal::Reference { .. } => {} // references are their own response
        }
    }
    (!notes.is_empty()).then(|| format!("\n\n{}", notes.join("\n")))
}

// ─────────────────────────────────────────────────────────────────────────
// Already-sent ledger (design §3b) — shared by MCP get and CLI search get.
// ─────────────────────────────────────────────────────────────────────────

/// Direct-mode ledger check: no daemon, so open `token.redb` in-process and run
/// the ledger against the doc's raw-file identity hash (`doc_hash`, the same key
/// the read-hook keys on — design §3b/§5d, issue #255). Returns the reference
/// envelope only on an `unchanged` verdict (records the send on first/changed).
/// `None` on no session token, no collection, or any error → the caller inlines
/// the full body. `content` is still passed so `bytes_saved` credits the exact
/// body the reference stood in for.
///
/// **Cross-surface unification (#255):** keying on `doc_hash` — the raw-file
/// identity captured at index time — instead of a hash of the reconstructed/
/// delivered body is what makes the `get` surfaces share ONE ledger entry with
/// the read-hook `token check` (which already keys on `doc_hash`). Before this,
/// each surface hashed a different byte-string for the same doc, so the two
/// ledgers never collided and cross-surface dedup silently never fired.
///
/// **`doc_hash == None` (unindexed doc):** skip the ledger entirely and inline
/// the full body. With no raw-file identity there is no cross-surface entry to
/// share (the read-hook would `Allow` such a doc too), so recording under some
/// other fallback hash would only ever match another `get`, never the read-hook
/// — dead weight. Never unwrap; a `None` here is a clean "inline it."
pub fn direct_ledger_reference(
    resolved: &ResolvedVault,
    doc_path: &str,
    doc_hash: Option<&str>,
    content: &str,
) -> Option<ReferenceEnvelope> {
    // No index identity → skip the ledger and inline (never a hard deny for a
    // doc we can't fingerprint). This is the safe-failure floor: the ledger
    // only ever gates on a genuine identity match.
    let hash = doc_hash?;
    let session = resolve_session_token_opt()?;
    // Write path: this records a ledger entry on FirstSend/Changed, so it may
    // legitimately create the collection's `token/` dir (#300).
    let db_path = token_db_path_for_write(resolved)?;
    // Per-call open: redb's non-blocking exclusive flock means two concurrent
    // tool calls can make one open silently no-op — but it FAILS SAFE (full
    // body, never a wrong answer). A shared cache handle is a v3.4.11 follow-up.
    let cache = TokenCache::open(&db_path).ok()?;
    let ledger = cache.ledger();
    match ledger.check(&session, doc_path, hash).ok()? {
        LedgerVerdict::Unchanged { sent_hash } => Some(ReferenceEnvelope::new(
            doc_path,
            sent_hash,
            content.len() as u64,
        )),
        LedgerVerdict::FirstSend | LedgerVerdict::Changed => {
            let _ = ledger.record(&session, doc_path, hash);
            None
        }
    }
}

/// Daemon-mode ledger check: the daemon owns `token.redb`, so POST to
/// `/api/token/ledger/check` (which enforces the M3 level gate) and read back
/// the reference on `unchanged`. `None` on a 404 (old daemon), non-`unchanged`
/// verdict, or any error.
///
/// **Cross-surface unification (#255):** send NO `content_hash`, so the daemon
/// derives the key from its own engine (`Engine::doc_hash`) — the SAME raw-file
/// identity the read-hook produces when it routes to the daemon (it also sends
/// no hash). That shared key is what lets a `get` and a later `token check` of
/// the same doc collide on one ledger entry. The delivered body size still
/// rides along (`content.len()`) so `bytes_saved` credits the real body.
pub fn daemon_ledger_reference(
    handle: &DaemonHandle,
    doc_path: &str,
    content: &str,
) -> Option<ReferenceEnvelope> {
    let value = handle
        .token_ledger_check(doc_path, None, Some(content.len() as u64), true)
        .ok()??;
    if value.get("verdict").and_then(|v| v.as_str()) != Some("unchanged") {
        return None;
    }
    serde_json::from_value::<ReferenceEnvelope>(value.get("reference")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TokenOptimizationConfig {
        TokenOptimizationConfig::default()
    }

    #[test]
    fn resolve_level_precedence_per_call_over_config() {
        let mut c = cfg();
        c.level = OptLevel::Balanced;
        // Per-call wins.
        assert_eq!(
            resolve_level(Some("aggressive"), &c).unwrap(),
            OptLevel::Aggressive
        );
        // Empty/whitespace per-call falls back to config.
        assert_eq!(resolve_level(Some("  "), &c).unwrap(), OptLevel::Balanced);
        assert_eq!(resolve_level(None, &c).unwrap(), OptLevel::Balanced);
    }

    #[test]
    fn resolve_level_default_is_config_default_conservative() {
        assert_eq!(resolve_level(None, &cfg()).unwrap(), OptLevel::Conservative);
    }

    #[test]
    fn resolve_level_bad_per_call_is_an_error() {
        assert!(resolve_level(Some("yolo"), &cfg()).is_err());
    }

    #[test]
    fn model_family_resolution() {
        assert_eq!(model_family_for("auto"), ModelFamily::ClaudeGeneric);
        assert_eq!(model_family_for("claude-opus"), ModelFamily::ClaudeGeneric);
        assert_eq!(model_family_for("gpt-4o"), ModelFamily::Gpt4Family);
        assert_eq!(model_family_for("o3-mini"), ModelFamily::Gpt4Family);
        assert_eq!(model_family_for("something"), ModelFamily::ClaudeGeneric);
    }

    #[test]
    fn ctx_for_applies_config_caps_above_off() {
        let mut c = cfg();
        c.get_max_tokens = Some(1234);
        c.snippet_max_chars = Some(77);
        let ctx = ctx_for(OptLevel::Balanced, &c, Some("a.md".into()), true, None);
        assert_eq!(ctx.get_max_tokens, Some(1234));
        assert_eq!(ctx.snippet_max_chars, Some(77));
        assert_eq!(ctx.doc_path.as_deref(), Some("a.md"));
        assert!(ctx.force);
    }

    #[test]
    fn ctx_for_get_max_tokens_zero_is_unlimited() {
        let mut c = cfg();
        c.get_max_tokens = Some(0);
        let ctx = ctx_for(OptLevel::Conservative, &c, None, false, None);
        assert_eq!(ctx.get_max_tokens, None);
    }

    #[test]
    fn ctx_for_off_keeps_no_caps() {
        let ctx = ctx_for(OptLevel::Off, &cfg(), None, false, None);
        assert_eq!(ctx.get_max_tokens, None);
        assert_eq!(ctx.snippet_max_chars, None);
    }

    #[test]
    fn ctx_for_unset_caps_follow_the_per_level_ladder() {
        // Gap 2a fix: with the config caps unset (None), the per-level ladder
        // stands — balanced tightens to 4000/150, aggressive to 4000/120 —
        // instead of the old flat 6000/200 at every level.
        let c = cfg();
        assert_eq!(c.get_max_tokens, None, "default config caps must be unset");
        assert_eq!(c.snippet_max_chars, None);

        let balanced = ctx_for(OptLevel::Balanced, &c, None, false, None);
        assert_eq!(balanced.get_max_tokens, Some(4000));
        assert_eq!(balanced.snippet_max_chars, Some(150));

        let aggressive = ctx_for(OptLevel::Aggressive, &c, None, false, None);
        assert_eq!(aggressive.get_max_tokens, Some(4000));
        assert_eq!(aggressive.snippet_max_chars, Some(120));

        let conservative = ctx_for(OptLevel::Conservative, &c, None, false, None);
        assert_eq!(conservative.get_max_tokens, Some(6000));
        assert_eq!(conservative.snippet_max_chars, Some(200));
    }

    #[test]
    fn ctx_for_fixed_cap_pins_the_same_value_at_every_active_level() {
        // A set cap is an operator override: the SAME value at every active
        // level, overriding the per-level ladder.
        let mut c = cfg();
        c.get_max_tokens = Some(3000);
        for level in [
            OptLevel::Conservative,
            OptLevel::Balanced,
            OptLevel::Aggressive,
        ] {
            let ctx = ctx_for(level, &c, None, false, None);
            assert_eq!(
                ctx.get_max_tokens,
                Some(3000),
                "fixed cap must pin at {level}"
            );
        }
    }

    #[test]
    fn ctx_for_maps_strip_frontmatter_policy() {
        // Gap 2b fix: the config enum reaches the ctx policy.
        for (cfg_val, expect) in [
            (StripFrontmatter::Auto, FrontmatterPolicy::Auto),
            (StripFrontmatter::Always, FrontmatterPolicy::Always),
            (StripFrontmatter::Never, FrontmatterPolicy::Never),
        ] {
            let mut c = cfg();
            c.strip_frontmatter = cfg_val;
            let ctx = ctx_for(OptLevel::Balanced, &c, None, false, None);
            assert_eq!(ctx.strip_frontmatter, expect);
        }
    }

    #[test]
    fn doc_signal_marker_none_when_no_signals() {
        assert!(doc_signal_marker(&[], "a.md").is_none());
        assert!(doc_signal_marker(
            &[Signal::Reference {
                doc_path: "a.md".into(),
                hash: "h".into(),
                bytes_saved: 1,
                rematerialize: "x".into(),
            }],
            "a.md"
        )
        .is_none());
    }

    #[test]
    fn doc_signal_marker_truncated_line_carries_cursor_and_force() {
        let m = doc_signal_marker(&[Signal::Truncated { next: "42".into() }], "a.md").unwrap();
        assert!(m.contains("line 42"), "{m}");
        assert!(m.contains("fromLine: 42"), "{m}");
        assert!(m.contains("onebrain search get a.md --force"), "{m}");
    }

    #[test]
    fn doc_signal_marker_head_only_and_omissions() {
        let m = doc_signal_marker(
            &[
                Signal::Truncated {
                    next: "body".into(),
                },
                Signal::SnippetOmitted,
                Signal::ChunksCollapsed { count: 3 },
            ],
            "a.md",
        )
        .unwrap();
        assert!(m.contains("head of each document"), "{m}");
        assert!(m.contains("snippet omitted"), "{m}");
        assert!(m.contains("3 duplicate chunk"), "{m}");
    }
}
