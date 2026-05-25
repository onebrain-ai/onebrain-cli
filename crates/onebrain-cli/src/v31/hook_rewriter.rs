//! Rewrite v3.0 hook entries in `.claude/settings.json` to their v3.1 paths.
//!
//! Called by `onebrain plugin update`. Strictly additive — does not touch
//! non-OneBrain hooks, permission entries, or unrelated top-level keys. The
//! rewrite is fully idempotent: running it on already-v3.1 hooks is a no-op
//! that returns zero rewrites.
//!
//! Mapping (skill-alignment §4.7 + design §7):
//!   - `["session-init"]`   → `["session", "init", "--json"]`
//!   - `["orphan-scan", L, T]` → `["checkpoint", "orphans", L, T, "--json"]`
//!   - `["qmd-reindex"]`    → `["qmd", "reindex", "--json"]`
//!   - `["session", "init"]`        → ensure trailing `--json`
//!   - `["checkpoint", "orphans", …]` → ensure trailing `--json`
//!   - `["qmd", "reindex"]`         → ensure trailing `--json`
//!   - `["checkpoint", "stop"]`     → ensure trailing `--json`
//!
//! v3.1 contract: hook-protocol commands default to TEXT output for
//! interactive use; machine consumers (Claude Code hooks) MUST pass `--json`
//! to get the structured envelope. This rewriter ensures existing installs
//! migrate without manual intervention.
//!
//! `--json` injection is idempotent — entries that already carry the flag
//! (anywhere in `args`) are left alone for that step. Path rewrites still
//! apply on top.

use serde_json::{Map, Value};

/// The flag that switches hook-protocol commands from default-text to JSON.
const JSON_FLAG: &str = "--json";

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

/// v3.1 hook-protocol commands that must carry `--json` for machine
/// consumers. Matched against the args[] prefix (post-rewrite); if the
/// prefix matches AND no `--json` is anywhere in args, the flag is
/// appended.
const JSON_REQUIRED_PREFIXES: &[&[&str]] = &[
    &["session", "init"],
    &["checkpoint", "orphans"],
    &["checkpoint", "stop"],
    &["qmd", "reindex"],
];

/// Result of a rewrite pass over a settings.json document.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RewriteReport {
    /// Per-mapping count of hook entries rewritten.
    pub rewrites: Vec<(String, String, u32)>,
    /// Total entries touched across all mappings.
    pub total: u32,
    /// Entries that gained a trailing `--json` flag (separate from `total`
    /// so callers can distinguish path-only rewrites from flag-only ones).
    /// v3.1 hook-protocol commands need `--json` because the default is
    /// now text; this counter records each entry the rewriter promoted.
    pub json_flag_added: u32,
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

    fn warn(&mut self, code: &str, message: impl Into<String>) {
        self.warnings.push(RewriteWarning {
            code: code.to_string(),
            message: message.into(),
        });
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

    // `command` shape check. Missing is fine (shell-form entries set the
    // full command in a single string under "command" without an args
    // array — handled by the register-hooks migrator, not us). But if
    // `command` is present and non-string (number/null/array/object), we
    // can't reason about it; surface a warning and skip.
    match obj.get("command") {
        Some(Value::String(s)) if s == "onebrain" => { /* fall through to args */ }
        Some(Value::String(_)) => return, // some other binary (bash, python, etc.)
        Some(other) => {
            report.warn(
                "W_MALFORMED_HOOK_ENTRY",
                format!(
                    "hook entry has non-string `command` field (got {}); skipped",
                    json_type_name(other)
                ),
            );
            return;
        }
        None => return,
    }

    // `args` shape check. Missing or non-array → can't rewrite; warn if
    // present-but-non-array, otherwise silently skip (shell-form).
    let args_arr = match obj.get_mut("args") {
        Some(Value::Array(a)) => a,
        Some(other) => {
            report.warn(
                "W_MALFORMED_HOOK_ENTRY",
                format!(
                    "hook entry has non-array `args` field (got {}); skipped",
                    json_type_name(other)
                ),
            );
            return;
        }
        None => return,
    };

    // Pass 1 — v3.0 → v3.1 path rewrite.
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
            break;
        }
    }

    // Pass 2 — v3.1 hook-protocol commands need `--json` so the SessionStart
    // / Stop / PostToolUse hook consumers still get the structured envelope
    // they parse. Idempotent: skip if `--json` is anywhere in `args`.
    ensure_json_flag(args_arr, report);
}

