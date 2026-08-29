//! Shared helpers for the `onebrain-cli` integration tests.
//!
//! Lives in a subdirectory so cargo does not compile it as its own test
//! binary; each test file pulls it in with a plain `mod support;`.

#![allow(dead_code)]

use std::path::PathBuf;

/// The scratch search-cache root every spawned `onebrain` binary must be
/// pinned to via `ONEBRAIN_CACHE_DIR` — see `tests/cache_isolation_sweep.rs`
/// for the rule and `tests/cache_root_untouched.rs` for the regression guard.
///
/// Why a shared root rather than a per-test `TempDir`: several tests spawn the
/// binary more than once and expect state written by the first spawn to be
/// visible to the second, and a `TempDir` bound per call site would break that
/// while also needing a binding threaded into every chain.
///
/// **What makes the shared root safe is its LOCATION**, not any disjointness
/// of its contents: it sits under `CARGO_TARGET_TMPDIR`, inside `target/`, so
/// it is never in the developer's home, is swept by `cargo clean`, and is
/// obvious in a `du` if it ever grows. That is the whole argument. Anything
/// written here is junk in a build directory; nothing written here can damage
/// real user state.
///
/// **Collection names in this root are NOT guaranteed disjoint.** An earlier
/// version of this comment claimed they were — "every test builds its vault in
/// a fresh random tempdir, so collection names are disjoint" — and both halves
/// of that are wrong:
///
/// - Not every vault is a tempdir. `checkpoint.rs` runs the binary against the
///   checked-in fixture `tests/fixtures/checkpoint/empty_vault/` (which has a
///   `vault.yml`), and `harness.rs` / `session_init.rs` likewise use fixed
///   fixture paths. A fixture path is stable across runs and identical across
///   tests, so its `<dir>-<hash-of-abs-path>` name is deterministic and
///   SHARED, not unique.
/// - The scope claim was too narrow: `CARGO_TARGET_TMPDIR` is one directory
///   per PACKAGE, shared by all of the crate's integration-test binaries,
///   which cargo runs concurrently — so this root is shared across parallel
///   processes as well as across tests within one binary.
///
/// It is benign today only because those fixture-based tests resolve no
/// collection, or never touch the collection directory. So the invariant to
/// hold is a rule, not a fact: **a test that (a) runs against a fixed path
/// rather than a fresh tempdir AND (b) writes into the collection dir must not
/// use this shared root** — give it its own `TempDir` cache root, or a
/// collection name unique to that test. Two such tests would otherwise race
/// over one index, with nothing in the failure pointing at the cause.
pub fn scratch_cache_root() -> PathBuf {
    // #305: export the test-collection marker for every binary this test
    // process spawns — the engine then stamps `.onebrain-test-collection`
    // into any collection dir it creates, making residue enumerable instead
    // of guessable. Process-wide on purpose: children inherit it no matter
    // which helper (or bare `.env`) wired the cache root.
    std::env::set_var("ONEBRAIN_TEST_COLLECTION_MARKER", "1");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("search-cache");
    // Best-effort: consumers `create_dir_all` what they need anyway, and a
    // failure here must not panic a test for a reason unrelated to its subject.
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ── Redacted capture tails for panic messages ────────────────────────────
//
// The gateway binary integration tests (`gateway_http.rs`,
// `gateway_oauth_e2e.rs`, `gateway_approval_e2e.rs`) all spawn `onebrain
// gateway run` with stdout/stderr redirected to files inside a `TempDir`. If
// the process dies before printing its `gateway listening on …` line, those
// files are the ONLY record of why — and the `TempDir` holding them is
// deleted during the panic unwind, so anything not put into the panic
// message itself is gone by the time a human reads the failure.
//
// Two things must therefore both be true of that message:
//
//   1. It must carry the actual diagnostic. A byte count does not survive
//      contact with a real CI failure (`AuthStore::open()` unable to set
//      0700 on the runner's filesystem exits 1 with a one-line error; "214
//      bytes of stderr captured" leaves no way forward but to patch the test
//      and re-run CI).
//   2. It must not carry a host path or a credential — a binding constraint
//      of this branch, and what motivated dropping the raw interpolation in
//      the first place.
//
// [`redacted_capture_tail`] is how both hold at once. It is applied to the
// gateway's STDERR only; stdout is never emitted in any form, because
// `gateway run` prints the device-pairing code there (see
// `commands/gateway/mod.rs`'s module docs — stdout of the foreground process
// is the one place that code is ever shown).

/// Placeholder every filesystem path is collapsed to.
const PATH_PLACEHOLDER: &str = "<path>";

/// Placeholder for anything shaped like a device-pairing code.
const PAIRING_PLACEHOLDER: &str = "<code>";

/// How many trailing lines of a captured stream [`redacted_capture_tail`]
/// keeps. A startup failure is one to three lines (an `anyhow` chain plus
/// whatever `tracing` emitted just before); this is generous enough for that
/// and small enough that a chatty `RUST_LOG` cannot turn one panic into a
/// screenful.
///
/// `pub` so the tests that pin this behavior can name it rather than
/// restate the number. Those tests live in `tests/gateway_http.rs`, not
/// here, so they run once instead of once per test binary that pulls this
/// module in.
pub const CAPTURE_TAIL_LINES: usize = 12;

/// Hard byte ceiling on the same tail, applied AFTER redaction and always on
/// a `char` boundary. Belt and braces against a single enormous line. `pub`
/// for the same reason as [`CAPTURE_TAIL_LINES`].
pub const CAPTURE_TAIL_BYTES: usize = 1200;

/// The last few lines of a captured stream, with every filesystem path and
/// anything pairing-code-shaped replaced by a fixed placeholder, ready to be
/// interpolated into a panic message.
///
/// Returns `"<empty>"` for a stream that captured nothing, so "the gateway
/// died silently" and "the gateway said something we redacted to nothing"
/// are distinguishable.
///
/// **Deliberately over-broad.** Any token containing a `/` or `\` is cut at
/// the first one (see [`path_start`]), so `http://127.0.0.1:9/mcp` comes out
/// as `http:<path>` and the fixed `loopback only — /mcp requires OAuth`
/// notice loses its route names. That is the right trade for a diagnostic:
/// the thing being protected is the host path, and a redactor exact enough
/// to keep the pretty cases pretty is a redactor with cases it silently
/// misses. What survives is the part that matters — the error text
/// ("create gateway audit log dir <path> Not a directory (os error 20)"),
/// which names the operation and the OS error.
pub fn redacted_capture_tail(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines.len().saturating_sub(CAPTURE_TAIL_LINES);
    let mut out = String::new();
    for line in &lines[start..] {
        let redacted = redact_line(line);
        if redacted.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&redacted);
    }
    if out.is_empty() {
        return "<empty>".to_string();
    }
    if out.len() > CAPTURE_TAIL_BYTES {
        // Keep the END: an `anyhow` chain prints its root cause last, and a
        // fatal error is the last thing written before the process exits.
        let mut cut = out.len() - CAPTURE_TAIL_BYTES;
        while cut < out.len() && !out.is_char_boundary(cut) {
            cut += 1;
        }
        out = format!("[…] {}", &out[cut..]);
    }
    out
}

