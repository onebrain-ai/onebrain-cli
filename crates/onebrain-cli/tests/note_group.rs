//! Layer-4 fixture-vault parity for the `note` resource group (v3.2.0).
//!
//! Builds a KNOWN fixture vault on disk, then drives the REAL compiled
//! `onebrain` binary against it (via `assert_cmd`), parsing each command's
//! `--json` output and asserting the canonical `Envelope<T>` contract:
//! `version == "1"`, `command == "note.<verb>"`, `ok == true`, and the
//! command-specific `data` payload.
//!
//! Why this exists: the v3.2.0 verbs each have library-level unit tests, but
//! those exercise the pure `onebrain_fs::note::*` functions — NOT the real
//! binary's arg parsing → vault resolution → envelope serialization →
//! exit-code path. The v3.1.5 update bug shipped precisely because that
//! real-binary path was never integration-tested (the validator only ever
//! ran against an injected mock). These tests close that gap for `note`.
//!
//! Each test builds a FRESH fixture (its own `TempDir`) so write verbs
//! (append/new/archive/move) can't pollute the read-verb fixtures and tests
//! stay independent + parallel-safe.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::{tempdir, TempDir};

// ─────────────────────────────────────────────────────────────────────────
// Fixture builder
// ─────────────────────────────────────────────────────────────────────────

/// Write `body` to `<root>/<rel>`, creating parent dirs as needed.
fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// Build a fixture vault with a KNOWN structure so every assertion below is
/// deterministic. Returns the owning `TempDir` (must outlive the command).
///
/// Layout:
/// - `onebrain.yml` — minimal valid config (vault detection)
/// - `03-knowledge/alpha.md` — `# Alpha`, `[[beta]]` link, 1 open + 1 done task, a `## Section`, a TODO line
/// - `03-knowledge/beta.md` — `# Beta` (target of alpha's `[[beta]]`)
/// - `01-projects/proj.md` — `# Proj` (an orphan in live notes)
/// - `00-inbox/raw.md` — inbox note (orphan-excluded)
/// - `06-archive/2026/old.md` — archived note linking `[[alpha]]` (backlink visible only with --include-archive)
/// - `07-logs/2026/05/x-checkpoint-01.md` — checkpoint-style file (find target)
///
/// Known counts (live notes only — `walk_notes` prunes `06-archive`):
/// - `03-knowledge/` contains exactly 2 notes (alpha, beta).
/// - Orphans (live, excluding inbox/logs/archive/TASKS/MOC): `alpha`, `proj`.
///   `beta` is NOT an orphan — `alpha.md` links `[[beta]]`. `alpha` IS an
///   orphan because only the ARCHIVED note links it (archive is pruned from
///   the link set).
fn build_fixture_vault() -> TempDir {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Minimal valid config. Vault resolution only needs the file to EXIST;
    // the folder keys are included for realism (the verbs themselves take
    // literal `--folder` paths and never read these keys).
    write(
        root,
        "onebrain.yml",
        "folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  knowledge: 03-knowledge\n  archive: 06-archive\n  logs: 07-logs\n",
    );

    write(
        root,
        "03-knowledge/alpha.md",
        "---\ntags: [topic, demo]\ncreated: 2026-05-20\n---\n\
# Alpha\n\
intro paragraph about TODO items\n\
\n\
see [[beta]] for the companion note\n\
\n\
## Section\n\
- [ ] open task 📅 2026-06-01\n\
- [x] done task\n\
plain trailing line\n",
    );

    write(root, "03-knowledge/beta.md", "# Beta\nthe companion note\n");

    write(root, "01-projects/proj.md", "# Proj\nproject body\n");

    write(root, "00-inbox/raw.md", "# Raw\nunprocessed thought\n");

    write(
        root,
        "06-archive/2026/old.md",
        "# Old\narchived note referencing [[alpha]]\n",
    );

    write(
        root,
        "07-logs/2026/05/x-checkpoint-01.md",
        "# checkpoint\nstop hook scratch\n",
    );

    dir
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

/// Run `onebrain <args> --vault <vault> --json`, assert exit 0, parse stdout
/// as JSON. `--vault` and `--json` are appended automatically.
fn run_note_json(vault: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::cargo_bin("onebrain").unwrap();
    cmd.args(args).arg("--vault").arg(vault).arg("--json");
    let output = cmd.output().expect("spawn failed");
    assert!(
        output.status.success(),
        "command {args:?} failed · status {:?} · stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 stdout");
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not valid JSON ({e}) · stdout was: {stdout}"))
}

/// Assert the canonical envelope wrapper: `version == "1"`, the expected
/// dotted `command`, and `ok == true`. Returns the `data` payload.
fn assert_envelope<'a>(env: &'a Value, command: &str) -> &'a Value {
    assert_eq!(env["version"], "1", "envelope version");
    assert_eq!(env["command"], command, "envelope command");
    assert_eq!(env["ok"], true, "envelope ok");
    assert!(
        env.get("vault").is_some(),
        "vault-required command must carry a vault field"
    );
    env.get("data")
        .unwrap_or_else(|| panic!("ok envelope for {command} must carry data"))
}

