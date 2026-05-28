//! Doctor's grouped-status renderer — a braille spinner + status rendering
//! layer. Today its only consumer is `doctor` (grouped sections), but the
//! surface is deliberately general (sections with optional headers, a static
//! seam) so a future CLI-layer linear consumer could adopt it without a
//! rewrite.
//!
//! A **step** is a labelled unit of work. While running it shows a spinner
//! frame; once resolved it renders a final status line:
//!
//! ```text
//! <glyph> <label>  <detail>
//!    └ <hint>            (optional, only for warn / fail)
//! ```
//!
//! Steps may be grouped under a **section** (a header + its steps). A section
//! with an empty header skips the header line, so a flat (headerless) run is
//! expressible too.
//!
//! ## TTY gating (the critical contract)
//!
//! Spinner animation (`\r` line redraw) + per-step stagger pacing happen
//! **only** when ALL of these hold:
//!   1. stdout is a real TTY, AND
//!   2. [`OutputMode`] is `Text { color: true, .. }`, AND
//!   3. not `--quiet`.
//!
//! In every other case — piped / non-TTY stdout, `--json` / `--yaml`,
//! `--no-color`, `--quiet` — there is NO spinner, NO pacing, NO
//! carriage-return redraw: the primitive just prints the final status lines
//! (plain glyphs; ANSI colour only when the resolved mode still carries
//! `color: true`, which it never does in any of those branches).
//!
//! The gating decision is made once by [`should_animate`] (a pure function
//! over the same inputs `banner.rs` uses) and frozen into the renderer.
//! Tests inject a writer + force `animate=false` so the static output is
//! deterministic without timing.

use crate::output::OutputMode;
use std::io::Write;
use std::time::Duration;

/// Braille spinner frames, cycled left→right. Matches the approved design.
pub(crate) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Per-step pacing band when animating (inclusive milliseconds). Each step
/// spins for a fresh random duration in this range so the reveal reads as
/// deliberate, watchable work rather than an instant dump. The delay is
/// purely cosmetic — every check has already run before rendering. Tests
/// override the delay to zero via [`ProgressRenderer::set_step_delay`] so
/// they exercise the animated branch without sleeping.
const STEP_DELAY_MIN_MS: u64 = 800;
const STEP_DELAY_MAX_MS: u64 = 2000;

/// Dwell time per spinner frame while a step is spinning. The braille frame
/// cycles every interval so the spinner visibly rotates for the whole step
/// delay; the number of frames shown is `step_delay / SPINNER_FRAME_MS`.
const SPINNER_FRAME_MS: u64 = 90;

/// A fresh pseudo-random per-step delay in
/// `[STEP_DELAY_MIN_MS, STEP_DELAY_MAX_MS]`. Uses wall-clock nanoseconds as a
/// cheap entropy source — a `rand` dependency would be overkill for a purely
/// cosmetic pacing jitter that has no correctness or security stake.
///
/// Shared with `update`, which pads its fast phases (fetch / warm cache) to
/// this same band so its braille spinner reads as deliberate work — one
/// pacing band across the two animated commands.
pub(crate) fn random_step_delay() -> Duration {
    let span = STEP_DELAY_MAX_MS - STEP_DELAY_MIN_MS + 1;
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % span;
    Duration::from_millis(STEP_DELAY_MIN_MS + jitter)
}

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

/// Whether `mode` is colour-bearing text (`Text { color: true, .. }`). The
/// single source of truth for the colour-text gate used by [`should_animate`],
/// [`ProgressRenderer::new`], and doctor's header/footer styling.
pub(crate) fn is_color_text(mode: &OutputMode) -> bool {
    matches!(mode, OutputMode::Text { color: true, .. })
}

/// Pure TTY-gating decision: should the spinner animate + pace?
///
/// `true` ⇔ stdout is a real TTY AND mode is colour-bearing text AND not
/// quiet. This mirrors the colour-text gate `banner.rs` uses (a banner only
/// shows for `Text { color: true }`), extended with the TTY + quiet conditions
/// the spinner needs.
pub fn should_animate(mode: &OutputMode, stdout_is_tty: bool, quiet: bool) -> bool {
    if quiet || !stdout_is_tty {
        return false;
    }
    is_color_text(mode)
}

/// Visible width (columns) of the horizontal rule framing command headers and
/// the doctor summary footer. Sized to span the widest framed line so the rule
/// always covers the text it frames rather than stopping short. Shared by
/// `doctor` (header + footer) and `update` (header) so the two stay one width.
pub const RULE_WIDTH: usize = 48;

