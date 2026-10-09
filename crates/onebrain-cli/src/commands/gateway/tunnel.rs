//! `onebrain gateway tunnel setup|status` (v3.5.0 T3, #412, design D3).
//!
//! The operator creates the tunnel and its public hostname in the Cloudflare
//! Zero Trust dashboard; this module stores the token (0600, never echoed)
//! and points `public_url` at the hostname. No `cert.pem`, no
//! `cloudflared tunnel login/create/route`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// `~/.onebrain/gateway/tunnel.token` — beside the auth store, same 0700 dir.
pub(crate) fn tunnel_token_path(onebrain_dir: &Path) -> PathBuf {
    onebrain_dir.join("gateway").join("tunnel.token")
}

/// Normalise a pasted hostname to a bare, lowercase public DNS name.
/// Accepts `https://host/` and a trailing dot; rejects any other scheme,
/// a path/port/userinfo, an IP, or a single label.
pub(crate) fn normalize_hostname(raw: &str) -> Result<String, String> {
    let lowered = raw.trim().to_ascii_lowercase();
    let mut h = lowered.as_str();
    if let Some(rest) = h.strip_prefix("https://") {
        h = rest;
    } else if h.contains("://") {
        return Err(
            "enter the hostname only (e.g. brain.example.com) — the tunnel is always https".into(),
        );
    }
    let h = h.strip_suffix('/').unwrap_or(h);
    let h = h.strip_suffix('.').unwrap_or(h).to_string();
    if h.is_empty() {
        return Err("no hostname entered".into());
    }
    if h.contains(['/', '?', '#', ':', '@']) || h.chars().any(char::is_whitespace) {
        return Err(
            "must be a bare hostname — no path, port, or spaces (e.g. brain.example.com)".into(),
        );
    }
    if h.len() > 253 {
        return Err("is longer than 253 characters".into());
    }
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() < 2 {
        return Err("must be a full public hostname with a domain (e.g. brain.example.com)".into());
    }
    for l in &labels {
        let ok_chars = l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if l.is_empty() || l.len() > 63 || !ok_chars || l.starts_with('-') || l.ends_with('-') {
            return Err(format!(
                "has an invalid label {l:?} — letters, digits and inner hyphens only, 1-63 long"
            ));
        }
    }
    if labels
        .last()
        .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(
            "looks like an IP address — use the public hostname from the Cloudflare dashboard"
                .into(),
        );
    }
    Ok(h)
}

/// The dashboard shows the token inside commands such as
/// `cloudflared service install <token>`; take the last word, minus any
/// quotes wrapped around it.
pub(crate) fn extract_token(raw: &str) -> &str {
    raw.split_whitespace()
        .last()
        .unwrap_or("")
        .trim_matches(['"', '\''])
}

/// Why a pasted value is not a tunnel token. `Display` never includes the
/// input — a near-miss token is still mostly a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenShapeError {
    Empty,
    NotBase64,
    NotJson,
    MissingField(&'static str),
    BadTunnelId,
}

impl std::fmt::Display for TokenShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("no token entered"),
            Self::NotBase64 => f.write_str("that is not a tunnel token (not base64)"),
            Self::NotJson => {
                f.write_str("that is not a tunnel token (does not decode to the expected JSON)")
            }
            Self::MissingField(k) => write!(f, "that tunnel token is incomplete (no {k:?} field)"),
            Self::BadTunnelId => f.write_str("that tunnel token has no valid tunnel id"),
        }
    }
}

/// Shape-check a Cloudflare tunnel token (base64 JSON `{"a","t","s"}`)
/// without contacting Cloudflare. Returns the tunnel id (`t`, not secret).
pub(crate) fn validate_tunnel_token(raw: &str) -> Result<String, TokenShapeError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(TokenShapeError::Empty);
    }
    let bytes = b64_decode(t).ok_or(TokenShapeError::NotBase64)?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| TokenShapeError::NotJson)?;
    for field in ["a", "t", "s"] {
        if !v
            .get(field)
            .and_then(|x| x.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            return Err(TokenShapeError::MissingField(field));
        }
    }
    let id = v["t"].as_str().unwrap_or_default();
    let parts: Vec<&str> = id.split('-').collect();
    let uuid = parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()));
    if !uuid {
        return Err(TokenShapeError::BadTunnelId);
    }
    Ok(id.to_string())
}

