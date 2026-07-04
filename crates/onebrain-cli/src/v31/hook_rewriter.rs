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
//!   - `["qmd-reindex"]`    → `["search", "reindex", "--json"]`
//!   - `["qmd", "reindex"]` → `["search", "reindex", "--json"]`
//!   - `["session", "init"]`        → ensure trailing `--json`
//!   - `["checkpoint", "orphans", …]` → ensure trailing `--json`
//!   - `["search", "reindex"]`      → ensure trailing `--json`
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

/// v3.4.5 Track 4: flag that scopes a PostToolUse `search reindex` hook to a
/// lexical-only pass (fast, no embedding) — full embedding is deferred to the
/// Stop hook. Only ever added by the migration below under `PostToolUse`.
const LEX_ONLY_FLAG: &str = "--lex-only";

/// v3.4.5 Track 4: flag on the Stop-event companion hook that runs a
/// pending-only embed pass at session end.
const PENDING_ONLY_FLAG: &str = "--pending-only";

/// The event name the lex-only migration and Stop-embed addition key off.
const POST_TOOL_USE: &str = "PostToolUse";
const STOP: &str = "Stop";

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
        to: &["search", "reindex"],
    },
    ArgsRewrite {
        from: &["qmd", "reindex"],
        to: &["search", "reindex"],
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
    &["search", "reindex"],
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
    /// v3.4.5 Track 4: true when a new Stop `search reindex --pending-only
    /// --json` entry was appended this run (the `plugin update` upgrade path
    /// for the same hook `onebrain-fs::register_hooks` adds on fresh
    /// installs). Counted into `total` so `rewrite_settings_file` writes the
    /// file when this is the only change.
    pub stop_embed_added: bool,
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
    for (event, event_val) in hooks_obj.iter_mut() {
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
                rewrite_entry(entry, event.as_str(), &mut report);
            }
        }
    }

    // v3.4.5 Track 4: after all per-entry rewrites, add the Stop-embed
    // companion entry when a canonical PostToolUse reindex hook exists and
    // no Stop `--pending-only` entry is present yet. This is the
    // `plugin update` upgrade path's counterpart to
    // `onebrain-fs::register_hooks::qmd::apply_embed_hook` (fresh installs).
    add_stop_embed_entry_if_needed(hooks_obj, &mut report);

    report
}

/// True if `hooks_obj["PostToolUse"]` contains a canonical OneBrain reindex
/// entry, i.e. `command == "onebrain"` and args starting with
/// `["search", "reindex"]` (post-migration; covers `--lex-only`, `--json`,
/// or both).
fn has_post_tool_use_reindex_entry(hooks_obj: &Map<String, Value>) -> bool {
    let Some(groups) = hooks_obj.get(POST_TOOL_USE).and_then(|v| v.as_array()) else {
        return false;
    };
    groups.iter().any(|group| {
        group
            .get("hooks")
            .and_then(|v| v.as_array())
            .map(|entries| entries.iter().any(is_search_reindex_entry))
            .unwrap_or(false)
    })
}

/// True if `hooks_obj["Stop"]` already contains an entry with args exactly
/// `["search", "reindex", "--pending-only", "--json"]` and command
/// `"onebrain"`.
fn has_stop_embed_entry(hooks_obj: &Map<String, Value>) -> bool {
    let Some(groups) = hooks_obj.get(STOP).and_then(|v| v.as_array()) else {
        return false;
    };
    groups.iter().any(|group| {
        group
            .get("hooks")
            .and_then(|v| v.as_array())
            .map(|entries| entries.iter().any(is_stop_embed_entry))
            .unwrap_or(false)
    })
}

fn is_search_reindex_entry(entry: &Value) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str());
    if cmd != Some("onebrain") {
        return false;
    }
    let Some(args) = entry.get("args").and_then(|v| v.as_array()) else {
        return false;
    };
    let args_str: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
    args_starts_with_str(&args_str, &["search", "reindex"])
}

fn is_stop_embed_entry(entry: &Value) -> bool {
    let cmd = entry.get("command").and_then(|v| v.as_str());
    if cmd != Some("onebrain") {
        return false;
    }
    entry
        .get("args")
        .and_then(|v| v.as_array())
        .is_some_and(|args| {
            args.as_slice()
                == [
                    Value::String("search".to_string()),
                    Value::String("reindex".to_string()),
                    Value::String(PENDING_ONLY_FLAG.to_string()),
                    Value::String(JSON_FLAG.to_string()),
                ]
        })
}

