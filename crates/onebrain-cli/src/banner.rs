//! R1 branded banner — TTY-only OneBrain wordmark.
//!
//! Folded into v3.1.0 per the design's "R1 fold-in" decision: render a small
//! `OneBrain CLI · vX.Y.Z` line in OneBrain primary pink (`#ff2d92`) when
//! stdout is a colourful TTY, suppress entirely otherwise. The banner exists
//! to make interactive sessions feel branded; it never appears in machine
//! output or hook-protocol invocations.
//!
//! Gating rules (see [`should_show_banner`]):
//! 1. `--quiet` always suppresses.
//! 2. Only `OutputMode::Text { color: true, .. }` shows the banner — pipes,
//!    `--json`, `--yaml`, `--output table|tsv`, and `NO_COLOR`/`TERM=dumb`/
//!    `CI=true`/`--no-color` all drop colour and therefore drop the banner.
//! 3. Hook-protocol commands (`session init`, `checkpoint stop/reset/orphans`,
//!    `qmd reindex`, and their hidden v3.0 aliases `session-init`/
//!    `orphan-scan`/`qmd-reindex`) MUST keep stdout free of any
//!    pre-subcommand bytes. They are suppressed even though their stdout is
//!    the machine JSON path — the banner would emit to stderr, but
//!    consumers parse stderr too on some shells, so the cleanest contract is
//!    "no banner at all for hook commands".
//! 4. `--help` and `--version` are handled by clap before dispatch is even
//!    called, so no extra gating is required (the function is never invoked
//!    for those paths).
//!
//! Banner emission target: **stderr**. Stdout stays untouched so that any
//! command piped through `| jq` / `| less` keeps a clean payload even in the
//! corner case where someone forces `--pretty` on a text-mode command piped
//! out.
//!
//! Perf budget: <100 ms first paint. The body is two `writeln!` calls with
//! static strings + `env!("CARGO_PKG_VERSION")` (a compile-time constant), no
//! allocations beyond the format buffer, no I/O beyond a single stderr flush.

use crate::cli::{
    CheckpointCmd, CheckpointVerb, Cli, Cmd, QmdCmd, QmdVerb, SessionCmd, SessionVerb,
};
use crate::output::OutputMode;
use std::io::Write;

/// OneBrain primary brand colour `#ff2d92` as a 24-bit ANSI foreground escape.
/// Truecolor is universal on every terminal that survives the TTY gate (the
/// gate excludes `TERM=dumb` and CI lines), so we don't need to fall back to
/// 256-colour or basic 16-colour.
const ANSI_PINK_FG: &str = "\x1b[38;2;255;45;146m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Should the banner render for this invocation? Pure decision function over
/// the parsed CLI + resolved [`OutputMode`] — no env / stdio access here.
pub fn should_show_banner(cli: &Cli, mode: &OutputMode) -> bool {
    if cli.quiet {
        return false;
    }
    // Only text mode with colour qualifies. Structured modes (JSON/YAML/
    // TSV/Table) and monochrome text both skip.
    let color_text = matches!(mode, OutputMode::Text { color: true, .. });
    if !color_text {
        return false;
    }
    !is_hook_protocol(&cli.command)
}

/// True for commands that participate in Claude Code's hook protocol —
/// session init, all checkpoint verbs, qmd reindex, plus their hidden v3.0
/// aliases. These commands MUST have a clean stdout (and effectively a clean
/// stderr too — banners on stderr can confuse log scrapers).
fn is_hook_protocol(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Session(SessionCmd {
            verb: SessionVerb::Init { .. },
        }) => true,
        Cmd::Checkpoint(CheckpointCmd { verb }) => matches!(
            verb,
            CheckpointVerb::Stop { .. }
                | CheckpointVerb::Reset { .. }
                | CheckpointVerb::Orphans { .. }
        ),
        Cmd::Qmd(QmdCmd {
            verb: QmdVerb::Reindex,
        }) => true,
        // Hidden v3.0 aliases that dispatch to the hook handlers.
        Cmd::SessionInitAlias(_) | Cmd::OrphanScanAlias(_) | Cmd::QmdReindexAlias => true,
        _ => false,
    }
}

/// Build the banner string (no I/O). Two lines:
///   `OneBrain CLI` (pink, bold)
///   `Personal AI OS · vX.Y.Z` (dim)
///
/// Trailing newline included so the caller can `write_all` directly.
pub fn render_banner() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "{pink}OneBrain CLI{reset}  {dim}Personal AI OS · v{version}{reset}\n",
        pink = ANSI_PINK_FG,
        dim = ANSI_DIM,
        reset = ANSI_RESET,
        version = version,
    )
}

