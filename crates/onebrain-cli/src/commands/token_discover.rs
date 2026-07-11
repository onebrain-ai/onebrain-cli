//! `onebrain token discover` — the read-hook field-test measurement
//! instrument (design §5c). Scans Claude Code session transcripts
//! (`~/.claude/projects/**/*.jsonl`, the same source RTK reads) for direct
//! `Read`/`Grep` tool calls that touched a vault `.md` doc a SECOND time
//! within the same session — exactly the traffic the already-sent ledger
//! (design §3b) would have denied had the read-hook been gating it. Reports
//! `{bypassed_reads, est_tokens_missed, top_paths}` so the field test has
//! real numbers deciding the v3.4.11 default flip.
//!
//! Minimal scope (design §5c): text + `--json` output, no auto-fix, Claude
//! Code transcripts only. Read-only — this command never writes to a
//! transcript, a vault doc, or `token.redb`.
//!
//! **Why "repeat" and not every touch:** the ledger only ever denies a
//! REPEAT delivery of unchanged content — the first read of any doc is
//! always allowed even with the hook active (design §5b: "First-time read or
//! changed hash → allow untouched"). Counting every touch would overstate
//! what the hook could actually have saved. "Repeat" is scoped PER SESSION
//! FILE (one `.jsonl` == one Claude Code session, exactly the ledger's own
//! `(session_token, doc_path)` key), and — deliberately broader than the
//! hook's own `Read`-only gate — treats a `Read` and a `Grep` on the SAME
//! path as touching the same content: both deliver the doc's bytes into the
//! agent's context, so a second touch by either tool is redundant from the
//! ledger's point of view.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::TokenDiscoverArgs;
use crate::output::{emit, Envelope, OutputMode};
use onebrain_token::estimate::{estimate_tokens, ModelFamily};

/// How many hottest paths to surface — a "top offenders" list, not a full
/// dump (design: "minimal scope").
const TOP_PATHS_LIMIT: usize = 10;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct PathCount {
    pub path: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct DiscoverData {
    /// Repeat Read/Grep touches of the same vault `.md` doc within one
    /// session — reads the already-sent ledger COULD have denied.
    pub bypassed_reads: u64,
    /// `estimate_tokens` (design-calibrated, per-model-family) summed over
    /// each bypassed read's file, read from CURRENT disk content — labeled
    /// "estimate" per the design's honesty-signal convention; a since-deleted
    /// file contributes 0 (best-effort, never guessed).
    pub est_tokens_missed: u64,
    /// The hottest repeated paths, most-touched first, capped at
    /// [`TOP_PATHS_LIMIT`].
    pub top_paths: Vec<PathCount>,
    /// Transcript files actually scanned (after the `--since-days` filter) —
    /// so a zero result is distinguishable from "found nothing" vs "looked at
    /// nothing".
    pub scanned_files: usize,
    /// Echoes `--since-days`, `None` when unset (no window applied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_days: Option<u32>,
}

/// `~/.claude/projects` (or `$CLAUDE_CONFIG_DIR/projects` when set) — the
/// Claude Code transcript root. Honors `%USERPROFILE%` on Windows explicitly
/// (rather than solely trusting `dirs::home_dir`, whose Windows resolution
/// goes through the Known Folder API and isn't guaranteed to observe a
/// test-set env override) so the path handling plan 5.3 asks for is real,
/// not incidental.
fn transcripts_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir).join("projects"));
        }
    }
    Ok(home_dir()?.join(".claude").join("projects"))
}

#[cfg(windows)]
fn home_dir() -> Result<PathBuf> {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.trim().is_empty() {
            return Ok(PathBuf::from(profile));
        }
    }
    dirs::home_dir().context("resolve home directory (USERPROFILE unset and no fallback)")
}

#[cfg(not(windows))]
fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("resolve home directory")
}

/// Recursively list every `*.jsonl` file under `root`. `None`/empty when
/// `root` doesn't exist — a fresh machine with no Claude Code history yet is
/// a normal, zero-result state, not an error.
fn list_jsonl_files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect()
}

fn passes_since_days(path: &Path, cutoff: Option<SystemTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return true;
    };
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime >= cutoff,
        // Can't stat it — be inclusive rather than silently drop data the
        // caller asked for; the file's own JSON parsing may still fail
        // separately (handled by `scan_one_file`'s per-line tolerance).
        Err(_) => true,
    }
}

