//! `onebrain-token` — pure transform functions, the optimization-level
//! ladder, the `never_worse` honesty backstop, and gain telemetry types.
//!
//! Everything in [`transform`], [`level`], and [`guard`] is pure: no I/O,
//! no clock reads beyond what callers pass in. The deliberate I/O
//! boundaries are `gain::writer` (the append-only JSONL gain log),
//! `gain::rollup` (the Tier-2 redb rollup tables `gain::pivot` reads
//! from), and [`cache`] (the redb-backed memoization + already-sent
//! ledger layers in `token.redb`).

pub mod cache;
pub mod estimate;
pub mod gain;
pub mod guard;
pub mod level;
pub mod reference;
pub mod transform;

pub use cache::{
    CacheError, CacheResult, Ledger, LedgerEntry, LedgerVerdict, Memo, MemoKey, TokenCache,
};
pub use estimate::{estimate_tokens, ModelFamily};
pub use gain::{
    CacheKind, Dim, GainEvent, GainSink, JsonlGainWriter, PivotQuery, PivotResult, PivotRow,
    PivotTotals, RebuildStats, RollupError, RollupKey, RollupValue, Surface, TimeAxis,
};
pub use guard::{never_worse, run_funnel};
pub use level::OptLevel;
pub use reference::{content_hash, ReferenceEnvelope};
pub use transform::{Payload, Signal, Transform, TransformCtx};

// Re-exported so callers that only need to open a `token.redb` handle (the
// CLI's `token gain` command today; the daemon's routes in Track 3) don't
// need their own direct `redb` dependency — one version pin, here.
pub use redb::Database;