/// RFC 4648 base64 decode, standard or URL-safe alphabet, padding optional.
/// Hand-rolled: ~20 lines beats a new crate.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let s = s.trim_end_matches('=');
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Older cloudflared has no `--token-file`; read it off `tunnel run --help`.
pub(crate) fn supports_token_file(run_help: &str) -> bool {
    run_help.contains("--token-file")
}
/// Everything [`run_tunnel_setup`] needs from the machine, injectable.
pub(crate) trait TunnelHost {
    fn cloudflared_path(&self) -> Option<PathBuf>;
    /// `cloudflared tunnel run --help` text (stdout+stderr), if it ran.
    fn cloudflared_run_help(&self, cloudflared: &Path) -> Option<String>;
    /// Read the token: no echo on a TTY, a plain line otherwise.
    fn read_secret(
        &self,
        prompt: &str,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
    ) -> anyhow::Result<String>;
    /// Whether `gateway service` exists on this OS (macOS only in v3.5.0).
    fn service_supported(&self) -> bool;
}

/// `cloudflared tunnel run --help` text (stdout+stderr), if it ran. Shared by
/// `tunnel setup` and `service install`.
pub(crate) fn cloudflared_run_help(cloudflared: &Path) -> Option<String> {
    let o = std::process::Command::new(cloudflared)
        .args(["tunnel", "run", "--help"])
        .output()
        .ok()?;
    Some(format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    ))
}

pub(crate) struct SystemTunnelHost {
    pub stdin_is_tty: bool,
}

impl TunnelHost for SystemTunnelHost {
    fn cloudflared_path(&self) -> Option<PathBuf> {
        which::which("cloudflared").ok()
    }
    fn cloudflared_run_help(&self, cloudflared: &Path) -> Option<String> {
        cloudflared_run_help(cloudflared)
    }
    fn read_secret(
        &self,
        prompt: &str,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
    ) -> anyhow::Result<String> {
        if self.stdin_is_tty {
            return inquire::Password::new(prompt)
                .without_confirmation()
                .prompt()
                .map_err(|e| anyhow::anyhow!("read tunnel token: {e}"));
        }
        write!(out, "{prompt} ")?;
        out.flush()?;
        let mut line = String::new();
        input
            .read_line(&mut line)
            .map_err(|e| anyhow::anyhow!("read tunnel token: {e}"))?;
        Ok(line)
    }
    fn service_supported(&self) -> bool {
        super::service::service_supported()
    }
}

#[derive(Debug)]
#[allow(dead_code)] // fields are read by tests; Task 4+ consume them
pub(crate) struct TunnelSetupOutcome {
    pub public_url: String,
    pub token_file_supported: bool,
}

fn hinted(plain: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(crate::output::HintedError::new(plain, hint))
}

