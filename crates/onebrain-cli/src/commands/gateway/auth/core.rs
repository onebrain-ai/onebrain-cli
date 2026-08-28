//! Pure auth primitives: base64url, PKCE S256, opaque-secret minting,
//! constant-time compare, epoch clock. NO I/O, NO HTTP — persisted state
//! (which uses these primitives to mint codes/tokens/pairing codes) lives in
//! [`super::store`].
//!
//! Zero new crate dependencies (hard constraint for this task): base64url and
//! constant-time-compare are hand-rolled, mirroring the exact precedents this
//! crate already ships —
//! [`crate::server::token::hex_encode`]/[`crate::server::token::generate_token`]
//! for the `getrandom::fill` + hand-rolled-encoding shape, and
//! [`crate::server::auth`]'s local `constant_time_eq` for the compare shape.

use sha2::{Digest, Sha256};

/// RFC 4648 §5 ("base64url") alphabet — the same 64-symbol table as standard
/// base64 except index 62/63 are `-`/`_` instead of `+`/`/`, which is what
/// makes it safe to embed unescaped in a URL path/query. We emit it
/// UNPADDED (no trailing `=`) — the shape PKCE (RFC 7636) and OAuth opaque
/// tokens both expect.
const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url-encode `bytes` per RFC 4648 §5, with padding omitted.
///
/// Hand-rolled (see module docs for why: zero new crate deps). Standard
/// 3-bytes-in/4-chars-out grouping: each 3-byte chunk becomes four 6-bit
/// indices into [`B64URL_ALPHABET`]; a trailing 1- or 2-byte chunk emits only
/// the 2 or 3 characters that carry real bits (no `=` padding chars at all,
/// per the "nopad" contract).
pub fn base64url_nopad(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);

        let c0 = b0 >> 2;
        let c1 = ((b0 & 0x03) << 4) | (b1 >> 4);
        let c2 = ((b1 & 0x0f) << 2) | (b2 >> 6);
        let c3 = b2 & 0x3f;

        out.push(B64URL_ALPHABET[c0 as usize] as char);
        out.push(B64URL_ALPHABET[c1 as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[c2 as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[c3 as usize] as char);
        }
    }
    out
}

/// Constant-time byte-slice equality — see
/// [`crate::server::auth`]'s local `constant_time_eq` for the full
/// rationale (short version: a naive `==` short-circuits on the first
/// differing byte, which leaks a byte-at-a-time timing oracle to an attacker
/// who can measure request latency; touching every byte regardless of an
/// early mismatch removes that signal).
fn constant_time_bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Constant-time string equality (UTF-8 byte-wise). Use this for EVERY
/// comparison of a caller-presented secret against a stored one (pairing
/// codes, and — via [`pkce_s256_matches`] — PKCE challenges). Do not use
/// plain `==` for those comparisons.
pub fn constant_time_str_eq(a: &str, b: &str) -> bool {
    constant_time_bytes_eq(a.as_bytes(), b.as_bytes())
}

/// PKCE S256 verification (RFC 7636 §4.6): does `code_verifier`, once hashed
/// with SHA-256 and base64url-nopad-encoded, equal `code_challenge`?
///
/// The comparison is constant-time via [`constant_time_str_eq`] — the
/// challenge is presented by a client at the token endpoint and is exactly
/// the kind of attacker-observable secret comparison that must not leak
/// timing. A malformed (non-base64url or wrong-length) `code_challenge`
/// simply fails to match the computed value; there's no separate validation
/// step to bypass.
pub fn pkce_s256_matches(code_verifier: &str, code_challenge: &str) -> bool {
    let digest = Sha256::digest(code_verifier.as_bytes());
    let computed = base64url_nopad(&digest);
    constant_time_str_eq(&computed, code_challenge)
}

/// Fill `buf` from the OS CSPRNG, panicking (loudly, never silently
/// downgrading) if it's unavailable. Mirrors
/// [`crate::server::token::generate_token`]'s stance exactly: a broken RNG is
/// an environment the auth server has no business minting secrets in.
fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect(
        "OS CSPRNG (getrandom) is unavailable — refusing to emit a weak gateway auth secret",
    );
}

/// Mint a fresh 32-random-byte secret, base64url-nopad encoded (~43 chars,
/// 256 bits of entropy). Used for opaque access/refresh tokens, auth codes,
/// and token-family ids — anywhere the global constraint "every token/code is
/// >= 32 random bytes via getrandom" applies.
pub fn mint_secret_32() -> String {
    let mut buf = [0u8; 32];
    fill_random(&mut buf);
    base64url_nopad(&buf)
}

/// Alphabet for human-typed pairing codes: A-Z plus digits 2-9 (34 symbols).
/// Digits `0`/`1` are dropped because they're easily confused with letters
/// `O`/`I` when read off a screen and typed on another device — that's the
/// "unambiguous" in the brief. 34^8 ≈ 3.5×10^12 possible codes.
const PAIRING_ALPHABET: &[u8; 34] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ23456789";

/// Draw one unbiased index into `alphabet` (length <= 256) from the OS CSPRNG
/// via rejection sampling: reject any byte in `[256 - 256 % len, 256)` so
/// every retained byte maps to a UNIFORM index in `0..len` (a plain `% len`
/// on unrejected bytes would over-represent the low indices whenever `len`
/// doesn't evenly divide 256 — 256 % 34 == 18 here).
fn random_alphabet_byte(alphabet: &[u8]) -> u8 {
    let len = alphabet.len();
    debug_assert!(len > 0 && len <= 256, "alphabet length must be in 1..=256");
    let limit = 256 - (256 % len);
    loop {
        let mut b = [0u8; 1];
        fill_random(&mut b);
        let v = b[0] as usize;
        if v < limit {
            return alphabet[v % len];
        }
    }
}

