//! Output dispatcher — picks the right serializer for an `Envelope<T>` given
//! the resolved [`OutputMode`].
//!
//! Commands build a typed envelope, hand it to [`emit`], and never call
//! `serde_json::to_string` directly. This keeps the JSON shape rules
//! (skipping `vault`/`error` when `None`, stable `warnings: []`) in one place.

use super::envelope::Envelope;
use super::mode::OutputMode;
use anyhow::Result;
use serde::Serialize;
use std::io::Write;

/// Emit `envelope` to `writer` in the format chosen by `mode`.
///
/// The `text_render` closure is only invoked in `OutputMode::Text` —
/// structured modes (`json`/`yaml`) ignore it entirely. This keeps text
/// rendering lazy so commands don't pay the formatting cost when piping to a
/// downstream parser.
///
/// v3.2.15: `Table` and `Tsv` variants were dropped from `OutputMode` — both
/// fell through to `serialize_json(value, false)` so the format flag silently
/// lied about its output. The arms that previously emitted a stub TSV header
/// or routed through the text renderer for `Table` are gone with the
/// variants.
pub fn emit<T, W, F>(
    envelope: &Envelope<T>,
    mode: &OutputMode,
    mut writer: W,
    text_render: F,
) -> Result<()>
where
    T: Serialize,
    W: Write,
    F: FnOnce(&Envelope<T>) -> String,
{
    match mode {
        OutputMode::Json { pretty } => {
            if *pretty {
                serde_json::to_writer_pretty(&mut writer, envelope)?;
            } else {
                serde_json::to_writer(&mut writer, envelope)?;
            }
            writeln!(writer)?;
        }
        OutputMode::Yaml => {
            let s = serde_yaml::to_string(envelope)?;
            writer.write_all(s.as_bytes())?;
        }
        OutputMode::Text { .. } => {
            let s = text_render(envelope);
            writer.write_all(s.as_bytes())?;
            if !s.ends_with('\n') {
                writeln!(writer)?;
            }
            // Surface soft warnings in human modes — structured modes already
            // carry them in the `warnings` array, but a text user would
            // otherwise never see e.g. "N notes were unreadable and skipped".
            for w in &envelope.warnings {
                writeln!(writer, "⚠️  {}", w.message)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::envelope::{Envelope, VaultInfo};
    use serde::Serialize;

    #[derive(Serialize)]
    struct P {
        n: u32,
    }

    fn vault() -> Option<VaultInfo> {
        Some(VaultInfo {
            name: "ob-1".into(),
            path: "/tmp/ob-1".into(),
        })
    }

    #[test]
    fn json_mode_emits_compact_envelope() {
        let env = Envelope::ok("task.list", vault(), P { n: 2 });
        let mut buf = Vec::new();
        emit(&env, &OutputMode::Json { pretty: false }, &mut buf, |_| {
            unreachable!("text renderer must not run in json mode")
        })
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Compact JSON ends with single newline.
        assert!(s.starts_with('{') && s.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["command"], "task.list");
        assert_eq!(v["data"]["n"], 2);
    }

    #[test]
    fn json_mode_pretty_emits_multiline() {
        let env = Envelope::ok("task.list", None, P { n: 1 });
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Json { pretty: true },
            &mut buf,
            |_| unreachable!(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('\n'));
        assert!(s.contains("  \"command\""));
    }

    #[test]
    fn yaml_mode_emits_envelope() {
        let env = Envelope::ok("task.list", None, P { n: 7 });
        let mut buf = Vec::new();
        emit(&env, &OutputMode::Yaml, &mut buf, |_| unreachable!()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("command: task.list"));
        assert!(s.contains("n: 7"));
    }

    #[test]
    fn text_mode_invokes_closure() {
        let env = Envelope::ok("task.list", None, P { n: 9 });
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Text {
                color: false,
                pretty: false,
            },
            &mut buf,
            |e| format!("hi {} ok={}", e.command, e.ok),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "hi task.list ok=true\n");
    }

    #[test]
    fn text_mode_does_not_double_newline() {
        let env = Envelope::ok("task.list", None, P { n: 9 });
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Text {
                color: false,
                pretty: false,
            },
            &mut buf,
            |_| "already-newlined\n".into(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "already-newlined\n");
    }

    #[test]
    fn text_mode_renders_warnings_after_body() {
        // Regression (round-2 fix): soft warnings must be visible to humans in
        // text mode, not only in the JSON `warnings` array.
        let env = Envelope::ok("note.search", None, P { n: 1 })
            .with_warning("W_NOTES_SKIPPED", "2 note(s) unreadable and skipped");
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Text {
                color: false,
                pretty: false,
            },
            &mut buf,
            |_| "result body".into(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("result body"));
        assert!(s.contains("2 note(s) unreadable and skipped"));
    }

    // `tsv_mode_emits_header_and_row` test removed in v3.2.15 along with the
    // `OutputMode::Tsv` variant — pre-3.2.15 every command routed Tsv (and
    // Table) through the JSON encoder, so the stub `command\tok` row this
    // test asserted was visible to no real consumer.

    // ── text mode edge cases ────────────────────────────────────────────

    #[test]
    fn text_mode_already_newlined_with_warning_no_double_newline() {
        // When text_render returns a string already ending with '\n', the
        // `if !s.ends_with('\n')` branch must NOT add another newline —
        // but soft warnings must still be appended on subsequent lines.
        let env = Envelope::ok("note.search", None, P { n: 3 })
            .with_warning("W_NOTES_SKIPPED", "1 note unreadable");
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Text {
                color: false,
                pretty: false,
            },
            &mut buf,
            |_| "result\n".into(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Body line is not doubled.
        assert!(s.starts_with("result\n"), "body must be preserved as-is");
        // Warning must still appear after the body.
        assert!(s.contains("1 note unreadable"), "warning must follow body");
        // No blank line between body and warning (no extra \n injected).
        assert!(
            !s.starts_with("result\n\n"),
            "must not double-newline the body"
        );
    }

    #[test]
    fn text_mode_multiple_warnings_all_rendered() {
        // The `for w in &envelope.warnings` loop must emit every warning,
        // not just the first. Two distinct warning messages must both appear.
        let env = Envelope::ok("note.search", vault(), P { n: 0 })
            .with_warning("W_A", "first warning message")
            .with_warning("W_B", "second warning message");
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Text {
                color: false,
                pretty: false,
            },
            &mut buf,
            |_| "body".into(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("first warning message"),
            "first warning must appear"
        );
        assert!(
            s.contains("second warning message"),
            "second warning must appear"
        );
    }

    // ── yaml mode with vault info ───────────────────────────────────────

    #[test]
    fn yaml_mode_with_vault_info_includes_vault_field() {
        // Existing yaml tests use vault=None. This confirms the vault sub-object
        // is serialised when present (distinct envelope shape).
        let env = Envelope::ok("session.init", vault(), P { n: 5 });
        let mut buf = Vec::new();
        emit(&env, &OutputMode::Yaml, &mut buf, |_| unreachable!()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("session.init"), "command must appear");
        assert!(
            s.contains("ob-1"),
            "vault name must appear in YAML when vault is Some"
        );
    }

    // ── json mode with warnings ─────────────────────────────────────────

    #[test]
    fn json_compact_with_warnings_includes_warnings_array() {
        // Soft warnings must be serialised into the JSON envelope's `warnings`
        // array so downstream parsers can inspect them.
        let env = Envelope::ok("task.list", None, P { n: 1 }).with_warning("W_X", "a soft warning");
        let mut buf = Vec::new();
        emit(
            &env,
            &OutputMode::Json { pretty: false },
            &mut buf,
            |_| unreachable!(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(
            v["warnings"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "warnings array must be non-empty in JSON output"
        );
    }
}
