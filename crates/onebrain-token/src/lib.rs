//! `onebrain-token` — pure transform functions, the optimization-level
//! ladder, the `never_worse` honesty backstop, and gain telemetry types.
//!
//! Everything in [`transform`], [`level`], and [`guard`] is pure: no I/O,
//! no clock reads beyond what callers pass in. The one deliberate I/O
//! boundary is `gain::writer` — the append-only JSONL gain log.

pub mod estimate;
pub mod gain;
pub mod guard;
pub mod level;
pub mod transform;

pub use estimate::{estimate_tokens, ModelFamily};
pub use gain::{CacheKind, GainEvent, GainSink, Surface};
pub use guard::{never_worse, run_funnel};
pub use level::OptLevel;
pub use transform::{Payload, Signal, Transform, TransformCtx};
