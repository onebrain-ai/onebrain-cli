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
// Doctor's grouped-status renderer — braille spinner + sectioned status lines.
// Re-exported at the `output` level so command modules use `output::…` without
// the deep path. `is_color_text` / `SPINNER_FRAMES` are crate-internal (doctor +
// tests); the rest are the public-to-crate surface.
pub(crate) use progress::is_color_text;
// `SPINNER_FRAMES` is re-exported only for the doctor static-render test
// (`crate::output::SPINNER_FRAMES`); unused in non-test builds.
#[allow(unused_imports)]
pub(crate) use progress::SPINNER_FRAMES;
#[allow(unused_imports)]
pub use progress::{should_animate, ProgressRenderer, Section, Step, StepStatus};
// `ErrorInfo` / `Warning` are part of the v3.1 envelope public surface.
// `ErrorInfo` is consumed by `main.rs`'s structured-mode error renderer
// (R1 B5) and by command bodies that build error envelopes directly.
// `Warning` is reserved for v3.2+ commands building partial-success
// reports (the `Envelope::with_warning` helper appends them by code/msg).
#[allow(unused_imports)]
pub use envelope::{Envelope, ErrorInfo, VaultInfo, Warning};
pub use mode::{resolve_output_mode, OutputMode, TtyInputs};
