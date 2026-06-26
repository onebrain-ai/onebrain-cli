//! Per-session auth token generation.
//!
//! **Dependency choice.** The brief asked to avoid pulling a new crate just for
//! random bytes. `getrandom`/`rand` ARE transitive deps (via tokio/axum), but
//! exposing them as a *direct* dep is still a new line in `Cargo.toml`. On Unix
//! we already lean on raw syscalls for the daemon (setsid, kill, getpgid), so
//! reading 16 bytes from `/dev/urandom` via `std::fs` is in keeping with the
//! crate's style and adds nothing. We hex-encode by hand (one fold) rather than
//! pull `hex`.
//!
//! 16 random bytes → 32 hex chars. That is 128 bits of entropy — far more than
//! enough for a localhost, single-session token whose only job is to stop other
//! *local* processes from poking the API. (The 127.0.0.1 bind is the primary
//! boundary; the token is the secondary one.)

/// Resolve the session token for a server run.
///
/// Honours a caller-supplied `ONEBRAIN_TOKEN` env var (≥ 16 chars) so a remote /
/// tunnel deploy can PIN a stable token across restarts — the `?token=` URL then
/// stays valid and bookmarkable, which is what makes `app.example.com` usable
/// without re-reading a fresh token after every restart. The operator is
/// responsible for making a pinned token long + unguessable. Otherwise (unset,
/// or too short) we generate a fresh random one as before.
pub fn resolve_token() -> String {
    resolve_token_from(std::env::var("ONEBRAIN_TOKEN").ok())
}

/// Pure core of [`resolve_token`] (env value injected) so the rule is testable
/// without touching process-global env state.
fn resolve_token_from(env: Option<String>) -> String {
    if let Some(raw) = env {
        let t = raw.trim();
        if t.len() >= 16 {
            return t.to_string();
        }
        if !t.is_empty() {
            tracing::warn!(
                "ONEBRAIN_TOKEN is too short (< 16 chars) — ignoring it and \
                 generating a fresh random token instead"
            );
        }
    }
    generate_token()
}

/// Generate a fresh 32-hex-char (128-bit) session token.
///
/// Unix: reads 16 bytes from `/dev/urandom`. If that read fails (extraordinarily
/// rare — `/dev/urandom` is always present on a functioning Unix host), we fall
/// back to a time-seeded value so the server can still start; the fallback is
/// clearly weaker but a dead `/dev/urandom` means much bigger problems exist.
///
/// Non-Unix: time-seeded fallback only (the daemon itself is Unix-only for now;
/// this keeps the module compiling on Windows for `serve`'s router tests).
pub fn generate_token() -> String {
    #[cfg(unix)]
    {
        if let Some(tok) = read_urandom() {
            return tok;
        }
    }
    // Reaching here means the strong source was unavailable (or this is a
    // non-Unix build). The time-seeded fallback is predictable enough that a
    // determined local attacker could guess it, so flag it loudly (fix K) — on
    // the daemon this lands in `daemon.log`; on `serve` it's on stderr.
    #[cfg(unix)]
    tracing::warn!(
        "/dev/urandom unavailable — falling back to a WEAK time-seeded session \
         token; the API token is guessable until the server restarts with a \
         working random source"
    );
    time_seeded_token()
}

/// Read 16 bytes from `/dev/urandom` and hex-encode them. `None` on any I/O
/// error so the caller can fall back.
#[cfg(unix)]
fn read_urandom() -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom").ok()?;
    let mut buf = [0u8; 16];
    f.read_exact(&mut buf).ok()?;
    Some(hex_encode(&buf))
}

/// Weak fallback: mix the wall clock + a per-process address into 16 bytes.
/// Only reached if `/dev/urandom` is unavailable. Not cryptographically strong,
/// but the token is a localhost secondary boundary, not a primary one.
fn time_seeded_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Stir in a stack address so two starts in the same nanosecond still differ.
    let stir = &nanos as *const u128 as u128;
    let mixed = nanos ^ stir.rotate_left(64);
    let bytes = mixed.to_le_bytes(); // 16 bytes
    hex_encode(&bytes)
}

/// Lowercase hex-encode a byte slice. `2 * n` chars for `n` bytes.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_32_hex_chars() {
        let tok = generate_token();
        assert_eq!(tok.len(), 32, "token should be 32 hex chars: {tok}");
        assert!(
            tok.chars().all(|c| c.is_ascii_hexdigit()),
            "token must be all hex: {tok}"
        );
    }

    #[test]
    fn tokens_are_unique_across_calls() {
        // Two fresh tokens must differ — a fixed token would let any local
        // process that read one stale value keep access forever.
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two generated tokens collided: {a}");
    }

    #[test]
    fn resolve_pins_a_strong_env_token() {
        let pinned = "my-stable-remote-token-123456";
        assert_eq!(resolve_token_from(Some(pinned.to_string())), pinned);
        // surrounding whitespace is trimmed
        assert_eq!(resolve_token_from(Some(format!("  {pinned}  "))), pinned);
    }

    #[test]
    fn resolve_falls_back_when_env_absent_or_too_short() {
        // unset → fresh 32-hex token
        assert_eq!(resolve_token_from(None).len(), 32);
        // too short (< 16) → ignored, fresh token instead (not the short value)
        let short = "abc";
        let got = resolve_token_from(Some(short.to_string()));
        assert_ne!(got, short);
        assert_eq!(got.len(), 32);
        // empty → fresh token
        assert_eq!(resolve_token_from(Some(String::new())).len(), 32);
    }

    #[test]
    fn hex_encode_pads_each_byte_to_two_chars() {
        // Leading-zero nibble must NOT be dropped (`0x0a` → "0a", not "a").
        assert_eq!(hex_encode(&[0x0a, 0xff, 0x00]), "0aff00");
    }
}