/// Mint an 8-character pairing code from [`PAIRING_ALPHABET`], formatted
/// `XXXX-XXXX` for easy human transcription (matches the shape of e.g.
/// Windows/Xbox device-pairing codes).
pub fn mint_pairing_code() -> String {
    let mut chars = [0u8; 8];
    for slot in &mut chars {
        *slot = random_alphabet_byte(PAIRING_ALPHABET);
    }
    // `PAIRING_ALPHABET` is pure ASCII, so this is always valid UTF-8.
    let s = std::str::from_utf8(&chars).expect("pairing alphabet is ASCII");
    format!("{}-{}", &s[0..4], &s[4..8])
}

/// Current wall-clock time as Unix epoch seconds. Used to stamp `created`/
/// `expires` fields in the persisted store. Panics only if the system clock
/// reports a time before the Unix epoch (a broken host clock, not a
/// recoverable runtime condition).
pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── Step 1: base64url — RFC 4648 vectors ────────────────────────────

    #[test]
    fn base64url_rfc4648_vectors() {
        assert_eq!(base64url_nopad(b""), "");
        assert_eq!(base64url_nopad(b"f"), "Zg");
        assert_eq!(base64url_nopad(b"fo"), "Zm8");
        assert_eq!(base64url_nopad(b"foo"), "Zm9v");
    }

    /// All-0xFF inputs exercise the alphabet's TOP end (index 63/62), which
    /// is exactly where base64url (`-`/`_`) diverges from standard base64
    /// (`+`/`/`) — the "f"/"fo"/"foo" vectors above never touch those
    /// indices, so this is the vector that actually proves we emit the URL-
    /// safe alphabet. Also covers all three non-empty chunk-remainder shapes
    /// (1, 2, 3 leftover bytes) in one sweep.
    #[test]
    fn base64url_all_0xff_padding_edge_uses_dash_underscore_alphabet() {
        assert_eq!(base64url_nopad(&[0xFF]), "_w");
        assert_eq!(base64url_nopad(&[0xFF, 0xFF]), "__8");
        assert_eq!(base64url_nopad(&[0xFF, 0xFF, 0xFF]), "____");
        assert_eq!(base64url_nopad(&[0xFF, 0xFF, 0xFF, 0xFF]), "_____w");
    }

    // ── Step 1: PKCE S256 — RFC 7636 Appendix B vector ──────────────────

    const RFC7636_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC7636_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn pkce_s256_matches_rfc7636_appendix_b_vector() {
        assert!(pkce_s256_matches(RFC7636_VERIFIER, RFC7636_CHALLENGE));
    }

    #[test]
    fn pkce_s256_rejects_mutated_challenge() {
        let mut mutated = RFC7636_CHALLENGE.to_string();
        mutated.pop();
        mutated.push('X'); // last char is 'M' in the real vector — always differs
        assert!(!pkce_s256_matches(RFC7636_VERIFIER, &mutated));
    }

    #[test]
    fn pkce_s256_rejects_non_base64url_challenge() {
        assert!(!pkce_s256_matches(
            RFC7636_VERIFIER,
            "not even close to base64url!!"
        ));
    }

    // ── Minting: length/alphabet/uniqueness ─────────────────────────────

    #[test]
    fn mint_secret_32_is_43_chars_base64url_and_unique_over_1000() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let s = mint_secret_32();
            assert_eq!(
                s.len(),
                43,
                "32 random bytes should nopad-encode to 43 chars: {s}"
            );
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "secret contains a non-base64url char: {s}"
            );
            assert!(seen.insert(s), "mint_secret_32 collided within 1000 draws");
        }
    }

    #[test]
    fn mint_pairing_code_has_xxxx_dash_xxxx_shape_and_is_unique_over_1000() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let code = mint_pairing_code();
            assert_eq!(code.len(), 9, "expected XXXX-XXXX (9 chars): {code}");
            assert_eq!(code.as_bytes()[4], b'-', "dash must sit at index 4: {code}");
            for (i, c) in code.char_indices() {
                if i == 4 {
                    continue;
                }
                assert!(
                    PAIRING_ALPHABET.contains(&(c as u8)),
                    "char {c:?} at {i} not in the unambiguous pairing alphabet: {code}"
                );
            }
            assert!(
                seen.insert(code),
                "mint_pairing_code collided within 1000 draws"
            );
        }
    }

    // ── constant_time_str_eq / now_epoch_secs ───────────────────────────

    #[test]
    fn constant_time_str_eq_matches_equal_and_rejects_unequal() {
        assert!(constant_time_str_eq("same-secret", "same-secret"));
        assert!(!constant_time_str_eq("same-secret", "same-secreT"));
        assert!(!constant_time_str_eq("short", "shorter"));
        assert!(!constant_time_str_eq("", "x"));
        assert!(constant_time_str_eq("", ""));
    }

    #[test]
    fn now_epoch_secs_is_a_plausible_recent_unix_time() {
        let t = now_epoch_secs();
        // Well past this task's authorship date; guards against an obviously
        // broken clock (e.g. epoch-0) without pinning an exact value.
        assert!(t > 1_700_000_000, "epoch seconds looks implausible: {t}");
    }
}
