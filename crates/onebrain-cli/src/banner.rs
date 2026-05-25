//! R1 branded banner — TTY-only OneBrain wordmark.
//!
//! Folded into v3.1.0 per the design's "R1 fold-in" decision: render a 5-line
//! FIGlet "Slant" ASCII-art `OneBrain` wordmark in OneBrain primary pink
//! (`#ff2d92`) followed by a dim `Your AI Thinking Partner · vX.Y.Z` tagline
//! when stdout is a colourful TTY, suppress entirely otherwise. The banner
//! exists to make interactive sessions feel branded; it never appears in
//! machine output or hook-protocol invocations.
//!
//! Gating rules (see [`should_show_banner`]):
//! 1. `--quiet` always suppresses.
//! 2. Only `OutputMode::Text { color: true, .. }` shows the banner — pipes,
//!    `--json`, `--yaml`, `--output table|tsv`, and `NO_COLOR`/`TERM=dumb`/
//!    `CI=true`/`--no-color` all drop colour and therefore drop the banner.
//! 3. Hook-protocol commands (`session init`, `checkpoint stop/reset/orphans`,
//!    `qmd reindex`, and their hidden v3.0 aliases `session-init`/
//!    `orphan-scan`/`qmd-reindex`) need deterministic stderr because Claude
//!    Code's Stop hook surfaces stderr to the user. Banner output would
//!    pollute the error UI, so the cleanest contract is "no banner at all
//!    for hook commands".
//! 4. `--help` and `--version` are handled by clap before dispatch is even
//!    called. The dispatch-time `should_show_banner` therefore never fires
//!    for those paths; the pre-parse `should_show_banner_for_help` (called
//!    from `main` BEFORE `Cli::parse()`) covers the `--help` surface so
//!    every help screen — top-level, group, verb — carries the brand line.
//!    `--version` is intentionally excluded from the pre-parse path so the
//!    `-V` output stays a single line (machine-friendly).
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

/// FIGlet "Slant" rendering of the wordmark `OneBrain`. Each art line is
/// stored without trailing whitespace so the rendered banner survives
/// terminals that highlight trailing spaces (and so the snapshot is stable
/// regardless of editor whitespace settings).
const BANNER_ART: &str = r"   ____             ___              _
  / __ \ ____  ___ / __ )_________ _(_)___
 / / / // __ \/ _ \/ __  / ___/ __ `/ // __ \
/ /_/ // / / /  __/ /_/ / /  / /_/ / // / / /
\____//_/ /_/\___/_____/_/   \__,_/_//_/ /_/";

/// Leading indent for the tagline so it sits visually under the centre of the
/// 44-char-wide art block. The tagline (`Your AI Thinking Partner · vX.Y.Z`)
/// is ~32 chars, so 10 spaces of leading indent puts its midpoint near col 26.
const TAGLINE_INDENT: &str = "          ";

/// Build the banner string (no I/O). Six lines total:
///   5 × pink ASCII-art lines (FIGlet "Slant" rendering of `OneBrain`)
///   1 × dim `Your AI Thinking Partner · vX.Y.Z` tagline, indented to centre
///       under the art block.
///
/// Each art line is wrapped in `ANSI_PINK_FG ... ANSI_RESET` and the tagline
/// in `ANSI_DIM ... ANSI_RESET` so the terminal state is always clean after
/// the banner. Trailing newline included so the caller can `write_all`
/// directly and the help body starts on a fresh line below.
pub fn render_banner() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let mut out = String::with_capacity(BANNER_ART.len() + 128);
    for line in BANNER_ART.lines() {
        out.push_str(ANSI_PINK_FG);
        out.push_str(line);
        out.push_str(ANSI_RESET);
        out.push('\n');
    }
    out.push_str(ANSI_DIM);
    out.push_str(TAGLINE_INDENT);
    out.push_str("Your AI Thinking Partner · v");
    out.push_str(version);
    out.push_str(ANSI_RESET);
    out.push('\n');
    out
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

