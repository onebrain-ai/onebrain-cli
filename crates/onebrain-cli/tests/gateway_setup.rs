//! `onebrain gateway setup` through the real binary (v3.5.2 T4, #423).
//! HOME is a tempdir; launchd is never touched
//! (`ONEBRAIN_SCHEDULER_NO_ACTIVATE=1`, which also skips the Quick Tunnel
//! reachability probe — no network); `cloudflared` is a shell-script fake on
//! PATH. Ports are OS-picked scratch ports — never the live 7717.
//!
//! Unix-only (`#!/bin/sh` fakes, SIGTERM); Windows is covered by the
//! `setup.rs` unit tests over `FakeCf` (red-team I6).

#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod support;

/// A `cloudflared` output sample from `cloudflare::fixtures` (a
/// `#[cfg(test)]` module the binary's integration tests cannot import), read
/// out of the source at compile time so a corrected fixture flows through.
fn cf_fixture(name: &str) -> String {
    const SRC: &str = include_str!("../src/commands/gateway/cloudflare.rs");
    let decl = format!("const {name}: &str =");
    let at = SRC
        .find(&decl)
        .unwrap_or_else(|| panic!("fixture {name} not in cloudflare.rs"))
        + decl.len();
    let rest = SRC[at..].trim_start();
    if let Some(raw) = rest.strip_prefix("r#\"") {
        return raw[..raw.find("\"#").expect("raw string end")].to_string();
    }
    let body = rest.strip_prefix('"').expect("a string literal");
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => return out,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\n') => {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                }
                other => panic!("unsupported escape {other:?} in {name}"),
            },
            c => out.push(c),
        }
    }
    panic!("unterminated fixture {name}")
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `<dir>/bin/cloudflared` logging its argv to `<dir>/argv.log`, then
/// running `body`. Returns the bin dir.
fn fake_cloudflared(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let p = bin.join("cloudflared");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\necho \"$*\" >> {}\n{body}\n",
            sh_quote(&dir.join("argv.log").display().to_string())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn argv(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn onebrain(home: &Path, path_dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_onebrain"));
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("ONEBRAIN_SCHEDULER_NO_ACTIVATE", "1")
        .env("PATH", format!("{}:/usr/bin:/bin", path_dir.display()))
        .env_remove("ONEBRAIN_VAULT");
    c
}

/// An OS-picked free loopback port (released before use; never 7717).
fn scratch_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    assert_ne!(port, 7717);
    port
}

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let s = Self {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(s.home().join(".onebrain")).unwrap();
        std::fs::create_dir_all(s.vault()).unwrap();
        std::fs::write(s.vault().join("onebrain.yml"), "").unwrap();
        s
    }
    fn dir(&self) -> &Path {
        self.root.path()
    }
    fn home(&self) -> PathBuf {
        self.dir().join("home")
    }
    fn vault(&self) -> PathBuf {
        self.dir().join("vault")
    }
    fn yml(&self) -> PathBuf {
        self.home().join(".onebrain/gateway.yml")
    }
}

/// (a) Own domain, piped: the cloudflared calls in order, gateway.yml and
/// the 0600 token, and no token on stdout/stderr.
#[test]
fn own_domain_over_piped_stdin_provisions_and_stores_the_tunnel() {
    let sb = Sandbox::new();
    let port = scratch_port();
    std::fs::write(sb.yml(), format!("port: {port}\n")).unwrap();
    let cert = sb.home().join(".cloudflared/cert.pem");
    std::fs::create_dir_all(cert.parent().unwrap()).unwrap();
    std::fs::write(&cert, "fake").unwrap();
    let token = cf_fixture("TOKEN_STDOUT");
    let bin = fake_cloudflared(
        sb.dir(),
        &format!(
            "case \"$1 $2\" in\n\
             'tunnel list') printf '%s' {list} ;;\n\
             'tunnel create') printf '%s' {create} >&2 ;;\n\
             'tunnel route') printf '%s' {route} >&2 ;;\n\
             'tunnel token') printf '%s' {token} ;;\n\
             'tunnel run') echo '   --token-file value   Filepath at which to read the tunnel token' ;;\n\
             esac",
            list = sh_quote(&cf_fixture("LIST_JSON_EMPTY")),
            create = sh_quote(&cf_fixture("CREATE_OUTPUT")),
            route = sh_quote(&cf_fixture("ROUTE_ADDED")),
            token = sh_quote(&token),
        ),
    );
    let mut child = onebrain(&sb.home(), &bin)
        .env("TUNNEL_ORIGIN_CERT", &cert)
        .current_dir(sb.vault())
        .args(["gateway", "setup"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Enter = the vault found from the cwd; 1 = own domain; the hostname
    // pasted as a URL; n = no Telegram.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\n1\nhttps://Brain.Example.com/\nn\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let token = token.trim();
    assert!(
        !stdout.contains(token) && !stderr.contains(token),
        "token printed"
    );

    let name = "onebrain-brain-example-com";
    let calls: Vec<String> = argv(sb.dir())
        .into_iter()
        .filter(|c| c != "tunnel run --help")
        .collect();
    assert_eq!(
        calls,
        [
            format!("tunnel list --name {name} -o json"),
            format!("tunnel create {name}"),
            format!("tunnel route dns {name} brain.example.com"),
            format!("tunnel token {name}"),
        ]
    );
    let yml: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(sb.yml()).unwrap()).unwrap();
    assert_eq!(
        yml["public_url"].as_str(),
        Some("https://brain.example.com")
    );
    assert_eq!(yml["tunnel_ingress"].as_str(), Some("local"));
    assert_eq!(yml["port"].as_u64(), Some(u64::from(port)));
    let vault = PathBuf::from(yml["default_vault"].as_str().unwrap());
    assert_eq!(
        vault.canonicalize().unwrap(),
        sb.vault().canonicalize().unwrap()
    );
    let token_file = sb.home().join(".onebrain/gateway/tunnel.token");
    assert_eq!(std::fs::read_to_string(&token_file).unwrap(), token);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        stdout.contains("Connector URL for the Claude app: https://brain.example.com/mcp"),
        "{stdout}"
    );
    // macOS: the service step wrote the tunnel agent with the local origin
    // (B1). Elsewhere: the manual lines carry it.
    let origin = format!("http://127.0.0.1:{port}");
    if cfg!(target_os = "macos") {
        let plist = std::fs::read_to_string(
            sb.home()
                .join("Library/LaunchAgents/com.onebrain.gateway-tunnel.plist"),
        )
        .unwrap();
        assert!(
            plist.contains(&format!("<string>{origin}</string>")),
            "{plist}"
        );
    } else {
        assert!(stdout.contains(&format!("--url {origin}")), "{stdout}");
    }
}

