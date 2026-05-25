//! Rewrite v3.0 hook entries in `.claude/settings.json` to their v3.1 paths.
//!
//! Called by `onebrain plugin update`. Strictly additive — does not touch
//! non-OneBrain hooks, permission entries, or unrelated top-level keys. The
//! rewrite is fully idempotent: running it on already-v3.1 hooks is a no-op
//! that returns zero rewrites.
//!
//! Mapping (skill-alignment §4.7 + design §7):
//!   - `["session-init"]`   → `["session", "init"]`
//!   - `["orphan-scan", L, T]` → `["checkpoint", "orphans", L, T]`
//!   - `["qmd-reindex"]`    → `["qmd", "reindex"]`
//!
//! `["checkpoint", "stop"]` and `["checkpoint", "reset"]` are NOT rewritten
//! because v3.0 already used those exact arg shapes (the `checkpoint` group
//! was 2-level in v3.0; the v3.1 rename only flattened the noun position).

use serde_json::{Map, Value};

/// One arg-shape mapping. `from` is matched as an exact prefix of the
/// entry's `args[]`; the prefix is replaced by `to` and any remaining args
/// are kept (so `orphan-scan logs token` → `checkpoint orphans logs token`).
struct ArgsRewrite {
    from: &'static [&'static str],
    to: &'static [&'static str],
}

const REWRITES: &[ArgsRewrite] = &[
    ArgsRewrite {
        from: &["session-init"],
        to: &["session", "init"],
    },
    ArgsRewrite {
        from: &["orphan-scan"],
        to: &["checkpoint", "orphans"],
    },
    ArgsRewrite {
        from: &["qmd-reindex"],
        to: &["qmd", "reindex"],
    },
];

/// Result of a rewrite pass over a settings.json document.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RewriteReport {
    /// Per-mapping count of hook entries rewritten.
    pub rewrites: Vec<(String, String, u32)>,
    /// Total entries touched across all mappings.
    pub total: u32,
}

impl RewriteReport {
    fn record(&mut self, from: &[&str], to: &[&str]) {
        let from_s = from.join(" ");
        let to_s = to.join(" ");
        // Update the existing tally if present; otherwise push.
        if let Some(entry) = self.rewrites.iter_mut().find(|(f, _, _)| f == &from_s) {
            entry.2 += 1;
        } else {
            self.rewrites.push((from_s, to_s, 1));
        }
        self.total += 1;
    }
}

/// Walk every hook entry in `settings` and rewrite known v3.0 arg shapes to
/// their v3.1 equivalents. Mutates in place. Returns a report listing what
/// was changed.
///
/// Only entries whose `command == "onebrain"` are considered (so a
/// user-customized hook running `bash` or another binary is untouched).
pub fn rewrite_hooks(settings: &mut Value) -> RewriteReport {
    let mut report = RewriteReport::default();
    let Some(hooks_obj) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return report;
    };
    for (_event, event_val) in hooks_obj.iter_mut() {
        let Some(group_arr) = event_val.as_array_mut() else {
            continue;
        };
        for group in group_arr.iter_mut() {
            let Some(group_obj) = group.as_object_mut() else {
                continue;
            };
            let Some(entries) = group_obj.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
                continue;
            };
            for entry in entries.iter_mut() {
                rewrite_entry(entry, &mut report);
            }
        }
    }
    report
}

fn rewrite_entry(entry: &mut Value, report: &mut RewriteReport) {
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    let cmd = obj
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if cmd != "onebrain" {
        return;
    }
    let Some(args_arr) = obj.get_mut("args").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let args_str: Vec<String> = args_arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    for rule in REWRITES {
        if args_starts_with(&args_str, rule.from) {
            let mut new_args: Vec<Value> = rule
                .to
                .iter()
                .map(|s| Value::String((*s).to_string()))
                .collect();
            // Preserve any trailing args after the matched prefix.
            for trailing in &args_str[rule.from.len()..] {
                new_args.push(Value::String(trailing.clone()));
            }
            *args_arr = new_args;
            report.record(rule.from, rule.to);
            return;
        }
    }
}

