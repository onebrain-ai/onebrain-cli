//! R1 branded banner — TTY-only OneBrain wordmark.
//!
//! Folded into v3.1.0 per the design's "R1 fold-in" decision: render a 6-line
//! "ANSI Shadow" block-with-shadow `OneBrain` wordmark in OneBrain primary pink
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
/// Kept as a named constant so tests can pin "brand pink is present" — it is
/// the right edge of the wordmark gradient (see [`gradient_fg`] / [`GRAD_PINK`]).
#[cfg_attr(not(test), allow(dead_code))]
const ANSI_PINK_FG: &str = "\x1b[38;2;255;45;146m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Wordmark gradient anchors — cyan → purple → pink, left to right across the
/// whole wordmark, matching the brain logo's node gradient.
const GRAD_CYAN: (u8, u8, u8) = (34, 211, 238);
const GRAD_PURPLE: (u8, u8, u8) = (168, 85, 247);
const GRAD_PINK: (u8, u8, u8) = (255, 45, 146);

/// Linear-interpolate two RGB triples at `t` (0.0..=1.0).
fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// ANSI foreground escape for the wordmark gradient at horizontal position `t`
/// (0.0 = left edge .. 1.0 = right edge). Truecolor terminals get the
/// cyan→purple→pink logo gradient; everyone else falls back to a light→dark
/// gray ramp via xterm-256 grayscale (the muted "ONE"-style treatment, so the
/// wordmark still renders cleanly instead of raw 24-bit escapes).
fn gradient_fg(t: f32, truecolor: bool) -> String {
    if truecolor {
        let (r, g, b) = if t < 0.5 {
            lerp_rgb(GRAD_CYAN, GRAD_PURPLE, t * 2.0)
        } else {
            lerp_rgb(GRAD_PURPLE, GRAD_PINK, (t - 0.5) * 2.0)
        };
        format!("\x1b[38;2;{r};{g};{b}m")
    } else {
        // xterm-256 grayscale ramp: 252 (light) → 240 (mid-gray).
        let level = 252 - (t * 12.0).round() as u8;
        format!("\x1b[38;5;{level}m")
    }
}

/// Truecolor (24-bit) support — `COLORTERM` is the de-facto signal terminals
/// set (`truecolor` / `24bit`). Absent → gray fallback ramp.
fn supports_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

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

/// Unicode block-shaded rendering of the wordmark `ONEBRAIN`. Three lines
/// of pixel-art letters using full-block (`█`), half-block (`▀`), and
/// shaded-block (`░`) characters · 32 cols wide. Split into ONE / BRAIN
/// halves that `build_banner` joins into one row, then colors per-column by
/// horizontal position for a continuous gradient across the whole wordmark.
/// Each letter is 4 chars wide; ONE = 3 letters (cols 0-11) · BRAIN = 5
/// letters (cols 12-31).
const BANNER_ONE_ART: [&str; 3] = ["░█▀█░█▀█░█▀▀", "░█░█░█░█░█▀▀", " ▀▀▀ ▀ ▀ ▀▀▀"];

const BANNER_BRAIN_ART: [&str; 3] = [
    "░█▀▄░█▀▄░█▀█░▀█▀░█▀█",
    "░█▀▄░█▀▄░█▀█░░█░░█░█",
    " ▀▀  ▀ ▀ ▀ ▀ ▀▀▀ ▀ ▀",
];

/// Visual width (in monospace columns) of every rendered line. Used for
/// centering the tagline. 8 letters × 4 cols = 32.
const BANNER_VISUAL_WIDTH: usize = 32;

/// Build the banner string (no I/O). Five rendered content lines total:
///   3 × ASCII-art lines (custom block-shaded `OneBrain` wordmark) colored
///       with a continuous horizontal gradient — cyan → purple → pink in
///       truecolor (matching the logo), or a light→dark gray ramp as the
///       non-truecolor fallback (see [`gradient_fg`]).
///   1 × dim `Your AI Thinking Partner · vX.Y.Z` tagline, indented to centre
///       under the art block.
///   plus 1 leading + 1 trailing blank line for breathing room — so
///   `str::lines()` yields 6 elements total.
///
/// Each art line colors every column by its horizontal position (one escape
/// per glyph), then emits `ANSI_RESET` at line end. The tagline is wrapped in
/// `ANSI_DIM ... ANSI_RESET` so the terminal state is always clean after
/// the banner. The leading newline separates the banner from the previous
/// shell prompt so it doesn't crowd the user's command line; the trailing
/// newlines terminate the tagline and add a blank line between the banner
/// and whatever help body follows.
pub fn render_banner() -> String {
    build_banner(env!("CARGO_PKG_VERSION"), supports_truecolor())
}

