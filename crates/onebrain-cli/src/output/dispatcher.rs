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
/// The `text_render` closure is only invoked in `OutputMode::Text` /
/// `OutputMode::Table` — structured modes (`json`/`yaml`/`tsv`) ignore it
/// entirely. This keeps text rendering lazy so commands don't pay the
/// formatting cost when piping to a downstream parser.
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
        OutputMode::Tsv => {
            // Generic TSV fallback: header `command\tok` + one row. Commands
            // with list payloads override by passing a structured text body
            // through the `text_render` closure when mode==Tsv if they want
            // a real columnar emit. v3.1 ships the fallback; v3.2+ widens
            // per-command.
            writeln!(writer, "command\tok")?;
            writeln!(writer, "{}\t{}", envelope.command, envelope.ok)?;
        }
        OutputMode::Table | OutputMode::Text { .. } => {
            let s = text_render(envelope);
            writer.write_all(s.as_bytes())?;
            if !s.ends_with('\n') {
                writeln!(writer)?;
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
    fn tsv_mode_emits_header_and_row() {
        let env = Envelope::ok("task.list", None, P { n: 9 });
        let mut buf = Vec::new();
        emit(&env, &OutputMode::Tsv, &mut buf, |_| unreachable!()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "command\tok\ntask.list\ttrue\n");
    }
}
