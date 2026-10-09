//! `onebrain gateway tunnel ...` / `gateway service ...` through the real
//! binary (v3.5.0 T3, #412). HOME is a tempdir; launchd is never touched
//! (`ONEBRAIN_SCHEDULER_NO_ACTIVATE=1`); `cloudflared` is a shell-script fake.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod support;

/// Fake token - base64 JSON with a non-secret `s`. Never a real credential.
const FAKE_TOKEN: &str = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmZmNDJhZTItNzY1ZC00YWRmLTgxMTItMzFjNTVjMTU1MWVmIiwicyI6ImJtOTBMV0V0Y21WaGJDMXpaV055WlhRPSJ9";

/// A `cloudflared` on PATH whose `tunnel run --help` advertises `--token-file`.
fn fake_cloudflared(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let p = bin.join("cloudflared");
    std::fs::write(
        &p,
        "#!/bin/sh\necho '   --token-file value   Filepath at which to read the tunnel token'\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn onebrain(home: &Path, path_dir: Option<&Path>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .env_remove("ONEBRAIN_VAULT");
    if let Some(d) = path_dir {
        c.env("PATH", format!("{}:/usr/bin:/bin", d.display()));
    }
    c
}

#[test]
fn tunnel_setup_over_piped_stdin_stores_the_token_and_public_url() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bin = fake_cloudflared(root.path());
    let mut child = onebrain(&home, Some(&bin))
        .args(["gateway", "tunnel", "setup"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("brain.example.com\n{FAKE_TOKEN}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains(FAKE_TOKEN) && !stderr.contains(FAKE_TOKEN));
    let token = home.join(".onebrain/gateway/tunnel.token");
    assert_eq!(std::fs::read_to_string(&token).unwrap(), FAKE_TOKEN);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&token).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A fresh file is serialized by serde_yaml, so compare the parsed value,
    // not the quoting.
    let yml: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(home.join(".onebrain/gateway.yml")).unwrap())
            .unwrap();
    assert_eq!(
        yml["public_url"].as_str(),
        Some("https://brain.example.com")
    );
}