/// Append `--json` to `args_arr` when the prefix names a hook-protocol
/// command AND no explicit output flag is already present. Returns true
/// if the array was mutated.
///
/// Idempotency rules (don't clobber an explicit user choice):
/// - `--json` anywhere → skip
/// - `--yaml` anywhere → skip (user wants YAML)
/// - `--output <fmt>` (any fmt) → skip
/// - `--output=<fmt>` → skip
fn ensure_json_flag(args_arr: &mut Vec<Value>, report: &mut RewriteReport) -> bool {
    if has_explicit_format_flag(args_arr) {
        return false;
    }

    let args_str: Vec<&str> = args_arr.iter().filter_map(|v| v.as_str()).collect();
    let matches_protocol = JSON_REQUIRED_PREFIXES
        .iter()
        .any(|prefix| args_starts_with_str(&args_str, prefix));
    if !matches_protocol {
        return false;
    }

    args_arr.push(Value::String(JSON_FLAG.to_string()));
    report.json_flag_added += 1;
    report.total += 1;
    true
}

/// True if any args entry is `--json` / `--yaml` / `--output` / `--output=*`.
/// Used to gate `--json` injection so an explicit user choice (e.g. an
/// admin who pasted `--yaml`) is never overridden.
fn has_explicit_format_flag(args_arr: &[Value]) -> bool {
    args_arr.iter().any(|v| {
        matches!(
            v.as_str(),
            Some("--json") | Some("--yaml") | Some("--output")
        ) || v.as_str().is_some_and(|s| s.starts_with("--output="))
    })
}

