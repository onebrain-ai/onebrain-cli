//! Converge CLI-managed Claude lifecycle hooks on the shared runner.
//!
//! Called by `onebrain plugin update`. Existing Stop and PostToolUse entries
//! are migrated to one `{ "type": "command", "command": "onebrain",
//! "args": ["hook"] }` runner per event. Historical direct
//! checkpoint/pending and qmd/search forms are collapsed, foreign entries and
//! unrelated fields are preserved, and stale managed lifecycle entries under
//! unsupported events are removed. The migration never creates a missing
//! event: only registration knows whether PostToolUse is enabled.

use serde_json::{Map, Value};

/// The only Claude events that the CLI registration manages.
const POST_TOOL_USE: &str = "PostToolUse";
const STOP: &str = "Stop";

/// Result of a rewrite pass over a settings.json document.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RewriteReport {
    /// Total entries rewritten, deduplicated, or removed.
    pub total: u32,
    /// Entries rewritten or deduplicated to the shared lifecycle runner.
    pub converged: u32,
    /// Managed lifecycle entries removed from unsupported Claude events.
    pub stale_entries_removed: u32,
    /// Soft warnings for malformed hook entries (non-string `command` /
    /// non-array `args` / etc.). Each entry is preserved as-is; the
    /// rewriter just skips it AND tells the caller. Empty when the
    /// settings.json is well-formed.
    pub warnings: Vec<RewriteWarning>,
}

/// Per-entry soft warning emitted by the rewriter when a hook entry has
/// an unexpected shape. The `code` field uses the canonical `W_*` prefix
/// from skill-alignment §4.5 so the envelope's `warnings[]` array stays
/// machine-readable.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct RewriteWarning {
    pub code: String,
    pub message: String,
}

impl RewriteReport {
    fn warn(&mut self, code: &str, message: impl Into<String>) {
        self.warnings.push(RewriteWarning {
            code: code.to_string(),
            message: message.into(),
        });
    }

    fn record_converged(&mut self) {
        self.total += 1;
        self.converged += 1;
    }

    fn record_stale_removal(&mut self) {
        self.total += 1;
        self.stale_entries_removed += 1;
    }
}

/// Friendly type name for use in warning messages.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Converge CLI-managed Claude lifecycle hooks on the one shared runner.
///
/// Plugin update cannot know whether a vault has search configured, so it
/// never creates a missing allowed event. It only rewrites registrations that
/// are already present and removes recognized stale lifecycle entries from
/// unsupported events.
pub fn rewrite_hooks(settings: &mut Value) -> RewriteReport {
    let mut report = RewriteReport::default();
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return report;
    };

    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        match event.as_str() {
            STOP | POST_TOOL_USE => converge_lifecycle_event(hooks, &event, &mut report),
            _ => remove_stale_lifecycle_entries(hooks, &event, &mut report),
        }
    }
    report
}

fn converge_lifecycle_event(
    hooks: &mut Map<String, Value>,
    event: &str,
    report: &mut RewriteReport,
) {
    let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
        return;
    };

    let mut retained = false;
    let mut removed = false;
    for group in groups.iter_mut() {
        let mut retained_in_group = false;
        let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain_mut(|entry| {
            warn_malformed_lifecycle_entry(entry, report);
            if !is_managed_lifecycle_entry_for_event(entry, event) {
                return true;
            }
            if retained {
                report.record_converged();
                removed = true;
                return false;
            }
            retained = true;
            retained_in_group = true;
            if !is_lifecycle_runner(entry) {
                write_lifecycle_runner(entry);
                report.record_converged();
            }
            true
        });
        if event == POST_TOOL_USE
            && retained_in_group
            && group.get("matcher").and_then(Value::as_str) != Some("Write|Edit")
        {
            group
                .as_object_mut()
                .expect("hook group with hooks is an object")
                .insert(
                    "matcher".to_string(),
                    Value::String("Write|Edit".to_string()),
                );
            report.record_converged();
        }
    }
    if removed {
        remove_empty_hook_groups(groups);
    }
}

fn remove_stale_lifecycle_entries(
    hooks: &mut Map<String, Value>,
    event: &str,
    report: &mut RewriteReport,
) {
    let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
        return;
    };

    let mut removed = false;
    for group in groups.iter_mut() {
        let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain(|entry| {
            warn_malformed_lifecycle_entry(entry, report);
            if is_any_managed_lifecycle_entry(entry) {
                report.record_stale_removal();
                removed = true;
                false
            } else {
                true
            }
        });
    }
    if removed {
        remove_empty_hook_groups(groups);
        if groups.is_empty() {
            hooks.remove(event);
        }
    }
}

fn remove_empty_hook_groups(groups: &mut Vec<Value>) {
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    });
}

fn is_lifecycle_runner(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("command")
        && entry.get("command").and_then(Value::as_str) == Some("onebrain")
        && entry
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| args.as_slice() == [serde_json::json!("hook")])
}

