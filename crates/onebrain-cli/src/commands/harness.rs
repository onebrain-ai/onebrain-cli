use crate::legacy_output::serialize_for_mode;
use crate::output::OutputMode;
use anyhow::{Context, Result};
use onebrain_fs::detect_harnesses;
use serde::Serialize;
use std::env;

#[derive(Serialize)]
struct HarnessOutput {
    harnesses: Vec<String>,
}

/// Run the `harness` diagnostic · lists detected harnesses.
///
/// v3.1: text is the default. Machine consumers must pass `--json` (or
/// `--yaml` / `--output <fmt>`) explicitly. Uses `cwd` as the vault root
/// (no walk-up) · matches the contract for internal subcommands invoked
/// from vault root by the agent / SessionStart hook.
pub fn run(mode: &OutputMode) -> Result<()> {
    let cwd = env::current_dir().context("read current directory")?;
    let harnesses = detect_harnesses(&cwd);
    let output = HarnessOutput {
        harnesses: harnesses.iter().map(|h| h.as_str().to_string()).collect(),
    };
    println!("{}", format_output(&output, mode));
    Ok(())
}

fn format_output(output: &HarnessOutput, mode: &OutputMode) -> String {
    if let OutputMode::Text { .. } = mode {
        return render_text(output);
    }
    serialize_for_mode(output, mode)
}

fn render_text(output: &HarnessOutput) -> String {
    if output.harnesses.is_empty() {
        return "No agent harness detected".to_string();
    }
    format!("Detected harnesses: {}", output.harnesses.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_mode() -> OutputMode {
        OutputMode::Text {
            color: false,
            pretty: false,
        }
    }

    fn json_mode() -> OutputMode {
        OutputMode::Json { pretty: false }
    }

    #[test]
    fn json_mode_emits_serialized_shape() {
        let out = HarnessOutput {
            harnesses: vec!["claude".to_string()],
        };
        assert_eq!(
            format_output(&out, &json_mode()),
            r#"{"harnesses":["claude"]}"#
        );
    }

    #[test]
    fn text_mode_lists_detected_harnesses() {
        let out = HarnessOutput {
            harnesses: vec!["claude".to_string(), "direct".to_string()],
        };
        assert_eq!(
            format_output(&out, &text_mode()),
            "Detected harnesses: claude, direct"
        );
    }

    #[test]
    fn text_mode_empty_harness_list_says_none() {
        let out = HarnessOutput { harnesses: vec![] };
        assert_eq!(
            format_output(&out, &text_mode()),
            "No agent harness detected"
        );
    }

    #[test]
    fn text_mode_does_not_emit_json_braces() {
        let out = HarnessOutput {
            harnesses: vec!["claude".to_string()],
        };
        let rendered = format_output(&out, &text_mode());
        assert!(
            !rendered.starts_with('{'),
            "text mode must not emit JSON: {rendered}"
        );
    }
}