/// Collect the `path` field of every element of a JSON array into a Vec<String>.
fn paths_of(arr: &Value) -> Vec<String> {
    arr.as_array()
        .expect("expected JSON array")
        .iter()
        .map(|e| e["path"].as_str().expect("path is a string").to_string())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// 1. note search
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn search_finds_term_with_accurate_total() {
    let vault = build_fixture_vault();
    let env = run_note_json(vault.path(), &["note", "search", "companion"]);
    let data = assert_envelope(&env, "note.search");

    // "companion" appears once in alpha.md (the [[beta]] line) and once in
    // beta.md (the body) → exactly 2 matches.
    assert_eq!(data["total_found"], 2, "total_found");
    assert_eq!(data["truncated"], false, "not truncated");
    let matches = data["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    let paths = paths_of(&data["matches"]);
    assert!(paths.contains(&"03-knowledge/alpha.md".to_string()));
    assert!(paths.contains(&"03-knowledge/beta.md".to_string()));
    // Each match carries 1-based line + trimmed context + the H1 title.
    let beta_match = matches
        .iter()
        .find(|m| m["path"] == "03-knowledge/beta.md")
        .unwrap();
    assert_eq!(beta_match["title"], "Beta");
    assert!(beta_match["line"].as_u64().unwrap() >= 1);
    assert!(beta_match["context"]
        .as_str()
        .unwrap()
        .contains("companion"));
}

#[test]
fn search_folder_scopes_results() {
    let vault = build_fixture_vault();
    // "body" appears in proj.md (01-projects) and beta.md says nothing of the
    // sort; scope the search to 01-projects and confirm only proj.md matches.
    let env = run_note_json(
        vault.path(),
        &["note", "search", "project", "--folder", "01-projects"],
    );
    let data = assert_envelope(&env, "note.search");
    assert_eq!(data["total_found"], 1);
    assert_eq!(paths_of(&data["matches"]), vec!["01-projects/proj.md"]);
}

// ─────────────────────────────────────────────────────────────────────────
// 2. note list
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn list_folder_returns_expected_notes_and_total() {
    let vault = build_fixture_vault();
    let env = run_note_json(vault.path(), &["note", "list", "--folder", "03-knowledge"]);
    let data = assert_envelope(&env, "note.list");

    // 03-knowledge holds exactly alpha.md + beta.md.
    assert_eq!(data["total"], 2, "total notes under 03-knowledge");
    let mut paths = paths_of(&data["notes"]);
    paths.sort();
    assert_eq!(paths, vec!["03-knowledge/alpha.md", "03-knowledge/beta.md"]);
    // Title falls back through H1: alpha.md's H1 is "Alpha".
    let alpha = data["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["path"] == "03-knowledge/alpha.md")
        .unwrap();
    assert_eq!(alpha["title"], "Alpha");
    assert!(alpha["size_bytes"].as_u64().unwrap() > 0);
}

// ─────────────────────────────────────────────────────────────────────────
// 3. note find
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn find_glob_file_type_locates_checkpoint() {
    let vault = build_fixture_vault();
    // find walks the WHOLE vault (incl. 07-logs) — basename glob, files only.
    let env = run_note_json(
        vault.path(),
        &["note", "find", "*-checkpoint-*.md", "--type", "file"],
    );
    let data = assert_envelope(&env, "note.find");

    let paths = paths_of(&data["entries"]);
    assert_eq!(paths, vec!["07-logs/2026/05/x-checkpoint-01.md"]);
    assert_eq!(data["entries"][0]["is_dir"], false);
    assert_eq!(data["total"], 1);
}

#[test]
fn find_basename_match() {
    let vault = build_fixture_vault();
    // No `/` in the glob → basename match (like `find -name`). "alpha.md"
    // lives under 03-knowledge but the basename glob still finds it.
    let env = run_note_json(vault.path(), &["note", "find", "alpha.md"]);
    let data = assert_envelope(&env, "note.find");
    assert_eq!(paths_of(&data["entries"]), vec!["03-knowledge/alpha.md"]);
}

// ─────────────────────────────────────────────────────────────────────────
// 4. note read (section / frontmatter-only / tasks-only)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn read_section_extracts_block() {
    let vault = build_fixture_vault();
    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "read",
            "03-knowledge/alpha.md",
            "--section",
            "Section",
        ],
    );
    let data = assert_envelope(&env, "note.read");
    assert_eq!(data["mode"], "section");
    let content = data["content"].as_str().unwrap();
    assert!(content.starts_with("## Section"));
    assert!(content.contains("- [ ] open task"));
    assert!(content.contains("- [x] done task"));
}