fn write_lifecycle_runner(entry: &mut Value) {
    let Some(entry) = entry.as_object_mut() else {
        return;
    };
    entry.insert("type".to_string(), Value::String("command".to_string()));
    entry.insert("command".to_string(), Value::String("onebrain".to_string()));
    entry.insert("args".to_string(), serde_json::json!(["hook"]));
}

fn lifecycle_tokens(entry: &Value) -> Option<Vec<&str>> {
    let command = entry.get("command")?.as_str()?;
    if command == "onebrain" {
        let args = entry.get("args")?.as_array()?;
        let mut tokens = Vec::with_capacity(args.len() + 1);
        tokens.push(command);
        for arg in args {
            tokens.push(arg.as_str()?);
        }
        Some(tokens)
    } else {
        let tokens: Vec<&str> = command.split_ascii_whitespace().collect();
        (tokens.first() == Some(&"onebrain")).then_some(tokens)
    }
}

fn lifecycle_starts_with(tokens: &[&str], expected: &[&str]) -> bool {
    tokens.len() >= expected.len() && tokens.iter().zip(expected).all(|(got, want)| got == want)
}

fn is_managed_lifecycle_entry_for_event(entry: &Value, event: &str) -> bool {
    let Some(tokens) = lifecycle_tokens(entry) else {
        return false;
    };
    if tokens == ["onebrain", "hook"] {
        return true;
    }
    match event {
        STOP => {
            lifecycle_starts_with(&tokens, &["onebrain", "checkpoint", "stop"])
                || (lifecycle_starts_with(&tokens, &["onebrain", "search", "reindex"])
                    && tokens.contains(&"--pending-only"))
        }
        POST_TOOL_USE => {
            tokens == ["onebrain", "qmd-reindex"]
                || lifecycle_starts_with(&tokens, &["onebrain", "qmd", "reindex"])
                || lifecycle_starts_with(&tokens, &["onebrain", "search", "reindex"])
        }
        _ => false,
    }
}

fn is_any_managed_lifecycle_entry(entry: &Value) -> bool {
    let Some(tokens) = lifecycle_tokens(entry) else {
        return false;
    };
    tokens == ["onebrain", "hook"]
        || tokens == ["onebrain", "session-init"]
        || lifecycle_starts_with(&tokens, &["onebrain", "session", "init"])
        || lifecycle_starts_with(&tokens, &["onebrain", "orphan-scan"])
        || lifecycle_starts_with(&tokens, &["onebrain", "checkpoint", "orphans"])
        || lifecycle_starts_with(&tokens, &["onebrain", "checkpoint", "stop"])
        || tokens == ["onebrain", "qmd-reindex"]
        || lifecycle_starts_with(&tokens, &["onebrain", "qmd", "reindex"])
        || lifecycle_starts_with(&tokens, &["onebrain", "search", "reindex"])
}

fn warn_malformed_lifecycle_entry(entry: &Value, report: &mut RewriteReport) {
    let Some(command) = entry.get("command") else {
        return;
    };
    if !command.is_string() {
        report.warn(
            "W_MALFORMED_HOOK_ENTRY",
            format!(
                "hook entry has non-string `command` field (got {}); skipped",
                json_type_name(command)
            ),
        );
    } else if command.as_str() == Some("onebrain")
        && entry.get("args").is_some_and(|args| !args.is_array())
    {
        report.warn(
            "W_MALFORMED_HOOK_ENTRY",
            format!(
                "hook entry has non-array `args` field (got {}); skipped",
                json_type_name(entry.get("args").expect("checked above"))
            ),
        );
    }
}

/// Convenience: load `settings.json`, rewrite in place, write back.
/// Idempotent — running again is a no-op (returns a report with total = 0).
///
/// `dry_run = true` skips the write but still reports what would change.
pub fn rewrite_settings_file(
    path: &std::path::Path,
    dry_run: bool,
) -> anyhow::Result<RewriteReport> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No settings.json at all — no hooks to rewrite.
            return Ok(RewriteReport::default());
        }
        Err(e) => return Err(e.into()),
    };
    let mut settings: Value = if body.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(&body)?
    };
    let report = rewrite_hooks(&mut settings);
    if !dry_run && report.total > 0 {
        let serialized = serde_json::to_string_pretty(&settings)?;
        std::fs::write(path, format!("{serialized}\n"))?;
    }
    Ok(report)
}