/// True when argv signals a help intent (`--help`, `-h`, or the `help`
/// subcommand keyword). Used by `main` to decide whether to emit the banner
/// BEFORE handing argv to clap (which prints help and exits in-process).
pub fn argv_requests_help(args: &[String]) -> bool {
    // Skip the binary name (`args[0]`) so we don't accidentally match
    // `/path/to/help-runner/onebrain` or similar.
    args.iter()
        .skip(1)
        .any(|a| a == "--help" || a == "-h" || a == "help")
}

/// True when argv signals a version-only intent (`--version` or `-V`). Used
/// to suppress the help banner when version is also present (`-V` wins).
pub fn argv_requests_version(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--version" || a == "-V")
}

/// Env snapshot passed to [`should_show_banner_for_help`] so the function
/// stays pure over its inputs (test-friendly, no process-env reads).
#[derive(Debug, Clone, Default)]
pub struct HelpBannerEnv {
    pub no_color: bool,
    pub term_dumb: bool,
    pub ci_truthy: bool,
}

impl HelpBannerEnv {
    /// Snapshot the live env. Mirrors the colour-suppression subset of the
    /// 6-rule chain in [`crate::output::mode::resolve_output_mode`].
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let term_dumb = std::env::var("TERM").ok().as_deref() == Some("dumb");
        let ci_truthy = std::env::var("CI").ok().is_some_and(|v| !v.is_empty());
        Self {
            no_color,
            term_dumb,
            ci_truthy,
        }
    }

    fn forces_mono(&self) -> bool {
        self.no_color || self.term_dumb || self.ci_truthy
    }
}

/// Decide whether to emit the banner specifically for `--help` invocations.
/// Pure function over its arguments — no env / stdio access. Called from
/// `main` BEFORE clap parses, so we work off raw `std::env::args()`, a
/// pre-resolved `OutputMode` (built from env + a lightweight argv scan by
/// [`tty_inputs_for_help`]), and a snapshot of the colour-relevant env
/// (`NO_COLOR` / `TERM=dumb` / `CI=<truthy>`) via [`HelpBannerEnv`].
///
/// Suppression precedence (any one disqualifies):
/// 1. `--version` / `-V` — version-only intent, no banner.
/// 2. `--quiet` / `-q` — explicit silence.
/// 3. `--json` / `--yaml` / `--output <json|yaml|table|tsv>` — machine consumer.
/// 4. `--no-color` flag — explicit colour suppression.
/// 5. Env says monochrome (`NO_COLOR` / `TERM=dumb` / `CI=<truthy>`).
/// 6. Mode is not `Text { .. }` (Json/Yaml/Table/Tsv).
///
/// The colour bit on the resolved mode is NOT checked: `assert_cmd` pipes
/// stdout in tests, which collapses the colour bit to `false`. Forcing
/// `color: true` here would make every integration test for this path
/// silently skip. The real-world "is the terminal alive?" check happens
/// at emit time via [`stderr_is_tty_or_test_forced`].
pub fn should_show_banner_for_help(
    mode: &OutputMode,
    args: &[String],
    env: &HelpBannerEnv,
) -> bool {
    if argv_requests_version(args) {
        return false;
    }
    if args.iter().skip(1).any(|a| a == "--quiet" || a == "-q") {
        return false;
    }
    if args.iter().skip(1).any(|a| a == "--json" || a == "--yaml") {
        return false;
    }
    // `--output <fmt>` / `-o <fmt>` where fmt ∈ {json,yaml,table,tsv}.
    let mut iter = args.iter().skip(1).peekable();
    while let Some(a) = iter.next() {
        if a == "-o" || a == "--output" {
            if let Some(val) = iter.peek() {
                if matches!(val.as_str(), "json" | "yaml" | "table" | "tsv") {
                    return false;
                }
            }
        } else if let Some(val) = a
            .strip_prefix("--output=")
            .or_else(|| a.strip_prefix("-o="))
        {
            if matches!(val, "json" | "yaml" | "table" | "tsv") {
                return false;
            }
        }
    }
    if args.iter().skip(1).any(|a| a == "--no-color") {
        return false;
    }
    if env.forces_mono() {
        return false;
    }
    matches!(mode, OutputMode::Text { .. })
}