#[test]
fn read_frontmatter_only() {
    let vault = build_fixture_vault();
    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "read",
            "03-knowledge/alpha.md",
            "--frontmatter-only",
        ],
    );
    let data = assert_envelope(&env, "note.read");
    assert_eq!(data["mode"], "frontmatter");
    assert!(data.get("content").is_none() || data["content"].is_null());
    let fm = &data["frontmatter"];
    assert_eq!(fm["created"], "2026-05-20");
    assert!(fm["tags"].is_array());
}

#[test]
fn read_tasks_only() {
    let vault = build_fixture_vault();
    let env = run_note_json(
        vault.path(),
        &["note", "read", "03-knowledge/alpha.md", "--tasks-only"],
    );
    let data = assert_envelope(&env, "note.read");
    assert_eq!(data["mode"], "tasks");
    let tasks: Vec<String> = data["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().any(|t| t.starts_with("- [ ] open task")));
    assert!(tasks.iter().any(|t| t == "- [x] done task"));
}

// ─────────────────────────────────────────────────────────────────────────
// 5. note stat
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn stat_reports_exact_counts() {
    let vault = build_fixture_vault();
    let env = run_note_json(vault.path(), &["note", "stat", "03-knowledge/alpha.md"]);
    let data = assert_envelope(&env, "note.stat");

    // alpha.md: 1 open + 1 done task; headings = `# Alpha` + `## Section` = 2;
    // wikilinks = `[[beta]]` = 1. (Frontmatter `---` lines are not headings.)
    assert_eq!(data["tasks_open"], 1, "tasks_open");
    assert_eq!(data["tasks_done"], 1, "tasks_done");
    assert_eq!(data["tasks_total"], 2, "tasks_total");
    assert_eq!(data["headings"], 2, "headings");
    assert_eq!(data["wikilinks"], 1, "wikilinks");
    assert!(data["size_bytes"].as_u64().unwrap() > 0);
    assert!(data["lines"].as_u64().unwrap() > 0);
    assert_eq!(data["path"], "03-knowledge/alpha.md");
}

// ─────────────────────────────────────────────────────────────────────────
// 6. note backlinks (default + --include-archive toggle)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn backlinks_reports_incoming_link() {
    let vault = build_fixture_vault();
    // beta.md is linked by alpha.md (`[[beta]]`), a live note.
    let env = run_note_json(vault.path(), &["note", "backlinks", "03-knowledge/beta.md"]);
    let data = assert_envelope(&env, "note.backlinks");
    assert_eq!(data["count"], 1, "beta has one live backlink");
    assert_eq!(data["target"], "03-knowledge/beta.md");
    assert_eq!(data["backlinks"][0]["source"], "03-knowledge/alpha.md");
}