/// A `Read`/`Grep` tool-call's target path, if that block is one of the two
/// tool names discover cares about. `Read` carries `input.file_path`; `Grep`
/// carries `input.path` (may be a directory — those are filtered out later
/// by [`vault_relative_md`], which requires a `.md` extension).
fn candidate_raw_path(block: &serde_json::Value) -> Option<&str> {
    if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
        return None;
    }
    match block.get("name").and_then(|v| v.as_str())? {
        "Read" => block.pointer("/input/file_path")?.as_str(),
        "Grep" => block.pointer("/input/path")?.as_str(),
        _ => None,
    }
}

/// Resolve `raw_path` to a vault-relative path IF it is a `.md` file under
/// `vault_root` — `None` otherwise ("ignores non-vault", plan 5.2). Tries a
/// canonical-vs-canonical comparison first (handles symlinked temp dirs,
/// `..` segments); falls back to a raw prefix comparison when either side
/// can't be canonicalized (e.g. the file no longer exists on disk — the
/// transcript is still real evidence of a past read).
fn vault_relative_md(raw_path: &str, vault_root: &Path, vault_root_canon: &Path) -> Option<String> {
    let path = Path::new(raw_path);
    let is_md = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false);
    if !is_md {
        return None;
    }
    let rel = match std::fs::canonicalize(path) {
        Ok(canon) => canon
            .strip_prefix(vault_root_canon)
            .ok()
            .map(|p| p.to_path_buf()),
        Err(_) => path.strip_prefix(vault_root).ok().map(|p| p.to_path_buf()),
    }?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn estimate_tokens_for(vault_root: &Path, rel_path: &str, model: ModelFamily) -> u64 {
    std::fs::read_to_string(vault_root.join(rel_path))
        .map(|content| estimate_tokens(&content, model))
        .unwrap_or(0)
}

#[derive(Default)]
struct ScanTotals {
    bypassed_reads: u64,
    est_tokens_missed: u64,
    path_counts: HashMap<String, u64>,
}

/// Walk one transcript file's lines in order, tracking first-seen vault `.md`
/// paths in a per-file set (design: "repeat" is scoped to one session file).
/// Tolerant of corrupt/partial lines (a crash mid-write can truncate the last
/// line of a live transcript) — unparseable lines are skipped, never fatal.
fn scan_one_file(
    path: &Path,
    vault_root: &Path,
    vault_root_canon: &Path,
    model: ModelFamily,
    totals: &mut ScanTotals,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut seen: HashSet<String> = HashSet::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(blocks) = value.pointer("/message/content").and_then(|v| v.as_array()) else {
            continue;
        };
        for block in blocks {
            let Some(raw_path) = candidate_raw_path(block) else {
                continue;
            };
            let Some(rel) = vault_relative_md(raw_path, vault_root, vault_root_canon) else {
                continue;
            };
            if seen.contains(&rel) {
                totals.bypassed_reads += 1;
                *totals.path_counts.entry(rel.clone()).or_insert(0) += 1;
                totals.est_tokens_missed += estimate_tokens_for(vault_root, &rel, model);
            } else {
                seen.insert(rel);
            }
        }
    }
}

/// The scan engine, decoupled from env/CLI-arg resolution so it's directly
/// unit-testable against a fixture transcripts dir + a fixture vault dir
/// (plan 5.2: "TDD against FIXTURE .jsonl transcripts with the real captured
/// shape").
pub(crate) fn scan(
    transcripts_root: &Path,
    vault_root: &Path,
    since_days: Option<u32>,
    model: ModelFamily,
) -> DiscoverData {
    let vault_root_canon =
        std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());
    let cutoff = since_days
        .and_then(|d| SystemTime::now().checked_sub(Duration::from_secs(u64::from(d) * 86_400)));

    let files = list_jsonl_files(transcripts_root);
    let mut totals = ScanTotals::default();
    let mut scanned_files = 0usize;
    for file in &files {
        if !passes_since_days(file, cutoff) {
            continue;
        }
        scanned_files += 1;
        scan_one_file(file, vault_root, &vault_root_canon, model, &mut totals);
    }

    let mut top_paths: Vec<PathCount> = totals
        .path_counts
        .into_iter()
        .map(|(path, count)| PathCount { path, count })
        .collect();
    top_paths.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
    top_paths.truncate(TOP_PATHS_LIMIT);

    DiscoverData {
        bypassed_reads: totals.bypassed_reads,
        est_tokens_missed: totals.est_tokens_missed,
        top_paths,
        scanned_files,
        since_days,
    }
}