/// The wizard. Validates everything before writing anything; then writes the
/// token (0600) and `public_url`. Never prints the token.
pub(crate) fn run_tunnel_setup(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    onebrain_dir: &Path,
    host: &dyn TunnelHost,
) -> anyhow::Result<TunnelSetupOutcome> {
    let gateway_yml = onebrain_dir.join("gateway.yml");
    let config = super::config::load_gateway_config_at(&gateway_yml)?;
    let port = config.port;

    writeln!(out, "OneBrain gateway - Cloudflare tunnel setup")?;
    writeln!(out)?;
    writeln!(out, "In the Cloudflare Zero Trust dashboard:")?;
    writeln!(
        out,
        "1. Networks > Tunnels > Create a tunnel > Cloudflared; name it (e.g. onebrain)"
    )?;
    writeln!(
        out,
        "2. Copy the token from the install command it shows (you can paste the whole command)"
    )?;
    // Red-team item 9: never `localhost` - the gateway binds only
    // 127.0.0.1, and a resolver that tries ::1 first would 502.
    writeln!(
        out,
        "3. Published application / public hostname: add one on your domain > Service: HTTP, URL: http://127.0.0.1:{port}"
    )?;
    writeln!(
        out,
        "   Leave the origin's Host header alone (no httpHostHeader / Host rewriting): the gateway checks it."
    )?;
    writeln!(out)?;

    let cloudflared = host.cloudflared_path().ok_or_else(|| {
        hinted(
            "cloudflared is not on PATH",
            "brew install cloudflared, then run this again",
        )
    })?;

    write!(out, "Tunnel hostname (e.g. brain.example.com): ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let hostname = normalize_hostname(&line).map_err(|r| anyhow::anyhow!("hostname {r}"))?;
    let public_url = format!("https://{hostname}");
    super::validate_public_url(&public_url)
        .map_err(|r| anyhow::anyhow!("public_url {public_url:?} {r}"))?;

    let raw = host.read_secret("Tunnel token:", input, out)?;
    let token = extract_token(&raw).to_string();
    let tunnel_id = validate_tunnel_token(&token).map_err(|e| {
        hinted(
            e.to_string(),
            "copy the token again from the tunnel's install command in the dashboard",
        )
    })?;

    let token_path = tunnel_token_path(onebrain_dir);
    super::config_write::ensure_private_dir(token_path.parent().unwrap_or(onebrain_dir))?;
    super::config_write::write_private_file(&token_path, token.as_bytes())?;
    let outcome = super::config_write::set_top_level_key(
        &gateway_yml,
        "public_url",
        serde_yaml::Value::String(public_url.clone()),
        &format!("public_url: '{public_url}'\n"),
    )?;

    let token_file_supported = host
        .cloudflared_run_help(&cloudflared)
        .is_some_and(|help| supports_token_file(&help));

    writeln!(out)?;
    writeln!(
        out,
        "Token saved for tunnel {tunnel_id} ({}, mode 0600)",
        token_path.display()
    )?;
    writeln!(out, "public_url set to {public_url}")?;
    if matches!(
        outcome,
        super::config_write::WriteOutcome::Rewrote {
            dropped_comments: true
        }
    ) {
        writeln!(
            out,
            "Note: gateway.yml was reformatted and its comments dropped."
        )?;
    }
    if !token_file_supported {
        writeln!(
            out,
            "Note: this cloudflared has no --token-file; the service will pass the token in its \
             LaunchAgent environment instead (plist kept 0600). `brew upgrade cloudflared` avoids that."
        )?;
    }
    writeln!(out, "Connector URL for the Claude app: {public_url}/mcp")?;
    writeln!(
        out,
        "A changed public_url only takes effect once the gateway restarts."
    )?;
    if host.service_supported() {
        writeln!(
            out,
            "Next: onebrain gateway service install   (starts - or restarts - the gateway and the tunnel)"
        )?;
    } else {
        writeln!(
            out,
            "Next, keep these two running (restart a running `gateway run` so it picks up public_url):"
        )?;
        writeln!(out, "  onebrain gateway run")?;
        writeln!(
            out,
            "  {} tunnel --no-autoupdate run --token-file {}",
            cloudflared.display(),
            token_path.display()
        )?;
    }
    Ok(TunnelSetupOutcome {
        public_url,
        token_file_supported,
    })
}

/// `onebrain gateway tunnel setup`.
pub fn tunnel_setup(_mode: &crate::output::OutputMode) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let onebrain_dir = super::config::gateway_config_path()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("resolve ~/.onebrain"))?;
    let stdin = std::io::stdin();
    let host = SystemTunnelHost {
        stdin_is_tty: stdin.is_terminal(),
    };
    let mut input = stdin.lock();
    run_tunnel_setup(&mut input, &mut std::io::stdout(), &onebrain_dir, &host).map(|_| ())
}

