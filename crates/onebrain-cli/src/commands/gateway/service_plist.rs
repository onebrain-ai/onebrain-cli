//! LaunchAgent plists for `gateway service` (v3.5.0 T3, design D2).
//!
//! Separate from `onebrain_core::scheduler::launchd::generate_plist`, which
//! renders CALENDAR jobs (`RunAtLoad false`, `StartCalendarInterval`); these
//! are always-on daemons: `KeepAlive true` + `RunAtLoad true`. Pure and
//! platform-neutral so the golden tests run (and are measured) on Linux CI.

use std::path::{Path, PathBuf};

use onebrain_core::scheduler::xml_escape;

pub(crate) const GATEWAY_LABEL: &str = "gateway";
pub(crate) const TUNNEL_LABEL: &str = "gateway-tunnel";

/// How cloudflared gets its token.
pub(crate) enum TunnelAuth {
    /// `--token-file <path>` (cloudflared that supports it).
    TokenFile(PathBuf),
    /// `TUNNEL_TOKEN` in the plist's environment; the plist then holds a
    /// secret and is written 0600 (it always is; see `service.rs`).
    EnvToken(String),
}

impl std::fmt::Debug for TunnelAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenFile(p) => f.debug_tuple("TokenFile").field(p).finish(),
            Self::EnvToken(_) => f.write_str("EnvToken(<redacted>)"),
        }
    }
}

pub(crate) struct AgentSpec {
    pub label: &'static str,
    pub program: Vec<String>,
    pub log_path: PathBuf,
    pub env: Vec<(String, String)>,
}

impl std::fmt::Debug for AgentSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("AgentSpec")
            .field("label", &self.label)
            .field("program", &self.program)
            .field("log_path", &self.log_path)
            .field("env_keys", &env)
            .finish()
    }
}

/// `<onebrain> gateway run`, logging to `<log_dir>/gateway.log`. The log
/// never receives the pairing code (T1: `gateway run` prints it only to a
/// TTY), but it does carry client ids, paths and approval summaries;
/// `service.rs` pre-creates it 0600.
pub(crate) fn gateway_agent(onebrain_exe: &Path, log_dir: &Path) -> AgentSpec {
    AgentSpec {
        label: GATEWAY_LABEL,
        program: vec![
            onebrain_exe.display().to_string(),
            "gateway".into(),
            "run".into(),
        ],
        log_path: log_dir.join("gateway.log"),
        env: Vec::new(),
    }
}

/// `<cloudflared> tunnel --no-autoupdate --protocol http2 run [--token-file <path>]`.
/// `--protocol http2`: smoke V (2026-10-09) - on a phone hotspot QUIC/7844 failed
/// and cloudflared's `auto` did not fall back to HTTP/2 for > 2 min (tunnel 530).
/// HTTP/2 over 443 works wherever HTTPS works; the gateway's traffic is tiny.
pub(crate) fn tunnel_agent(cloudflared: &Path, auth: &TunnelAuth, log_dir: &Path) -> AgentSpec {
    let mut program = vec![
        cloudflared.display().to_string(),
        "tunnel".into(),
        "--no-autoupdate".into(),
        "--protocol".into(),
        "http2".into(),
        "run".into(),
    ];
    let mut env = Vec::new();
    match auth {
        TunnelAuth::TokenFile(p) => {
            program.push("--token-file".into());
            program.push(p.display().to_string());
        }
        TunnelAuth::EnvToken(t) => env.push(("TUNNEL_TOKEN".to_string(), t.clone())),
    }
    AgentSpec {
        label: TUNNEL_LABEL,
        program,
        log_path: log_dir.join("gateway-tunnel.log"),
        env,
    }
}

