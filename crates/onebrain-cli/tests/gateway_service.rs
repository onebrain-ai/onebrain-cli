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

/// A port nothing listens on, so the port-clash probe stays quiet.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[cfg(target_os = "macos")]
#[test]
fn service_install_and_uninstall_round_trip_without_touching_launchd() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let vault = root.path().join("vault");
    std::fs::create_dir_all(home.join(".onebrain/gateway")).unwrap();
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        home.join(".onebrain/gateway.yml"),
        format!(
            "port: {}\ndefault_vault: {}\npublic_url: 'https://brain.example.com'\n",
            free_port(),
            vault.display()
        ),
    )
    .unwrap();
    std::fs::write(home.join(".onebrain/gateway/tunnel.token"), FAKE_TOKEN).unwrap();
    let bin = fake_cloudflared(root.path());

    let out = onebrain(&home, Some(&bin))
        .args(["gateway", "service", "install"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let agents = home.join("Library/LaunchAgents");
    let gw = std::fs::read_to_string(agents.join("com.onebrain.gateway.plist")).unwrap();
    assert!(
        gw.contains(&format!(
            "<string>{}</string>",
            env!("CARGO_BIN_EXE_onebrain")
        )),
        "{gw}"
    );
    let tn = std::fs::read_to_string(agents.join("com.onebrain.gateway-tunnel.plist")).unwrap();
    assert!(tn.contains("<string>--token-file</string>"), "{tn}");
    // PATH here has no `onebrain`, so the plist names the build under test.
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_BIN_EXE_onebrain")));
    let log = home.join("Library/Logs/onebrain/gateway.log");
    assert_eq!(
        std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let out = onebrain(&home, Some(&bin))
        .args(["gateway", "service", "uninstall"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!agents.join("com.onebrain.gateway.plist").exists());
    assert!(home.join(".onebrain/gateway/tunnel.token").exists());
}

/// Hub ruling (T1 × T3b): the local probe and `service install`'s up-poll
/// send `Host: 127.0.0.1:<port>`; with T1's host guard merged, the REAL
/// binary must answer both discovery documents for that Host (loopback,
/// any port) — while `/approvals` stays loopback-only for a tunnel Host.
#[test]
fn well_known_answers_host_127_0_0_1_with_port_through_the_t1_guard() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let vault = root.path().join("vault");
    std::fs::create_dir_all(home.join(".onebrain")).unwrap();
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("onebrain.yml"), "folders: {}\n").unwrap();
    let port = free_port();
    std::fs::write(
        home.join(".onebrain/gateway.yml"),
        format!(
            "port: {port}\ndefault_vault: {}\npublic_url: 'https://brain.example.com'\n",
            vault.display()
        ),
    )
    .unwrap();
    let out_path = root.path().join("gw.out");
    let err_path = root.path().join("gw.err");
    let mut child = onebrain(&home, None)
        .env("ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL", "1")
        .env("ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX", "1")
        .current_dir(&vault)
        .args(["gateway", "run"])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !std::fs::read_to_string(&out_path)
        .unwrap_or_default()
        .contains("gateway listening on ")
    {
        assert!(
            child.try_wait().unwrap().is_none(),
            "gateway exited early: {}",
            support::redacted_capture_tail(&std::fs::read_to_string(&err_path).unwrap_or_default())
        );
        assert!(
            std::time::Instant::now() < deadline,
            "gateway did not start within 30s"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let host = format!("127.0.0.1:{port}");
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource",
    ] {
        let status = agent
            .get(&format!("http://127.0.0.1:{port}{path}"))
            .header("host", &host)
            .call()
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(status, 200, "{path} with Host {host}");
    }
    let status = agent
        .get(&format!("http://127.0.0.1:{port}/approvals"))
        .header("host", "brain.example.com")
        .call()
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 403, "/approvals must be loopback-only (T1)");
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(target_os = "macos"))]
#[test]
fn service_install_says_not_supported_yet_off_macos() {
    let home = tempfile::tempdir().unwrap();
    let out = onebrain(home.path(), None)
        .args(["gateway", "service", "install"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not supported yet on"), "{err}");
    assert!(!home.path().join("Library").exists());
}
