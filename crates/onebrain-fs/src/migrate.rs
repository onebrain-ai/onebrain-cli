//! `migrate` — idempotent vault structure migrations.
//!
//! Currently supports a single migration: `backfill-recapped`.
//!
//! `backfill-recapped` walks every session log under
//! `[logs_folder]/session/YYYY/MM/*.md` (the post-v2.4.0 layout) and adds a
//! `recapped: <today>` field to the YAML frontmatter when missing.
//!
//! Behavior mirrors the Bun reference implementation
//! (`src/commands/internal/migrate.ts`) byte-for-byte:
//!
//! - whitelist: filename must contain `-session-` (checkpoint / update /
//!   weekly logs are not session logs and never carry `recapped:`).
//! - optional `cutoff_date` (ISO `YYYY-MM-DD`) skips files whose date prefix
//!   is **strictly greater** than the cutoff (boundary inclusive).
//! - a file with `recapped:` already present is left untouched and is **not**
//!   counted as skipped — only true failures count toward `skipped`.
//! - malformed frontmatter, missing frontmatter, or write errors all
//!   increment `skipped` and emit a one-line stderr warning.
//!
//! The Bun helper `parseFrontmatterWithRest` is reimplemented locally because
//! the shared `frontmatter` module returns only the parsed mapping; backfill
//! also needs the body text to rebuild the file.

use std::fs;
use std::path::Path;

use serde_yaml::{Mapping, Value};

/// Result of running a migration — counts of files mutated and files that
/// failed (malformed, unreadable, or unwritable).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrateResult {
    pub backfilled: usize,
    pub skipped: usize,
}

/// Parse YAML frontmatter and the trailing body in one pass.
///
/// Recognises CRLF-or-LF, `---\n…\n---\n` (closing delimiter must be a bare
/// line followed by a newline or end-of-file). A body line such as
/// `---some-separator` is **not** treated as a closer.
///
/// Returns `Some((mapping, body))` only when the frontmatter parses
/// successfully **and** is a YAML mapping at the top level. Sequences,
/// scalars, or parse errors return `None` — callers treat `None` as
/// "malformed or absent frontmatter".
fn parse_frontmatter_with_rest(raw: &str) -> Option<(Mapping, String)> {
    let text = raw.replace("\r\n", "\n");
    let after_open = text.strip_prefix("---\n")?;

    // Match Bun's `/\n---(\n|$)/`: locate `\n---` followed by `\n` or EOF.
    // Iterate matches so we skip false closers like `\n---some-text`.
    let mut search_from = 0;
    let (fm_block, body) = loop {
        let rel = after_open[search_from..].find("\n---")?;
        let abs = search_from + rel;
        let after_dashes = abs + 4; // position right after the "\n---"
        let next_byte = after_open.as_bytes().get(after_dashes).copied();
        match next_byte {
            None => {
                // Closing `\n---` at EOF.
                break (&after_open[..abs], String::new());
            }
            Some(b'\n') => {
                let body_start = after_dashes + 1;
                break (&after_open[..abs], after_open[body_start..].to_string());
            }
            _ => {
                // Not a bare closer (e.g. `---some-separator`). Advance past
                // the literal newline so the next iteration searches after
                // this candidate.
                search_from = abs + 1;
            }
        }
    };

    let trimmed = fm_block.trim();
    let parsed: Value = serde_yaml::from_str(trimmed).ok()?;
    match parsed {
        Value::Mapping(m) => Some((m, body)),
        _ => None,
    }
}