#[cfg(test)]
mod lifecycle_runner_tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn converges_cli_managed_lifecycle_entries_without_touching_foreign_hooks() {
        let mut settings = json!({
            "theme": "dark",
            "hooks": {
                "Stop": [
                    {"matcher": "", "group_note": "keep", "hooks": [
                        {"type": "command", "command": "onebrain", "args": ["checkpoint", "stop", "--json"], "entry_note": "keep-me"},
                        {"type": "command", "command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]},
                        {"type": "command", "command": "notify-send", "args": ["done"]}
                    ]},
                    {"matcher": "", "hooks": [{"type": "command", "command": "onebrain", "args": ["hook"]}]}
                ],
                "PostToolUse": [
                    {"matcher": "Write|Edit", "group_note": "keep", "hooks": [
                        {"type": "command", "command": "onebrain", "args": ["qmd-reindex"], "entry_note": "keep-me"},
                        {"type": "command", "command": "onebrain", "args": ["search", "reindex", "--lex-only", "--json"]},
                        {"type": "command", "command": "my-index-observer", "args": []}
                    ]},
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "onebrain", "args": ["hook"]}]}
                ],
                "SessionStart": [{"hooks": [
                    {"type": "command", "command": "onebrain", "args": ["session-init"]},
                    {"type": "command", "command": "welcome-script", "args": []}
                ]}]
            }
        });

        let report = rewrite_hooks(&mut settings);

        for event in [STOP, POST_TOOL_USE] {
            let managed: Vec<&Value> = settings["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|group| group["hooks"].as_array())
                .flatten()
                .filter(|entry| entry["command"] == "onebrain")
                .collect();
            assert_eq!(managed.len(), 1, "{event}: {managed:?}");
            assert_eq!(managed[0]["type"], "command");
            assert_eq!(managed[0]["args"], json!(["hook"]));
        }
        assert_eq!(settings["hooks"]["Stop"][0]["group_note"], "keep");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["entry_note"],
            "keep-me"
        );
        assert_eq!(settings["hooks"]["PostToolUse"][0]["group_note"], "keep");
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"][0]["entry_note"],
            "keep-me"
        );
        assert_eq!(settings["theme"], "dark");
        assert!(settings["hooks"]["Stop"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["command"] == "notify-send"));
        assert!(settings["hooks"]["PostToolUse"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["command"] == "my-index-observer"));
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"],
            json!([{"type": "command", "command": "welcome-script", "args": []}])
        );
        assert_eq!(report.converged, 6);
        assert_eq!(report.stale_entries_removed, 1);

        let first_pass = settings.clone();
        assert_eq!(rewrite_hooks(&mut settings).total, 0);
        assert_eq!(settings, first_pass);
    }

    #[test]
    fn converges_shell_forms_and_does_not_create_missing_allowed_events() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{"hooks": [
                    {"command": "onebrain checkpoint stop --json", "entry_note": "keep-me"},
                    {"command": "onebrain search reindex --pending-only --json"},
                    {"command": "echo onebrain checkpoint stop"}
                ]}],
                "SessionStart": [{"hooks": [
                    {"type": "command", "command": "onebrain", "args": ["hook"]},
                    {"type": "command", "command": "welcome-script", "args": []}
                ]}]
            }
        });

        let report = rewrite_hooks(&mut settings);

        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"],
            json!([
                {"type": "command", "command": "onebrain", "args": ["hook"], "entry_note": "keep-me"},
                {"command": "echo onebrain checkpoint stop"}
            ])
        );
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"],
            json!([{"type": "command", "command": "welcome-script", "args": []}])
        );
        assert!(settings["hooks"].get(POST_TOOL_USE).is_none());
        assert_eq!(report.converged, 2);
        assert_eq!(report.stale_entries_removed, 1);
    }

    #[test]
    fn normalizes_post_tool_use_matcher_and_preserves_group_fields_idempotently() {
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Read",
                    "group_note": "keep-me",
                    "hooks": [
                        {"type": "command", "command": "onebrain", "args": ["hook"]},
                        {"type": "command", "command": "foreign-indexer", "args": []}
                    ]
                }]
            }
        });

        let first = rewrite_hooks(&mut settings);

        assert_eq!(first.converged, 1);
        assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
        assert_eq!(settings["hooks"]["PostToolUse"][0]["group_note"], "keep-me");
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let after_first = settings.clone();
        assert_eq!(rewrite_hooks(&mut settings).total, 0);
        assert_eq!(settings, after_first);
    }

    #[test]
    fn preserves_malformed_entries_and_reports_them() {
        let mut settings = json!({"hooks": {"Stop": [{"hooks": [
            {"command": ["onebrain", "checkpoint", "stop"]},
            {"command": "onebrain", "args": "checkpoint stop"}
        ]}]}});
        let before = settings.clone();

        let report = rewrite_hooks(&mut settings);

        assert_eq!(settings, before);
        assert_eq!(report.total, 0);
        assert_eq!(report.warnings.len(), 2);
    }

    #[test]
    fn rewrite_settings_file_respects_dry_run_and_is_idempotent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let original = serde_json::to_string_pretty(&json!({"hooks": {"Stop": [{"hooks": [
            {"command": "onebrain checkpoint stop"},
            {"command": "onebrain search reindex --pending-only"}
        ]}]}}))
        .unwrap();
        std::fs::write(&path, &original).unwrap();

        assert_eq!(rewrite_settings_file(&path, true).unwrap().converged, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(rewrite_settings_file(&path, false).unwrap().converged, 2);
        let after_first_pass = std::fs::read_to_string(&path).unwrap();
        assert_eq!(rewrite_settings_file(&path, false).unwrap().total, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first_pass);
    }
}
