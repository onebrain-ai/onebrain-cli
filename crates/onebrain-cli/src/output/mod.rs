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

pub use dispatcher::emit;
// `ErrorInfo` / `Warning` are part of the v3.1 envelope public surface —
// re-exported so the v3.2+ commands consuming this module don't need to
// reach into `envelope::` directly. They are currently used only by the
// envelope's internal tests, which is intentional: command bodies build
// them via `Envelope::with_warning` / `Envelope::err` helpers.
#[allow(unused_imports)]
pub use envelope::{Envelope, ErrorInfo, VaultInfo, Warning};
pub use mode::{resolve_output_mode, OutputMode, TtyInputs};