fn render_text(envelope: &Envelope<DiscoverData>) -> String {
    let Some(data) = envelope.data.as_ref() else {
        return "token discover: no data\n".to_string();
    };
    let mut out = String::new();
    out.push_str("Token Discover — direct-read field test\n");
    if let Some(days) = data.since_days {
        out.push_str(&format!("  Window:              last {days} day(s)\n"));
    }
    out.push_str(&format!("  Transcripts scanned: {}\n", data.scanned_files));
    out.push_str(&format!(
        "  Bypassed reads:      {} (repeat Read/Grep the ledger could have denied)\n",
        data.bypassed_reads
    ));
    out.push_str(&format!(
        "  Est. tokens missed:  {} (estimate)\n",
        data.est_tokens_missed
    ));
    if data.top_paths.is_empty() {
        out.push_str("  Top paths:           (none)\n");
    } else {
        out.push_str("  Top paths:\n");
        for p in &data.top_paths {
            out.push_str(&format!("    {:>6}  {}\n", p.count, p.path));
        }
    }
    out
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &TokenDiscoverArgs) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    let model = onebrain_core::config::load_vault_config_at(resolved.root.as_path())
        .map(|cfg| crate::commands::token_runner::model_family_for(&cfg.token_optimization.model))
        .unwrap_or(ModelFamily::ClaudeGeneric);

    let root = transcripts_root().context("resolve Claude Code transcripts directory")?;
    let data = scan(&root, resolved.root.as_path(), args.since_days, model);

    let effective_mode = if args.json && !mode.is_structured() {
        OutputMode::Json { pretty: false }
    } else {
        mode.clone()
    };

    let envelope = Envelope::ok("token.discover", Some(vault_info), data);
    emit(
        &envelope,
        &effective_mode,
        std::io::stdout().lock(),
        render_text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_jsonl(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    /// One `assistant` transcript line whose `message.content` is a single
    /// `Read` tool_use — the real captured shape (verified against a live
    /// `~/.claude/projects/**/*.jsonl` entry 2026-07-11).
    fn read_line(file_path: &str) -> String {
        serde_json::json!({
            "parentUuid": "p1",
            "isSidechain": false,
            "message": {
                "model": "claude-opus-4-8",
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "Read",
                    "input": { "file_path": file_path },
                    "caller": { "type": "direct" }
                }],
                "stop_reason": "tool_use"
            },
            "type": "assistant",
            "uuid": "u1",
            "timestamp": "2026-07-11T00:00:00.000Z",
            "sessionId": "s1",
            "version": "2.1.156"
        })
        .to_string()
    }

    /// Real captured `Grep` tool_use shape.
    fn grep_line(target_path: &str) -> String {
        serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "name": "Grep",
                    "input": {
                        "pattern": "something",
                        "path": target_path,
                        "output_mode": "content",
                        "-n": true
                    }
                }]
            },
            "type": "assistant"
        })
        .to_string()
    }

    fn vault_with_doc(rel: &str, content: &str) -> tempfile::TempDir {
        let vault = tempfile::tempdir().unwrap();
        let full = vault.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
        vault
    }

    #[test]
    fn first_read_of_a_path_is_not_counted_only_the_repeat_is() {
        let vault = vault_with_doc("notes/a.md", "hello world, this is a note");
        let transcripts = tempfile::tempdir().unwrap();
        let path = vault.path().join("notes/a.md");
        let path_str = path.to_string_lossy().to_string();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[read_line(&path_str), read_line(&path_str)],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(data.bypassed_reads, 1, "only the 2nd read counts");
        assert_eq!(data.scanned_files, 1);
        assert_eq!(data.top_paths.len(), 1);
        assert_eq!(data.top_paths[0].path, "notes/a.md");
        assert_eq!(data.top_paths[0].count, 1);
        assert!(
            data.est_tokens_missed > 0,
            "should estimate the repeat's tokens"
        );
    }

    #[test]
    fn single_read_of_two_different_paths_counts_zero() {
        let vault = vault_with_doc("a.md", "one");
        fs::write(vault.path().join("b.md"), "two").unwrap();
        let transcripts = tempfile::tempdir().unwrap();
        let a = vault.path().join("a.md").to_string_lossy().to_string();
        let b = vault.path().join("b.md").to_string_lossy().to_string();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[read_line(&a), read_line(&b)],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(data.bypassed_reads, 0);
        assert!(data.top_paths.is_empty());
    }

    #[test]
    fn non_vault_path_is_ignored_even_if_repeated() {
        let vault = vault_with_doc("a.md", "one");
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("elsewhere.md");
        fs::write(&outside_file, "not in the vault").unwrap();
        let outside_str = outside_file.to_string_lossy().to_string();

        let transcripts = tempfile::tempdir().unwrap();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[read_line(&outside_str), read_line(&outside_str)],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(data.bypassed_reads, 0, "non-vault repeats are ignored");
    }

    #[test]
    fn non_md_vault_path_is_ignored() {
        let vault = vault_with_doc("image.png", "binary-ish content");
        let img = vault.path().join("image.png").to_string_lossy().to_string();
        let transcripts = tempfile::tempdir().unwrap();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[read_line(&img), read_line(&img)],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(data.bypassed_reads, 0, "non-.md vault paths are ignored");
    }

    #[test]
    fn repeat_via_grep_after_read_counts_as_a_repeat() {
        let vault = vault_with_doc("notes/a.md", "hello world");
        let path_str = vault
            .path()
            .join("notes/a.md")
            .to_string_lossy()
            .to_string();
        let transcripts = tempfile::tempdir().unwrap();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[read_line(&path_str), grep_line(&path_str)],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(
            data.bypassed_reads, 1,
            "Grep on an already-Read path is a repeat"
        );
    }

    #[test]
    fn repeat_is_scoped_per_session_file_not_across_files() {
        let vault = vault_with_doc("a.md", "hello");
        let path_str = vault.path().join("a.md").to_string_lossy().to_string();
        let transcripts = tempfile::tempdir().unwrap();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[read_line(&path_str)],
        );
        write_jsonl(
            transcripts.path(),
            "session2.jsonl",
            &[read_line(&path_str)],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(
            data.bypassed_reads, 0,
            "one read per session file — neither is a repeat within its own file"
        );
        assert_eq!(data.scanned_files, 2);
    }

    #[test]
    fn since_days_filters_by_file_mtime() {
        let vault = vault_with_doc("a.md", "hello");
        let path_str = vault.path().join("a.md").to_string_lossy().to_string();
        let transcripts = tempfile::tempdir().unwrap();
        let old = write_jsonl(
            transcripts.path(),
            "old.jsonl",
            &[read_line(&path_str), read_line(&path_str)],
        );
        let new = write_jsonl(
            transcripts.path(),
            "new.jsonl",
            &[read_line(&path_str), read_line(&path_str)],
        );

        // Back-date the "old" file's mtime by 30 days; leave "new" as-is.
        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let old_time = filetime::FileTime::from_system_time(thirty_days_ago);
        filetime::set_file_mtime(&old, old_time).unwrap();
        let _ = &new;

        let data = scan(
            transcripts.path(),
            vault.path(),
            Some(7),
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(
            data.scanned_files, 1,
            "only the recent file passes --since-days 7"
        );
        assert_eq!(
            data.bypassed_reads, 1,
            "only the recent file's repeat is counted"
        );
    }

    #[test]
    fn corrupt_and_partial_lines_are_tolerated_not_fatal() {
        let vault = vault_with_doc("a.md", "hello");
        let path_str = vault.path().join("a.md").to_string_lossy().to_string();
        let transcripts = tempfile::tempdir().unwrap();
        write_jsonl(
            transcripts.path(),
            "session1.jsonl",
            &[
                "not json at all".to_string(),
                read_line(&path_str),
                read_line(&path_str),
                r#"{"message":{"content":[{"type":"tool_use","name":"Read","input":"#.to_string(), // truncated
            ],
        );

        let data = scan(
            transcripts.path(),
            vault.path(),
            None,
            ModelFamily::ClaudeGeneric,
        );
        assert_eq!(
            data.bypassed_reads, 1,
            "corrupt lines are skipped, not fatal"
        );
    }

    #[test]
    fn missing_transcripts_root_is_a_zero_result_not_an_error() {
        let vault = vault_with_doc("a.md", "hello");
        let missing = tempfile::tempdir().unwrap().path().join("does-not-exist");
        let data = scan(&missing, vault.path(), None, ModelFamily::ClaudeGeneric);
        assert_eq!(data.scanned_files, 0);
        assert_eq!(data.bypassed_reads, 0);
        assert!(data.top_paths.is_empty());
    }

    // ── transcripts_root: $CLAUDE_CONFIG_DIR override ──────────────────────

    #[test]
    fn transcripts_root_honors_claude_config_dir_override() {
        let fake = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("CLAUDE_CONFIG_DIR", fake.path());
        let root = transcripts_root().unwrap();
        assert_eq!(root, fake.path().join("projects"));
    }

    #[cfg(windows)]
    #[test]
    fn transcripts_root_honors_userprofile_on_windows() {
        let fake = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("USERPROFILE", fake.path().as_os_str()),
            ("CLAUDE_CONFIG_DIR", std::ffi::OsStr::new("")),
        ]);
        let root = transcripts_root().unwrap();
        assert_eq!(root, fake.path().join(".claude").join("projects"));
    }
}
