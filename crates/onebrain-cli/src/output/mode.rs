//! Output mode + TTY detection (skill-alignment §4.3 + v3.1.0 design §4).
//!
//! Six-rule TTY detection (v3.1 R1 fold-in):
//!   1. `atty` stdout — non-TTY ⇒ plain text
//!   2. `NO_COLOR` env var present — force monochrome
//!   3. `TERM=dumb` — force monochrome
//!   4. `CI=true` — force monochrome
//!   5. `--no-color` flag — force monochrome
//!   6. stdout piped (`!is_terminal`) — force monochrome AND plain text

use std::io::IsTerminal;

/// Resolved output mode for the current invocation. Each command renders
/// according to this; the dispatcher in [`super::dispatcher`] picks the
/// serializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputMode {
    /// Default — TTY-friendly text. `color` and `pretty` already reflect the
    /// resolved 6-rule decision; rendering code consumes them directly.
    Text { color: bool, pretty: bool },
    /// `--json` / `-o json`. Canonical envelope.
    Json { pretty: bool },
    /// `--yaml` / `-o yaml`. Same envelope shape, YAML serialised.
    Yaml,
    /// `--output table`. Aligned columns; falls back to text for non-list
    /// commands.
    Table,
    /// `--output tsv`. Header + tab-separated rows.
    Tsv,
}

impl OutputMode {
    /// True when the mode is structured (JSON/YAML/TSV/table). Used by
    /// `main.rs`'s error-path renderer (R1 B5) to decide between the
    /// human-readable `Error: <msg>` line on stderr and the canonical
    /// JSON envelope on stdout.
    pub fn is_structured(&self) -> bool {
        !matches!(self, OutputMode::Text { .. })
    }
}

/// Inputs needed to resolve the output mode. Centralised so the resolver is a
/// pure function (no implicit env / stdio access), which is necessary for
/// deterministic unit tests.
#[derive(Debug, Clone, Default)]
pub struct TtyInputs {
    /// `--output <fmt>` value from clap (`text|json|yaml|table|tsv`).
    pub output_flag: String,
    /// `--json` shorthand.
    pub json_shortcut: bool,
    /// `--yaml` shorthand.
    pub yaml_shortcut: bool,
    /// `--pretty` flag (force pretty even when piped).
    pub pretty: bool,
    /// `--no-color` flag.
    pub no_color: bool,
    /// stdout `IsTerminal` result.
    pub stdout_is_tty: bool,
    /// `NO_COLOR` env var present (any value).
    pub no_color_env: bool,
    /// `TERM` env var value.
    pub term_env: Option<String>,
    /// `CI` env var value. Any non-empty string is treated as truthy to
    /// match `is-ci` / `ci-info` and most CI providers' convention
    /// (`CI=true`, `CI=1`, `CI=yes`, `CI=on` all flip into CI mode).
    /// `CI=` (empty) or unset leaves CI mode off.
    pub ci_env: Option<String>,
}

impl TtyInputs {
    /// Snapshot the live process env + stdio. Use this at CLI entry; pure
    /// resolver below is used everywhere else.
    pub fn from_env(
        output_flag: &str,
        json_shortcut: bool,
        yaml_shortcut: bool,
        pretty: bool,
        no_color_flag: bool,
    ) -> Self {
        Self {
            output_flag: output_flag.to_string(),
            json_shortcut,
            yaml_shortcut,
            pretty,
            no_color: no_color_flag,
            stdout_is_tty: std::io::stdout().is_terminal(),
            no_color_env: std::env::var_os("NO_COLOR").is_some(),
            term_env: std::env::var("TERM").ok(),
            ci_env: std::env::var("CI").ok(),
        }
    }
}