/// Append the Stop `search reindex --pending-only --json` embed entry as a
/// new group when a PostToolUse reindex hook exists and no Stop embed entry
/// is present yet. Never touches existing Stop groups (the checkpoint `stop`
/// entry stays byte-identical) — pushes a brand-new group onto `hooks.Stop`,
/// creating the array if absent.
fn add_stop_embed_entry_if_needed(hooks_obj: &mut Map<String, Value>, report: &mut RewriteReport) {
    if !has_post_tool_use_reindex_entry(hooks_obj) {
        return;
    }
    if has_stop_embed_entry(hooks_obj) {
        return;
    }

    let stop_val = hooks_obj
        .entry(STOP.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !stop_val.is_array() {
        // Malformed `Stop` shape — don't clobber whatever is there; skip.
        return;
    }
    let stop_arr = stop_val.as_array_mut().unwrap();
    stop_arr.push(serde_json::json!({
        "hooks": [
            {
                "type": "command",
                "command": "onebrain",
                "args": ["search", "reindex", PENDING_ONLY_FLAG, JSON_FLAG]
            }
        ]
    }));
    report.stop_embed_added = true;
    report.total += 1;
}

fn rewrite_entry(entry: &mut Value, event: &str, report: &mut RewriteReport) {
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

    // Pass 2 — v3.4.5 Track 4: PostToolUse-only lex-only migration. An
    // onebrain reindex entry whose args are EXACTLY `["search", "reindex"]`
    // or `["search", "reindex", "--json"]` (either the pre-existing form or
    // the result of Pass 1's `qmd-reindex`/`qmd reindex` rewrite above) gets
    // `--lex-only` inserted right after `search reindex`. Scoped to
    // PostToolUse only — a user's custom `search reindex` entry under any
    // other event (e.g. their own Stop hook) is theirs and stays untouched.
    // Entries that already carry `--lex-only` or `--pending-only` anywhere
    // don't match this exact-shape check, so they're left alone too.
    if event == POST_TOOL_USE {
        ensure_lex_only_flag(args_arr, report);
    }

    // Pass 3 — v3.1 hook-protocol commands need `--json` so the SessionStart
    // / Stop / PostToolUse hook consumers still get the structured envelope
    // they parse. Idempotent: skip if `--json` is anywhere in `args`.
    ensure_json_flag(args_arr, report);
}

/// Insert `--lex-only` into a PostToolUse `search reindex` entry when its
/// args are exactly `["search", "reindex"]`, optionally followed by a single
/// explicit format-flag suffix (`--json`, `--yaml`, `--output <fmt>`, or
/// `--output=<fmt>`) and NOTHING else. The flag is inserted right after
/// `reindex`, i.e. BEFORE any format flag, so `--json` (or `--yaml`) ends up
/// after `--lex-only` — matching the canonical `["search", "reindex",
/// "--lex-only", "--json"]` shape.
///
/// Entries that already carry `--lex-only`/`--pending-only`, or that have
/// any other trailing args (e.g. a user's custom positional args), don't
/// match this exact-shape check and are left untouched. Returns true if
/// mutated.
fn ensure_lex_only_flag(args_arr: &mut Vec<Value>, report: &mut RewriteReport) -> bool {
    let args_str: Vec<&str> = args_arr.iter().filter_map(|v| v.as_str()).collect();
    if args_str.len() < 2 || args_str[0] != "search" || args_str[1] != "reindex" {
        return false;
    }
    let suffix = &args_str[2..];
    let suffix_is_bare_or_format_flag = match suffix {
        [] => true,
        ["--json"] | ["--yaml"] => true,
        ["--output", _fmt] => true,
        [s] if s.starts_with("--output=") => true,
        _ => false,
    };
    if !suffix_is_bare_or_format_flag {
        return false;
    }

    // Insert --lex-only at index 2 (right after "reindex", before any format flag).
    args_arr.insert(2, Value::String(LEX_ONLY_FLAG.to_string()));
    report.record(&["search", "reindex"], &["search", "reindex", "--lex-only"]);
    true
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
        // 2 path rewrites (session-init, qmd-reindex) + 1 lex-only migration
        // (search reindex -> search reindex --lex-only, PostToolUse only) +
        // 3 flag injections (session init, checkpoint stop, search reindex
        // --lex-only) + 1 Stop-embed addition (PostToolUse reindex now
        // exists, no Stop --pending-only entry yet) = 7.
        assert_eq!(report.total, 7);
        assert_eq!(report.json_flag_added, 3);
        assert!(report.stop_embed_added);
        assert!(report
            .rewrites
            .iter()
            .any(|(f, t, c)| f == "session-init" && t == "session init" && *c == 1));
    }

    #[test]
    fn rewrites_qmd_reindex_to_search_reindex() {
        let mut s = settings_with_v30_hooks();
        rewrite_hooks(&mut s);
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        // v3.4.5 Track 4: PostToolUse reindex entries land on the lex-only
        // canonical shape, not the bare `search reindex --json`.
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
    }

    #[test]
    fn checkpoint_stop_path_unchanged_but_gets_json_flag() {
        // v3.0 already used `["checkpoint", "stop"]` so no path rewrite,
        // but the v3.1 default-text contract means it still needs `--json`.
        // This is the FIRST Stop group entry (checkpoint) — it must stay
        // exactly as before; the new embed entry is a separate, second Stop
        // group appended after it (see `track2_state_migrates_to_lex_only_and_adds_stop_embed`
        // below for the full Stop-array shape assertion).
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
        assert!(!second.stop_embed_added);
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
        // v3.1: 2 path rewrites (session-init, qmd-reindex) + 1 lex-only
        // migration + 3 flag injections (session init, checkpoint stop,
        // search reindex --lex-only) + 1 Stop-embed addition = 7.
        assert_eq!(report.total, 7);
        assert_eq!(report.json_flag_added, 3);
        assert!(report.stop_embed_added);
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
            json!(["search", "reindex", "--lex-only", "--json"])
        );
        assert_eq!(
            after["hooks"]["Stop"][1]["hooks"][0]["args"],
            json!(["search", "reindex", "--pending-only", "--json"])
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
        // v3.1: 2 path rewrites + 1 lex-only migration + 3 flag injections +
        // 1 Stop-embed addition = 7.
        assert_eq!(report.total, 7);
        // File contents unchanged.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original);
    }

    // ── malformed document shapes ───────────────────────────────────────

    #[test]
    fn hooks_value_not_object_returns_empty_report() {
        // `hooks` key exists but is a string (not an object of event arrays).
        // `and_then(|v| v.as_object_mut())` returns None → early return.
        let mut s = json!({ "hooks": "not-an-object" });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn event_hooks_value_not_array_is_skipped() {
        // Event value is a plain object instead of an array of groups.
        // The `let Some(group_arr) = event_val.as_array_mut() else { continue }`
        // branch fires and the event is skipped without panicking.
        let mut s = json!({
            "hooks": {
                "SessionStart": { "wrong": "shape" }
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn group_entry_not_object_is_skipped() {
        // Groups array contains a raw string instead of an object.
        let mut s = json!({
            "hooks": {
                "Stop": [ "not-a-group-object" ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn group_without_hooks_key_is_skipped() {
        // Group object exists but carries no "hooks" array.
        let mut s = json!({
            "hooks": {
                "Stop": [
                    { "type": "command", "matcher": ".*" }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn inner_entry_not_object_is_skipped() {
        // The hooks array inside a group contains a non-object (string).
        // `entry.as_object_mut()` returns None → early return from rewrite_entry.
        let mut s = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ "just-a-string" ] }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn onebrain_command_without_args_key_is_skipped() {
        // command == "onebrain" but no "args" key at all (None arm, line 185).
        // Different from `ignores_missing_args_array` which uses a non-onebrain command.
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain" }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    // ── json_type_name non-array branch ────────────────────────────────

    #[test]
    fn malformed_null_command_emits_warning_with_type_name() {
        // command: null → json_type_name returns "null"; warning message must
        // contain the type so the user knows what they put there.
        let mut s = json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": null,
                              "args": ["checkpoint", "stop"] }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, "W_MALFORMED_HOOK_ENTRY");
        assert!(
            report.warnings[0].message.contains("null"),
            "warning must name the type; got: {}",
            report.warnings[0].message
        );
    }

    // ── record() tally accumulation ────────────────────────────────────

    #[test]
    fn record_increments_existing_tally_for_same_rewrite() {
        // Two hook entries both carry `session-init`. The first call to
        // record() pushes a new tally entry; the second call finds it and
        // increments `.2` rather than pushing a duplicate (lines 99-102).
        let mut s = json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain",
                              "args": ["session-init"] },
                            { "type": "command", "command": "onebrain",
                              "args": ["session-init"] }
                        ]
                    }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        // Each entry: 1 path rewrite + 1 flag injection = 2; two entries = 4.
        assert_eq!(report.total, 4);
        assert_eq!(report.json_flag_added, 2);
        // Exactly one tally row with count = 2 (not two rows with count 1 each).
        assert_eq!(
            report.rewrites.len(),
            1,
            "should have one combined tally, not two"
        );
        let (from, to, count) = &report.rewrites[0];
        assert_eq!(from, "session-init");
        assert_eq!(to, "session init");
        assert_eq!(*count, 2);
    }

    // ── rewrite_settings_file edge cases ───────────────────────────────

    #[test]
    fn rewrite_settings_file_empty_body_is_no_op() {
        // Whitespace-only body is treated as empty → `Value::Object(Map::new())`
        // (line 299). No hooks → report.total = 0; no error.
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "   ").unwrap();
        let report = rewrite_settings_file(&path, false).unwrap();
        assert_eq!(report.total, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn rewrite_settings_file_invalid_json_returns_error() {
        // Malformed JSON body → `serde_json::from_str` fails; error propagates
        // as Err rather than panicking or silently returning an empty report.
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not: valid json }").unwrap();
        let result = rewrite_settings_file(&path, false);
        assert!(result.is_err(), "invalid JSON must return Err");
    }

    #[test]
    fn rewrite_settings_file_already_migrated_no_write_occurs() {
        // dry_run=false on already-v3.1 content: report.total = 0 →
        // `!dry_run && report.total > 0` is false → write is skipped entirely.
        // File on disk must remain byte-for-byte identical.
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let already_migrated = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["session", "init", "--json"] }
                    ] }
                ]
            }
        });
        let original = serde_json::to_string_pretty(&already_migrated).unwrap();
        std::fs::write(&path, &original).unwrap();
        let report = rewrite_settings_file(&path, false).unwrap();
        assert_eq!(report.total, 0);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original, "file must not be rewritten when total = 0");
    }

    // ── v3.4.5 Track 4: lex-only migration + Stop-embed addition ───────

    fn settings_with_track2_hooks() -> Value {
        // Track-2 state: PostToolUse `search reindex --json` (no
        // `--lex-only` yet), Stop `checkpoint stop --json` only.
        json!({
            "hooks": {
                "PostToolUse": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain",
                              "args": ["search", "reindex", "--json"] }
                        ]
                    }
                ],
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "onebrain",
                              "args": ["checkpoint", "stop", "--json"] }
                        ]
                    }
                ]
            }
        })
    }

    /// Brief test 1: Track-2 state -> PostToolUse gains `--lex-only`; Stop
    /// keeps the checkpoint entry byte-identical AND gains a new embed
    /// entry; report.total counts both changes.
    #[test]
    fn track2_state_migrates_to_lex_only_and_adds_stop_embed() {
        let mut s = settings_with_track2_hooks();
        let report = rewrite_hooks(&mut s);

        let post_tool_use = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(
            post_tool_use["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );

        let stop_groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_groups.len(), 2, "expected a new, separate Stop group");
        assert_eq!(
            stop_groups[0]["hooks"][0]["args"],
            json!(["checkpoint", "stop", "--json"]),
            "existing checkpoint entry must stay byte-identical"
        );
        assert_eq!(
            stop_groups[1]["hooks"][0]["args"],
            json!(["search", "reindex", "--pending-only", "--json"])
        );
        assert_eq!(stop_groups[1]["hooks"][0]["command"], "onebrain");
        assert_eq!(stop_groups[1]["hooks"][0]["type"], "command");

        // 1 lex-only rewrite + 1 Stop-embed addition = 2 (no --json flag
        // injection needed; both entries already had --json).
        assert_eq!(report.total, 2);
        assert_eq!(report.json_flag_added, 0);
        assert!(report.stop_embed_added);
    }

    /// Brief test 2: v3.0 state (`["qmd-reindex"]` on PostToolUse) -> single
    /// pass lands on the lex-only canonical form + Stop embed added.
    #[test]
    fn v30_qmd_reindex_single_pass_lands_on_lex_only_plus_stop_embed() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["qmd-reindex"] }
                    ] }
                ],
                "Stop": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["checkpoint", "stop", "--json"] }
                    ] }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);

        assert_eq!(
            s["hooks"]["PostToolUse"][0]["hooks"][0]["args"],
            json!(["search", "reindex", "--lex-only", "--json"])
        );
        let stop_groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_groups.len(), 2);
        assert_eq!(
            stop_groups[1]["hooks"][0]["args"],
            json!(["search", "reindex", "--pending-only", "--json"])
        );
        assert!(report.stop_embed_added);
        // 1 path rewrite (qmd-reindex -> search reindex) + 1 lex-only
        // rewrite + 1 json flag injection + 1 Stop-embed addition = 4.
        assert_eq!(report.total, 4);
        assert_eq!(report.json_flag_added, 1);
    }

    /// Brief test 3: idempotent — running twice yields zero changes the
    /// second time and leaves the document unchanged.
    #[test]
    fn lex_only_and_stop_embed_are_idempotent_on_second_pass() {
        let mut s = settings_with_track2_hooks();
        let _first = rewrite_hooks(&mut s);
        let before = s.clone();
        let second = rewrite_hooks(&mut s);
        assert_eq!(second.total, 0);
        assert!(!second.stop_embed_added);
        assert_eq!(second.json_flag_added, 0);
        assert!(second.rewrites.is_empty());
        assert_eq!(s, before, "document must be unchanged on the second pass");
    }

    /// Brief test 4: Stop embed NOT added when there is no OneBrain
    /// PostToolUse reindex entry at all (e.g. a vault without search hooks).
    #[test]
    fn stop_embed_not_added_without_post_tool_use_reindex_entry() {
        let mut s = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["checkpoint", "stop", "--json"] }
                    ] }
                ]
            }
        });
        let before = s.clone();
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(!report.stop_embed_added);
        assert_eq!(s, before, "no PostToolUse reindex entry -> no Stop embed");
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    /// Brief test 5: an existing Stop `--pending-only --json` entry is not
    /// duplicated and not rewritten — in particular it must NOT gain
    /// `--lex-only` (that flag is PostToolUse-only).
    #[test]
    fn existing_stop_pending_only_entry_not_duplicated_or_rewritten() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["search", "reindex", "--lex-only", "--json"] }
                    ] }
                ],
                "Stop": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["checkpoint", "stop", "--json"] }
                    ] },
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["search", "reindex", "--pending-only", "--json"] }
                    ] }
                ]
            }
        });
        let before = s.clone();
        let report = rewrite_hooks(&mut s);
        assert_eq!(report.total, 0);
        assert!(!report.stop_embed_added);
        assert_eq!(s, before, "already-canonical document must be untouched");
        let stop_groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_groups.len(), 2, "must not duplicate the embed entry");
        assert_eq!(
            stop_groups[1]["hooks"][0]["args"],
            json!(["search", "reindex", "--pending-only", "--json"]),
            "must not gain --lex-only"
        );
    }

    /// Brief test 6: `search reindex --json` under an event OTHER than
    /// PostToolUse (a user's custom Stop entry) is untouched by the
    /// lex-only rule.
    #[test]
    fn search_reindex_under_non_post_tool_use_event_untouched_by_lex_only() {
        let mut s = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["search", "reindex", "--json"] }
                    ] }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        // No PostToolUse reindex entry exists, so no Stop embed is added
        // either — this custom Stop entry is the user's own, not the
        // canonical embed hook.
        let entry = &s["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--json"]),
            "lex-only rule must not apply outside PostToolUse"
        );
        assert_eq!(report.total, 0);
        assert!(!report.stop_embed_added);
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    /// Brief test 7: `--yaml` PostToolUse entry gains `--lex-only` (inserted
    /// before `--yaml`), and does NOT gain `--json` on top of the explicit
    /// YAML choice.
    #[test]
    fn yaml_post_tool_use_entry_gains_lex_only_no_json_added() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    { "hooks": [
                        { "type": "command", "command": "onebrain",
                          "args": ["search", "reindex", "--yaml"] }
                    ] }
                ]
            }
        });
        let report = rewrite_hooks(&mut s);
        let entry = &s["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(
            entry["args"],
            json!(["search", "reindex", "--lex-only", "--yaml"])
        );
        // 1 lex-only rewrite + 1 Stop-embed addition (PostToolUse reindex
        // now exists). No json_flag_added since --yaml is explicit.
        assert_eq!(report.json_flag_added, 0);
        assert!(report.stop_embed_added);
        let stop_groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop_groups[0]["hooks"][0]["args"],
            json!(["search", "reindex", "--pending-only", "--json"])
        );
    }
}
