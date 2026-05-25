//! `onebrain` binary entry point (v3.1 Consistency Standard).
//!
//! All command surface is declared in [`cli`]; dispatch lives in
//! [`v31::dispatch`]. Exit-code mapping is centralised in [`exit`].

mod banner;
mod cli;
mod commands;
mod exit;
mod legacy_output;
mod migration;
mod output;
mod safety;
mod tokio_helper;
mod v31;
mod vault_ctx;

use clap::Parser;
use cli::Cli;
use onebrain_core::CoreError;
use output::{emit, Envelope, ErrorInfo, OutputMode};

fn main() {
    let cli = Cli::parse();
    // Capture the resolved output mode BEFORE dispatching so we can render a
    // canonical envelope on the error path. dispatch() also resolves it
    // internally; cheap to compute twice (pure function over env+flags).
    let mode = v31::dispatch::output_mode(&cli);
    let exit_code = match v31::dispatch::dispatch(cli) {
        Ok(()) => 0,
        Err(e) => {
            render_error(&e, &mode);
            exit::exit_code_for(&e)
        }
    };
    std::process::exit(exit_code);
}

/// Render an error in a way that matches the requested output mode.
///
/// Text mode (default human) writes the v3.0-style `Error: <msg>` line to
/// stderr — keeps the human-readable path unchanged. Structured modes
/// (`--json` / `--yaml` / `--output table|tsv`) build a canonical envelope
/// (`ok: false`, `error: {code, message}`) and emit it on stdout so machine
/// consumers always see one well-formed JSON document per invocation.
///
/// R2-H2: structured-mode error envelopes are always rendered as JSON
/// regardless of the requested format. `--output table` and `--output tsv`
/// have no columnar slots for `code`/`message`/`version` — the generic
/// fallback in `output::dispatcher::emit` produces empty / header-only
/// output in those modes, so a machine consumer reading stdout sees no
/// error signal. Forcing JSON guarantees a lossless canonical envelope on
/// every structured invocation. `--yaml` is also forced to JSON for the
/// error path so consumers don't need a format-switch on the error branch.
///
/// R2-H3: when the error chain carries the `AlreadyReported` sentinel the
/// envelope has been emitted already (e.g. `plugin update`'s partial
/// envelope) — skip the duplicate emission but let `exit_code_for`
/// propagate the exit code unchanged.
fn render_error(e: &anyhow::Error, mode: &OutputMode) {
    if !mode.is_structured() {
        eprintln!("Error: {e:#}");
        return;
    }

    // R2-H3: dispatcher already emitted the canonical envelope. Skip.
    // NOTE: anyhow stores `.context(C)` in a private wrapper type — walking
    // `chain()` and matching `c.is::<C>()` does NOT pierce that wrapper. Use
    // `anyhow::Error::downcast_ref::<C>()` directly, which IS plumbed for
    // context-as-error retrieval.
    if e.downcast_ref::<v31::dispatch::AlreadyReported>().is_some() {
        return;
    }

    // Extract the canonical E_* code from the error chain when available.
    // Falls back to E_INTERNAL for unclassified errors so the envelope is
    // always machine-routable.
    let (code, message) = error_code_and_message(e);
    let env: Envelope<()> = Envelope::err("onebrain.error", None, ErrorInfo::new(code, message));
    // Force JSON for the error envelope regardless of the requested
    // structured mode (R2-H2). Pretty-print only if the requested mode was
    // already pretty JSON; otherwise compact.
    let pretty = matches!(mode, OutputMode::Json { pretty: true });
    let error_mode = OutputMode::Json { pretty };
    // Best-effort emit — if even this fails (e.g., broken pipe), fall
    // through to the exit code. Don't try to write a stderr fallback in
    // structured mode because the consumer is parsing stdout only.
    let _ = emit(&env, &error_mode, std::io::stdout().lock(), |_| {
        String::new()
    });
}

/// Walk the anyhow chain looking for a `CoreError` to grab its stable code.
/// Fallback uses the wrapped error variants in priority order.
fn error_code_and_message(e: &anyhow::Error) -> (&'static str, String) {
    for cause in e.chain() {
        if let Some(core) = cause.downcast_ref::<CoreError>() {
            return (core.error_code(), e.to_string());
        }
    }
    if e.downcast_ref::<onebrain_fs::FsError>().is_some() {
        return ("E_FS_ERROR", e.to_string());
    }
    if e.downcast_ref::<onebrain_cache::CacheError>().is_some() {
        return ("E_CACHE_ERROR", e.to_string());
    }
    ("E_INTERNAL", e.to_string())
}
