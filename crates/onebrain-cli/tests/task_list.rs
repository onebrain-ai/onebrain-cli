//! E2E: `onebrain task list` against a real temp vault, asserting fenced demo
//! tasks are excluded and JSON shape is stable.

use std::process::Command;
use tempfile::tempdir;

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[test]
fn task_list_json_excludes_fenced_and_respects_due_by() {
    let dir = tempdir().unwrap();
    // Search-cache isolation, mandatory for any test that names a collection
    // and then runs the binary: several commands OPEN a collection that already
    // exists under the resolved cache root, and opening is not read-only. Named
    // `t` here, so without this the child reaches the developer's real index the
    // moment a collection called `t` exists. Enforced by
    // `tests/cache_isolation_sweep.rs`.
    let cache = tempdir().unwrap();
    let root = dir.path();
    write(root, "onebrain.yml", "qmd_collection: t\n");
    write(
        root,
        "01-projects/p.md",
        "- [ ] overdue real 📅 2026-06-01\n\
         ```\n- [ ] fenced demo 📅 2026-06-01\n```\n\
         - [ ] future 📅 2999-01-01\n",
    );

    let out = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args([
            "--vault",
            root.to_str().unwrap(),
            "--json",
            "task",
            "list",
            "--due-by",
            "2026-06-29",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tasks = v["data"]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "only the overdue non-fenced task: {v}");
    assert_eq!(tasks[0]["text"], "overdue real");
}

#[test]
fn task_list_limit_keeps_full_total_and_deterministic_order() {
    let dir = tempdir().unwrap();
    let cache = tempdir().unwrap();
    let root = dir.path();
    write(root, "onebrain.yml", "qmd_collection: t\n");
    write(
        root,
        "01-projects/b.md",
        "- [ ] sixth 📅 2026-06-06\n\
         - [ ] second-b 📅 2026-06-02\n",
    );
    write(
        root,
        "01-projects/a.md",
        "- [ ] fifth 📅 2026-06-05\n\
         - [ ] second-a-first 📅 2026-06-02\n\
         - [ ] first 📅 2026-06-01\n\
         - [ ] seventh 📅 2026-06-07\n\
         - [ ] second-a-second 📅 2026-06-02\n",
    );

    let out = Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args([
            "--vault",
            root.to_str().unwrap(),
            "--json",
            "task",
            "list",
            "--limit",
            "5",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tasks = v["data"]["tasks"].as_array().unwrap();
    let texts: Vec<&str> = tasks
        .iter()
        .map(|task| task["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            "first",
            "second-a-first",
            "second-a-second",
            "second-b",
            "fifth",
        ]
    );
    assert_eq!(v["data"]["total"], 7);
}
