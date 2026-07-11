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

use onebrain_core::config::TokenOptimizationConfig;
use onebrain_core::path::ResolvedVault;
use onebrain_token::{
    estimate::ModelFamily,
    gain::{GainSink, JsonlGainWriter, Surface},
    run_funnel, GainEvent, OptLevel, Payload, TransformCtx,
};

use super::search_common::{collection_cache_dir, collection_for};

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
/// `token_optimization` config. Per-level cap defaults (design §2) are the
/// baseline; the config's explicit `get_max_tokens`/`snippet_max_chars`
/// override them when the level would otherwise use its own default. `force`
/// bypasses the ledger and the size cap (design §3c).
pub fn ctx_for(
    level: OptLevel,
    cfg: &TokenOptimizationConfig,
    doc_path: Option<String>,
    force: bool,
    session_token: Option<String>,
) -> TransformCtx {
    let mut ctx = TransformCtx::for_level(level);
    ctx.model = model_family_for(&cfg.model);
    // The config caps are authoritative when the level actually shapes output
    // (level > off). `get_max_tokens: 0` means "unlimited" per the config docs.
    if level != OptLevel::Off {
        ctx.get_max_tokens = if cfg.get_max_tokens == 0 {
            None
        } else {
            Some(cfg.get_max_tokens)
        };
        ctx.snippet_max_chars = Some(cfg.snippet_max_chars);
    }
    ctx.doc_path = doc_path;
    ctx.force = force;
    ctx.session_token = session_token;
    ctx
}

/// `<collection_cache>/token/gain/` for this vault, or `None` when the
/// collection can't be resolved (no search configured) — in which case there's
/// nothing to meter against and the surface simply skips gain recording.
pub fn gain_dir(resolved: &ResolvedVault) -> Option<PathBuf> {
    let collection = collection_for(resolved).ok()?;
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

/// Convenience for a plain document body (`get`/`multi_get` surfaces): shape
/// the text and return just the shaped string. Signals are still recorded in
/// the payload internally but doc-body surfaces render plain text, so only the
/// text is surfaced here.
pub fn shape_document(
    resolved: &ResolvedVault,
    surface: Surface,
    text: String,
    ctx: &TransformCtx,
) -> String {
    shape(resolved, surface, Payload::new(text), ctx).text
}

/// Shape a JSON hit list (query surfaces): serialize the canonical hits to a
/// JSON array string, run the funnel, and parse the result back. Callers map
/// their own hit struct to/from canonical `serde_json::Value` objects that use
/// the `doc_path` + `text` keys the query transforms understand (design §1:
/// the transform layer is shared, the field names are not). On any serialize
/// error the hits pass through unshaped (metering still fires with a "none"
/// transform) — shaping must never lose the result set.
pub fn shape_query_hits(
    resolved: &ResolvedVault,
    surface: Surface,
    hits: Vec<serde_json::Value>,
    ctx: &TransformCtx,
) -> Vec<serde_json::Value> {
    let Ok(json) = serde_json::to_string(&hits) else {
        return hits;
    };
    let shaped = shape(resolved, surface, Payload::new(json), ctx);
    serde_json::from_str::<Vec<serde_json::Value>>(&shaped.text).unwrap_or(hits)
}

/// Like [`shape_query_hits`] but records NO gain event — for the memoization
/// hit path (design §3a), where the caller records its own `MemoHit` event so
/// the call still meters exactly once. Same canonical-hit contract.
pub fn shape_query_hits_unmetered(
    surface: Surface,
    hits: Vec<serde_json::Value>,
    ctx: &TransformCtx,
) -> Vec<serde_json::Value> {
    let Ok(json) = serde_json::to_string(&hits) else {
        return hits;
    };
    let shaped = run_funnel(Payload::new(json), ctx, surface, &mut NullGainSink);
    serde_json::from_str::<Vec<serde_json::Value>>(&shaped.text).unwrap_or(hits)
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
        c.get_max_tokens = 1234;
        c.snippet_max_chars = 77;
        let ctx = ctx_for(OptLevel::Balanced, &c, Some("a.md".into()), true, None);
        assert_eq!(ctx.get_max_tokens, Some(1234));
        assert_eq!(ctx.snippet_max_chars, Some(77));
        assert_eq!(ctx.doc_path.as_deref(), Some("a.md"));
        assert!(ctx.force);
    }

    #[test]
    fn ctx_for_get_max_tokens_zero_is_unlimited() {
        let mut c = cfg();
        c.get_max_tokens = 0;
        let ctx = ctx_for(OptLevel::Conservative, &c, None, false, None);
        assert_eq!(ctx.get_max_tokens, None);
    }

    #[test]
    fn ctx_for_off_keeps_no_caps() {
        let ctx = ctx_for(OptLevel::Off, &cfg(), None, false, None);
        assert_eq!(ctx.get_max_tokens, None);
        assert_eq!(ctx.snippet_max_chars, None);
    }
}
