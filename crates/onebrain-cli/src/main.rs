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
    // Pre-parse help-banner pass. Clap renders `--help` / `-h` / `help <verb>`
    // output and exits in-process, BEFORE `dispatch::dispatch` would otherwise
    // emit the banner. Emit it here so every help screen — top-level, group,
    // verb — carries the brand line. Gating mirrors `should_show_banner` minus
    // the parsed-CLI-only checks (hook-protocol gate isn't relevant for help).
    let raw_args: Vec<String> = std::env::args().collect();
    let pre_parse_mode = output::resolve_output_mode(&banner::tty_inputs_for_help(&raw_args));
    let pre_parse_env = banner::HelpBannerEnv::from_env();
    let argv_requests_help = banner::argv_requests_help(&raw_args);
    if argv_requests_help {
        banner::emit_help_banner(
            std::io::stderr().lock(),
            &pre_parse_mode,
            &raw_args,
            &pre_parse_env,
        );
    }

    // `try_parse` so we can intercept the `arg_required_else_help` path —
    // group commands like `onebrain harness` (which require a subcommand) trip
    // `DisplayHelpOnMissingArgumentOrSubcommand` and clap auto-prints the help
    // screen + exits. The pre-parse banner pass above only sees argv tokens
    // (`--help` / `-h` / no-subcommand-at-all), so a bare group hop lands here
    // without a banner. Emit it before delegating to `err.exit()`.
    //
    // `MissingSubcommand` is included as defense-in-depth: clap's `cli.rs`
    // round-trip test already documents that `harness` (no verb) can return
    // either `DisplayHelpOnMissingArgumentOrSubcommand` OR `MissingSubcommand`
    // depending on the version. If a future clap upgrade switches to the
    // latter, the banner stays emitted (the trade is one banner above clap's
    // "missing required arg" error, which is acceptable noise vs. silently
    // regressing the issue #2 fix).
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            let prints_help = matches!(
                err.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | ErrorKind::MissingSubcommand
            );
            if prints_help && !argv_requests_help {
                banner::emit_help_banner(
                    std::io::stderr().lock(),
                    &pre_parse_mode,
                    &raw_args,
                    &pre_parse_env,
                );
            }
            err.exit();
        }
    };
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
/// regardless of the requested format. Tsv and Table modes have no
/// columnar slots for nested error fields (no `error.code` /
/// `error.message` / `version` columns), so error envelopes are forced to
/// Json regardless of requested mode. Forcing JSON guarantees a lossless
/// canonical envelope on every structured invocation. `--yaml` is also
/// forced to JSON for the error path so consumers don't need a
/// format-switch on the error branch.
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
        // Mirror the exit-code probe — `FsError::Core(CoreError)` doesn't
        // expose the inner CoreError through anyhow's chain() walk (see
        // exit.rs for the full reasoning), so pattern-match for it here.
        if let Some(onebrain_fs::FsError::Core(core)) = cause.downcast_ref::<onebrain_fs::FsError>()
        {
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