/// Render `spec`. Refuses control characters: XML cannot represent them,
/// and `launchctl bootstrap` would reject the file with an opaque error
/// (same rule as the scheduler's #355). The error never names a value.
pub(crate) fn render_keepalive_plist(spec: &AgentSpec) -> Result<String, String> {
    let log = spec.log_path.display().to_string();
    let mut all = spec
        .program
        .iter()
        .chain(std::iter::once(&log))
        .chain(spec.env.iter().flat_map(|(k, v)| [k, v]));
    if all.any(|s| s.chars().any(char::is_control)) {
        return Err(format!(
            "com.onebrain.{}: a path or argument contains a control character",
            spec.label
        ));
    }
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    x.push_str(&format!(
        "    <key>Label</key>\n    <string>com.onebrain.{}</string>\n",
        xml_escape(spec.label)
    ));
    x.push_str("    <key>ProgramArguments</key>\n    <array>\n");
    for arg in &spec.program {
        x.push_str(&format!("        <string>{}</string>\n", xml_escape(arg)));
    }
    x.push_str("    </array>\n");
    if !spec.env.is_empty() {
        x.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in &spec.env {
            x.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        x.push_str("    </dict>\n");
    }
    x.push_str("    <key>KeepAlive</key>\n    <true/>\n    <key>RunAtLoad</key>\n    <true/>\n");
    for key in ["StandardOutPath", "StandardErrorPath"] {
        x.push_str(&format!(
            "    <key>{key}</key>\n    <string>{}</string>\n",
            xml_escape(&log)
        ));
    }
    x.push_str("</dict>\n</plist>\n");
    Ok(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGS: &str = "/Users/test/Library/Logs/onebrain";
    const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n";

    #[test]
    fn gateway_plist_golden() {
        let spec = gateway_agent(Path::new("/opt/homebrew/bin/onebrain"), Path::new(LOGS));
        let expected = format!(
            "{HEAD}    <key>Label</key>\n    <string>com.onebrain.gateway</string>\n    <key>ProgramArguments</key>\n    <array>\n        <string>/opt/homebrew/bin/onebrain</string>\n        <string>gateway</string>\n        <string>run</string>\n    </array>\n    <key>KeepAlive</key>\n    <true/>\n    <key>RunAtLoad</key>\n    <true/>\n    <key>StandardOutPath</key>\n    <string>/Users/test/Library/Logs/onebrain/gateway.log</string>\n    <key>StandardErrorPath</key>\n    <string>/Users/test/Library/Logs/onebrain/gateway.log</string>\n</dict>\n</plist>\n"
        );
        assert_eq!(render_keepalive_plist(&spec).unwrap(), expected);
    }

    #[test]
    fn tunnel_plist_golden_with_token_file() {
        let spec = tunnel_agent(
            Path::new("/opt/homebrew/bin/cloudflared"),
            &TunnelAuth::TokenFile(PathBuf::from("/Users/test/.onebrain/gateway/tunnel.token")),
            Path::new(LOGS),
        );
        let expected = format!(
            "{HEAD}    <key>Label</key>\n    <string>com.onebrain.gateway-tunnel</string>\n    <key>ProgramArguments</key>\n    <array>\n        <string>/opt/homebrew/bin/cloudflared</string>\n        <string>tunnel</string>\n        <string>--no-autoupdate</string>\n        <string>--protocol</string>\n        <string>http2</string>\n        <string>run</string>\n        <string>--token-file</string>\n        <string>/Users/test/.onebrain/gateway/tunnel.token</string>\n    </array>\n    <key>KeepAlive</key>\n    <true/>\n    <key>RunAtLoad</key>\n    <true/>\n    <key>StandardOutPath</key>\n    <string>/Users/test/Library/Logs/onebrain/gateway-tunnel.log</string>\n    <key>StandardErrorPath</key>\n    <string>/Users/test/Library/Logs/onebrain/gateway-tunnel.log</string>\n</dict>\n</plist>\n"
        );
        assert_eq!(render_keepalive_plist(&spec).unwrap(), expected);
    }

    /// Design open question 2 fallback: token in `EnvironmentVariables`.
    #[test]
    fn tunnel_plist_with_env_token_passes_it_as_tunnel_token_and_debug_redacts_it() {
        let auth = TunnelAuth::EnvToken("fake-token-value".into());
        let spec = tunnel_agent(Path::new("/c"), &auth, Path::new(LOGS));
        let xml = render_keepalive_plist(&spec).unwrap();
        assert!(
            xml.contains(
                "    <key>EnvironmentVariables</key>\n    <dict>\n        <key>TUNNEL_TOKEN</key>\n        <string>fake-token-value</string>\n    </dict>\n"
            ),
            "env block missing"
        );
        // F1: http2 is always passed, including in the env-token fallback.
        assert!(xml.contains(
            "        <string>--no-autoupdate</string>\n        <string>--protocol</string>\n        <string>http2</string>\n        <string>run</string>\n    </array>"
        ));
        assert!(!xml.contains("--token-file"));
        assert!(!format!("{spec:?}{auth:?}").contains("fake-token-value"));
    }

    #[test]
    fn xml_metacharacters_are_escaped_and_control_chars_refused() {
        let spec = gateway_agent(Path::new("/A & <B> \"q\"/onebrain"), Path::new(LOGS));
        assert!(render_keepalive_plist(&spec)
            .unwrap()
            .contains("<string>/A &amp; &lt;B&gt; &quot;q&quot;/onebrain</string>"));
        let bad = gateway_agent(Path::new("/a\u{1}b"), Path::new(LOGS));
        assert!(render_keepalive_plist(&bad)
            .unwrap_err()
            .contains("control character"));
    }

    /// The rendered XML is a valid plist to Apple's own linter.
    #[cfg(target_os = "macos")]
    #[test]
    fn rendered_plists_pass_plutil_lint() {
        let dir = tempfile::tempdir().unwrap();
        for spec in [
            gateway_agent(Path::new("/opt/homebrew/bin/onebrain"), Path::new(LOGS)),
            tunnel_agent(
                Path::new("/c"),
                &TunnelAuth::EnvToken("x".into()),
                Path::new(LOGS),
            ),
        ] {
            let p = dir.path().join(format!("{}.plist", spec.label));
            std::fs::write(&p, render_keepalive_plist(&spec).unwrap()).unwrap();
            let o = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&p)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        }
    }
}
