//! The `never_worse` backstop and the single runner funnel — the structural
//! enforcement point (design §5c-1/2, ADR 0028) that no surface can bypass
//! by construction: every agent-facing response is meant to flow through
//! [`run_funnel`], which always ends with `never_worse` before recording
//! exactly one [`GainEvent`].

use std::time::{SystemTime, UNIX_EPOCH};

use crate::gain::{CacheKind, GainEvent, GainSink, Surface};
use crate::level::OptLevel;
use crate::transform::{registry, Payload, TransformCtx};

/// The structural choke-point: if `transformed` would be larger (by byte
/// length of its text) than `original`, return `original` unchanged
/// instead. No transform, however well-intentioned, is ever allowed to
/// make a response bigger — this is the backstop that catches a bug in any
/// individual transform before it reaches an agent.
pub fn never_worse(original: &Payload, transformed: Payload) -> Payload {
    if transformed.text.len() > original.text.len() {
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

/// Applies every registered transform gated to `level` (in registry order),
/// runs the result through [`never_worse`], and records exactly one
/// [`GainEvent`] on `sink` — THE single emit path every agent-facing
/// surface is meant to call. The `transform` field of the recorded event
/// names the transforms that actually changed the payload (comma-joined),
/// or `"none"` if nothing applied or `never_worse` reverted everything.
pub fn run_funnel(
    input: Payload,
    level: OptLevel,
    surface: Surface,
    sink: &mut dyn GainSink,
) -> Payload {
    let ctx = TransformCtx::for_level(level);
    let bytes_before = input.text.len() as u64;

    let mut current = input.clone();
    let mut applied: Vec<&'static str> = Vec::new();
    for t in registry() {
        if t.min_level() > level {
            continue;
        }
        let next = t.apply(&current, &ctx);
        if next != current {
            applied.push(t.name());
        }
        current = next;
    }

    let result = never_worse(&input, current);
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
        level,
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
    use crate::estimate::{estimate_tokens, ModelFamily};
    use crate::gain::MemoryGainSink;
    use crate::transform::Signal;

    #[test]
    fn never_worse_returns_original_when_transformed_is_larger() {
        let original = Payload::new("short");
        let bloated = Payload::new("this is a much longer replacement text");
        let out = never_worse(&original, bloated);
        assert_eq!(out, original);
    }

    #[test]
    fn never_worse_passes_through_transformed_when_not_larger() {
        let original = Payload::new("some original text here");
        let shrunk = Payload::new("shrunk");
        let out = never_worse(&original, shrunk.clone());
        assert_eq!(out, shrunk);
    }

    #[test]
    fn never_worse_at_equal_size_keeps_transformed() {
        let original = Payload::new("abcde");
        let same_size = Payload {
            text: "fghij".to_string(),
            signals: vec![Signal::SnippetOmitted],
        };
        let out = never_worse(&original, same_size.clone());
        assert_eq!(out, same_size);
    }

    #[test]
    fn run_funnel_records_exactly_one_gain_event_per_call() {
        let mut sink = MemoryGainSink::default();
        let input = Payload::new(include_str!("../tests/fixtures/whitespace/input.md"));
        run_funnel(input, OptLevel::Conservative, Surface::CliSearch, &mut sink);
        assert_eq!(sink.events.len(), 1);
    }

    #[test]
    fn run_funnel_at_off_applies_nothing() {
        let mut sink = MemoryGainSink::default();
        let text = include_str!("../tests/fixtures/whitespace/input.md");
        let out = run_funnel(
            Payload::new(text),
            OptLevel::Off,
            Surface::McpQuery,
            &mut sink,
        );
        assert_eq!(out.text, text, "off must be byte-for-byte today's behavior");
        assert_eq!(sink.events[0].transform, "none");
        assert_eq!(sink.events[0].bytes_before, sink.events[0].bytes_after);
    }

    #[test]
    fn run_funnel_gates_transforms_by_level() {
        let mut sink = MemoryGainSink::default();
        let text = include_str!("../tests/fixtures/frontmatter/input.md");

        // Conservative: frontmatter strip (min_level = Balanced) must NOT apply.
        let conservative = run_funnel(
            Payload::new(text),
            OptLevel::Conservative,
            Surface::McpGet,
            &mut sink,
        );
        assert!(conservative.text.contains("tags: [onebrain, cli]"));

        // Balanced: frontmatter strip now applies.
        let balanced = run_funnel(
            Payload::new(text),
            OptLevel::Balanced,
            Surface::McpGet,
            &mut sink,
        );
        assert!(!balanced.text.contains("tags: [onebrain, cli]"));

        assert_eq!(sink.events.len(), 2);
    }

    #[test]
    fn run_funnel_records_surface_and_level_on_the_event() {
        let mut sink = MemoryGainSink::default();
        let text = include_str!("../tests/fixtures/whitespace/input.md");
        run_funnel(
            Payload::new(text),
            OptLevel::Balanced,
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
            let out = run_funnel(Payload::new(text), level, Surface::CliSearch, &mut sink);
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
        // A pathological "transform" scenario is simulated by feeding
        // run_funnel content that the registered transforms cannot shrink
        // (already-minimal, no frontmatter, no whitespace, under every
        // cap) — bytes_before must equal bytes_after, never grow.
        let mut sink = MemoryGainSink::default();
        let tiny = "x";
        run_funnel(
            Payload::new(tiny),
            OptLevel::Aggressive,
            Surface::McpMultiGet,
            &mut sink,
        );
        let event = &sink.events[0];
        assert!(event.bytes_after <= event.bytes_before);
    }

    #[test]
    fn estimate_tokens_is_available_for_ctx_construction_in_this_module() {
        // Sanity: guard.rs's TransformCtx::for_level path is exercised via
        // run_funnel above; this just confirms the crate's public re-export
        // surface compiles together (estimate + level + transform + gain).
        assert!(estimate_tokens("hello", ModelFamily::ClaudeGeneric) > 0);
    }
}