/// True when stderr is a TTY, OR when an explicit override env var is set.
/// The env override (`ONEBRAIN_FORCE_BANNER=1`) exists exclusively so
/// integration tests can exercise the banner path under `assert_cmd`'s
/// piped IO; production code never sets it.
fn stderr_is_tty_or_test_forced() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("ONEBRAIN_FORCE_BANNER").is_some() {
        return true;
    }
    std::io::stderr().is_terminal()
}

/// Emit the help banner to `writer` (typically stderr). No-op when
/// [`should_show_banner_for_help`] returns false OR when stderr is not a
/// real terminal (i.e. `onebrain --help 2>file` would otherwise spill raw
/// ANSI escape sequences into the file). Used by `main` BEFORE `Cli::parse()`
/// so the banner sits above clap's help screen at every level of the
/// subcommand tree.
///
/// The `ONEBRAIN_FORCE_BANNER=1` env var lifts the stderr-tty gate; it
/// exists exclusively so integration tests under `assert_cmd` (which pipes
/// stderr by default) can exercise this path. Production callers never set
/// it.
pub fn emit_help_banner<W: Write>(
    mut writer: W,
    mode: &OutputMode,
    args: &[String],
    env: &HelpBannerEnv,
) {
    if !should_show_banner_for_help(mode, args, env) {
        return;
    }
    if !stderr_is_tty_or_test_forced() {
        return;
    }
    let _ = writer.write_all(render_banner().as_bytes());
}