/// List `*.md` files directly inside `dir`. Returns an empty vec on any I/O
/// failure — Bun's helper swallows errors the same way.
fn list_md_files(dir: &Path) -> Vec<String> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        if let Some(s) = name.to_str() {
            if s.ends_with(".md") {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// Run the `backfill-recapped` migration.
///
/// `today` is injected (rather than reading the wall clock) so tests can
/// assert on exact frontmatter content. Callers should pass
/// `chrono::Utc::now().date_naive().to_string()` to match Bun's
/// `new Date().toISOString().slice(0, 10)` (UTC).
///
/// `warn` receives one stderr-style line per failure (already newline-free).
pub fn run_backfill_recapped(
    logs_folder: &Path,
    cutoff_date: Option<&str>,
    today: &str,
    mut warn: impl FnMut(&str),
) -> MigrateResult {
    let mut result = MigrateResult::default();

    let session_root = logs_folder.join("session");
    let year_dirs = match fs::read_dir(&session_root) {
        Ok(r) => r,
        Err(_) => return result, // session/ missing → nothing to backfill
    };

    for year_entry in year_dirs.flatten() {
        let year_path = year_entry.path();
        let month_dirs = match fs::read_dir(&year_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for month_entry in month_dirs.flatten() {
            let month_path = month_entry.path();
            for fname in list_md_files(&month_path) {
                if !fname.contains("-session-") {
                    continue;
                }

                if let Some(cutoff) = cutoff_date {
                    // YYYY-MM-DD prefix; lexical compare is correct.
                    if fname.len() >= 10 {
                        let prefix = &fname[..10];
                        if prefix
                            .as_bytes()
                            .iter()
                            .take(10)
                            .all(|b| b.is_ascii_digit() || *b == b'-')
                            && prefix > cutoff
                        {
                            continue;
                        }
                    }
                }

                let fpath = month_path.join(&fname);
                let content = match fs::read_to_string(&fpath) {
                    Ok(s) => s,
                    Err(err) => {
                        warn(&format!("migrate: error processing {fname}: {err}"));
                        result.skipped += 1;
                        continue;
                    }
                };

                let parsed = match parse_frontmatter_with_rest(&content) {
                    Some(p) => p,
                    None => {
                        warn(&format!("migrate: {fname} — malformed frontmatter"));
                        result.skipped += 1;
                        continue;
                    }
                };

                let (mut mapping, rest) = parsed;
                let recapped_key = Value::String("recapped".to_string());
                if mapping.contains_key(&recapped_key) {
                    continue; // already migrated — no count change
                }

                mapping.insert(recapped_key, Value::String(today.to_string()));

                let yaml = match serde_yaml::to_string(&mapping) {
                    Ok(s) => s,
                    Err(err) => {
                        warn(&format!("migrate: error processing {fname}: {err}"));
                        result.skipped += 1;
                        continue;
                    }
                };

                // `serde_yaml::to_string` always appends a trailing newline,
                // so the final byte before `---` is `\n` (matches Bun).
                let updated = format!("---\n{yaml}---\n{rest}");

                if let Err(err) = fs::write(&fpath, updated) {
                    warn(&format!("migrate: error processing {fname}: {err}"));
                    result.skipped += 1;
                    continue;
                }
                result.backfilled += 1;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_month_dir(logs: &Path, year: &str, month: &str) -> std::path::PathBuf {
        let dir = logs.join("session").join(year).join(month);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session_log(dir: &Path, fname: &str, body_fm: &str) {
        let path = dir.join(fname);
        let content = if body_fm.is_empty() {
            "## Session\n\nTest content\n".to_string()
        } else {
            format!("---\n{body_fm}---\n\n## Session\n\nTest content\n")
        };
        fs::write(path, content).unwrap();
    }

    fn no_warn() -> impl FnMut(&str) {
        |_msg: &str| {}
    }

    // ---------------- parse_frontmatter_with_rest unit tests --------------

    #[test]
    fn parse_fm_crlf_normalises() {
        let (m, rest) = parse_frontmatter_with_rest("---\r\nkey: val\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(
            m.get(Value::String("key".into())).and_then(Value::as_str),
            Some("val")
        );
        assert!(rest.starts_with("body"));
    }

    #[test]
    fn parse_fm_eof_close_no_body() {
        // Closing `\n---` at end of file with no trailing newline.
        let (m, rest) = parse_frontmatter_with_rest("---\nkey: val\n---").unwrap();
        assert_eq!(
            m.get(Value::String("key".into())).and_then(Value::as_str),
            Some("val")
        );
        assert_eq!(rest, "");
    }

    #[test]
    fn parse_fm_body_dash_separator_not_a_closer() {
        let raw = "---\nkey: val\n---\n\n---some-separator\n\nMore body.\n";
        let (m, rest) = parse_frontmatter_with_rest(raw).unwrap();
        assert_eq!(
            m.get(Value::String("key".into())).and_then(Value::as_str),
            Some("val")
        );
        assert!(rest.contains("---some-separator"));
    }

    #[test]
    fn parse_fm_no_leading_dashes_returns_none() {
        assert!(parse_frontmatter_with_rest("hello body").is_none());
    }

    #[test]
    fn parse_fm_no_closing_delim_returns_none() {
        assert!(parse_frontmatter_with_rest("---\nkey: val\nbody").is_none());
    }

    #[test]
    fn parse_fm_root_sequence_returns_none() {
        // Valid YAML but not a mapping → reject.
        assert!(parse_frontmatter_with_rest("---\n- one\n- two\n---\nbody").is_none());
    }

    // ---------------- run_backfill_recapped tests ------------------------

    #[test]
    fn empty_logs_returns_zero_zero() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        fs::create_dir_all(&logs).unwrap();
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 0,
                skipped: 0
            }
        );
    }

    #[test]
    fn missing_logs_dir_returns_zero_zero() {
        let d = tempdir().unwrap();
        let logs = d.path().join("does-not-exist");
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 0,
                skipped: 0
            }
        );
    }

    #[test]
    fn skips_checkpoint_files() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-20-abc12345-checkpoint-01.md",
            "tags: checkpoint\ncheckpoint: '01'\nrecapped: false\n",
        );
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 0,
                skipped: 0
            }
        );
    }

    #[test]
    fn skips_files_with_recapped_present() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-20-session-01.md",
            "tags: session-log\nrecapped: '2026-04-20'\n",
        );
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 0,
                skipped: 0
            }
        );
    }

    #[test]
    fn skips_files_with_recapped_false() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-21-session-01.md",
            "tags: session-log\nrecapped: false\n",
        );
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 0,
                skipped: 0
            }
        );
    }

    #[test]
    fn adds_recapped_to_session_log_missing_it() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-20-session-01.md",
            "tags: session-log\ndate: '2026-04-20'\n",
        );
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 1,
                skipped: 0
            }
        );
        let after = fs::read_to_string(m.join("2026-04-20-session-01.md")).unwrap();
        assert!(after.contains("recapped: '2026-05-20'") || after.contains("recapped: 2026-05-20"));
    }

    #[test]
    fn malformed_frontmatter_skipped() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        // Missing closing ---
        fs::write(
            m.join("2026-04-20-session-01.md"),
            "---\ntags: session-log\ndate: 2026-04-20\n\n## Session\nContent\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let r = run_backfill_recapped(&logs, None, "2026-05-20", |msg| {
            warnings.push(msg.to_string())
        });
        assert!(r.skipped > 0);
        assert!(warnings.iter().any(|w| w.contains("malformed frontmatter")));
    }

    #[test]
    fn multi_month_walk() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m1 = make_month_dir(&logs, "2026", "04");
        let m2 = make_month_dir(&logs, "2026", "03");
        write_session_log(
            &m1,
            "2026-04-20-session-01.md",
            "tags: session-log\ndate: '2026-04-20'\n",
        );
        write_session_log(
            &m2,
            "2026-03-15-session-01.md",
            "tags: session-log\ndate: '2026-03-15'\n",
        );
        write_session_log(
            &m1,
            "2026-04-19-session-01.md",
            "tags: session-log\ndate: '2026-04-19'\nrecapped: '2026-04-19'\n",
        );
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 2,
                skipped: 0
            }
        );
    }

    #[test]
    fn no_frontmatter_skipped() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        fs::write(
            m.join("2026-04-20-session-01.md"),
            "## Session\n\nContent\n",
        )
        .unwrap();
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 0,
                skipped: 1
            }
        );
    }

    #[test]
    fn body_dash_separator_not_a_closer() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        let content = [
            "---",
            "tags: session-log",
            "date: '2026-04-20'",
            "---",
            "",
            "## Session",
            "",
            "---some-separator",
            "",
            "Content below separator.",
        ]
        .join("\n");
        fs::write(m.join("2026-04-20-session-01.md"), content).unwrap();
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 1,
                skipped: 0
            }
        );
        let after = fs::read_to_string(m.join("2026-04-20-session-01.md")).unwrap();
        assert!(after.contains("---some-separator"));
        assert!(after.contains("recapped:"));
    }

    #[test]
    fn preserves_existing_fields() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-20-session-01.md",
            "tags: session-log\ndate: '2026-04-20'\nfoo: bar\n",
        );
        let r = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r,
            MigrateResult {
                backfilled: 1,
                skipped: 0
            }
        );
        let after = fs::read_to_string(m.join("2026-04-20-session-01.md")).unwrap();
        // `serde_yaml::to_string` normalises scalar quoting (matches Bun's
        // `yaml.stringify`) — assert on the key=value semantics, not the
        // exact quoting style.
        assert!(after.contains("tags: session-log"));
        assert!(after.contains("date: 2026-04-20") || after.contains("date: '2026-04-20'"));
        assert!(after.contains("foo: bar"));
        assert!(after.contains("recapped:"));
    }

    #[cfg(unix)]
    #[test]
    fn write_failure_eaccess_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");

        write_session_log(
            &m,
            "2026-04-20-session-01.md",
            "tags: session-log\ndate: '2026-04-20'\n",
        );
        let file2 = m.join("2026-04-21-session-01.md");
        write_session_log(
            &m,
            "2026-04-21-session-01.md",
            "tags: session-log\ndate: '2026-04-21'\n",
        );
        let mut perms = fs::metadata(&file2).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&file2, perms).unwrap();

        let mut warnings = Vec::new();
        let r = run_backfill_recapped(&logs, None, "2026-05-20", |msg| {
            warnings.push(msg.to_string())
        });

        // Restore so tempdir cleanup works.
        let mut perms = fs::metadata(&file2).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&file2, perms).unwrap();

        assert_eq!(r.backfilled, 1);
        assert_eq!(r.skipped, 1);
        assert!(warnings
            .iter()
            .any(|w| w.starts_with("migrate: error processing")));
    }

    #[test]
    fn cutoff_skips_newer_inclusive() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-20-session-01.md",
            "tags: session-log\ndate: '2026-04-20'\n",
        );
        write_session_log(
            &m,
            "2026-04-25-session-01.md",
            "tags: session-log\ndate: '2026-04-25'\n",
        );
        let r = run_backfill_recapped(&logs, Some("2026-04-22"), "2026-05-20", no_warn());
        assert_eq!(r.backfilled, 1);
        assert_eq!(r.skipped, 0);

        let older = fs::read_to_string(m.join("2026-04-20-session-01.md")).unwrap();
        let newer = fs::read_to_string(m.join("2026-04-25-session-01.md")).unwrap();
        assert!(older.contains("recapped:"));
        assert!(!newer.contains("recapped:"));
    }

    #[test]
    fn cutoff_boundary_equal_backfills() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-22-session-01.md",
            "tags: session-log\ndate: '2026-04-22'\n",
        );
        let r = run_backfill_recapped(&logs, Some("2026-04-22"), "2026-05-20", no_warn());
        assert_eq!(r.backfilled, 1);
    }

    #[test]
    fn parse_fm_yaml_parse_error_returns_none() {
        // An unclosed YAML flow mapping "{" is syntactically invalid.
        // serde_yaml returns Err → .ok()? propagates None before the match.
        assert!(parse_frontmatter_with_rest("---\n{\n---\nbody").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_failure_increments_skipped() {
        // Covers the fs::read_to_string Err arm in run_backfill_recapped.
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "05");
        let fpath = m.join("2026-05-01-session-01.md");
        fs::write(&fpath, "---\ntags: session-log\n---\nContent\n").unwrap();
        let mut perms = fs::metadata(&fpath).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&fpath, perms.clone()).unwrap();
        let mut warnings = Vec::new();
        let r = run_backfill_recapped(&logs, None, "2026-06-01", |msg| {
            warnings.push(msg.to_string())
        });
        // Restore before assertion so tempdir cleanup succeeds.
        perms.set_mode(0o644);
        fs::set_permissions(&fpath, perms).unwrap();
        assert_eq!(r.skipped, 1);
        assert_eq!(r.backfilled, 0);
        assert!(warnings.iter().any(|w| w.contains("error processing")));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_month_dir_produces_no_files() {
        // list_md_files returns Vec::new() when fs::read_dir fails on the dir.
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "05");
        fs::write(
            m.join("2026-05-01-session-01.md"),
            "---\ntags: session-log\n---\nContent\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&m).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&m, perms.clone()).unwrap();
        let r = run_backfill_recapped(&logs, None, "2026-06-01", no_warn());
        // Restore before assertion so tempdir cleanup succeeds.
        perms.set_mode(0o755);
        fs::set_permissions(&m, perms).unwrap();
        assert_eq!(r.backfilled, 0);
        assert_eq!(r.skipped, 0);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_year_dir_is_skipped() {
        // Covers the `Err(_) => continue` branch when fs::read_dir on a year
        // directory fails.
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let year_path = logs.join("session").join("2026");
        let month_path = year_path.join("04");
        fs::create_dir_all(&month_path).unwrap();
        fs::write(
            month_path.join("2026-04-01-session-01.md"),
            "---\ntags: session-log\n---\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&year_path).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&year_path, perms.clone()).unwrap();
        let r = run_backfill_recapped(&logs, None, "2026-06-01", no_warn());
        // Restore before assertion.
        perms.set_mode(0o755);
        fs::set_permissions(&year_path, perms).unwrap();
        assert_eq!(r.backfilled, 0);
        assert_eq!(r.skipped, 0);
    }

    #[test]
    fn idempotent_rerun() {
        let d = tempdir().unwrap();
        let logs = d.path().join("07-logs");
        let m = make_month_dir(&logs, "2026", "04");
        write_session_log(
            &m,
            "2026-04-20-session-01.md",
            "tags: session-log\ndate: '2026-04-20'\n",
        );
        let r1 = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r1,
            MigrateResult {
                backfilled: 1,
                skipped: 0
            }
        );
        let r2 = run_backfill_recapped(&logs, None, "2026-05-20", no_warn());
        assert_eq!(
            r2,
            MigrateResult {
                backfilled: 0,
                skipped: 0
            }
        );
    }
}
