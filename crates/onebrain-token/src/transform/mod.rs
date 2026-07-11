//! `Payload`, `Signal`, the `Transform` trait, and the registered-transform
//! registry consumed by [`crate::guard::run_funnel`].

use crate::estimate::ModelFamily;
use crate::level::OptLevel;

pub mod disclosure;
pub mod doc_dedup;
pub mod frontmatter;
pub mod get_cap;
pub mod head_only;
pub mod json_compact;
pub mod snippet;
pub mod whitespace;

pub use disclosure::Disclosure;
pub use doc_dedup::DocDedup;
pub use frontmatter::FrontmatterStrip;
pub use get_cap::GetCap;
pub use head_only::HeadOnly;
pub use json_compact::JsonCompact;
pub use snippet::SnippetCap;
pub use whitespace::Whitespace;

/// The unit every transform consumes and produces: text plus the honesty
/// signals accumulated so far. Transforms append to `signals`, never remove
/// from it — earlier transforms' honesty claims stay true downstream.
#[derive(Debug, Clone, PartialEq)]
pub struct Payload {
    pub text: String,
    pub signals: Vec<Signal>,
}

impl Payload {
    /// A fresh payload with no signals recorded yet.
    pub fn new(text: impl Into<String>) -> Self {
        Payload {
            text: text.into(),
            signals: Vec::new(),
        }
    }
}

/// The complete, frozen honesty-signal vocabulary (design §4 / ADR 0028).
/// Every lossy transform emits one of these — never a silent drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// Content was cut; `next` is the recovery cursor/instruction (a line
    /// index for pagination, or a short tag naming what was cut, e.g.
    /// `"frontmatter"`).
    Truncated { next: String },
    /// A snippet was shortened or removed entirely.
    SnippetOmitted,
    /// `N` near-duplicate chunks were collapsed into one.
    ChunksCollapsed(u32),
    /// The content already sent earlier in this session was replaced by a
    /// reference — the ledger's (Track 3) honesty envelope.
    Reference {
        doc_path: String,
        hash: String,
        bytes_saved: u64,
        rematerialize: String,
    },
}

/// Per-call parameters transforms may need. Not itself part of the frozen
/// interface list — [`crate::guard::run_funnel`] derives sane per-level
/// defaults (`TransformCtx::for_level`); later tracks may construct one
/// directly for finer per-surface control.
#[derive(Debug, Clone, Default)]
pub struct TransformCtx {
    pub model: ModelFamily,
    /// `get`-style continuation cap, in estimated tokens. `None`/`Some(0)`
    /// means unlimited — the cap transform is a no-op.
    pub get_max_tokens: Option<u32>,
    /// Max snippet length, in chars.
    pub snippet_max_chars: Option<u32>,
    /// The document path this payload was sourced from, when known — used
    /// by transforms that need it for signal context (e.g. dedup identity).
    pub doc_path: Option<String>,
}

impl TransformCtx {
    /// Sane per-level defaults for the caps the ladder table (design §2)
    /// assigns to each rung: `get_cap` 6k/4k/4k tokens, snippet 200/150/120
    /// chars. `off` carries no caps since no transform runs at that level.
    pub fn for_level(level: OptLevel) -> Self {
        let (get_max_tokens, snippet_max_chars) = match level {
            OptLevel::Off => (None, None),
            OptLevel::Conservative => (Some(6000), Some(200)),
            OptLevel::Balanced => (Some(4000), Some(150)),
            OptLevel::Aggressive => (Some(4000), Some(120)),
        };
        TransformCtx {
            model: ModelFamily::default(),
            get_max_tokens,
            snippet_max_chars,
            doc_path: None,
        }
    }
}

/// A single reshaping step. Pure: no I/O, no hidden state — same input +
/// ctx always produces the same output.
pub trait Transform {
    /// Stable identifier, used in gain events and the fixture-guard test.
    fn name(&self) -> &'static str;
    /// The lowest ladder rung at which this transform is active.
    fn min_level(&self) -> OptLevel;
    /// Whether this transform can discard information (vs. purely
    /// reshaping/compacting it losslessly).
    fn lossy(&self) -> bool;
    fn apply(&self, input: &Payload, ctx: &TransformCtx) -> Payload;
}

