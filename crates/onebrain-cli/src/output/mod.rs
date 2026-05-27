//! v3.1 output stack — canonical envelope, format dispatcher, TTY mode.
//!
//! Every v3.1 command body builds typed data and hands it off to
//! [`dispatcher::emit`], which picks the right renderer based on
//! [`mode::OutputMode`] resolved from CLI flags + environment + TTY state.
//!
//! Legacy v3.0 output shapes (currently just `SessionInitBlock` /
//! `SessionInitOutput`) live in [`crate::legacy_output`] and remain
//! byte-stable for the back-compat aliases.

pub mod dispatcher;
pub mod envelope;
pub mod mode;
pub mod progress;

pub use dispatcher::emit;
// Progress primitive — braille spinner + grouped status rendering. Shared by
// `doctor` (sectioned) and `update` (linear). Re-exported at the `output`
// level so command modules use `output::progress::…` without the deep path.
#[allow(unused_imports)]
pub use progress::{should_animate, ProgressRenderer, Section, Step, StepStatus, SPINNER_FRAMES};
// `ErrorInfo` / `Warning` are part of the v3.1 envelope public surface.
// `ErrorInfo` is consumed by `main.rs`'s structured-mode error renderer
// (R1 B5) and by command bodies that build error envelopes directly.
// `Warning` is reserved for v3.2+ commands building partial-success
// reports (the `Envelope::with_warning` helper appends them by code/msg).
#[allow(unused_imports)]
pub use envelope::{Envelope, ErrorInfo, VaultInfo, Warning};
pub use mode::{resolve_output_mode, OutputMode, TtyInputs};
