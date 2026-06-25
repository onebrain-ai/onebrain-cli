//! Integration tests for `note edit`, `note delete`, and `note mkdir`.
//! Drives the real compiled `onebrain` binary via `assert_cmd`.

use assert_cmd::Command;
use tempfile::tempdir;

/// Minimal vault fixture: `onebrain.yml` is the only thing vault_ctx::require
/// needs to detect the vault root.
fn vault() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("onebrain.yml"),
        "folders:\n  inbox: 00-inbox\n",
    )
    .unwrap();
    dir
}

#[test]
fn note_edit_overwrites_body() {
    let dir = vault();
    let root = dir.path();
    std::fs::write(root.join("a.md"), "old").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "note",
            "edit",
            "a.md",
            "new body",
            "--vault",
            root.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(root.join("a.md")).unwrap(),
        "new body"
    );
}

#[test]
fn note_edit_creates_when_missing() {
    let dir = vault();
    let root = dir.path();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "note",
            "edit",
            "sub/new.md",
            "hello",
            "--vault",
            root.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(root.join("sub/new.md")).unwrap(),
        "hello"
    );
}

#[test]
fn note_delete_moves_to_trash() {
    let dir = vault();
    let root = dir.path();
    std::fs::write(root.join("a.md"), "x").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["note", "delete", "a.md", "--vault", root.to_str().unwrap()])
        .assert()
        .success();
    assert!(!root.join("a.md").exists());
    assert!(root.join(".trash/a.md").exists());
}

#[test]
fn note_mkdir_creates_folder() {
    let dir = vault();
    let root = dir.path();
    Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "note",
            "mkdir",
            "01-projects/new",
            "--vault",
            root.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(root.join("01-projects/new").is_dir());
}