#[test]
fn backlinks_include_archive_toggle() {
    let vault = build_fixture_vault();
    // alpha.md is linked ONLY by the archived note (`06-archive/2026/old.md`).
    // Default: 0 backlinks (archive pruned). With --include-archive: 1.
    let default_env = run_note_json(
        vault.path(),
        &["note", "backlinks", "03-knowledge/alpha.md"],
    );
    let default_data = assert_envelope(&default_env, "note.backlinks");
    assert_eq!(
        default_data["count"], 0,
        "archive backlink hidden by default"
    );

    let arch_env = run_note_json(
        vault.path(),
        &[
            "note",
            "backlinks",
            "03-knowledge/alpha.md",
            "--include-archive",
        ],
    );
    let arch_data = assert_envelope(&arch_env, "note.backlinks");
    assert_eq!(arch_data["count"], 1, "archive backlink revealed by flag");
    assert_eq!(
        arch_data["backlinks"][0]["source"],
        "06-archive/2026/old.md"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// 7. note orphans
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn orphans_lists_unlinked_excludes_linked_and_inbox_archive() {
    let vault = build_fixture_vault();
    let env = run_note_json(vault.path(), &["note", "orphans"]);
    let data = assert_envelope(&env, "note.orphans");

    let orphans: Vec<String> = data["orphans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect();

    // alpha + proj are orphans (no LIVE incoming links); beta is linked by
    // alpha so it's NOT an orphan; inbox/archive/logs are excluded.
    assert!(orphans.contains(&"03-knowledge/alpha.md".to_string()));
    assert!(orphans.contains(&"01-projects/proj.md".to_string()));
    assert!(
        !orphans.contains(&"03-knowledge/beta.md".to_string()),
        "linked note must not be an orphan"
    );
    assert!(
        !orphans.contains(&"00-inbox/raw.md".to_string()),
        "inbox is excluded"
    );
    assert!(
        !orphans.iter().any(|p| p.starts_with("06-archive")),
        "archive is excluded"
    );
    assert!(
        !orphans.iter().any(|p| p.starts_with("07-logs")),
        "logs are excluded"
    );
    assert_eq!(data["total"], 2, "exactly two live orphans");
}

// ─────────────────────────────────────────────────────────────────────────
// 8. note append (write verb) — then re-read to confirm it landed
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn append_under_section_then_read_back() {
    let vault = build_fixture_vault();
    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "append",
            "03-knowledge/alpha.md",
            "freshly appended marker",
            "--section",
            "Section",
        ],
    );
    let data = assert_envelope(&env, "note.append");
    assert_eq!(data["section"], "Section");
    assert_eq!(data["created_section"], false);
    assert!(data["bytes_appended"].as_u64().unwrap() > 0);

    // Re-read the Section to confirm the appended line landed inside it (and
    // not at EOF) — it must sit alongside the section's existing task lines.
    let read_env = run_note_json(
        vault.path(),
        &[
            "note",
            "read",
            "03-knowledge/alpha.md",
            "--section",
            "Section",
        ],
    );
    let read_data = assert_envelope(&read_env, "note.read");
    let section = read_data["content"].as_str().unwrap();
    assert!(section.contains("freshly appended marker"));
    assert!(section.starts_with("## Section"));
}

#[test]
fn append_accepts_leading_dash_task_line() {
    // Regression: Markdown task/list lines start with `-`. `NoteAppendArgs.content`
    // uses `allow_hyphen_values` so `note append <path> "- [ ] …"` parses the line
    // as content instead of clap rejecting it as an unknown flag (the OneBrain
    // agent appends task lines constantly).
    let vault = build_fixture_vault();
    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "append",
            "03-knowledge/alpha.md",
            "- [ ] dash-led task appended verbatim",
            "--section",
            "Section",
        ],
    );
    let data = assert_envelope(&env, "note.append");
    assert!(data["bytes_appended"].as_u64().unwrap() > 0);

    let read_env = run_note_json(
        vault.path(),
        &["note", "read", "03-knowledge/alpha.md", "--tasks-only"],
    );
    let tasks = assert_envelope(&read_env, "note.read");
    let task_lines = tasks["tasks"].as_array().unwrap();
    assert!(task_lines
        .iter()
        .any(|t| t.as_str() == Some("- [ ] dash-led task appended verbatim")));
}