/// One raw HTTP/1.1 GET; returns (status, body).
fn get(port: u16, host: &str, path: &str) -> Option<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    let status = raw.split_whitespace().nth(1)?.parse().ok()?;
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string())?;
    Some((status, body))
}

/// (mtime, inode): a byte-identical tmp+rename rewrite changes both.
fn stamp(p: &Path) -> (std::time::SystemTime, u64) {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).unwrap();
    (m.modified().unwrap(), m.ino())
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// (b) Quick Tunnel: the in-process gateway's host guard and OAuth issuer
/// both follow the trycloudflare URL (only in memory); SIGTERM stops the
/// gateway, cloudflared dies with it, gateway.yml is byte-identical and no
/// token / LaunchAgent appeared.
#[test]
fn quick_tunnel_drives_the_issuer_and_host_guard_and_writes_nothing() {
    let sb = Sandbox::new();
    let port = scratch_port();
    std::fs::write(
        sb.yml(),
        format!("default_vault: {}\nport: {port}\n", sb.vault().display()),
    )
    .unwrap();
    let before = std::fs::read(sb.yml()).unwrap();
    let before_stamp = stamp(&sb.yml());
    let quick_url = cf_fixture("QUICK_URL");
    let bin = fake_cloudflared(
        sb.dir(),
        &format!(
            "echo $$ > {pid}\necho {req} >&2\necho {line} >&2\nexec sleep 120",
            pid = sh_quote(&sb.dir().join("cf.pid").display().to_string()),
            req = sh_quote(&cf_fixture("QUICK_REQUESTING_LINE")),
            line = sh_quote(&cf_fixture("QUICK_URL_LINE")),
        ),
    );
    let mut child = onebrain(&sb.home(), &bin)
        .current_dir(sb.dir())
        .args(["gateway", "setup"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"3\n").unwrap();

    let quick_host = quick_url.trim_start_matches("https://").to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    let discovery = "/.well-known/oauth-authorization-server";
    let answer = loop {
        if let Some(a) = get(port, &quick_host, discovery) {
            break Some(a);
        }
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let result = std::panic::catch_unwind(|| {
        let (status, body) = answer.expect("the in-process gateway never answered");
        assert_eq!(status, 200, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["issuer"].as_str(), Some(quick_url.as_str()), "{body}");
        let (status, body) = get(port, "evil.example", discovery).unwrap();
        assert_eq!(status, 403, "another Host must be refused: {body}");
    });

    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Err(e) = result {
        panic!(
            "{e:?}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("testing only"), "{stdout}");
    assert!(stdout.contains(&format!("{quick_url}/mcp")), "{stdout}");
    assert_eq!(
        argv(sb.dir()),
        [format!(
            "tunnel --no-autoupdate --protocol http2 --url http://127.0.0.1:{port}"
        )]
    );
    assert_eq!(
        std::fs::read(sb.yml()).unwrap(),
        before,
        "gateway.yml changed"
    );
    // Not even a byte-identical rewrite (review flip F2).
    assert_eq!(stamp(&sb.yml()), before_stamp, "gateway.yml rewritten");
    // `gateway run`'s banner says what is really served (review H).
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("through a Quick Tunnel (testing only)"),
        "{stderr}"
    );
    assert!(!stderr.contains("loopback only"), "{stderr}");
    assert!(!sb.home().join(".onebrain/gateway/setup.pending").exists());
    assert!(!sb.home().join(".onebrain/gateway/tunnel.token").exists());
    assert!(!sb.home().join("Library/LaunchAgents").exists());
    let pid = std::fs::read_to_string(sb.dir().join("cf.pid")).unwrap();
    let pid = pid.trim();
    let gone = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < gone {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(pid),
        "the Quick Tunnel cloudflared outlived the wizard"
    );
}

/// EOF in a pipe before the connection choice: non-zero, progress summary.
#[test]
fn eof_before_the_connection_choice_exits_non_zero_with_the_summary() {
    let sb = Sandbox::new();
    std::fs::write(
        sb.yml(),
        format!("default_vault: {}\nport: 9\n", sb.vault().display()),
    )
    .unwrap();
    let before = std::fs::read(sb.yml()).unwrap();
    let bin = fake_cloudflared(sb.dir(), "exit 0");
    let out = onebrain(&sb.home(), &bin)
        .args(["gateway", "setup"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("Done so far:\n  ✓ vault "), "{stdout}");
    assert!(
        stdout.contains("Re-run: onebrain gateway setup (safe — finished steps are kept)"),
        "{stdout}"
    );
    assert_eq!(std::fs::read(sb.yml()).unwrap(), before);
    assert!(argv(sb.dir()).is_empty());
}
