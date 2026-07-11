//! Lossy progressive disclosure: the aggressive-rung "snippet-less query"
//! technique — drops per-hit snippet text entirely, leaving the agent to
//! explicitly request full content via `get`. Reuses `SnippetOmitted`
//! since a full omission is the same honesty claim as a partial one: the
//! snippet field is not trustworthy as-is.

use super::{Payload, Signal, Transform, TransformCtx};
use crate::level::OptLevel;

pub struct Disclosure;

impl Transform for Disclosure {
    fn name(&self) -> &'static str {
        "disclosure"
    }

    fn min_level(&self) -> OptLevel {
        OptLevel::Aggressive
    }

    fn lossy(&self) -> bool {
        true
    }

    fn apply(&self, input: &Payload, _ctx: &TransformCtx) -> Payload {
        if input.text.is_empty() {
            return input.clone();
        }
        let mut signals = input.signals.clone();
        signals.push(Signal::SnippetOmitted);
        Payload {
            text: String::new(),
            signals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = include_str!("../../tests/fixtures/disclosure/input.txt");

    #[test]
    fn drops_snippet_text_entirely_and_signals_omission() {
        let out = Disclosure.apply(&Payload::new(INPUT), &TransformCtx::default());
        assert_eq!(out.text, "");
        assert_eq!(out.signals, vec![Signal::SnippetOmitted]);
    }

    #[test]
    fn already_empty_snippet_is_a_no_op() {
        let empty = Payload::new("");
        let out = Disclosure.apply(&empty, &TransformCtx::default());
        assert_eq!(out, empty);
    }

    #[test]
    fn is_lossy_at_aggressive() {
        assert_eq!(Disclosure.min_level(), OptLevel::Aggressive);
        assert!(Disclosure.lossy());
    }
}