/// Emit the banner to `writer` (typically stderr). No-op when
/// [`should_show_banner`] returns false — the gating happens here so callers
/// don't have to remember the check.
pub fn emit_banner<W: Write>(mut writer: W, cli: &Cli, mode: &OutputMode) {
    if !should_show_banner(cli, mode) {
        return;
    }
    // Best-effort write — banner failures must never abort the actual command.
    let _ = writer.write_all(render_banner().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn color_text_mode() -> OutputMode {
        OutputMode::Text {
            color: true,
            pretty: true,
        }
    }

    fn mono_text_mode() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: true,
        }
    }

    // ── Gate: --quiet ────────────────────────────────────────────────────

    #[test]
    fn quiet_flag_suppresses_banner() {
        let cli = parse(&["onebrain", "--quiet", "vault", "current"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    // ── Gate: output mode ────────────────────────────────────────────────

    #[test]
    fn json_mode_suppresses_banner() {
        let cli = parse(&["onebrain", "vault", "current"]);
        assert!(!should_show_banner(
            &cli,
            &OutputMode::Json { pretty: true }
        ));
        assert!(!should_show_banner(
            &cli,
            &OutputMode::Json { pretty: false }
        ));
    }

    #[test]
    fn yaml_mode_suppresses_banner() {
        let cli = parse(&["onebrain", "vault", "current"]);
        assert!(!should_show_banner(&cli, &OutputMode::Yaml));
    }

    #[test]
    fn table_mode_suppresses_banner() {
        let cli = parse(&["onebrain", "vault", "current"]);
        assert!(!should_show_banner(&cli, &OutputMode::Table));
    }

    #[test]
    fn tsv_mode_suppresses_banner() {
        let cli = parse(&["onebrain", "vault", "current"]);
        assert!(!should_show_banner(&cli, &OutputMode::Tsv));
    }

    #[test]
    fn mono_text_mode_suppresses_banner() {
        // Piped stdout / NO_COLOR / CI / TERM=dumb all collapse into
        // `Text { color: false, .. }`. Banner gate is colour-only.
        let cli = parse(&["onebrain", "vault", "current"]);
        assert!(!should_show_banner(&cli, &mono_text_mode()));
    }

    // ── Gate: hook-protocol commands ─────────────────────────────────────

    #[test]
    fn session_init_suppresses_banner() {
        let cli = parse(&["onebrain", "session", "init"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn checkpoint_stop_suppresses_banner() {
        let cli = parse(&["onebrain", "checkpoint", "stop"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn checkpoint_reset_suppresses_banner() {
        let cli = parse(&["onebrain", "checkpoint", "reset"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn checkpoint_orphans_suppresses_banner() {
        let cli = parse(&["onebrain", "checkpoint", "orphans", "07-logs", "tok"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn qmd_reindex_suppresses_banner() {
        let cli = parse(&["onebrain", "qmd", "reindex"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn legacy_session_init_alias_suppresses_banner() {
        let cli = parse(&["onebrain", "session-init"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn legacy_orphan_scan_alias_suppresses_banner() {
        let cli = parse(&["onebrain", "orphan-scan", "07-logs", "tok"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn legacy_qmd_reindex_alias_suppresses_banner() {
        let cli = parse(&["onebrain", "qmd-reindex"]);
        assert!(!should_show_banner(&cli, &color_text_mode()));
    }

    // ── Positive cases: banner shows ─────────────────────────────────────

    #[test]
    fn interactive_vault_command_shows_banner_in_color_text_mode() {
        let cli = parse(&["onebrain", "vault", "current"]);
        assert!(should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn interactive_task_list_shows_banner_in_color_text_mode() {
        let cli = parse(&["onebrain", "task", "list"]);
        assert!(should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn interactive_note_read_shows_banner_in_color_text_mode() {
        let cli = parse(&["onebrain", "note", "read", "Some.md"]);
        assert!(should_show_banner(&cli, &color_text_mode()));
    }

    #[test]
    fn root_doctor_shows_banner_in_color_text_mode() {
        let cli = parse(&["onebrain", "doctor"]);
        assert!(should_show_banner(&cli, &color_text_mode()));
    }

    // ── Render content ───────────────────────────────────────────────────

    #[test]
    fn banner_text_contains_brand_and_version() {
        let s = render_banner();
        assert!(s.contains("OneBrain"), "missing brand text: {s:?}");
        // Version comes from CARGO_PKG_VERSION — always present at compile
        // time. Check the literal `v` prefix the format emits.
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            s.contains(&format!("v{version}")),
            "missing v{version}: {s:?}"
        );
    }

    #[test]
    fn banner_text_includes_ansi_reset() {
        // Defence against future edits dropping the reset and bleeding pink
        // into the user's next prompt.
        let s = render_banner();
        assert!(s.contains(ANSI_RESET), "missing ANSI reset: {s:?}");
    }

    #[test]
    fn emit_banner_writes_to_buffer_when_gated_on() {
        let cli = parse(&["onebrain", "vault", "current"]);
        let mut buf: Vec<u8> = Vec::new();
        emit_banner(&mut buf, &cli, &color_text_mode());
        assert!(!buf.is_empty());
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("OneBrain"));
    }

    #[test]
    fn emit_banner_writes_nothing_when_gated_off() {
        let cli = parse(&["onebrain", "--quiet", "vault", "current"]);
        let mut buf: Vec<u8> = Vec::new();
        emit_banner(&mut buf, &cli, &color_text_mode());
        assert!(buf.is_empty());
    }

    // ── Snapshot ─────────────────────────────────────────────────────────

    /// Lock the banner string shape (ANSI codes + wording) so palette or
    /// wording drift surfaces in `cargo insta review`. The Cargo version is
    /// normalised so a routine version bump doesn't force a snapshot update.
    #[test]
    fn banner_text_snapshot() {
        let raw = render_banner();
        let version = env!("CARGO_PKG_VERSION");
        let normalised = raw.replace(&format!("v{version}"), "v<VERSION>");
        insta::assert_snapshot!("banner_text", normalised);
    }
}