// ─────────────────────────────────────────────────────────────────────────
// 9. note new (write verb) — then stat the created file
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn new_creates_note_with_frontmatter_then_stat() {
    let vault = build_fixture_vault();
    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "new",
            "03-knowledge/fresh-topic.md",
            "--frontmatter",
            "tags=demo",
        ],
    );
    let data = assert_envelope(&env, "note.new");
    assert_eq!(data["created"], true);
    assert_eq!(data["overwritten"], false);
    assert_eq!(data["path"], "03-knowledge/fresh-topic.md");

    // The file exists on disk.
    assert!(vault.path().join("03-knowledge/fresh-topic.md").exists());

    // And stat'ing it through the binary succeeds (title H1 from the path stem).
    let stat_env = run_note_json(
        vault.path(),
        &["note", "stat", "03-knowledge/fresh-topic.md"],
    );
    let stat_data = assert_envelope(&stat_env, "note.stat");
    assert_eq!(stat_data["path"], "03-knowledge/fresh-topic.md");
    // Minimal note has a `# fresh-topic` H1 → 1 heading.
    assert_eq!(stat_data["headings"], 1);

    // Frontmatter carries the user tag.
    let fm_env = run_note_json(
        vault.path(),
        &[
            "note",
            "read",
            "03-knowledge/fresh-topic.md",
            "--frontmatter-only",
        ],
    );
    let fm_data = assert_envelope(&fm_env, "note.read");
    assert_eq!(fm_data["frontmatter"]["tags"], "demo");
}

// ─────────────────────────────────────────────────────────────────────────
// 10. note archive (write verb)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn archive_moves_into_dated_bucket() {
    let vault = build_fixture_vault();
    let env = run_note_json(vault.path(), &["note", "archive", "01-projects/proj.md"]);
    let data = assert_envelope(&env, "note.archive");

    assert_eq!(data["from"], "01-projects/proj.md");
    let to = data["to"].as_str().unwrap();
    // Destination is 06-archive/YYYY/MM/proj.md (current UTC date).
    let re = regex_lite(to);
    assert!(
        re,
        "archive `to` must match 06-archive/YYYY/MM/proj.md, got {to}"
    );
    // Source gone, destination present on disk.
    assert!(!vault.path().join("01-projects/proj.md").exists());
    assert!(vault.path().join(to).exists());
}

/// Tiny path-shape check (avoids pulling the `regex` crate as a dev-dep): the
/// archive destination must be `06-archive/<4-digit>/<2-digit>/proj.md`.
fn regex_lite(p: &str) -> bool {
    let parts: Vec<&str> = p.split('/').collect();
    parts.len() == 4
        && parts[0] == "06-archive"
        && parts[1].len() == 4
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && parts[2].len() == 2
        && parts[2].chars().all(|c| c.is_ascii_digit())
        && parts[3] == "proj.md"
}

