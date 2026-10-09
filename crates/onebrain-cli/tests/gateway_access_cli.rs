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
        .unwrap_or_else(|e| {
            panic!(
                "spawn onebrain ({} args, verb `{}`): {e}",
                args.len(),
                args.get(2).copied().unwrap_or("")
            )
        })
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

const ACCESS: &str = "access-token-value-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const REFRESH: &str = "refresh-token-value-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
const NEXT_REFRESH: &str = "next-refresh-value-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
const FAMILY: &str = "family-value-DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
const OTHER: &str = "other-client-token-EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";
const CODE: &str = "pending-code-value-FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
const SECRETS: [&str; 6] = [ACCESS, REFRESH, NEXT_REFRESH, FAMILY, OTHER, CODE];

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Plants the store in the exact on-disk shape `store.rs` persists
/// (`TokenKind`/`AppType` lowercase): c-target holds a live access token, a
/// rotated+revoked refresh, and its live successor, all in FAMILY, plus one
/// pending code. c-other holds one live access token. c-target's
/// self-registered name carries an ANSI escape.
fn plant(home: &Path) {
    let dir = gateway_dir(home);
    std::fs::create_dir_all(&dir).unwrap();
    let n = now();
    let tok = |token: &str,
               kind: &str,
               family: &str,
               client: &str,
               revoked: bool,
               rotated_to: Option<&str>| {
        serde_json::json!({
            "token": token, "kind": kind, "family": family, "client_id": client,
            "scope": "brain", "expires": n + 3600, "revoked": revoked, "rotated_to": rotated_to,
        })
    };
    let tokens = serde_json::json!({
        ACCESS: tok(ACCESS, "access", FAMILY, "c-target", false, None),
        REFRESH: tok(REFRESH, "refresh", FAMILY, "c-target", true, Some(NEXT_REFRESH)),
        NEXT_REFRESH: tok(NEXT_REFRESH, "refresh", FAMILY, "c-target", false, None),
        OTHER: tok(OTHER, "access", "other-family", "c-other", false, None),
    });
    let client = |id: &str, name: &str| {
        serde_json::json!({
            "client_id": id, "client_name": name,
            "redirect_uris": ["http://127.0.0.1/callback"],
            "application_type": "native", "created": n,
        })
    };
    let clients = serde_json::json!({
        "c-other": client("c-other", "Other"),
        "c-target": client("c-target", "Target\u{1b}[31mRED"),
    });
    let codes = serde_json::json!({
        CODE: {
            "code": CODE, "client_id": "c-target", "redirect_uri": "http://127.0.0.1/callback",
            "code_challenge": "chal", "resource": "res", "scope": "brain",
            "expires": n + 600, "used": false, "minted_family": null,
        }
    });
    for (name, value) in [
        ("tokens.json", tokens),
        ("clients.json", clients),
        ("codes.json", codes),
    ] {
        std::fs::write(dir.join(name), serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
}

fn read_json(home: &Path, name: &str) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(gateway_dir(home).join(name)).unwrap()).unwrap()
}

fn assert_no_secret(out: &Output, what: &str) {
    let both = format!("{}{}", stdout(out), stderr(out));
    for s in SECRETS {
        assert!(
            !both.contains(s),
            "a raw token/family/code value leaked into `{what}` output"
        );
    }
}

#[test]
fn tokens_list_never_prints_a_token_value_in_text_or_json() {
    let home = tempdir().unwrap();
    plant(home.path());
    for args in [
        vec!["gateway", "tokens", "list"],
        vec!["gateway", "tokens", "list", "--all"],
        vec!["gateway", "tokens", "list", "--all", "--json"],
        vec!["gateway", "clients", "list", "--json"],
    ] {
        let out = onebrain(home.path(), &args);
        assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
        assert_no_secret(&out, &args.join(" "));
    }
    let live = stdout(&onebrain(home.path(), &["gateway", "tokens", "list"]));
    assert!(live.contains(&display_id(ACCESS)));
    assert!(live.contains(&display_id(FAMILY)));
    assert!(
        !live.contains(&display_id(REFRESH)),
        "revoked token must be hidden by default"
    );
    let all = stdout(&onebrain(
        home.path(),
        &["gateway", "tokens", "list", "--all"],
    ));
    assert!(all.contains(&display_id(REFRESH)));

    let v: serde_json::Value = serde_json::from_str(&stdout(&onebrain(
        home.path(),
        &["gateway", "tokens", "list", "--all", "--json"],
    )))
    .unwrap();
    assert_eq!(v["command"], "gateway.tokens.list");
    assert_eq!(v["data"]["tokens"].as_array().unwrap().len(), 4);
    assert_eq!(v["data"]["hidden"], 0);
}