/// Pure resolver — turns [`TtyInputs`] into an [`OutputMode`].
///
/// Format precedence: `--json` > `--yaml` > `--output <fmt>`. Clap's
/// `conflicts_with` enforces "at most one structured shorthand"; the
/// precedence here is only relevant for the unit tests that feed flags
/// directly without going through clap.
pub fn resolve_output_mode(i: &TtyInputs) -> OutputMode {
    if i.json_shortcut {
        return OutputMode::Json {
            pretty: i.pretty || i.stdout_is_tty,
        };
    }
    if i.yaml_shortcut {
        return OutputMode::Yaml;
    }
    match i.output_flag.as_str() {
        "json" => OutputMode::Json {
            pretty: i.pretty || i.stdout_is_tty,
        },
        "yaml" => OutputMode::Yaml,
        "table" => OutputMode::Table,
        "tsv" => OutputMode::Tsv,
        _ => {
            // text — apply the 6-rule chain. CI is "any non-empty value" per
            // is-ci / ci-info convention (R1 C4).
            let force_mono = i.no_color
                || i.no_color_env
                || i.term_env.as_deref() == Some("dumb")
                || matches!(i.ci_env.as_deref(), Some(s) if !s.is_empty())
                || !i.stdout_is_tty;
            OutputMode::Text {
                color: !force_mono && (i.stdout_is_tty || i.pretty),
                pretty: i.stdout_is_tty || i.pretty,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> TtyInputs {
        TtyInputs {
            output_flag: "text".into(),
            json_shortcut: false,
            yaml_shortcut: false,
            pretty: false,
            no_color: false,
            stdout_is_tty: true,
            no_color_env: false,
            term_env: Some("xterm-256color".into()),
            ci_env: None,
        }
    }

    #[test]
    fn tty_default_is_color_pretty_text() {
        let mode = resolve_output_mode(&base());
        assert_eq!(
            mode,
            OutputMode::Text {
                color: true,
                pretty: true
            }
        );
    }

    #[test]
    fn piped_stdout_drops_color_and_pretty() {
        let mut i = base();
        i.stdout_is_tty = false;
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: false,
                pretty: false
            }
        );
    }

    #[test]
    fn pretty_flag_forces_pretty_even_when_piped() {
        let mut i = base();
        i.stdout_is_tty = false;
        i.pretty = true;
        // pretty=true overrides the "piped" branch for the pretty bit, but
        // color stays off because !stdout_is_tty still satisfies force_mono.
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: false,
                pretty: true
            }
        );
    }

    #[test]
    fn no_color_flag_drops_color_but_keeps_pretty() {
        let mut i = base();
        i.no_color = true;
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: false,
                pretty: true
            }
        );
    }

    #[test]
    fn no_color_env_drops_color() {
        let mut i = base();
        i.no_color_env = true;
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: false,
                pretty: true
            }
        );
    }

    #[test]
    fn term_dumb_drops_color() {
        let mut i = base();
        i.term_env = Some("dumb".into());
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: false,
                pretty: true
            }
        );
    }

    #[test]
    fn ci_true_drops_color() {
        // R1 C4: any non-empty CI value is treated as truthy per the
        // is-ci / ci-info convention; empty / unset leaves CI mode off.
        for truthy in ["true", "1", "yes", "on"] {
            let mut i = base();
            i.ci_env = Some(truthy.into());
            assert_eq!(
                resolve_output_mode(&i),
                OutputMode::Text {
                    color: false,
                    pretty: true
                },
                "CI={truthy} should force monochrome",
            );
        }
    }

    #[test]
    fn ci_empty_or_unset_keeps_color() {
        // `CI=` (empty) is NOT truthy — common in shell `export CI=` to
        // clear the variable without unsetting it.
        let mut i = base();
        i.ci_env = Some("".into());
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: true,
                pretty: true
            }
        );
        // Unset (None) also leaves CI off.
        i.ci_env = None;
        assert_eq!(
            resolve_output_mode(&i),
            OutputMode::Text {
                color: true,
                pretty: true
            }
        );
    }

    #[test]
    fn json_shortcut_picks_json_mode() {
        let mut i = base();
        i.json_shortcut = true;
        assert_eq!(resolve_output_mode(&i), OutputMode::Json { pretty: true });
    }

    #[test]
    fn yaml_shortcut_picks_yaml_mode() {
        let mut i = base();
        i.yaml_shortcut = true;
        assert_eq!(resolve_output_mode(&i), OutputMode::Yaml);
    }

    #[test]
    fn output_flag_json_picks_json_mode() {
        let mut i = base();
        i.output_flag = "json".into();
        assert_eq!(resolve_output_mode(&i), OutputMode::Json { pretty: true });
    }

    #[test]
    fn output_flag_yaml_table_tsv() {
        for (flag, expected) in [
            ("yaml", OutputMode::Yaml),
            ("table", OutputMode::Table),
            ("tsv", OutputMode::Tsv),
        ] {
            let mut i = base();
            i.output_flag = flag.into();
            assert_eq!(resolve_output_mode(&i), expected, "flag={flag}");
        }
    }

    #[test]
    fn json_mode_is_compact_when_piped() {
        let mut i = base();
        i.json_shortcut = true;
        i.stdout_is_tty = false;
        assert_eq!(resolve_output_mode(&i), OutputMode::Json { pretty: false });
    }

    #[test]
    fn is_structured_matches_format() {
        assert!(!OutputMode::Text {
            color: true,
            pretty: true
        }
        .is_structured());
        assert!(OutputMode::Json { pretty: true }.is_structured());
        assert!(OutputMode::Yaml.is_structured());
        assert!(OutputMode::Table.is_structured());
        assert!(OutputMode::Tsv.is_structured());
    }
}