/// The framing rule at an explicit width — `width` box-drawing dashes. `doctor`
/// widens it to span its longest line so the frame never stops short of the
/// text it encloses; `update` passes the default [`RULE_WIDTH`].
pub fn framing_rule_n(width: usize) -> String {
    "─".repeat(width)
}

/// Write a framed command header — a dim rule, ` <emoji>  <title>`, then a dim
/// rule, all `rule_width` columns wide. Shared by `doctor` and `update`.
///
/// Two spaces follow the emoji because brand glyphs like 🧠 render as a
/// double-width cell in many terminals (e.g. the Obsidian terminal), where a
/// single trailing space gets visually swallowed and the title butts against
/// the emoji. `color` toggles the dim rule styling (off for mono TTYs).
pub fn write_framed_header<W: Write>(
    w: &mut W,
    emoji: &str,
    title: &str,
    color: bool,
    rule_width: usize,
) -> std::io::Result<()> {
    let (dim, reset) = if color {
        ("\x1b[2m", "\x1b[0m")
    } else {
        ("", "")
    };
    let rule = framing_rule_n(rule_width);
    writeln!(w, "{dim}{rule}{reset}")?;
    writeln!(w, " {emoji}  {title}")?;
    writeln!(w, "{dim}{rule}{reset}")?;
    Ok(())
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
    /// Test seam for per-step pacing. `None` (production) → each animated step
    /// waits a fresh [`random_step_delay`]; `Some(d)` (tests) → fixed `d`, so
    /// the animated branch can be exercised with `Duration::ZERO` and no sleep.
    /// Only consulted when `animate` is true.
    step_delay_override: Option<Duration>,
}

impl ProgressRenderer<std::io::Stdout> {
    /// Production constructor — writes to stdout, computes `animate` from live
    /// stdout TTY state + `mode` + `quiet`. Doctor drives the gating decision
    /// through the pure [`should_animate`] / [`is_color_text`] helpers and
    /// passes a stdout handle to [`with_writer`], so this convenience
    /// constructor currently has no caller; it stays as the documented entry
    /// point for a future stdout-driven consumer.
    #[allow(dead_code)]
    pub fn new(mode: &OutputMode, quiet: bool) -> Self {
        use std::io::IsTerminal;
        let stdout_is_tty = std::io::stdout().is_terminal();
        let animate = should_animate(mode, stdout_is_tty, quiet);
        let color = is_color_text(mode);
        Self {
            writer: std::io::stdout(),
            animate,
            color,
            step_delay_override: None,
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
            step_delay_override: None,
        }
    }

    /// Pin the per-step pacing to a fixed duration. **Test seam only**: production
    /// callers leave the override unset so the renderer falls back to
    /// [`random_step_delay`] (800–2000ms jitter).
    ///
    /// Exposed `pub(crate)` (not `pub`) so the cross-module animated-path tests
    /// in `v31::dispatch::tests` can inject `Duration::ZERO` to exercise the
    /// animated branch without sleeping. v3.2.14 promoted it from
    /// `#[cfg(test)]` to crate-visible; v3.2.15 trimmed it back to
    /// `pub(crate)` after the production path confirmed it always passes
    /// `None`.
    pub(crate) fn set_step_delay(&mut self, delay: Duration) {
        self.step_delay_override = Some(delay);
    }