/// All registered transforms, in the canonical order [`crate::guard::run_funnel`]
/// applies them. This is the single source of truth the fixture-coverage
/// guard test (1.8) enumerates.
pub fn registry() -> Vec<Box<dyn Transform>> {
    vec![
        Box::new(JsonCompact),
        Box::new(DocDedup),
        Box::new(Whitespace),
        Box::new(GetCap),
        Box::new(FrontmatterStrip),
        Box::new(SnippetCap),
        Box::new(Disclosure),
        Box::new(HeadOnly),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique() {
        let names: Vec<&str> = registry().iter().map(|t| t.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate transform name in registry: {names:?}"
        );
    }

    #[test]
    fn for_level_off_has_no_caps() {
        let ctx = TransformCtx::for_level(OptLevel::Off);
        assert!(ctx.get_max_tokens.is_none());
        assert!(ctx.snippet_max_chars.is_none());
    }

    #[test]
    fn for_level_caps_tighten_up_the_ladder() {
        let conservative = TransformCtx::for_level(OptLevel::Conservative);
        let balanced = TransformCtx::for_level(OptLevel::Balanced);
        let aggressive = TransformCtx::for_level(OptLevel::Aggressive);
        assert_eq!(conservative.get_max_tokens, Some(6000));
        assert_eq!(balanced.get_max_tokens, Some(4000));
        assert_eq!(aggressive.get_max_tokens, Some(4000));
        assert_eq!(conservative.snippet_max_chars, Some(200));
        assert_eq!(balanced.snippet_max_chars, Some(150));
        assert_eq!(aggressive.snippet_max_chars, Some(120));
    }

    /// Property test (plan step 1.5): applying any registered transform
    /// twice, with the same ctx, must equal applying it once — every
    /// transform's "already handled" branch must be a true no-op, not a
    /// re-cut that drifts on repeated application.
    #[test]
    fn every_transform_is_idempotent() {
        const JSON_INPUT: &str = include_str!("../../tests/fixtures/json_compact/input.json");
        const DEDUP_INPUT: &str = include_str!("../../tests/fixtures/doc_dedup/input.json");
        const PROSE_INPUT: &str = include_str!("../../tests/fixtures/whitespace/input.md");
        const LONG_INPUT: &str = include_str!("../../tests/fixtures/get_cap/input.md");
        const SNIPPET_INPUT: &str = include_str!("../../tests/fixtures/snippet/input.txt");

        let cases: Vec<(&str, Payload, TransformCtx)> = vec![
            (
                "json_compact",
                Payload::new(JSON_INPUT),
                TransformCtx::default(),
            ),
            (
                "doc_dedup",
                Payload::new(DEDUP_INPUT),
                TransformCtx::default(),
            ),
            (
                "whitespace",
                Payload::new(PROSE_INPUT),
                TransformCtx::default(),
            ),
            (
                "get_cap",
                Payload::new(LONG_INPUT),
                TransformCtx {
                    get_max_tokens: Some(20),
                    ..TransformCtx::default()
                },
            ),
            (
                "frontmatter",
                Payload::new(include_str!("../../tests/fixtures/frontmatter/input.md")),
                TransformCtx::default(),
            ),
            (
                "snippet",
                Payload::new(SNIPPET_INPUT.trim()),
                TransformCtx {
                    snippet_max_chars: Some(50),
                    ..TransformCtx::default()
                },
            ),
            (
                "disclosure",
                Payload::new(include_str!("../../tests/fixtures/disclosure/input.txt")),
                TransformCtx::default(),
            ),
            (
                "head_only",
                Payload::new(LONG_INPUT),
                TransformCtx {
                    get_max_tokens: Some(20),
                    ..TransformCtx::default()
                },
            ),
        ];

        assert_eq!(
            cases.len(),
            registry().len(),
            "every registered transform needs an idempotence case"
        );

        for t in registry() {
            let (_, payload, ctx) = cases
                .iter()
                .find(|(name, _, _)| *name == t.name())
                .unwrap_or_else(|| panic!("no idempotence case for transform {:?}", t.name()));

            let once = t.apply(payload, ctx);
            let twice = t.apply(&once, ctx);
            assert_eq!(once, twice, "transform {:?} is not idempotent", t.name());
        }
    }
}
