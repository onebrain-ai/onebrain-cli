//! Gain telemetry: [`event::GainEvent`] plus the append-only JSONL writer,
//! the Tier-2 [`rollup`] redb tables, and the [`pivot`] engine that reads
//! from them.

pub mod event;
pub mod pivot;
pub mod rollup;
pub mod writer;

pub use event::{CacheKind, GainEvent, Surface};
pub use pivot::{Dim, PivotQuery, PivotResult, PivotRow, PivotTotals, TimeAxis};
pub use rollup::{RebuildStats, RollupError, RollupKey, RollupValue};
pub use writer::JsonlGainWriter;

/// Where a recorded [`GainEvent`] goes. [`crate::guard::run_funnel`] takes
/// `&mut dyn GainSink` so callers choose the destination — an in-memory
/// collector in tests, the JSONL writer in production, a daemon-side
/// broadcaster in later tracks — without the funnel knowing which.
pub trait GainSink {
    fn record(&mut self, event: GainEvent);
}

/// Simple in-memory sink — the default choice in tests and anywhere a
/// caller wants to inspect events before deciding what to do with them.
#[derive(Debug, Default)]
pub struct MemoryGainSink {
    pub events: Vec<GainEvent>,
}

impl GainSink for MemoryGainSink {
    fn record(&mut self, event: GainEvent) {
        self.events.push(event);
    }
}