fn args_starts_with_str(args: &[&str], prefix: &[&str]) -> bool {
    if args.len() < prefix.len() {
        return false;
    }
    prefix.iter().zip(args.iter()).all(|(p, a)| p == a)
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
        // v3.1: rewrite path AND append --json so the hook consumer
        // continues to see the structured envelope (text is the new default
        // for human invocations).
        assert_eq!(entry["args"], json!(["session", "init", "--json"]));
        // 2 path rewrites (session-init, qmd-reindex) + 3 flag injections
        // (session init, checkpoint stop, qmd reindex).
        assert_eq!(report.total, 5);
        assert_eq!(report.json_flag_added, 3);
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
        assert_eq!(entry["args"], json!(["qmd", "reindex", "--json"]));
    }

    #[test]
    fn checkpoint_stop_path_unchanged_but_gets_json_flag() {
        // v3.0 already used `["checkpoint", "stop"]` so no path rewrite,
        // but the v3.1 default-text contract means it still needs `--json`.
        let mut s = settings_with_v30_hooks();
        let _ = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(entry["args"], json!(["checkpoint", "stop", "--json"]));
    }

    #[test]
    fn second_pass_is_a_no_op() {
        let mut s = settings_with_v30_hooks();
        let _first = rewrite_hooks(&mut s);
        let second = rewrite_hooks(&mut s);
        assert_eq!(second.total, 0, "expected zero rewrites on second pass");
        assert_eq!(second.json_flag_added, 0);
        assert!(second.rewrites.is_empty());
    }

    #[test]
    fn preserves_trailing_args_after_orphan_scan_rewrite() {
        // Hypothetical hook entry that passed `logs_folder` and a token to
        // orphan-scan; v3.1 mapping should preserve those positional args
        // AND append --json so the consumer keeps getting the JSON shape.
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
            json!(["checkpoint", "orphans", "07-logs", "tokenABC", "--json"])
        );
        // 1 path rewrite + 1 flag injection.
        assert_eq!(report.total, 2);
        assert_eq!(report.json_flag_added, 1);
    }

    // ── v3.1 --json flag injection ──────────────────────────────────────

    #[test]
    fn appends_json_flag_to_already_v31_session_init() {
        // Entry is already on v3.1 path but pre-dates the --json contract.
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["session", "init"] }
                    ] }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(entry["args"], json!(["session", "init", "--json"]));
        assert_eq!(report.json_flag_added, 1);
        assert_eq!(report.total, 1);
        assert!(report.rewrites.is_empty(), "no path rewrite expected");
    }

    #[test]
    fn idempotent_when_json_already_present() {
        // User pre-migrated by hand — leave args alone.
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["session", "init", "--json"] }
                    ] }
                ]
            }
        });
        let before = s.clone();
        let report = rewrite_hooks(&mut s);
        assert_eq!(s, before, "no mutation expected");
        assert_eq!(report.total, 0);
        assert_eq!(report.json_flag_added, 0);
    }

    #[test]
    fn idempotent_when_output_json_long_form_present() {
        // `--output json` is equivalent to `--json`; rewriter must respect it.
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["session", "init", "--output", "json"] }
                    ] }
                ]
            }
        });
        let before = s.clone();
        let report = rewrite_hooks(&mut s);
        assert_eq!(s, before);
        assert_eq!(report.total, 0);
    }

    #[test]
    fn idempotent_when_output_eq_json_present() {
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["session", "init", "--output=json"] }
                    ] }
                ]
            }
        });
        let before = s.clone();
        let report = rewrite_hooks(&mut s);
        assert_eq!(s, before);
        assert_eq!(report.total, 0);
    }

    #[test]
    fn yaml_flag_is_respected_no_json_appended() {
        // User actively chose YAML. We must NOT clobber that with --json.
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["session", "init", "--yaml"] }
                    ] }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["SessionStart"][0]["hooks"][0];
        // Stays YAML; flag injection only targets the JSON contract for
        // hook entries that previously relied on JSON-by-default.
        // Conservative behaviour: don't auto-add --json on top of an
        // explicit --yaml. Users who pasted --yaml know what they want.
        assert!(entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("--yaml")));
        // `has_explicit_format_flag` matches `--yaml` alongside `--json` /
        // `--output`, so the rewriter never stacks `--json` on top of an
        // explicit YAML choice (clap's `conflicts_with` would otherwise
        // reject the invocation at parse time).
        let has_json = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("--json"));
        assert!(
            !has_json,
            "rewriter must not inject --json when --yaml is already explicit; got: {:?}",
            entry["args"]
        );
        let _ = report;
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
        // v3.1: 2 path rewrites (session-init, qmd-reindex) + 3 flag
        // injections (session init, checkpoint stop, qmd reindex).
        assert_eq!(report.total, 5);
        assert_eq!(report.json_flag_added, 3);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after["hooks"]["SessionStart"][0]["hooks"][0]["args"],
            json!(["session", "init", "--json"])
        );
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["args"],
            json!(["checkpoint", "stop", "--json"])
        );
        assert_eq!(
            after["hooks"]["PostToolUse"][0]["hooks"][0]["args"],
            json!(["qmd", "reindex", "--json"])
        );
    }

    #[test]
    fn malformed_hook_entry_emits_warning_and_preserves_valid_entries() {
        // Non-string `command` (e.g. array): emit W_MALFORMED_HOOK_ENTRY,
        // preserve the entry as-is, continue rewriting siblings.
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            // Malformed: `command` is an array.
                            { "type": "command", "command": ["onebrain", "session-init"] },
                            // Valid v3.0 entry alongside.
                            { "type": "command", "command": "onebrain", "args": ["session-init"] }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        // 1 path rewrite + 1 flag injection on the valid entry.
        assert_eq!(report.total, 2, "valid entry must be rewritten + flagged");
        assert_eq!(report.json_flag_added, 1);
        assert_eq!(report.warnings.len(), 1, "expected one warning");
        assert_eq!(report.warnings[0].code, "W_MALFORMED_HOOK_ENTRY");
        assert!(report.warnings[0].message.contains("non-string `command`"));
        // Valid entry got rewritten with --json appended.
        let entries = &s["hooks"]["SessionStart"][0]["hooks"];
        assert_eq!(entries[1]["args"], json!(["session", "init", "--json"]));
        // Malformed entry preserved.
        assert_eq!(entries[0]["command"], json!(["onebrain", "session-init"]));
    }

    #[test]
    fn malformed_args_field_emits_warning() {
        // `args` present but not an array (here: a string).
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain", "args": "session-init" }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, "W_MALFORMED_HOOK_ENTRY");
        assert!(report.warnings[0].message.contains("non-array `args`"));
    }

    #[test]
    fn rewrite_settings_file_dry_run_does_not_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = serde_json::to_string_pretty(&settings_with_v30_hooks()).unwrap();
        std::fs::write(&path, &original).unwrap();
        let report = rewrite_settings_file(&path, true).unwrap();
        // v3.1: 2 path rewrites + 3 flag injections.
        assert_eq!(report.total, 5);
        // File contents unchanged.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original);
    }
}
