//! Reusable terminal-UX progress primitive — a braille spinner + status
//! rendering layer shared by `doctor` (grouped sections) and `update`
//! (linear steps).
//!
//! A **step** is a labelled unit of work. While running it shows a spinner
//! frame; once resolved it renders a final status line:
//!
//! ```text
//! <glyph> <label>  <detail>
//!    └ <hint>            (optional, only for warn / fail)
//! ```
//!
//! Steps may be grouped under a **section** (a header + its steps) — `doctor`
//! uses sections; `update` can ignore them and emit a flat run.
//!
//! ## TTY gating (the critical contract)
//!
//! Spinner animation (`\r` line redraw) + per-step stagger pacing happen
//! **only** when ALL of these hold:
//!   1. stdout is a real TTY, AND
//!   2. [`OutputMode`] is `Text { color: true, .. }`, AND
//!   3. not `--quiet`.
//!
//! In every other case — piped / non-TTY stdout, `--json` / `--yaml` /
//! `--tsv` / `--table`, `--no-color`, `--quiet` — there is NO spinner, NO
//! pacing, NO carriage-return redraw: the primitive just prints the final
//! status lines (plain glyphs; ANSI colour only when the resolved mode still
//! carries `color: true`, which it never does in any of those branches).
//!
//! The gating decision is made once by [`should_animate`] (a pure function
//! over the same inputs `banner.rs` uses) and frozen into the renderer.
//! Tests inject a writer + force `animate=false` so the static output is
//! deterministic without timing.

use crate::output::OutputMode;
use std::io::Write;
use std::time::Duration;

/// Braille spinner frames, cycled left→right. Matches the approved design.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Default per-step stagger when animating (milliseconds). Picked inside the
/// approved 60–110 ms band. Overridable via `ONEBRAIN_PROGRESS_STEP_MS`
/// (`0` disables pacing — instant reveal, handy for demos / impatient users;
/// tests use the force-static seam instead so they never sleep).
pub const DEFAULT_STEP_MS: u64 = 80;

/// Resolved status of a step. Glyph + colour treatment per the approved
/// design: passes are quiet (dim green), warnings/fails prominent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// `✓` — healthy. Dim/green, deliberately low-key.
    Ok,
    /// `⚠` — needs attention. Yellow, prominent.
    Warn,
    /// `✗` — broken. Red, prominent.
    Fail,
}

impl StepStatus {
    /// The bare glyph for this status (no colour).
    pub fn glyph(self) -> &'static str {
        match self {
            StepStatus::Ok => "✓",
            StepStatus::Warn => "⚠",
            StepStatus::Fail => "✗",
        }
    }

    /// ANSI SGR foreground/style prefix for this status. Empty string when
    /// `color` is false so callers can unconditionally wrap.
    pub fn ansi_prefix(self, color: bool) -> &'static str {
        if !color {
            return "";
        }
        match self {
            // Passes are quiet → dim. Warn → yellow. Fail → red.
            StepStatus::Ok => "\x1b[2;32m", // dim green
            StepStatus::Warn => "\x1b[33m", // yellow
            StepStatus::Fail => "\x1b[31m", // red
        }
    }
}

/// One unit of work: a label, its resolved status, an optional trailing
/// detail string, and an optional hint (rendered as the indented `└` line —
/// only shown for warn / fail per the design).
#[derive(Debug, Clone)]
pub struct Step {
    pub label: String,
    pub status: StepStatus,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl Step {
    /// Construct an already-resolved step.
    pub fn new(
        label: impl Into<String>,
        status: StepStatus,
        detail: Option<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            status,
            detail,
            hint,
        }
    }
}

/// A header + the steps rendered under it. `update` (linear steps) can pass a
/// section with an empty header to skip the header line.
#[derive(Debug, Clone)]
pub struct Section {
    pub header: String,
    pub steps: Vec<Step>,
}

impl Section {
    pub fn new(header: impl Into<String>, steps: Vec<Step>) -> Self {
        Self {
            header: header.into(),
            steps,
        }
    }
}

/// Pure TTY-gating decision: should the spinner animate + pace?
///
/// `true` ⇔ stdout is a real TTY AND mode is `Text { color: true, .. }` AND
/// not quiet. This mirrors the colour-text gate `banner.rs` uses (a banner
/// only shows for `Text { color: true }`), extended with the TTY + quiet
/// conditions the spinner needs.
pub fn should_animate(mode: &OutputMode, stdout_is_tty: bool, quiet: bool) -> bool {
    if quiet || !stdout_is_tty {
        return false;
    }
    matches!(mode, OutputMode::Text { color: true, .. })
}

