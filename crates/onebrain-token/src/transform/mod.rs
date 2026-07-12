//! `Payload`, `Signal`, the `Transform` trait, and the registered-transform
//! registry consumed by [`crate::guard::run_funnel`].

use crate::estimate::ModelFamily;
use crate::gain::Surface;
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
/// Every lossy transform emits one of these — never a silent drop. Serializable
/// so a surface can hand the signals straight to a structured (`--output
/// json`/MCP) client (design §4: "no silent drops, ever").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

/// Frontmatter-strip policy carried on [`TransformCtx`], mapped from the
/// vault's `token_optimization.strip_frontmatter` config by the surface layer
/// (`ctx_for`). Kept crate-local so `onebrain-token` stays dependency-pure
/// (it never imports `onebrain-core`): the config enum and this one carry the
/// same three cases and are mapped at the boundary.
///
/// - `Auto` (default): defer to the ladder — strip only at `Balanced`+.
/// - `Always`: strip at any active level (`Conservative`+).
/// - `Never`: never strip — a true pass-through, no signal, no byte change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrontmatterPolicy {
    #[default]
    Auto,
    Always,
    Never,
}

/// Per-call parameters the funnel and transforms need. [`run_funnel`] takes
/// this by reference so a caller can build defaults via
/// [`TransformCtx::for_level`] and then mutate per-call overrides (level,
/// caps, `doc_path`) before dispatching — without any new funnel signature.
/// Extra fields later tracks need (e.g. ledger `session_token`, `force`) can
/// be added here without breaking the `&TransformCtx` signature.
///
/// [`run_funnel`]: crate::guard::run_funnel
#[derive(Debug, Clone, Default)]
pub struct TransformCtx {
    /// The ladder rung the funnel gates transforms against. A transform runs
    /// iff `transform.min_level() <= ctx.level`.
    pub level: OptLevel,
    pub model: ModelFamily,
    /// `get`-style continuation cap, in estimated tokens. `None`/`Some(0)`
    /// means unlimited — the cap transform is a no-op.
    pub get_max_tokens: Option<u32>,
    /// Max snippet length, in chars.
    pub snippet_max_chars: Option<u32>,
    /// Whether `FrontmatterStrip` may strip YAML frontmatter, and when.
    /// Defaults to [`FrontmatterPolicy::Auto`] (ladder-driven: strip at
    /// `Balanced`+). `ctx_for` maps the vault's `strip_frontmatter` config
    /// onto this so `always`/`never` are honored.
    pub strip_frontmatter: FrontmatterPolicy,
    /// The document path this payload was sourced from, when known — used
    /// by transforms that need it for signal context (e.g. dedup identity).
    pub doc_path: Option<String>,
    /// Re-materialize / force flag (design §3c): when `true`, the caller has
    /// asked for full content — the ledger AND the size cap are bypassed so a
    /// doc that was already sent (or is over the cap) comes back in full. Set
    /// by MCP `get`'s `force` param and the CLI `--force` flag. Deferred from
    /// Track 1 as YAGNI; the ledger path (Track 3) is the first consumer.
    pub force: bool,
    /// The resolved session token for this call, when one is available
    /// (design §3b). The already-sent ledger keys on `(session_token,
    /// doc_path)`; a `None` here means no token was resolvable (headless / odd
    /// host), so the ledger is silently inactive for this call and content is
    /// inlined normally — never guessed. Session-independent memoization is
    /// unaffected.
    pub session_token: Option<String>,
}

impl TransformCtx {
    /// Sane per-level defaults for the caps the ladder table (design §2)
    /// assigns to each rung: `get_cap` 6k/4k/4k tokens, snippet 200/150/120
    /// chars. `off` carries no caps since no transform runs at that level.
    /// Sets `.level = level` so the funnel gates correctly with no extra
    /// argument.
    pub fn for_level(level: OptLevel) -> Self {
        let (get_max_tokens, snippet_max_chars) = match level {
            OptLevel::Off => (None, None),
            OptLevel::Conservative => (Some(6000), Some(200)),
            OptLevel::Balanced => (Some(4000), Some(150)),
            OptLevel::Aggressive => (Some(4000), Some(120)),
        };
        TransformCtx {
            level,
            model: ModelFamily::default(),
            get_max_tokens,
            snippet_max_chars,
            strip_frontmatter: FrontmatterPolicy::Auto,
            doc_path: None,
            force: false,
            session_token: None,
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
    /// Which surfaces this transform may run on (design §2a). A transform is
    /// gated by BOTH `min_level() <= ctx.level` AND `applies_to(surface)`:
    /// the ladder says *when* a technique switches on, this says *where* it
    /// may run. Default `true`; each transform overrides per the matrix so a
    /// query-hit transform (`snippet`/`disclosure`) can never run on a `get`
    /// document body and empty/truncate it. `ReadHook` never flows through
    /// the funnel for shaping, so it is `false` for every shaping transform.
    fn applies_to(&self, surface: Surface) -> bool {
        let _ = surface;
        true
    }
    /// Whether this transform can discard information (vs. purely
    /// reshaping/compacting it losslessly).
    fn lossy(&self) -> bool;
    fn apply(&self, input: &Payload, ctx: &TransformCtx) -> Payload;
}

/// Query/result-set surfaces — hit lists with per-hit snippets. `json_compact`,
/// `doc_dedup`, `snippet`, `disclosure` operate here (design §2a).
pub(crate) fn is_query_surface(surface: Surface) -> bool {
    matches!(
        surface,
        Surface::McpQuery | Surface::CliSearch | Surface::DaemonHttp
    )
}

/// Document-body surfaces — whole-doc bodies. `whitespace`, `frontmatter`
/// operate here; `get_cap` on `get` only, `head_only` on `multi_get` only.
pub(crate) fn is_doc_surface(surface: Surface) -> bool {
    matches!(surface, Surface::McpGet | Surface::McpMultiGet)
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
        assert_eq!(ctx.level, OptLevel::Off);
        assert!(ctx.get_max_tokens.is_none());
        assert!(ctx.snippet_max_chars.is_none());
    }

    #[test]
    fn for_level_sets_the_level_field() {
        for level in OptLevel::ALL {
            assert_eq!(TransformCtx::for_level(level).level, level);
        }
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
                // Balanced so Auto policy actually strips — exercises the
                // real cut path for idempotence, not the gated no-op.
                TransformCtx::for_level(OptLevel::Balanced),
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