/// Pure banner builder (no I/O, no env). `truecolor` picks the logo gradient
/// vs the gray fallback — split out so tests pin both color paths
/// deterministically.
fn build_banner(version: &str, truecolor: bool) -> String {
    let mut out = String::with_capacity(1024);
    // Breathing room above the banner so it doesn't visually butt up against
    // the prompt line that invoked it.
    out.push('\n');
    for i in 0..BANNER_ONE_ART.len() {
        // Join the ONE+BRAIN halves into one row, then color each column by its
        // horizontal position so the gradient flows continuously across the
        // whole wordmark (cyan → purple → pink), matching the logo. A per-column
        // escape overrides the previous fg, so one trailing reset per line is
        // enough.
        let row: String = format!("{}{}", BANNER_ONE_ART[i], BANNER_BRAIN_ART[i]);
        let last_col = row.chars().count().saturating_sub(1).max(1);
        for (col, ch) in row.chars().enumerate() {
            let t = col as f32 / last_col as f32;
            out.push_str(&gradient_fg(t, truecolor));
            out.push(ch);
        }
        out.push_str(ANSI_RESET);
        out.push('\n');
    }
    // Compute tagline indent dynamically so it stays centred under the art
    // block even when the version string grows (e.g., `v3.10.0` adds a char).
    // `chars().count()` is correct here because every char in the tagline is
    // a single column (ASCII + the U+00B7 separator which renders 1 col).
    let tagline_body = format!("Your AI Thinking Partner · v{version}");
    let tagline_indent = BANNER_VISUAL_WIDTH.saturating_sub(tagline_body.chars().count()) / 2;
    out.push_str(ANSI_DIM);
    for _ in 0..tagline_indent {
        out.push(' ');
    }
    out.push_str(&tagline_body);
    out.push_str(ANSI_RESET);
    out.push('\n');
    // Blank line between banner and the help body (clap's `Usage:` etc.).
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

/// True when argv signals a help intent — clap will print help output in any
/// of these cases:
///   1. Explicit `--help`, `-h`, or the `help` subcommand keyword
///   2. No subcommand at all (`onebrain` bare, or with only global flags) —
///      clap defaults to printing top-level help when the subcommand is
///      missing
///
/// Used by `main` to decide whether to emit the banner BEFORE handing argv
/// to clap (which prints help and exits in-process). The actual color/mode
/// gating still happens inside `should_show_banner_for_help` — this function
/// only answers "will clap print a help screen?"
pub fn argv_requests_help(args: &[String]) -> bool {
    // Skip the binary name (`args[0]`) so we don't accidentally match
    // `/path/to/help-runner/onebrain` or similar.
    let after_binary: Vec<&String> = args.iter().skip(1).collect();

    // (1) Explicit help keywords.
    if after_binary
        .iter()
        .any(|a| a.as_str() == "--help" || a.as_str() == "-h" || a.as_str() == "help")
    {
        return true;
    }

    // (2) No subcommand: walk argv looking for the first non-global-flag
    // token. If we never find one, clap will print top-level help.
    let mut iter = after_binary.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // Global value-flags — consume the next arg as their value.
            "--vault" | "-o" | "--output" => {
                let _ = iter.next();
            }
            // Global boolean flags — skip and continue scanning.
            "--json" | "--yaml" | "--pretty" | "--no-color" | "-q" | "--quiet" | "--version"
            | "-V" => {}
            // Anything else is a subcommand (or unknown · clap will error,
            // but that error message is itself a usage screen, not a help
            // screen, so we don't want the banner there).
            _ => return false,
        }
    }
    // Exhausted argv without finding a subcommand → help screen incoming.
    true
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

    /// Serialises tests that mutate the process-global `ONEBRAIN_FORCE_BANNER`
    /// env var, which would otherwise race under parallel test threads.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        // The wordmark is now FIGlet "chunky" ASCII art — the literal string
        // `OneBrain` no longer appears, but the art is recognisable via its
        // characteristic pixel-block underscores + pipes (the `_______`
        // top-line motif is unique to chunky among the fonts we'd ever swap to).
        assert!(s.contains("█"), "missing ASCII art glyphs: {s:?}");
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
    fn banner_renders_3_line_art_plus_tagline() {
        // 6 lines: 1 leading blank spacer + 3 art + 1 tagline + 1 trailing
        // blank spacer between banner and help body. `render_banner` starts
        // with a newline (breathing room above) and ends with two trailing
        // newlines, so `str::lines()` yields 6 elements (1 leading empty + 4
        // content + 1 trailing empty).
        let s = render_banner();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 6, "expected 6 banner lines, got {lines:?}");
        assert_eq!(
            lines[0], "",
            "expected blank leading spacer, got {:?}",
            lines[0]
        );
        assert_eq!(BANNER_ONE_ART.len(), 3, "ONE art block should be 3 lines");
        assert_eq!(
            BANNER_BRAIN_ART.len(),
            3,
            "BRAIN art block should be 3 lines"
        );
        // The wordmark is colored per-column (a gradient escape before each
        // glyph), so strip ANSI first, then assert the visible glyphs equal the
        // combined ONE+BRAIN row. Art lives on lines 1..=3 (after the blank).
        let strip_ansi = |s: &str| -> String {
            let mut out = String::new();
            let mut it = s.chars();
            while let Some(c) = it.next() {
                if c == '\x1b' {
                    for e in it.by_ref() {
                        if e == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        for i in 0..3 {
            let visible = strip_ansi(lines[i + 1]);
            let expected = format!("{}{}", BANNER_ONE_ART[i], BANNER_BRAIN_ART[i]);
            assert_eq!(
                visible,
                expected,
                "rendered line {} visible glyphs {:?} != combined art {:?}",
                i + 1,
                visible,
                expected
            );
        }
        // Tagline on line 5 (index 4).
        assert!(
            lines[4].contains("Your AI Thinking Partner"),
            "expected tagline on line 5, got {:?}",
            lines[4]
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
    fn banner_art_uses_logo_gradient_when_truecolor() {
        // Truecolor path: the cyan→purple→pink logo gradient. Cyan anchors the
        // left edge, pink (brand) the right. Dropping the wrapper collapses the
        // brand presence to monochrome.
        let s = build_banner(env!("CARGO_PKG_VERSION"), true);
        assert!(
            s.contains("\x1b[38;2;34;211;238m"),
            "missing cyan (gradient left edge): {s:?}"
        );
        assert!(
            s.contains(ANSI_PINK_FG),
            "missing pink (gradient right edge): {s:?}"
        );
        assert!(
            s.contains(ANSI_DIM),
            "missing dim escape for tagline: {s:?}"
        );
    }

    #[test]
    fn banner_art_falls_back_to_gray_without_truecolor() {
        // Non-truecolor terminals get a 256-color gray ramp — no 24-bit color.
        let s = build_banner(env!("CARGO_PKG_VERSION"), false);
        assert!(
            s.contains("\x1b[38;5;"),
            "missing 256-color gray ramp: {s:?}"
        );
        assert!(
            !s.contains(ANSI_PINK_FG),
            "fallback must not emit truecolor pink: {s:?}"
        );
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
        // Hold ENV_LOCK for the whole env-mutation window so this can't race
        // the sibling test that toggles the same var under parallel threads.
        let _guard = ENV_LOCK.lock().unwrap();
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
        // The ASCII art's chunky-font `_______` top-line motif is the
        // cheapest stable brand-presence check — the literal word `OneBrain`
        // no longer appears anywhere in the rendered banner.
        assert!(out.contains("█"), "missing ASCII art: {out:?}");
        assert!(
            out.contains("Your AI Thinking Partner"),
            "missing tagline: {out:?}"
        );
    }

    #[test]
    fn emit_help_banner_writes_nothing_when_version_present() {
        // Force the stderr-tty gate ON so the suppression we observe can
        // only come from the `--version` short-circuit. Hold ENV_LOCK for the
        // whole env-mutation window (see the sibling test) to avoid a race.
        let _guard = ENV_LOCK.lock().unwrap();
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
        let version = env!("CARGO_PKG_VERSION");
        // Pin the truecolor banner deterministically — independent of the test
        // host's COLORTERM. Gray-fallback rendering is covered by unit tests.
        let raw = build_banner(version, true);
        let normalised = raw.replace(&format!("v{version}"), "v<VERSION>");
        insta::assert_snapshot!("banner_text", normalised);
    }
}