/// Redact one line: split on ASCII whitespace and pass every token through
/// [`redact_token`]. Whitespace runs collapse to a single space, which also
/// normalizes tabs and stray indentation out of the panic message.
fn redact_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for token in line.split_ascii_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&redact_token(token));
    }
    out
}

/// Redact one whitespace-delimited token.
///
/// A path is replaced from where it STARTS to the end of the token, so
/// `path=/home/runner/.onebrain` keeps its `path=` key and loses its value.
/// A whole token shaped like a pairing code is replaced outright.
fn redact_token(token: &str) -> String {
    if let Some(at) = path_start(token) {
        return format!("{}{PATH_PLACEHOLDER}", &token[..at]);
    }
    if looks_like_pairing_code(token) {
        return PAIRING_PLACEHOLDER.to_string();
    }
    token.to_string()
}

/// Byte offset of the first path separator in `token` — the point from
/// which the rest of the token is replaced.
///
/// **The rule is deliberately the crudest one that cannot miss: ANY `/` or
/// `\`, anywhere in the token.** An earlier draft required the separator to
/// sit at the token start or just after a delimiter byte (`path=/x`, `"/x"`,
/// `(/x)`), which is prettier — and which silently misses `dir/Users/alice`,
/// where the host path is glued to a preceding word with no delimiter. There
/// is no bound on the shapes a future `tracing` line can produce, so the
/// precondition is dropped rather than extended; `and/or` becoming
/// `and<path>` is the price, and it is the right way round to be wrong.
///
/// The returned offset is always a `char` boundary: it points at an ASCII
/// byte, and an ASCII byte can never be a UTF-8 continuation byte.
fn path_start(token: &str) -> Option<usize> {
    token.bytes().position(|b| b == b'/' || b == b'\\')
}

/// Is `token` shaped like a device-pairing code (`XXXX-XXXX` over the
/// uppercase-alphanumeric pairing alphabet)?
///
/// Defense in depth only: the pairing code is printed to the gateway's
/// STDOUT, and [`redacted_capture_tail`] is only ever applied to stderr. The
/// check costs nothing and means a future `tracing` line that leaks the code
/// does not also leak it into CI logs through a test panic.
fn looks_like_pairing_code(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() == 9
        && b[4] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_uppercase() || c.is_ascii_digit())
}
