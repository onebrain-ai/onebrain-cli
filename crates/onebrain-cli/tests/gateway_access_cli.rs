//! `onebrain gateway tokens list|revoke` + `gateway clients list|remove`
//! (#406): binary-level tests. No gateway server is spawned. These verbs only
//! touch the on-disk store under `$HOME/.onebrain/gateway/`, sandboxed here
//! to a tempdir HOME. The live-revocation proof against a RUNNING gateway is
//! `gateway_oauth_e2e.rs::gateway_tokens_revoke_takes_effect_on_the_running_gateways_next_request`.
//!
//! Security rule for every assertion below (CodeQL `rust/cleartext-logging`):
//! planted token/family values are credential-shaped. Never interpolate one
//! into an assertion message.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::tempdir;

mod support;

fn onebrain(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_onebrain"))
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("ONEBRAIN_VAULT")
        .current_dir(home)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn onebrain {args:?}: {e}"))
}

fn gateway_dir(home: &Path) -> PathBuf {
    home.join(".onebrain").join("gateway")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Same algorithm as `store.rs::display_id` (copied: bin-only crate, no lib target).
/// Unused until the revoke/remove tests land (Task 5).
#[allow(dead_code)]
fn display_id(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(12);
    for &b in &digest[..6] {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[test]
fn tokens_list_on_a_fresh_home_is_empty_not_an_error() {
    let home = tempdir().unwrap();
    let out = onebrain(home.path(), &["gateway", "tokens", "list"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("no live tokens"), "{}", stdout(&out));

    let out = onebrain(home.path(), &["gateway", "tokens", "list", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["command"], "gateway.tokens.list");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["tokens"].as_array().unwrap().len(), 0);
    assert!(gateway_dir(home.path()).is_dir());
}