    /// Render a section header line: a blank spacer then the header. Skipped
    /// entirely when the header is empty (headerless / linear-run callers).
    fn section_header(&mut self, header: &str) -> std::io::Result<()> {
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
    fn step(&mut self, step: &Step) -> std::io::Result<()> {
        if self.animate {
            // Production: a fresh random pace per step. Tests pin a zero
            // override so this branch runs without actually sleeping.
            let delay = self.step_delay_override.unwrap_or_else(random_step_delay);
            self.spin(&step.label, delay)?;
            write!(self.writer, "\r\x1b[K")?; // carriage-return + clear EOL
        }
        self.write_resolved_step(step)
    }

    /// Paint the transient spinner line for `total`, cycling braille frames in
    /// place (each frame redrawn after a `\r`) so the spinner visibly rotates.
    /// Always paints at least one frame, so a zero-duration test seam still
    /// exercises the spinner glyph without sleeping. The frame count is
    /// `total / SPINNER_FRAME_MS` (clamped to ≥1); the dwell between frames is
    /// `SPINNER_FRAME_MS`, so the line spins for roughly `total` before the
    /// caller clears it and writes the resolved status.
    fn spin(&mut self, label: &str, total: Duration) -> std::io::Result<()> {
        let interval = Duration::from_millis(SPINNER_FRAME_MS);
        let frame_count = match total.as_millis().checked_div(interval.as_millis()) {
            Some(n) => (n as usize).max(1),
            None => 1, // interval is zero — paint a single frame
        };
        for frame in 0..frame_count {
            write!(
                self.writer,
                "\r  {} {}",
                SPINNER_FRAMES[frame % SPINNER_FRAMES.len()],
                label
            )?;
            self.writer.flush()?;
            // Dwell between frames only — the last frame hands straight off to
            // the resolved line, so a step never sleeps past its budget.
            if frame + 1 < frame_count {
                std::thread::sleep(interval);
            }
        }
        Ok(())
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
        // v3.2.15: Table / Tsv variants dropped from `OutputMode`. Json / Yaml
        // are the remaining structured targets.
        for mode in [
            OutputMode::Json { pretty: true },
            OutputMode::Json { pretty: false },
            OutputMode::Yaml,
        ] {
            assert!(
                !should_animate(&mode, true, false),
                "structured mode {mode:?} must not animate"
            );
        }
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

    // ── Shared framed header (doctor + update) ───────────────────────────

    #[test]
    fn framed_header_two_spaces_after_emoji_and_full_width_rules() {
        let mut buf = Vec::new();
        write_framed_header(&mut buf, "🚀", "OneBrain Update", false, RULE_WIDTH).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("🚀  OneBrain Update"),
            "two spaces after emoji: {out:?}"
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0].chars().count(), RULE_WIDTH, "top rule width");
        assert!(lines[0].chars().all(|c| c == '─'), "top rule dashes");
        assert_eq!(lines[2].chars().count(), RULE_WIDTH, "bottom rule width");
    }

    #[test]
    fn framed_header_color_wraps_rules_in_dim() {
        let mut buf = Vec::new();
        write_framed_header(&mut buf, "🧠", "OneBrain Doctor · ob-1", true, RULE_WIDTH).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[2m"), "dim rule: {out:?}");
        assert!(out.contains("\x1b[0m"), "reset: {out:?}");
    }

    // ── Animated path (exercises the spinner branch; one short sleep) ────

    #[test]
    fn random_step_delay_is_within_band() {
        // The cosmetic jitter must always land inside the inclusive
        // [MIN, MAX] band so a slow run never stalls indefinitely nor flashes
        // past. Sample enough times to catch an off-by-one in the modulo math.
        let lo = Duration::from_millis(STEP_DELAY_MIN_MS);
        let hi = Duration::from_millis(STEP_DELAY_MAX_MS);
        for _ in 0..1000 {
            let d = random_step_delay();
            assert!(d >= lo && d <= hi, "delay {d:?} outside [{lo:?}, {hi:?}]");
        }
    }

    #[test]
    fn animated_step_paints_spinner_then_clears_and_resolves() {
        // `force_static = false` puts the renderer on the animated path so the
        // spinner frame + `\r` clear + resolved line all run. Pin the per-step
        // delay to zero so the branch is exercised without sleeping for the
        // production random band.
        let mut r = ProgressRenderer::with_writer(Vec::new(), false, false);
        r.set_step_delay(Duration::ZERO);
        let step = Step::new("folders", StepStatus::Ok, Some("8/8 present".into()), None);
        r.step(&step).unwrap();
        let out = String::from_utf8(r.writer.clone()).unwrap();
        // Transient spinner frame painted.
        assert!(out.contains(SPINNER_FRAMES[0]), "spinner frame: {out:?}");
        // Cleared with carriage-return + clear-to-EOL.
        assert!(out.contains("\r\x1b[K"), "CR + clear-EOL: {out:?}");
        // Final resolved line lands after the clear.
        assert!(out.contains("✓ folders"), "resolved line: {out:?}");
    }

    #[test]
    fn animated_step_cycles_multiple_spinner_frames() {
        // A multi-interval delay must paint more than one distinct braille
        // frame — i.e. the spinner actually rotates instead of freezing on the
        // first frame. Three frame-intervals → frames 0,1,2 (~180 ms of real
        // sleep, the cost of covering the rotation branch).
        let mut r = ProgressRenderer::with_writer(Vec::new(), false, false);
        r.set_step_delay(Duration::from_millis(SPINNER_FRAME_MS * 3));
        let step = Step::new("folders", StepStatus::Ok, None, None);
        r.step(&step).unwrap();
        let out = String::from_utf8(r.writer.clone()).unwrap();
        let distinct = SPINNER_FRAMES.iter().filter(|f| out.contains(**f)).count();
        assert!(
            distinct >= 2,
            "spinner must cycle ≥2 distinct frames, got {distinct}: {out:?}"
        );
    }
}