#[test]
fn tokens_revoke_by_id_prefix_flips_exactly_that_token_on_disk() {
    let home = tempdir().unwrap();
    plant(home.path());
    let id = display_id(ACCESS);
    let out = onebrain(home.path(), &["gateway", "tokens", "revoke", &id[..6]]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains(&id));
    assert!(
        stdout(&out).contains("💡"),
        "single-token revoke must print the cut-off hint"
    );
    assert_no_secret(&out, "tokens revoke <id>");
    let tokens = read_json(home.path(), "tokens.json");
    assert_eq!(tokens[ACCESS]["revoked"], true);
    assert_eq!(tokens[NEXT_REFRESH]["revoked"], false);
    assert_eq!(tokens[OTHER]["revoked"], false);
}

#[test]
fn tokens_revoke_rejects_a_pasted_raw_token_without_echoing_it() {
    let home = tempdir().unwrap();
    plant(home.path());
    let before = std::fs::read(gateway_dir(home.path()).join("tokens.json")).unwrap();

    let out = onebrain(home.path(), &["gateway", "tokens", "revoke", ACCESS]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("✗ nothing revoked"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("💡"));
    assert_no_secret(&out, "tokens revoke <raw token>");

    let out = onebrain(
        home.path(),
        &["gateway", "tokens", "revoke", ACCESS, "--json"],
    );
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["ok"], false);
    assert!(!v["error"]["message"].as_str().unwrap().contains('✗'));
    assert_no_secret(&out, "tokens revoke <raw token> --json");

    let after = std::fs::read(gateway_dir(home.path()).join("tokens.json")).unwrap();
    assert!(
        before == after,
        "a rejected revoke must not touch tokens.json"
    );
}

#[test]
fn tokens_revoke_requires_exactly_one_selector() {
    let home = tempdir().unwrap();
    for args in [
        vec!["gateway", "tokens", "revoke"],
        vec!["gateway", "tokens", "revoke", "abcd", "--client", "c1"],
    ] {
        assert_eq!(
            onebrain(home.path(), &args).status.code(),
            Some(2),
            "{args:?}"
        );
    }
}

#[test]
fn tokens_revoke_family_and_client_are_scoped_bulk_cut_offs() {
    let home = tempdir().unwrap();
    plant(home.path());
    let fam = display_id(FAMILY);
    let out = onebrain(
        home.path(),
        &["gateway", "tokens", "revoke", "--family", &fam[..8]],
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let tokens = read_json(home.path(), "tokens.json");
    assert_eq!(tokens[ACCESS]["revoked"], true);
    assert_eq!(tokens[NEXT_REFRESH]["revoked"], true);
    assert_eq!(tokens[OTHER]["revoked"], false);

    let out = onebrain(
        home.path(),
        &[
            "gateway", "tokens", "revoke", "--client", "c-other", "--json",
        ],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["command"], "gateway.tokens.revoke");
    assert_eq!(v["data"]["selector"], "client");
    assert_eq!(
        read_json(home.path(), "tokens.json")[OTHER]["revoked"],
        true
    );

    let out = onebrain(
        home.path(),
        &["gateway", "tokens", "revoke", "--client", "nobody"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("nothing revoked — no tokens found for that client id"));
    assert!(
        !stderr(&out).contains("nobody"),
        "the client value must not be echoed"
    );
}

#[test]
fn clients_list_neutralizes_control_characters_and_counts_live_tokens() {
    let home = tempdir().unwrap();
    plant(home.path());
    let text = stdout(&onebrain(home.path(), &["gateway", "clients", "list"]));
    assert!(
        !text.contains('\u{1b}'),
        "ANSI escape from a client_name reached the terminal"
    );
    assert!(text.contains("c-target"));
    let v: serde_json::Value = serde_json::from_str(&stdout(&onebrain(
        home.path(),
        &["gateway", "clients", "list", "--json"],
    )))
    .unwrap();
    let clients = v["data"]["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 2);
    assert_eq!(clients[0]["client_id"], "c-other");
    assert_eq!(clients[0]["live_tokens"], 1);
    assert_eq!(clients[1]["live_tokens"], 2);
}

#[test]
fn clients_remove_revokes_tokens_deletes_codes_and_is_not_repeatable() {
    let home = tempdir().unwrap();
    plant(home.path());
    let out = onebrain(home.path(), &["gateway", "clients", "remove", "c-target"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("revoked 2 tokens, deleted 1 pending authorization code"));
    assert_no_secret(&out, "clients remove");

    let clients = read_json(home.path(), "clients.json");
    assert!(clients.get("c-target").is_none());
    assert!(clients.get("c-other").is_some());
    let tokens = read_json(home.path(), "tokens.json");
    for t in [ACCESS, REFRESH, NEXT_REFRESH] {
        assert_eq!(tokens[t]["revoked"], true);
    }
    assert_eq!(tokens[OTHER]["revoked"], false);
    assert_eq!(read_json(home.path(), "codes.json"), serde_json::json!({}));

    let again = onebrain(home.path(), &["gateway", "clients", "remove", "c-target"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(stderr(&again).contains("💡 run `onebrain gateway clients list`"));
}