/// `onebrain gateway tunnel status`.
pub fn tunnel_status(_mode: &crate::output::OutputMode) -> anyhow::Result<()> {
    super::health::print_status(&[
        super::health::CHECK_CONFIG,
        super::health::CHECK_SERVICE,
        super::health::CHECK_TUNNEL,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake token: base64 of {"a":<32 hex>,"t":<uuid>,"s":"bm90LWEtcmVhbC1zZWNyZXQ="}
    /// — the secret decodes to "not-a-real-secret". Never a real credential.
    const FAKE_TOKEN: &str = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmZmNDJhZTItNzY1ZC00YWRmLTgxMTItMzFjNTVjMTU1MWVmIiwicyI6ImJtOTBMV0V0Y21WaGJDMXpaV055WlhRPSJ9";
    const FAKE_SECRET: &str = "bm90LWEtcmVhbC1zZWNyZXQ";

    #[test]
    fn hostname_accepts_bare_and_pasted_url_forms() {
        for raw in [
            "brain.example.com",
            " Brain.Example.COM ",
            "https://brain.example.com/",
            "HTTPS://Brain.Example.com",
            "brain.example.com.",
        ] {
            assert_eq!(
                normalize_hostname(raw).unwrap(),
                "brain.example.com",
                "{raw}"
            );
        }
    }

    #[test]
    fn hostname_rejects_paths_ports_schemes_ips_and_single_labels() {
        for raw in [
            "",
            "localhost",
            "brain.example.com/mcp",
            "brain.example.com:443",
            "http://brain.example.com",
            "ftp://x.y",
            "10.0.0.1",
            "[::1]",
            "-bad.example.com",
            "bad_label.example.com",
            "brain .example.com",
            "user@brain.example.com",
            "brain.example.com..",
        ] {
            assert!(normalize_hostname(raw).is_err(), "{raw:?} must be rejected");
        }
        assert!(normalize_hostname(&format!("{}.com", "a".repeat(64))).is_err());
    }

    #[test]
    fn accepted_hostnames_always_yield_a_public_url_the_gateway_accepts() {
        for raw in [
            "brain.example.com",
            "https://A-b.Example.co.uk/",
            "x1.y2.",
            "a.b",
        ] {
            let host = normalize_hostname(raw).unwrap();
            let url = format!("https://{host}");
            assert!(
                crate::commands::gateway::validate_public_url(&url).is_ok(),
                "{url} rejected by validate_public_url"
            );
        }
    }

    #[test]
    fn token_shape_accepts_a_dashboard_token_and_returns_the_tunnel_id() {
        assert_eq!(
            validate_tunnel_token(FAKE_TOKEN).unwrap(),
            "6ff42ae2-765d-4adf-8112-31c55c1551ef"
        );
    }

    /// Review Focus 1: the dashboard shows the token inside a command.
    #[test]
    fn extract_token_takes_the_token_out_of_a_pasted_command() {
        let pasted = format!("sudo cloudflared service install {FAKE_TOKEN}\n");
        assert_eq!(extract_token(&pasted), FAKE_TOKEN);
        assert_eq!(extract_token(&format!("  {FAKE_TOKEN}  ")), FAKE_TOKEN);
        assert_eq!(
            validate_tunnel_token(extract_token(&pasted)).unwrap(),
            "6ff42ae2-765d-4adf-8112-31c55c1551ef"
        );
    }

    #[test]
    fn token_shape_names_what_is_wrong_and_never_echoes_the_input() {
        let missing_s = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmZmNDJhZTItNzY1ZC00YWRmLTgxMTItMzFjNTVjMTU1MWVmIn0=";
        let bad_id = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0Ijoibm90LWEtdXVpZCIsInMiOiJlQT09In0=";
        let cases = [
            ("", TokenShapeError::Empty),
            ("not base64 !!", TokenShapeError::NotBase64),
            ("aGk=", TokenShapeError::NotJson),
            (missing_s, TokenShapeError::MissingField("s")),
            (bad_id, TokenShapeError::BadTunnelId),
        ];
        for (raw, want) in cases {
            let err = validate_tunnel_token(raw).unwrap_err();
            assert_eq!(err, want);
            let msg = err.to_string();
            assert!(
                raw.is_empty() || !msg.contains(raw),
                "message echoed the input"
            );
            assert!(!msg.contains(FAKE_SECRET));
        }
    }

    #[test]
    fn base64_decodes_standard_and_url_safe_with_or_without_padding() {
        assert_eq!(b64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(b64_decode("aGk").unwrap(), b"hi");
        assert_eq!(b64_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(b64_decode("+/8=").unwrap(), vec![0xfb, 0xff]);
        assert!(b64_decode("a").is_none());
        assert!(b64_decode("a*b=").is_none());
    }

    #[test]
    fn token_file_support_is_read_from_the_run_help_text() {
        assert!(supports_token_file(
            "   --token-file value   Filepath at which to read the tunnel token"
        ));
        assert!(!supports_token_file("   --token value   The Tunnel token"));
    }

    #[test]
    fn the_token_lives_next_to_the_auth_store() {
        assert_eq!(
            tunnel_token_path(Path::new("/h/.onebrain")),
            Path::new("/h/.onebrain/gateway/tunnel.token")
        );
    }

    #[test]
    fn extract_token_strips_surrounding_quotes() {
        assert_eq!(extract_token(&format!("\"{FAKE_TOKEN}\"\n")), FAKE_TOKEN);
        assert_eq!(extract_token(&format!("'{FAKE_TOKEN}'")), FAKE_TOKEN);
        assert_eq!(
            extract_token(&format!("cloudflared service install '{FAKE_TOKEN}'")),
            FAKE_TOKEN
        );
    }

    struct FakeHost {
        cloudflared: Option<PathBuf>,
        help: Option<String>,
        service: bool,
    }
    impl TunnelHost for FakeHost {
        fn cloudflared_path(&self) -> Option<PathBuf> {
            self.cloudflared.clone()
        }
        fn cloudflared_run_help(&self, _c: &Path) -> Option<String> {
            self.help.clone()
        }
        fn read_secret(
            &self,
            _p: &str,
            input: &mut dyn BufRead,
            _o: &mut dyn Write,
        ) -> anyhow::Result<String> {
            let mut line = String::new();
            input.read_line(&mut line)?;
            Ok(line)
        }
        fn service_supported(&self) -> bool {
            self.service
        }
    }

    fn host() -> FakeHost {
        FakeHost {
            cloudflared: Some(PathBuf::from("/opt/homebrew/bin/cloudflared")),
            help: Some("--token-file value  Filepath".into()),
            service: true,
        }
    }

    fn run(stdin: &str, h: &FakeHost, dir: &Path) -> (anyhow::Result<TunnelSetupOutcome>, String) {
        let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
        let mut out = Vec::new();
        let r = run_tunnel_setup(&mut input, &mut out, dir, h);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn setup_stores_the_token_0600_sets_public_url_and_never_prints_the_token() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".onebrain");
        let (r, out) = run(
            &format!("https://Brain.Example.com/\n{FAKE_TOKEN}\n"),
            &host(),
            &dir,
        );
        let outcome = r.unwrap();
        assert_eq!(outcome.public_url, "https://brain.example.com");
        assert!(outcome.token_file_supported);
        assert_eq!(
            std::fs::read_to_string(tunnel_token_path(&dir)).unwrap(),
            FAKE_TOKEN
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tunnel_token_path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let cfg = super::super::config::load_gateway_config_at(&dir.join("gateway.yml")).unwrap();
        assert_eq!(cfg.public_url.as_deref(), Some("https://brain.example.com"));
        assert!(out.contains("https://brain.example.com/mcp"), "{out}");
        assert!(out.contains("onebrain gateway service install"), "{out}");
        // Red-team item 9: the origin is the IPv4 loopback the gateway
        // actually binds, never `localhost`.
        assert!(out.contains("URL: http://127.0.0.1:7717"), "{out}");
        assert!(!out.contains("localhost"), "{out}");
        // Hub ruling 2: restart + Host-header guidance.
        assert!(out.contains("restart"), "{out}");
        assert!(out.contains("Host"), "{out}");
        assert!(
            !out.contains(FAKE_TOKEN) && !out.contains(FAKE_SECRET),
            "token leaked"
        );
    }

    #[test]
    fn setup_without_cloudflared_fails_before_asking_and_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".onebrain");
        let h = FakeHost {
            cloudflared: None,
            ..host()
        };
        let (r, _out) = run(&format!("brain.example.com\n{FAKE_TOKEN}\n"), &h, &dir);
        let err = r.unwrap_err();
        let hinted = err
            .downcast_ref::<crate::output::HintedError>()
            .expect("HintedError");
        assert!(
            hinted.hint.contains("brew install cloudflared"),
            "{}",
            hinted.hint
        );
        assert!(!dir.exists());
    }

    #[test]
    fn setup_with_a_bad_token_writes_nothing_and_does_not_echo_it() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".onebrain");
        let bad = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0Ijoibm90LWEtdXVpZCIsInMiOiJlQT09In0=";
        let (r, out) = run(&format!("brain.example.com\n{bad}\n"), &host(), &dir);
        let err = format!("{:#}", r.unwrap_err());
        assert!(!err.contains(bad) && !out.contains(bad), "token echoed");
        assert!(!tunnel_token_path(&dir).exists());
        assert!(!dir.join("gateway.yml").exists());
    }

    #[test]
    fn setup_rejects_a_bad_hostname_before_reading_the_token() {
        let root = tempfile::tempdir().unwrap();
        let (r, _) = run(
            &format!("brain.example.com/mcp\n{FAKE_TOKEN}\n"),
            &host(),
            root.path(),
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("bare hostname"));
    }

    /// Design open question 2: no `--token-file` -> say how `service install`
    /// will cope, and report it.
    #[test]
    fn setup_reports_a_cloudflared_without_token_file_support() {
        let root = tempfile::tempdir().unwrap();
        let h = FakeHost {
            help: Some("--token value".into()),
            ..host()
        };
        let (r, out) = run(
            &format!("brain.example.com\n{FAKE_TOKEN}\n"),
            &h,
            root.path(),
        );
        assert!(!r.unwrap().token_file_supported);
        assert!(out.contains("brew upgrade cloudflared"), "{out}");
    }

    #[test]
    fn rerunning_setup_with_a_new_hostname_replaces_public_url_and_keeps_comments() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".onebrain");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("gateway.yml"),
            "# my notes\npublic_url: 'https://old.example.com'\n",
        )
        .unwrap();
        let (r, _) = run(&format!("new.example.com\n{FAKE_TOKEN}\n"), &host(), &dir);
        r.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("gateway.yml")).unwrap(),
            "# my notes\npublic_url: 'https://new.example.com'\n"
        );
    }

    #[test]
    fn setup_on_a_platform_without_service_support_prints_the_manual_commands() {
        let root = tempfile::tempdir().unwrap();
        let h = FakeHost {
            service: false,
            ..host()
        };
        let (r, out) = run(
            &format!("brain.example.com\n{FAKE_TOKEN}\n"),
            &h,
            root.path(),
        );
        r.unwrap();
        assert!(
            out.contains("cloudflared tunnel --no-autoupdate run --token-file"),
            "{out}"
        );
        assert!(!out.contains("service install"), "{out}");
    }

    #[test]
    fn system_host_reads_the_secret_from_input_when_stdin_is_not_a_tty() {
        let mut input = std::io::Cursor::new(b"abc\n".to_vec());
        let mut out = Vec::new();
        let got = SystemTunnelHost {
            stdin_is_tty: false,
        }
        .read_secret("Token:", &mut input, &mut out)
        .unwrap();
        assert_eq!(got.trim(), "abc");
    }
}
