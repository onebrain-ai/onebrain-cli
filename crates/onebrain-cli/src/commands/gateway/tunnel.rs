//! `onebrain gateway tunnel setup|status` (v3.5.0 T3, #412, design D3).
//!
//! The operator creates the tunnel and its public hostname in the Cloudflare
//! Zero Trust dashboard; this module stores the token (0600, never echoed)
//! and points `public_url` at the hostname. No `cert.pem`, no
//! `cloudflared tunnel login/create/route`.

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
/// `cloudflared service install <token>`; take the last word.
pub(crate) fn extract_token(raw: &str) -> &str {
    raw.split_whitespace().last().unwrap_or("")
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
}
