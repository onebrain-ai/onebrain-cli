//! Per-session auth token generation.
//!
//! **Randomness source.** Bytes come from the OS CSPRNG via `getrandom`, which
//! maps to the right primitive on every platform — `getrandom(2)` / `/dev/urandom`
//! on Unix, `BCryptGenRandom` on Windows. An earlier version read `/dev/urandom`
//! by hand and fell back to a *time-seeded* token whenever that read failed —
//! and, because the read was `#[cfg(unix)]`, it took that weak fallback on
//! **every** Windows run. A wall-clock-derived token is guessable, so that path
//! made the API token predictable. There is no fallback now: if the CSPRNG is
//! unavailable we panic rather than emit a weak token (the process aborts under
//! the release `panic = "abort"` profile — a loud failure, never a silent
//! downgrade). We hex-encode by hand (one fold) rather than pull `hex`.
//!
//! 16 random bytes → 32 hex chars. That is 128 bits of entropy — far more than
//! enough for a localhost, single-session token whose only job is to stop other
//! *local* processes from poking the API. (The 127.0.0.1 bind is the primary
//! boundary; the token is the secondary one.)

/// Resolve the session token for a server run.
///
/// Honours a caller-supplied `ONEBRAIN_TOKEN` env var (≥ 32 chars, charset
/// `[A-Za-z0-9_-]`) so a remote / tunnel deploy can PIN a stable token across
/// restarts — the `?token=` URL then stays valid and bookmarkable, which is
/// what makes `app.example.com` usable without re-reading a fresh token after
/// every restart. The operator is responsible for making a pinned token long +
/// unguessable. If it's set but too short (unset is fine — we generate one),
/// or set but contains characters outside `[A-Za-z0-9_-]`, this returns an
/// error instead of silently swapping in a random token: the token round-trips
/// through `serve --open`'s Windows `cmd /C start` launch (see
/// `commands::serve::open_browser`), and a token with shell/cmd metacharacters
/// (`&`, `%`, `"`, spaces, ...) is exactly what an operator pins BY MISTAKE if
/// they typo or copy a secret with the wrong shape — silently replacing it
/// with a random value would also break their tunnel setup (the URL they
/// bookmarked stops working) with no visible cause. A hard error is loud and
/// actionable in a way a silent swap or a buried warning log is not.
pub fn resolve_token() -> anyhow::Result<String> {
    resolve_token_from(std::env::var("ONEBRAIN_TOKEN").ok())
}

/// Pure core of [`resolve_token`] (env value injected) so the rule is testable
/// without touching process-global env state.
fn resolve_token_from(env: Option<String>) -> anyhow::Result<String> {
    if let Some(raw) = env {
        let t = raw.trim();
        if !t.is_empty() {
            if t.len() < 32 {
                anyhow::bail!(
                    "ONEBRAIN_TOKEN is too short ({} chars, need >= 32) — refusing to \
                     silently swap in a random token (that would break a pinned tunnel \
                     URL with no visible cause). Pin a strong value, e.g. the output of \
                     `openssl rand -hex 16`, or unset $ONEBRAIN_TOKEN to use a generated one.",
                    t.chars().count()
                );
            }
            if !t.chars().all(is_token_char) {
                anyhow::bail!(
                    "ONEBRAIN_TOKEN contains characters outside [A-Za-z0-9_-] — refusing \
                     to use it (and refusing to silently swap in a random token instead, \
                     which would break a pinned tunnel URL with no visible cause). This \
                     token rides in a URL and, on Windows, in a `cmd /C start` command \
                     line — metacharacters like `&`, `%`, `\"`, or spaces are unsafe there. \
                     Pin a value made only of letters, digits, `_`, and `-`, e.g. the \
                     output of `openssl rand -hex 16`."
                );
            }
            return Ok(t.to_string());
        }
    }
    Ok(generate_token())
}

/// `true` for the charset a pinned `ONEBRAIN_TOKEN` must stick to:
/// `[A-Za-z0-9_-]`. Generated tokens are hex (a strict subset), so this only
/// ever rejects operator-supplied values.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Generate a fresh 32-hex-char (128-bit) session token from the OS CSPRNG.
///
/// `getrandom::fill` pulls from the platform's cryptographic RNG on every target
/// (`getrandom(2)` / `/dev/urandom` on Unix, `BCryptGenRandom` on Windows). It
/// only errors if the OS RNG is genuinely unavailable — a broken environment in
/// which the server has no business handing out a security token. We panic
/// rather than fall back to anything weaker: no predictable token ever ships.
pub fn generate_token() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect(
        "OS CSPRNG (getrandom) is unavailable — refusing to emit a weak session \
         token; the server cannot start without a working random source",
    );
    hex_encode(&buf)
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
    fn batch_of_tokens_has_no_collisions() {
        // Regression guard for the removed weak fallback. The old time-seeded
        // token derived from the wall clock + a (constant, per-function) stack
        // address, so a tight generation loop could repeat a value when the
        // clock didn't advance between calls. A real CSPRNG makes 1024 draws of
        // 128-bit tokens collision-free (birthday odds ≈ 2⁻¹⁰⁷). This runs on
        // every CI target, so it also proves the Windows path is no longer weak.
        use std::collections::HashSet;
        const N: usize = 1024;
        let mut seen = HashSet::with_capacity(N);
        for _ in 0..N {
            assert!(
                seen.insert(generate_token()),
                "token collision within a batch of {N} — RNG is not cryptographic"
            );
        }
    }

    #[test]
    fn resolve_pins_a_strong_env_token() {
        let pinned = "my-stable-remote-pinned-token-0123456789"; // ≥ 32 chars, valid charset
        assert_eq!(
            resolve_token_from(Some(pinned.to_string())).unwrap(),
            pinned
        );
        // surrounding whitespace is trimmed
        assert_eq!(
            resolve_token_from(Some(format!("  {pinned}  "))).unwrap(),
            pinned
        );
    }

    #[test]
    fn resolve_falls_back_when_env_absent() {
        // unset → fresh 32-hex token
        assert_eq!(resolve_token_from(None).unwrap().len(), 32);
        // empty → fresh token
        assert_eq!(resolve_token_from(Some(String::new())).unwrap().len(), 32);
    }

    #[test]
    fn resolve_rejects_too_short_env_token() {
        // too short (< 32) → hard error, never silently swapped for a fresh
        // token (that would break a pinned tunnel URL with no visible cause).
        let short = "abc";
        let err = resolve_token_from(Some(short.to_string())).unwrap_err();
        assert!(err.to_string().contains("too short"), "{err}");

        // a 20-char token (honoured under the old 16-char floor) is now rejected too
        let medium = "0123456789abcdef0123"; // 20 chars
        assert!(resolve_token_from(Some(medium.to_string())).is_err());
    }

    #[test]
    fn resolve_rejects_bad_charset_env_token() {
        // ≥ 32 chars but containing characters unsafe in a Windows `cmd /C
        // start` command line — must be a hard error, not a silent swap.
        for bad in [
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&calc&", // & (cmd command separator)
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa with space",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa%PATH%", // % (cmd/env expansion)
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"quoted\"",
        ] {
            assert!(bad.trim().chars().count() >= 32, "fixture too short: {bad}");
            let err = resolve_token_from(Some(bad.to_string()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("[A-Za-z0-9_-]"), "{bad}: {err}");
        }
    }

    #[test]
    fn hex_encode_pads_each_byte_to_two_chars() {
        // Leading-zero nibble must NOT be dropped (`0x0a` → "0a", not "a").
        assert_eq!(hex_encode(&[0x0a, 0xff, 0x00]), "0aff00");
    }
}
