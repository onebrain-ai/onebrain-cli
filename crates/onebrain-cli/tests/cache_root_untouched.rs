//! Regression guard for #300: running the binary against a vault that has
//! never been indexed must create **nothing** in the search-cache root.
//!
//! ## The leak this pins
//!
//! `onebrain serve` boots a token cache for its vault
//! (`server::token_api::try_open_held_token_cache`). That function used to run
//! `create_dir_all(<cache_root>/<collection>/token)` unconditionally, and the
//! collection name for an unconfigured vault is generated from its directory —
//! so every `serve` against a throwaway tempdir vault minted a NEW permanent
//! directory in the developer's real cache root. `tests/serve_bind_order.rs`
//! did that twice per workspace test run; 125 such directories (124 MB) had
//! accumulated when it was found. The `token/`-only shape matched 117 of them.
//!
//! ## Why this test is not vacuous
//!
//! A guard that only asserts "nothing appeared" passes trivially whenever the
//! leaking code never runs — and that is a live risk here, because the cheapest
//! way to exercise `serve` is a bind that fails, which exits before reaching
//! the token cache at all. So this test:
//!
//! 1. binds for real (`--port 0`, kernel-assigned) and **waits for the banner**
//!    before asserting anything. The banner prints from `on_bind`, strictly
//!    after the daemon state — including the token cache — is constructed. If
//!    the banner never appears, the test fails instead of passing quietly.
//! 2. was verified by reverting the fix on a throwaway copy of the tree
//!    (restoring the unconditional `create_dir_all`) and confirming this test
//!    goes RED, naming the leaked `<collection>/token` directory.
//!
//! ## Platform coverage — stated plainly, not implied
//!
//! **This guard covers Unix only, and is `#[cfg(unix)]` for that reason.** It
//! works by pointing `HOME` at a tempdir and then inspecting that tempdir. On
//! Windows the state root comes from `dirs::data_dir()`, which reads `%APPDATA%`
//! through the Known Folders API and ignores `HOME` entirely (see
//! `src/migration.rs`, `default_state_dir`). CI runs `windows-latest`, so it is
//! worth being explicit: on that runner this test does not execute, and a
//! Windows-only regression of the same shape would NOT be caught here. It would
//! still be caught by `tests/cache_isolation_sweep.rs`, which is a static scan
//! and therefore platform-independent — that is the cross-platform half of the
//! defense, and this test is the behavioral half.
//!
//! Deliberately NOT fixed by setting `ONEBRAIN_CACHE_DIR`: that env var makes
//! `migrate_search_cache` early-return (ADR 0021 / #114), so pinning it here
//! would leave this test unable to observe the very code path it exists to
//! watch. `HOME` redirection keeps the production resolution intact.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// Both places `default_state_dir()` can land under a redirected `HOME` on
/// Unix: macOS's `~/Library/Application Support`, and the XDG default
/// `~/.local/share` used on Linux. The test clears `XDG_DATA_HOME` so the
/// latter is the only Linux possibility.
fn candidate_search_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library/Application Support/onebrain/search"),
        home.join(".local/share/onebrain/search"),
    ]
}

/// Every entry (including dotfiles — the leaked dirs are named after `.tmpXXXX`
/// tempdirs and so start with a dot) under each candidate root.
fn entries_under(home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for root in candidate_search_roots(home) {
        let Ok(rd) = fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            out.push(format!("{}", e.path().display()));
        }
    }
    out
}

/// RAII kill for the long-running `serve` subprocess (mirrors
/// `tests/serve_bind_order.rs`): a failed assertion must never leak a listener.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_against_a_never_indexed_vault_creates_nothing_in_the_cache_root() {
    let home = tempdir().unwrap();
    let vault = tempdir().unwrap();
    fs::write(vault.path().join("onebrain.yml"), "method: onebrain\n").unwrap();

    // Baseline: a pristine fake HOME has no search-cache root at all.
    assert!(
        entries_under(home.path()).is_empty(),
        "fresh tempdir HOME already had cache-root entries: {:?}",
        entries_under(home.path())
    );

    let stdout_path = vault.path().join("serve-stdout.log");
    let stdout_file = fs::File::create(&stdout_path).unwrap();
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("onebrain"))
        // NO `ONEBRAIN_CACHE_DIR` on purpose — this test's whole subject is
        // where the binary lands when nothing redirects it. `HOME` is the
        // redirection instead, so the real production resolution runs.
        .env("HOME", home.path())
        .env_remove("ONEBRAIN_CACHE_DIR")
        .env_remove("XDG_DATA_HOME")
        .env_remove("ONEBRAIN_VAULT")
        .env("ONEBRAIN_NO_DAEMON", "1")
        .current_dir(vault.path())
        .args(["serve", "--port", "0"])
        .stdout(stdout_file)
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn onebrain binary");
    let mut child = KillOnDrop(child);

    // Wait for a REAL bind. This is the non-vacuity gate: the banner prints
    // from `on_bind`, after the server state (token cache included) is built.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = fs::read_to_string(&stdout_path).unwrap_or_default();
        if s.contains("Ctrl-C to stop") {
            break;
        }
        if let Some(status) = child.0.try_wait().expect("probe serve child") {
            panic!(
                "serve exited early ({status}) — this guard asserts about a server that \
                 actually came up; a passing assertion below would be vacuous. stdout={s}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "serve never bound within 5s, so the leaking startup path never ran and this \
             guard would pass vacuously: stdout={s}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let leaked = entries_under(home.path());

    child.0.kill().expect("kill serve child");
    child.0.wait().expect("reap serve child");

    assert!(
        leaked.is_empty(),
        "`onebrain serve` created {} entr(ies) in the search-cache root of a vault it \
         never indexed (#300). Nothing may be created there until something actually \
         indexes the vault:\n  {}",
        leaked.len(),
        leaked.join("\n  ")
    );
}