/// Build the `TtyInputs` the help-banner path needs from a lightweight argv
/// scan. Avoids invoking clap (which would print + exit on `--help`).
///
/// Only inspects flags relevant to colour/mode resolution: `--json`, `--yaml`,
/// `--output <fmt>` / `-o <fmt>`, `--pretty`, `--no-color`. Subcommand-scoped
/// duplicates of those globals are also caught because the global flags accept
/// both pre- and post-subcommand positions in clap.
pub fn tty_inputs_for_help(args: &[String]) -> crate::output::TtyInputs {
    let mut output_flag = "text".to_string();
    let mut json_shortcut = false;
    let mut yaml_shortcut = false;
    let mut pretty = false;
    let mut no_color = false;

    let mut iter = args.iter().skip(1).peekable();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--json" => json_shortcut = true,
            "--yaml" => yaml_shortcut = true,
            "--pretty" => pretty = true,
            "--no-color" => no_color = true,
            "-o" | "--output" => {
                if let Some(val) = iter.next() {
                    output_flag = val.clone();
                }
            }
            other => {
                if let Some(val) = other
                    .strip_prefix("--output=")
                    .or_else(|| other.strip_prefix("-o="))
                {
                    output_flag = val.to_string();
                }
            }
        }
    }

    crate::output::TtyInputs::from_env(&output_flag, json_shortcut, yaml_shortcut, pretty, no_color)
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
        // The wordmark is now ASCII art — the literal string `OneBrain` no
        // longer appears, but the FIGlet "Slant" art is recognisable via its
        // characteristic underscore baseline + first-line `____`.
        assert!(s.contains("____"), "missing ASCII art baseline: {s:?}");
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
    fn banner_renders_5_line_art_plus_tagline() {
        // Exactly 6 lines: 5 art + 1 tagline. `render_banner` ends with a
        // trailing newline so `split('\n')` yields 7 elements (6 lines + the
        // empty tail). Each art line must appear verbatim in order.
        let s = render_banner();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 6, "expected 6 banner lines, got {lines:?}");
        let art_lines: Vec<&str> = BANNER_ART.lines().collect();
        assert_eq!(art_lines.len(), 5, "art block should be 5 lines");
        for (i, art_line) in art_lines.iter().enumerate() {
            assert!(
                lines[i].contains(art_line),
                "rendered line {i} ({:?}) missing art line ({art_line:?})",
                lines[i]
            );
        }
        // Last rendered line is the tagline.
        assert!(
            lines[5].contains("Your AI Thinking Partner"),
            "expected tagline on line 6, got {:?}",
            lines[5]
        );
    }

    #[test]
    fn banner_tagline_includes_version() {
        // Tagline must pull the version dynamically from CARGO_PKG_VERSION —
        // a hard-coded `v3.0.0` would silently drift on every bump.
        let s = render_banner();
        let version = env!("CARGO_PKG_VERSION");
        let tagline_line = s
            .lines()
            .find(|l| l.contains("Your AI Thinking Partner"))
            .expect("tagline line present");
        assert!(
            tagline_line.contains(&format!("v{version}")),
            "tagline {tagline_line:?} missing dynamic v{version}"
        );
    }

    #[test]
    fn banner_art_uses_pink_color_when_enabled() {
        // Every art line is wrapped in the pink truecolor escape. If a
        // future refactor drops the wrapper, the art renders monochrome and
        // the brand presence collapses.
        let s = render_banner();
        assert!(
            s.contains(ANSI_PINK_FG),
            "missing pink truecolor escape: {s:?}"
        );
        // And the dim escape for the tagline.
        assert!(
            s.contains(ANSI_DIM),
            "missing dim escape for tagline: {s:?}"
        );
    }

    #[test]
    fn emit_banner_writes_to_buffer_when_gated_on() {
        let cli = parse(&["onebrain", "vault", "current"]);
        let mut buf: Vec<u8> = Vec::new();
        emit_banner(&mut buf, &cli, &color_text_mode());
        assert!(!buf.is_empty());
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Your AI Thinking Partner"));
    }

    #[test]
    fn emit_banner_writes_nothing_when_gated_off() {
        let cli = parse(&["onebrain", "--quiet", "vault", "current"]);
        let mut buf: Vec<u8> = Vec::new();
        emit_banner(&mut buf, &cli, &color_text_mode());
        assert!(buf.is_empty());
    }

    // ── Help-banner gating (pre-parse path) ──────────────────────────────
    //
    // The help-banner path runs in `main` BEFORE `Cli::parse()` because clap
    // prints `--help` output and exits in-process. These tests pin the argv
    // scanner's decision table directly — no clap involved.

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn should_show_banner_for_help_top_level_text_color() {
        let args = s(&["onebrain", "--help"]);
        assert!(should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        assert!(argv_requests_help(&args));
    }

    #[test]
    fn should_show_banner_for_help_with_subcommand() {
        let args = s(&["onebrain", "plugin", "--help"]);
        assert!(should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
    }

    #[test]
    fn should_show_banner_for_help_with_help_keyword() {
        // `onebrain plugin help` — clap's `help` subcommand keyword.
        let args = s(&["onebrain", "plugin", "help"]);
        assert!(should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        assert!(argv_requests_help(&args));
    }

    #[test]
    fn should_show_banner_for_help_with_short_h() {
        let args = s(&["onebrain", "-h"]);
        assert!(should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        assert!(argv_requests_help(&args));
    }

    #[test]
    fn should_show_banner_for_help_version_takes_priority() {
        let args = s(&["onebrain", "--version"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        assert!(argv_requests_version(&args));
    }

    #[test]
    fn should_show_banner_for_help_version_with_help_combo() {
        // `--version` wins even when `--help` is also present.
        let args = s(&["onebrain", "--version", "--help"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
    }

    #[test]
    fn should_show_banner_for_help_quiet_suppresses() {
        let args = s(&["onebrain", "--help", "--quiet"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        let args2 = s(&["onebrain", "--help", "-q"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args2,
            &HelpBannerEnv::default()
        ));
    }

    #[test]
    fn should_show_banner_for_help_json_suppresses() {
        let args = s(&["onebrain", "--help", "--json"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        let args2 = s(&["onebrain", "--help", "--yaml"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args2,
            &HelpBannerEnv::default()
        ));
    }

    #[test]
    fn should_show_banner_for_help_output_json_suppresses() {
        let args = s(&["onebrain", "--help", "--output", "json"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
        let args2 = s(&["onebrain", "--help", "-o", "yaml"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args2,
            &HelpBannerEnv::default()
        ));
        let args3 = s(&["onebrain", "--help", "--output=table"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args3,
            &HelpBannerEnv::default()
        ));
        let args4 = s(&["onebrain", "--help", "-o=tsv"]);
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args4,
            &HelpBannerEnv::default()
        ));
    }

    #[test]
    fn should_show_banner_for_help_accepts_mono_text_mode() {
        // The help-banner gate intentionally accepts both colour AND mono
        // text modes — colour decisions land at the `emit_help_banner` /
        // `stderr_is_tty_or_test_forced` layer instead, so the pure
        // decision function stays deterministic across test harnesses
        // (which collapse colour to false). The integration tests cover
        // the real-world "is colour available?" path end-to-end.
        let args = s(&["onebrain", "--help"]);
        assert!(should_show_banner_for_help(
            &mono_text_mode(),
            &args,
            &HelpBannerEnv::default()
        ));
    }

    #[test]
    fn should_show_banner_for_help_structured_mode_suppresses() {
        // Even if argv has no structured flag, OutputMode could already be
        // structured (e.g. consumer-side mode override) — the gate also
        // suppresses for Json/Yaml/Table/Tsv modes.
        let args = s(&["onebrain", "--help"]);
        for mode in [
            OutputMode::Json { pretty: true },
            OutputMode::Yaml,
            OutputMode::Table,
            OutputMode::Tsv,
        ] {
            assert!(
                !should_show_banner_for_help(&mode, &args, &HelpBannerEnv::default()),
                "expected suppression for {mode:?}"
            );
        }
    }

    #[test]
    fn should_show_banner_for_help_no_color_env_suppresses() {
        let args = s(&["onebrain", "--help"]);
        let env = HelpBannerEnv {
            no_color: true,
            ..Default::default()
        };
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &env
        ));
    }

    #[test]
    fn should_show_banner_for_help_ci_env_suppresses() {
        let args = s(&["onebrain", "--help"]);
        let env = HelpBannerEnv {
            ci_truthy: true,
            ..Default::default()
        };
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &env
        ));
    }

    #[test]
    fn should_show_banner_for_help_term_dumb_suppresses() {
        let args = s(&["onebrain", "--help"]);
        let env = HelpBannerEnv {
            term_dumb: true,
            ..Default::default()
        };
        assert!(!should_show_banner_for_help(
            &color_text_mode(),
            &args,
            &env
        ));
    }

    #[test]
    fn emit_help_banner_writes_to_buffer_when_gated_on() {
        // `cargo test`'s harness pipes stderr, so the stderr-tty gate would
        // suppress emission. Lift it the same way integration tests do.
        // Env mutation is intentionally scoped to this test; sibling tests
        // either don't read the var or wrap their own scoped set/remove.
        std::env::set_var("ONEBRAIN_FORCE_BANNER", "1");
        let args = s(&["onebrain", "--help"]);
        let mut buf: Vec<u8> = Vec::new();
        emit_help_banner(
            &mut buf,
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default(),
        );
        std::env::remove_var("ONEBRAIN_FORCE_BANNER");
        assert!(!buf.is_empty(), "expected banner emission");
        let out = String::from_utf8(buf).unwrap();
        // The ASCII art's first-line baseline (`____`) is the cheapest stable
        // brand-presence check — the literal word `OneBrain` no longer
        // appears anywhere in the rendered banner.
        assert!(out.contains("____"), "missing ASCII art: {out:?}");
        assert!(
            out.contains("Your AI Thinking Partner"),
            "missing tagline: {out:?}"
        );
    }

    #[test]
    fn emit_help_banner_writes_nothing_when_version_present() {
        // Force the stderr-tty gate ON so the suppression we observe can
        // only come from the `--version` short-circuit.
        std::env::set_var("ONEBRAIN_FORCE_BANNER", "1");
        let args = s(&["onebrain", "--version"]);
        let mut buf: Vec<u8> = Vec::new();
        emit_help_banner(
            &mut buf,
            &color_text_mode(),
            &args,
            &HelpBannerEnv::default(),
        );
        std::env::remove_var("ONEBRAIN_FORCE_BANNER");
        assert!(buf.is_empty(), "expected no banner for --version path");
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