/// Read the per-step stagger from `ONEBRAIN_PROGRESS_STEP_MS`, falling back to
/// [`DEFAULT_STEP_MS`]. `0` (or an unparseable value mapping to 0) disables
/// pacing. Only consulted on the animated path.
pub fn step_delay_ms() -> u64 {
    std::env::var("ONEBRAIN_PROGRESS_STEP_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(DEFAULT_STEP_MS)
}

/// Renders sections/steps to an injected writer.
///
/// Construct via [`ProgressRenderer::new`] (computes the gating decision from
/// live stdout + mode + quiet) or [`ProgressRenderer::with_writer`] (tests:
/// inject any `Write` + force `animate`). When `animate` is false the renderer
/// emits only the final static status lines — no spinner, no `\r`, no sleep.
pub struct ProgressRenderer<W: Write> {
    writer: W,
    /// Whether to animate the spinner + pace steps. Frozen at construction.
    animate: bool,
    /// Whether to emit ANSI colour. Independent of `animate` so a colour
    /// pipe-forced-pretty edge case still colours static output.
    color: bool,
    /// Per-step pacing. Only consulted when `animate` is true.
    step_delay: Duration,
}

impl ProgressRenderer<std::io::Stdout> {
    /// Production constructor — writes to stdout, computes `animate` from live
    /// stdout TTY state + `mode` + `quiet`.
    pub fn new(mode: &OutputMode, quiet: bool) -> Self {
        use std::io::IsTerminal;
        let stdout_is_tty = std::io::stdout().is_terminal();
        let animate = should_animate(mode, stdout_is_tty, quiet);
        let color = matches!(mode, OutputMode::Text { color: true, .. });
        Self {
            writer: std::io::stdout(),
            animate,
            color,
            step_delay: Duration::from_millis(step_delay_ms()),
        }
    }
}

impl<W: Write> ProgressRenderer<W> {
    /// Test / embedding constructor — inject a writer and force the gating
    /// decision. `force_static = true` guarantees no spinner / no sleep / no
    /// `\r` regardless of the other inputs, so tests assert deterministic
    /// output.
    pub fn with_writer(writer: W, force_static: bool, color: bool) -> Self {
        Self {
            writer,
            animate: !force_static,
            color,
            step_delay: Duration::from_millis(step_delay_ms()),
        }
    }

    /// True when this renderer will animate (spinner + pacing).
    pub fn is_animated(&self) -> bool {
        self.animate
    }

    /// Render a section header line: a blank spacer then the header. Skipped
    /// entirely when the header is empty (linear-run callers).
    pub fn section_header(&mut self, header: &str) -> std::io::Result<()> {
        if header.is_empty() {
            return Ok(());
        }
        writeln!(self.writer)?;
        if self.color {
            // Bold header.
            writeln!(self.writer, " \x1b[1m{header}\x1b[0m")?;
        } else {
            writeln!(self.writer, " {header}")?;
        }
        Ok(())
    }

    /// Render one fully-resolved step. On the animated path, first paint a
    /// transient spinner line, pace, then clear it with `\r` + clear-to-EOL
    /// and write the final status line. On the static path, write only the
    /// final status line.
    pub fn step(&mut self, step: &Step) -> std::io::Result<()> {
        if self.animate {
            // Transient spinner line — first frame is enough for the brief
            // stagger window; the line is cleared before the result lands.
            write!(self.writer, "  {} {}", SPINNER_FRAMES[0], step.label)?;
            self.writer.flush()?;
            if !self.step_delay.is_zero() {
                std::thread::sleep(self.step_delay);
            }
            write!(self.writer, "\r\x1b[K")?; // carriage-return + clear EOL
        }
        self.write_resolved_step(step)
    }

    /// Static status line(s) for a resolved step. Shared by both paths.
    fn write_resolved_step(&mut self, step: &Step) -> std::io::Result<()> {
        let prefix = step.status.ansi_prefix(self.color);
        let reset = if self.color { "\x1b[0m" } else { "" };
        let glyph = step.status.glyph();
        // `<glyph> <label>  <detail>` — label padded so details align.
        let detail = step.detail.as_deref().unwrap_or("");
        if detail.is_empty() {
            writeln!(
                self.writer,
                "  {prefix}{glyph}{reset} {label}",
                label = step.label
            )?;
        } else {
            writeln!(
                self.writer,
                "  {prefix}{glyph}{reset} {label:<18} {detail}",
                label = step.label,
            )?;
        }
        // Hint → indented `└` line. Only shown for warn / fail (the design
        // keeps passes quiet); enforced by the caller passing `hint: None`
        // for OK steps, but we also guard here for safety.
        if step.status != StepStatus::Ok {
            if let Some(hint) = &step.hint {
                if self.color {
                    writeln!(self.writer, "     \x1b[2m└ {hint}\x1b[0m")?;
                } else {
                    writeln!(self.writer, "     └ {hint}")?;
                }
            }
        }
        Ok(())
    }

    /// Convenience: render a whole section (header + each step).
    pub fn render_section(&mut self, section: &Section) -> std::io::Result<()> {
        self.section_header(&section.header)?;
        for step in &section.steps {
            self.step(step)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color_tty_mode() -> OutputMode {
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

    // ── Spinner frame cycle ──────────────────────────────────────────────

    #[test]
    fn spinner_frames_are_the_braille_cycle() {
        assert_eq!(
            SPINNER_FRAMES,
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        );
        // 10-frame cycle, all distinct.
        let mut seen = std::collections::HashSet::new();
        for f in SPINNER_FRAMES {
            assert!(seen.insert(f), "duplicate frame {f}");
        }
        assert_eq!(seen.len(), 10);
    }

    #[test]
    fn spinner_cycles_by_modulo() {
        // The frame for index N is SPINNER_FRAMES[N % 10] — the documented
        // cycling contract callers (update's linear spinner) rely on.
        for i in 0..25usize {
            let expected = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
            assert_eq!(SPINNER_FRAMES[i % SPINNER_FRAMES.len()], expected);
        }
        assert_eq!(SPINNER_FRAMES[10 % 10], SPINNER_FRAMES[0]);
        assert_eq!(SPINNER_FRAMES[11 % 10], SPINNER_FRAMES[1]);
    }

    // ── TTY-gating decision ──────────────────────────────────────────────

    #[test]
    fn animate_only_for_color_tty_non_quiet() {
        // The one true case.
        assert!(should_animate(&color_tty_mode(), true, false));
    }

    #[test]
    fn no_animate_when_quiet() {
        assert!(!should_animate(&color_tty_mode(), true, true));
    }

    #[test]
    fn no_animate_when_not_tty() {
        assert!(!should_animate(&color_tty_mode(), false, false));
    }

    #[test]
    fn no_animate_for_mono_text() {
        // --no-color / NO_COLOR / CI / TERM=dumb / piped all collapse to
        // Text { color: false, .. }.
        assert!(!should_animate(&mono_text_mode(), true, false));
    }

    #[test]
    fn no_animate_for_structured_modes() {
        for mode in [
            OutputMode::Json { pretty: true },
            OutputMode::Json { pretty: false },
            OutputMode::Yaml,
            OutputMode::Table,
            OutputMode::Tsv,
        ] {
            assert!(
                !should_animate(&mode, true, false),
                "structured mode {mode:?} must not animate"
            );
        }
    }

    #[test]
    fn force_static_renderer_reports_not_animated() {
        let r = ProgressRenderer::with_writer(Vec::new(), true, false);
        assert!(!r.is_animated());
    }

    #[test]
    fn non_static_renderer_reports_animated() {
        let r = ProgressRenderer::with_writer(Vec::new(), false, true);
        assert!(r.is_animated());
    }

    // ── Static rendering (deterministic, no timing) ──────────────────────

    fn render_static(section: &Section, color: bool) -> String {
        let mut r = ProgressRenderer::with_writer(Vec::new(), true, color);
        r.render_section(section).unwrap();
        String::from_utf8(r.writer.clone()).unwrap()
    }

    #[test]
    fn static_ok_step_has_check_glyph_label_detail_no_hint() {
        let section = Section::new(
            "Config",
            vec![Step::new(
                "onebrain.yml",
                StepStatus::Ok,
                Some("valid · stable".into()),
                Some("should-not-show".into()),
            )],
        );
        let out = render_static(&section, false);
        assert!(out.contains("Config"), "header missing: {out:?}");
        assert!(out.contains("✓ onebrain.yml"), "glyph+label: {out:?}");
        assert!(out.contains("valid · stable"), "detail: {out:?}");
        // Passes stay quiet: even if a hint is set, OK steps never print the
        // `└` line.
        assert!(
            !out.contains("└"),
            "OK step must not show hint line: {out:?}"
        );
    }

    #[test]
    fn static_warn_step_shows_indented_hint_line() {
        let section = Section::new(
            "Integration",
            vec![Step::new(
                "hooks",
                StepStatus::Warn,
                Some("PostToolUse (qmd) duplicated (×2)".into()),
                Some("onebrain doctor --fix".into()),
            )],
        );
        let out = render_static(&section, false);
        assert!(out.contains("⚠ hooks"), "warn glyph+label: {out:?}");
        assert!(
            out.contains("PostToolUse (qmd) duplicated (×2)"),
            "warn detail: {out:?}"
        );
        // Hint rendered as the indented `└` line.
        assert!(
            out.contains("└ onebrain doctor --fix"),
            "warn hint line: {out:?}"
        );
    }

    #[test]
    fn static_fail_step_uses_cross_glyph_and_hint() {
        let section = Section::new(
            "Vault structure",
            vec![Step::new(
                "folders",
                StepStatus::Fail,
                Some("0/8 present".into()),
                Some("onebrain init --force".into()),
            )],
        );
        let out = render_static(&section, false);
        assert!(out.contains("✗ folders"), "fail glyph+label: {out:?}");
        assert!(
            out.contains("└ onebrain init --force"),
            "fail hint: {out:?}"
        );
    }

    #[test]
    fn static_render_emits_no_spinner_or_carriage_return() {
        // The force-static path must never paint a spinner frame or a `\r`.
        let section = Section::new(
            "Config",
            vec![Step::new(
                "onebrain.yml",
                StepStatus::Ok,
                Some("valid".into()),
                None,
            )],
        );
        let out = render_static(&section, true);
        assert!(!out.contains('\r'), "static must not redraw: {out:?}");
        for f in SPINNER_FRAMES {
            assert!(
                !out.contains(f),
                "static must not paint spinner {f}: {out:?}"
            );
        }
    }

    #[test]
    fn color_static_wraps_glyph_in_ansi_and_resets() {
        let section = Section::new(
            "Config",
            vec![Step::new(
                "onebrain.yml",
                StepStatus::Ok,
                Some("valid".into()),
                None,
            )],
        );
        let out = render_static(&section, true);
        // Dim-green prefix for OK + reset present.
        assert!(out.contains("\x1b[2;32m"), "dim-green prefix: {out:?}");
        assert!(out.contains("\x1b[0m"), "ansi reset: {out:?}");
    }

    #[test]
    fn no_color_static_has_no_ansi_escapes() {
        let section = Section::new(
            "Config",
            vec![
                Step::new("onebrain.yml", StepStatus::Ok, Some("valid".into()), None),
                Step::new(
                    "hooks",
                    StepStatus::Warn,
                    Some("dup".into()),
                    Some("fix it".into()),
                ),
            ],
        );
        let out = render_static(&section, false);
        assert!(!out.contains('\x1b'), "no ANSI in mono mode: {out:?}");
    }

    #[test]
    fn empty_header_section_skips_header_line() {
        // Linear-run callers (update) pass an empty header to suppress it.
        let section = Section::new("", vec![Step::new("step one", StepStatus::Ok, None, None)]);
        let out = render_static(&section, false);
        assert!(out.contains("✓ step one"), "step rendered: {out:?}");
        // No leading header / spacer beyond the step itself: first non-empty
        // line is the step.
        let first = out.lines().find(|l| !l.trim().is_empty()).unwrap();
        assert!(
            first.contains("step one"),
            "first line is the step: {out:?}"
        );
    }

    #[test]
    fn step_status_glyphs_are_stable() {
        assert_eq!(StepStatus::Ok.glyph(), "✓");
        assert_eq!(StepStatus::Warn.glyph(), "⚠");
        assert_eq!(StepStatus::Fail.glyph(), "✗");
    }

    // ── Animated path (no real timing — pacing disabled via env) ─────────

    #[test]
    fn animated_step_paints_spinner_then_clears_and_resolves() {
        // Disable the sleep so the test never blocks; the animation branch
        // (spinner frame + `\r` clear + resolved line) still runs.
        std::env::set_var("ONEBRAIN_PROGRESS_STEP_MS", "0");
        let mut r = ProgressRenderer::with_writer(Vec::new(), false, false);
        assert!(r.is_animated(), "renderer should be on the animated path");
        let step = Step::new("folders", StepStatus::Ok, Some("8/8 present".into()), None);
        r.step(&step).unwrap();
        std::env::remove_var("ONEBRAIN_PROGRESS_STEP_MS");
        let out = String::from_utf8(r.writer.clone()).unwrap();
        // Transient spinner frame painted.
        assert!(out.contains(SPINNER_FRAMES[0]), "spinner frame: {out:?}");
        // Cleared with carriage-return + clear-to-EOL.
        assert!(out.contains("\r\x1b[K"), "CR + clear-EOL: {out:?}");
        // Final resolved line lands after the clear.
        assert!(out.contains("✓ folders"), "resolved line: {out:?}");
    }
}