// ─────────────────────────────────────────────────────────────────────────
// 11. note move (write verb) — real rewrite + --dry-run no-op
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn move_rewrites_links_and_updates_files() {
    let vault = build_fixture_vault();
    // Move beta.md → beta-renamed.md. alpha.md links `[[beta]]` and must be
    // rewritten to `[[beta-renamed]]`.
    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "move",
            "03-knowledge/beta.md",
            "03-knowledge/beta-renamed.md",
        ],
    );
    let data = assert_envelope(&env, "note.move");

    assert_eq!(data["from"], "03-knowledge/beta.md");
    assert_eq!(data["to"], "03-knowledge/beta-renamed.md");
    assert_eq!(data["dry_run"], false);
    assert!(
        data["links_rewritten"].as_u64().unwrap() > 0,
        "at least one link rewritten"
    );
    assert!(
        data["files_updated"].as_u64().unwrap() > 0,
        "at least one file updated"
    );

    // On disk: file moved, alpha's link rewritten.
    assert!(!vault.path().join("03-knowledge/beta.md").exists());
    assert!(vault.path().join("03-knowledge/beta-renamed.md").exists());
    let alpha = fs::read_to_string(vault.path().join("03-knowledge/alpha.md")).unwrap();
    assert!(alpha.contains("[[beta-renamed]]"));
    assert!(!alpha.contains("[[beta]]"));
}

#[test]
fn move_dry_run_changes_nothing_on_disk() {
    let vault = build_fixture_vault();
    let beta_before = fs::read_to_string(vault.path().join("03-knowledge/beta.md")).unwrap();
    let alpha_before = fs::read_to_string(vault.path().join("03-knowledge/alpha.md")).unwrap();

    let env = run_note_json(
        vault.path(),
        &[
            "note",
            "move",
            "03-knowledge/beta.md",
            "03-knowledge/beta-renamed.md",
            "--dry-run",
        ],
    );
    let data = assert_envelope(&env, "note.move");
    assert_eq!(data["dry_run"], true);
    // The plan still reports what WOULD change.
    assert!(data["links_rewritten"].as_u64().unwrap() > 0);

    // Nothing actually moved or rewritten.
    assert!(vault.path().join("03-knowledge/beta.md").exists());
    assert!(!vault.path().join("03-knowledge/beta-renamed.md").exists());
    let beta_after = fs::read_to_string(vault.path().join("03-knowledge/beta.md")).unwrap();
    let alpha_after = fs::read_to_string(vault.path().join("03-knowledge/alpha.md")).unwrap();
    assert_eq!(beta_before, beta_after, "beta unchanged");
    assert_eq!(alpha_before, alpha_after, "alpha unchanged (link intact)");
}

// ─────────────────────────────────────────────────────────────────────────
// 12. cross-cutting: vault-required (no onebrain.yml → exit 64)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn verb_without_vault_exits_64() {
    let empty = tempdir().unwrap(); // no onebrain.yml
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["note", "list", "--vault"])
        .arg(empty.path())
        .arg("--json")
        .assert()
        .failure()
        .code(64);
}

// ─────────────────────────────────────────────────────────────────────────
// 13. cross-cutting: bad input (invalid regex / glob → exit 71)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn search_invalid_regex_exits_71() {
    let vault = build_fixture_vault();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["note", "search", "(", "--mode", "regex", "--vault"])
        .arg(vault.path())
        .arg("--json")
        .assert()
        .failure()
        .code(71)
        .stdout(predicate::str::contains("E_INVALID_TARGET"));
}

#[test]
fn find_invalid_glob_exits_71() {
    let vault = build_fixture_vault();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["note", "find", "[", "--vault"])
        .arg(vault.path())
        .arg("--json")
        .assert()
        .failure()
        .code(71);
}
