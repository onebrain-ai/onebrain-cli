//! The `never_worse` backstop and the single runner funnel — the structural
//! enforcement point (design §5c-1/2, ADR 0028) that no surface can bypass
//! by construction: every agent-facing response is meant to flow through
//! [`run_funnel`], which always ends with `never_worse` before recording
//! exactly one [`GainEvent`].

use std::time::{SystemTime, UNIX_EPOCH};

use crate::estimate::{estimate_tokens, ModelFamily};
use crate::gain::{CacheKind, GainEvent, GainSink, Surface};
use crate::transform::{registry, Payload, TransformCtx};

/// The structural choke-point: if `transformed` would cost more *estimated
/// tokens* than `original`, return `original` unchanged instead. No
/// transform, however well-intentioned, is ever allowed to make a response
/// cost more — this is the backstop that catches a bug in any individual
/// transform before it reaches an agent.
///
/// Compares **estimated tokens**, not raw byte length: on a Thai/CJK-heavy
/// vault, byte length and token count diverge (multibyte scripts are ~3
/// bytes/char but nowhere near 3 tokens/char), so a byte comparison could
/// revert a compaction that is genuinely token-smaller — or keep one that is
/// token-larger. Token count is what an agent's context window actually
/// spends, so it is the honest thing to guard on. (Gain events still carry
/// raw byte counts; the gain report converts to tokens at display time.)
pub fn never_worse(original: &Payload, transformed: Payload, model: ModelFamily) -> Payload {
    if estimate_tokens(&transformed.text, model) > estimate_tokens(&original.text, model) {
        original.clone()
    } else {
        transformed
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Applies every registered transform gated to `ctx.level` (in registry
/// order), runs the result through [`never_worse`], and records exactly one
/// [`GainEvent`] on `sink` — THE single emit path every agent-facing surface
/// is meant to call.
///
/// Takes a caller-built `&TransformCtx` rather than a bare level so a surface
/// (Track 4) can supply per-call overrides — level, caps, `doc_path` — and
/// still go through the funnel, preserving the "no surface escapes metering"
/// rule. Callers wanting plain per-level defaults pass
/// `&TransformCtx::for_level(level)`; callers with overrides mutate the ctx
/// first.
///
/// The `transform` field of the recorded event names the transforms that
/// actually changed the payload (comma-joined), or `"none"` if nothing
/// applied or `never_worse` reverted everything.
pub fn run_funnel(
    input: Payload,
    ctx: &TransformCtx,
    surface: Surface,
    sink: &mut dyn GainSink,
) -> Payload {
    let bytes_before = input.text.len() as u64;

    let mut current = input.clone();
    let mut applied: Vec<&'static str> = Vec::new();
    for t in registry() {
        if t.min_level() > ctx.level {
            continue;
        }
        let next = t.apply(&current, ctx);
        if next != current {
            applied.push(t.name());
        }
        current = next;
    }

    let result = never_worse(&input, current, ctx.model);
    let bytes_after = result.text.len() as u64;

    // If never_worse reverted us all the way back to the original, no
    // transform's effect actually survived into the response.
    let transform_label = if result == input || applied.is_empty() {
        "none".to_string()
    } else {
        applied.join(",")
    };

    sink.record(GainEvent {
        ts: now_ts(),
        surface,
        transform: transform_label,
        level: ctx.level,
        bytes_before,
        bytes_after,
        cache: CacheKind::None,
        session_token: None,
    });

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gain::MemoryGainSink;
    use crate::level::OptLevel;
    use crate::transform::Signal;

    const MODEL: ModelFamily = ModelFamily::ClaudeGeneric;

    fn ctx(level: OptLevel) -> TransformCtx {
        TransformCtx::for_level(level)
    }

    #[test]
    fn never_worse_returns_original_when_transformed_costs_more_tokens() {
        let original = Payload::new("short");
        let bloated = Payload::new("this is a much longer replacement text");
        let out = never_worse(&original, bloated, MODEL);
        assert_eq!(out, original);
    }

    #[test]
    fn never_worse_passes_through_transformed_when_not_costlier() {
        let original = Payload::new("some original text here");
        let shrunk = Payload::new("shrunk");
        let out = never_worse(&original, shrunk.clone(), MODEL);
        assert_eq!(out, shrunk);
    }

    #[test]
    fn never_worse_at_equal_cost_keeps_transformed() {
        let original = Payload::new("abcde");
        let same_size = Payload {
            text: "fghij".to_string(),
            signals: vec![Signal::SnippetOmitted],
        };
        let out = never_worse(&original, same_size.clone(), MODEL);
        assert_eq!(out, same_size);
    }

    /// The crux of Fix 1: a case where byte-order and token-order DISAGREE.
    /// `transformed` is strictly MORE bytes than `original` but strictly
    /// FEWER estimated tokens — a real compaction that swaps ASCII metadata
    /// for a shorter multibyte Thai body. A byte-length guard would wrongly
    /// revert it; the token guard must keep it.
    #[test]
    fn never_worse_uses_tokens_not_bytes_when_they_disagree() {
        // 28 ASCII chars: 28 bytes, ceil(28/4.0) = 7 tokens.
        let original = Payload::new("a".repeat(28));
        // 12 Thai chars: 36 bytes (3/char), ceil(12/2.2) = 6 tokens.
        let transformed = Payload::new("ก".repeat(12));

        // Byte-order and token-order genuinely point opposite ways:
        assert!(
            transformed.text.len() > original.text.len(),
            "transformed must be MORE bytes (36 > 28)"
        );
        assert!(
            estimate_tokens(&transformed.text, MODEL) < estimate_tokens(&original.text, MODEL),
            "transformed must be FEWER tokens (6 < 7)"
        );

        // A byte guard would revert here; the token guard must keep it.
        let out = never_worse(&original, transformed.clone(), MODEL);
        assert_eq!(
            out, transformed,
            "token-smaller payload must be kept even though it has more bytes"
        );
    }

    #[test]
    fn never_worse_reverts_a_token_costlier_payload_that_is_fewer_bytes() {
        // Inverse disagreement: transformed has FEWER bytes but MORE tokens.
        // 10 Thai chars: 30 bytes, ceil(10/2.2) = 5 tokens.
        let original = Payload::new("ก".repeat(10));
        // 24 ASCII chars: 24 bytes, ceil(24/4.0) = 6 tokens.
        let transformed = Payload::new("z".repeat(24));

        assert!(
            transformed.text.len() < original.text.len(),
            "transformed must be FEWER bytes (24 < 30)"
        );
        assert!(
            estimate_tokens(&transformed.text, MODEL) > estimate_tokens(&original.text, MODEL),
            "transformed must be MORE tokens (6 > 5)"
        );

        // A byte guard would keep transformed; the token guard must revert.
        let out = never_worse(&original, transformed, MODEL);
        assert_eq!(
            out, original,
            "token-costlier payload must be reverted even though it has fewer bytes"
        );
    }

    #[test]
    fn run_funnel_records_exactly_one_gain_event_per_call() {
        let mut sink = MemoryGainSink::default();
        let input = Payload::new(include_str!("../tests/fixtures/whitespace/input.md"));
        run_funnel(
            input,
            &ctx(OptLevel::Conservative),
            Surface::CliSearch,
            &mut sink,
        );
        assert_eq!(sink.events.len(), 1);
    }

    #[test]
    fn run_funnel_at_off_applies_nothing() {
        let mut sink = MemoryGainSink::default();
        let text = include_str!("../tests/fixtures/whitespace/input.md");
        let out = run_funnel(
            Payload::new(text),
            &ctx(OptLevel::Off),
            Surface::McpQuery,
            &mut sink,
        );
        assert_eq!(out.text, text, "off must be byte-for-byte today's behavior");
        assert_eq!(sink.events[0].transform, "none");
        assert_eq!(sink.events[0].bytes_before, sink.events[0].bytes_after);
    }

    #[test]
    fn run_funnel_gates_transforms_by_ctx_level() {
        let mut sink = MemoryGainSink::default();
        let text = include_str!("../tests/fixtures/frontmatter/input.md");

        // Conservative: frontmatter strip (min_level = Balanced) must NOT apply.
        let conservative = run_funnel(
            Payload::new(text),
            &ctx(OptLevel::Conservative),
            Surface::McpGet,
            &mut sink,
        );
        assert!(conservative.text.contains("tags: [onebrain, cli]"));

        // Balanced: frontmatter strip now applies.
        let balanced = run_funnel(
            Payload::new(text),
            &ctx(OptLevel::Balanced),
            Surface::McpGet,
            &mut sink,
        );
        assert!(!balanced.text.contains("tags: [onebrain, cli]"));

        assert_eq!(sink.events.len(), 2);
    }

    /// Fix 2: a caller can supply per-call overrides on the ctx and the
    /// funnel honors them — here a tighter `get_max_tokens` than the level
    /// default, proving the funnel reads the caller's ctx, not a rebuilt one.
    #[test]
    fn run_funnel_honors_caller_ctx_overrides() {
        let text = include_str!("../tests/fixtures/get_cap/input.md");

        let mut default_sink = MemoryGainSink::default();
        let default_out = run_funnel(
            Payload::new(text),
            &ctx(OptLevel::Conservative),
            Surface::McpGet,
            &mut default_sink,
        );

        let mut override_ctx = TransformCtx::for_level(OptLevel::Conservative);
        override_ctx.get_max_tokens = Some(10); // far tighter than the 6000 default
        let mut override_sink = MemoryGainSink::default();
        let override_out = run_funnel(
            Payload::new(text),
            &override_ctx,
            Surface::McpGet,
            &mut override_sink,
        );

        assert!(
            override_out.text.len() < default_out.text.len(),
            "caller's tighter get_max_tokens override must produce a smaller payload"
        );
    }

    #[test]
    fn run_funnel_records_surface_and_level_on_the_event() {
        let mut sink = MemoryGainSink::default();
        let text = include_str!("../tests/fixtures/whitespace/input.md");
        run_funnel(
            Payload::new(text),
            &ctx(OptLevel::Balanced),
            Surface::DaemonHttp,
            &mut sink,
        );
        let event = &sink.events[0];
        assert_eq!(event.surface, Surface::DaemonHttp);
        assert_eq!(event.level, OptLevel::Balanced);
    }

    /// Property test (plan step 1.5/1.7): a higher level must never emit
    /// more bytes than a lower level for the same input — the ladder only
    /// ever compacts further as you climb it.
    #[test]
    fn higher_level_never_emits_more_bytes() {
        let text = include_str!("../tests/fixtures/get_cap/input.md");
        let mut sink = MemoryGainSink::default();

        let mut prev_bytes: Option<u64> = None;
        for level in OptLevel::ALL {
            let out = run_funnel(
                Payload::new(text),
                &ctx(level),
                Surface::CliSearch,
                &mut sink,
            );
            let bytes = out.text.len() as u64;
            if let Some(prev) = prev_bytes {
                assert!(
                    bytes <= prev,
                    "level {level} produced {bytes} bytes, more than the previous rung's {prev}"
                );
            }
            prev_bytes = Some(bytes);
        }
    }

    #[test]
    fn never_worse_backstop_is_reflected_in_the_gain_event() {
        // Content the registered transforms cannot shrink (already-minimal,
        // no frontmatter, no whitespace, under every cap) — bytes_before
        // must equal bytes_after, never grow.
        let mut sink = MemoryGainSink::default();
        let tiny = "x";
        run_funnel(
            Payload::new(tiny),
            &ctx(OptLevel::Aggressive),
            Surface::McpMultiGet,
            &mut sink,
        );
        let event = &sink.events[0];
        assert!(event.bytes_after <= event.bytes_before);
    }
}