fn args_starts_with(args: &[String], prefix: &[&str]) -> bool {
    if args.len() < prefix.len() {
        return false;
    }
    prefix.iter().zip(args.iter()).all(|(p, a)| p == a)
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
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn settings_with_v30_hooks() -> Value {
        json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain", "args": ["session-init"] }
                        ]
                    }
                ],
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain", "args": ["checkpoint", "stop"] }
                        ]
                    }
                ],
                "PostToolUse": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain", "args": ["qmd-reindex"] }
                        ]
                    }
                ]
            },
            "permissions": { "allow": ["Read", "Write"] }
        })
    }

    #[test]
    fn rewrites_session_init_to_session_init() {
        let mut s = settings_with_v30_hooks();
        let report = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(entry["args"], json!(["session", "init"]));
        assert_eq!(report.total, 2);
        assert!(report
            .rewrites
            .iter()
            .any(|(f, t, c)| f == "session-init" && t == "session init" && *c == 1));
    }

    #[test]
    fn rewrites_qmd_reindex() {
        let mut s = settings_with_v30_hooks();
        rewrite_hooks(&mut s);
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(entry["args"], json!(["qmd", "reindex"]));
    }

    #[test]
    fn checkpoint_stop_is_not_rewritten_idempotent() {
        let mut s = settings_with_v30_hooks();
        let _ = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["Stop"][0]["hooks"][0];
        // Already v3.1 shape; left alone.
        assert_eq!(entry["args"], json!(["checkpoint", "stop"]));
    }

    #[test]
    fn second_pass_is_a_no_op() {
        let mut s = settings_with_v30_hooks();
        let _first = rewrite_hooks(&mut s);
        let second = rewrite_hooks(&mut s);
        assert_eq!(second.total, 0, "expected zero rewrites on second pass");
        assert!(second.rewrites.is_empty());
    }

    #[test]
    fn preserves_trailing_args_after_orphan_scan_rewrite() {
        // Hypothetical hook entry that passed `logs_folder` and a token to
        // orphan-scan; v3.1 mapping should preserve those positional args.
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain",
                              "args": ["orphan-scan", "07-logs", "tokenABC"] }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(
            entry["args"],
            json!(["checkpoint", "orphans", "07-logs", "tokenABC"])
        );
        assert_eq!(report.total, 1);
    }

    #[test]
    fn ignores_non_onebrain_hooks() {
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "bash",
                              "args": ["-c", "echo hi"] }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        // Untouched.
        let entry = &s["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(entry["command"], "bash");
    }

    #[test]
    fn ignores_settings_without_hooks_key() {
        let mut s = json!({ "permissions": { "allow": [] } });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
    }

    #[test]
    fn ignores_missing_args_array() {
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            // Shell-form entry — has `command` but no `args`.
                            // Out of scope for v3.1 rewriter (the existing
                            // register-hooks shell-form migration handles it).
                            { "type": "command", "command": "onebrain checkpoint stop" }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
    }

    #[test]
    fn rewrite_settings_file_handles_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let report = rewrite_settings_file(&path, false).unwrap();
        assert_eq!(report.total, 0);
        assert!(!path.exists()); // no file created
    }

    #[test]
    fn rewrite_settings_file_writes_back_on_real_rewrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let body = serde_json::to_string_pretty(&settings_with_v30_hooks()).unwrap();
        std::fs::write(&path, body).unwrap();
        let report = rewrite_settings_file(&path, false).unwrap();
        assert_eq!(report.total, 2);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after["hooks"]["SessionStart"][0]["hooks"][0]["args"],
            json!(["session", "init"])
        );
    }

    #[test]
    fn rewrite_settings_file_dry_run_does_not_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = serde_json::to_string_pretty(&settings_with_v30_hooks()).unwrap();
        std::fs::write(&path, &original).unwrap();
        let report = rewrite_settings_file(&path, true).unwrap();
        assert_eq!(report.total, 2);
        // File contents unchanged.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original);
    }
}
