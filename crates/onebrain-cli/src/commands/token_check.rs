//! `onebrain token check <path>` — the CLI a PreToolUse read-hook (design
//! §5b, Track 8) calls to gate a repeat vault-doc `Read`. Exit-code protocol
//! (RTK-inspired, frozen by design):
//!
//!   - exit 0 = ALLOW (stdout empty) — let the read proceed untouched.
//!   - exit 2 = DENY  — the doc was already delivered unchanged this
//!     session; the [`ReferenceEnvelope`] JSON (T4's frozen shape) is on
//!     stdout so the hook can hand the agent the `--force` recovery path.
//!
//! **Fail-open, unconditionally.** ANY trouble getting a trustworthy verdict
//! — no vault, no daemon, a daemon too old to have the route, a transport
//! error, the daemon reporting no resolvable session token, or the whole
//! round-trip exceeding a 200ms budget — exits 0 (allow) and records a
//! `hook_failopen` [`GainEvent`] (design §5c-5: "fail-open is visible in gain
//! data, never silent") so `/doctor` can surface a dead daemon instead of
//! silent degradation. A read is NEVER blocked by infrastructure trouble.
//!
//! The 200ms budget is enforced by THIS command, not by the daemon client's
//! own (much longer, 2-30s) HTTP timeouts — the daemon round-trip runs on a
//! detached thread; the caller only waits up to [`CHECK_TIMEOUT`] on a
//! channel and fails open the instant that elapses, regardless of what the
//! background thread is still doing (the process exits right after `run`
//! returns anyway, via the dispatcher's `std::process::exit`).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use onebrain_core::path::ResolvedVault;
use onebrain_token::{CacheKind, GainEvent, OptLevel, ReferenceEnvelope, Surface};

use crate::commands::daemon_client::{self, DaemonHandle};
use crate::commands::token_runner::{record_gain, resolve_session_token_opt};

/// Hard wall-clock budget for the whole daemon round-trip (design §5b:
/// "decision target <200ms"). Enforced client-side — the daemon client's own
/// timeouts (2-30s, sized for cold embeds) are all longer than this and would
/// otherwise let a wedged daemon block the very read it's supposed to gate.
const CHECK_TIMEOUT: Duration = Duration::from_millis(200);

/// What the daemon round-trip (or a client-side pre-check) produced. Kept
/// separate from [`Decision`] so the exit-code/stdout mapping ([`decide`])
/// stays a pure function with no I/O in sight — the TDD target for plan 5.1.
#[derive(Debug, Clone, PartialEq)]
enum CheckOutcome {
    /// The ledger says this exact content was already delivered this session.
    Unchanged(ReferenceEnvelope),
    /// Any verdict that doesn't gate the read (first send, changed, ledger
    /// inactive at this level, or an unindexed doc).
    Allow,
    /// Couldn't get a trustworthy verdict — always resolves to an allow, but
    /// tagged with WHY so the gain event (and `/doctor`) can tell degraded
    /// infrastructure apart from a legitimate "allow" business decision.
    FailOpen(&'static str),
}

/// The 0/2 exit protocol (design §5b) — the ONLY place it's decided.
#[derive(Debug, Clone, PartialEq)]
enum Decision {
    Allow,
    Deny(ReferenceEnvelope),
    FailOpen(&'static str),
}

/// Pure `CheckOutcome` → `Decision` mapping. Trivial today (1:1), but kept as
/// its own step so [`outcome_from_response`] (the interesting branching) and
/// the exit-code protocol are independently testable and can't silently drift
/// from one another as either grows.
fn decide(outcome: CheckOutcome) -> Decision {
    match outcome {
        CheckOutcome::Unchanged(r) => Decision::Deny(r),
        CheckOutcome::Allow => Decision::Allow,
        CheckOutcome::FailOpen(reason) => Decision::FailOpen(reason),
    }
}

/// Parse the daemon's `/api/token/ledger/check` response body (§the shape
/// `token_api::LedgerCheckResponse` serializes) into a [`CheckOutcome`] — the
/// verdict→outcome mapping plan 5.1 asks to TDD first. Every verdict the
/// route can answer today is handled explicitly; an unrecognized FUTURE
/// verdict fails open rather than guessing — the same "absence of a rule must
/// never map to auto-allow" lesson (design §5c-4) applied in the other
/// direction: an unknown answer must never map to auto-DENY either, so it
/// fails open, the universally safe default for a gate that must never block
/// a read on trouble.
fn outcome_from_response(value: &serde_json::Value) -> CheckOutcome {
    let verdict = value.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
    match verdict {
        "unchanged" => match value
            .get("reference")
            .and_then(|r| serde_json::from_value::<ReferenceEnvelope>(r.clone()).ok())
        {
            Some(reference) => CheckOutcome::Unchanged(reference),
            None => CheckOutcome::FailOpen("unchanged_verdict_missing_reference"),
        },
        "first_send" | "changed" | "inactive" | "unknown_doc" => CheckOutcome::Allow,
        // The daemon read our attached session_token as empty. Client-side
        // this IS the "no session token" fail-open condition the plan calls
        // out explicitly — `DaemonHandle::token_ledger_check` resolves the
        // token internally and sends `""` when unresolvable, so we only
        // learn about it here, one round-trip later.
        "no_session" => CheckOutcome::FailOpen("no_session_token"),
        _ => CheckOutcome::FailOpen("unrecognized_verdict"),
    }
}

/// Query the daemon for `path`'s ledger verdict — the part of [`run`] that
/// touches the network, isolated so it can run on a background thread under
/// [`CHECK_TIMEOUT`]. `vault_root` scopes daemon discovery to the SAME vault
/// (a mismatched-vault daemon is never adopted for this check — same rule
/// every other daemon-client caller follows).
fn query_daemon(vault_root: &Path, path: &str) -> CheckOutcome {
    let handle: DaemonHandle = match daemon_client::discover_matching(Some(vault_root)) {
        Ok(Some(handle)) => handle,
        Ok(None) => return CheckOutcome::FailOpen("no_daemon"),
        Err(_) => return CheckOutcome::FailOpen("daemon_discovery_error"),
    };
    match handle.token_ledger_check(path, None, None, true) {
        Ok(Some(value)) => outcome_from_response(&value),
        // 404 — daemon predates the token routes (design §8 version skew).
        Ok(None) => CheckOutcome::FailOpen("daemon_version_skew"),
        Err(_) => CheckOutcome::FailOpen("ledger_check_error"),
    }
}

/// Run [`query_daemon`] on a detached thread and wait up to [`CHECK_TIMEOUT`].
/// A timeout fails open; the background thread is simply abandoned (the
/// process exits right after `run` returns anyway, taking it down with it).
fn query_daemon_with_deadline(vault_root: PathBuf, path: String) -> CheckOutcome {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = query_daemon(&vault_root, &path);
        let _ = tx.send(outcome);
    });
    rx.recv_timeout(CHECK_TIMEOUT)
        .unwrap_or(CheckOutcome::FailOpen("timeout_200ms"))
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record the fail-open [`GainEvent`] (design §5c-5). Best-effort: when the
/// vault/collection isn't resolvable there's nowhere to write it, and
/// dropping it silently is the same "no sink → event dropped" contract every
/// other surface uses ([`record_gain`]). `reason` rides in `transform` (no
/// new field on the frozen `GainEvent` schema) so `/doctor` can tell WHY a
/// fail-open happened, not just that one did.
fn record_failopen(resolved: Option<&ResolvedVault>, reason: &str, session_token: Option<String>) {
    let Some(resolved) = resolved else {
        return;
    };
    record_gain(
        resolved,
        GainEvent {
            ts: now_ts(),
            surface: Surface::ReadHook,
            transform: reason.to_string(),
            level: OptLevel::Off,
            bytes_before: 0,
            bytes_after: 0,
            cache: CacheKind::HookFailopen,
            session_token,
        },
    );
}

/// Entry point invoked from the dispatcher — mirrors `harness_run::run`'s
/// contract: returns the exit code the binary should call
/// `std::process::exit` with. `path` is the doc path the hook is about to let
/// `Read` touch (vault-relative or absolute — only used for the ledger key
/// and daemon discovery, never opened by this process).
pub fn run(vault_flag: Option<PathBuf>, path: &str) -> Result<i32> {
    let path = path.trim();

    // Local, filesystem-only resolution — NOT part of the network timeout
    // budget. `None` (no vault findable at all) still fails open; there's
    // just nowhere to record the gain event (best-effort, like every other
    // surface — see `record_failopen`).
    let resolved = crate::vault_ctx::require(vault_flag).ok();

    if path.is_empty() {
        record_failopen(resolved.as_ref(), "empty_path", None);
        return Ok(0);
    }

    let outcome = match &resolved {
        Some(r) => query_daemon_with_deadline(r.root.as_path().to_path_buf(), path.to_string()),
        None => CheckOutcome::FailOpen("no_vault"),
    };

    match decide(outcome) {
        Decision::Allow => Ok(0),
        Decision::Deny(reference) => {
            println!("{}", serde_json::to_string(&reference)?);
            Ok(2)
        }
        Decision::FailOpen(reason) => {
            record_failopen(resolved.as_ref(), reason, resolve_session_token_opt());
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_token::JsonlGainWriter;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // ── Pure verdict→outcome mapping (plan 5.1's primary TDD target) ───────

    fn envelope() -> ReferenceEnvelope {
        ReferenceEnvelope::new("a.md", "deadbeef", 4096)
    }

    #[test]
    fn unchanged_verdict_with_reference_denies() {
        let value = serde_json::json!({
            "verdict": "unchanged",
            "reference": envelope(),
        });
        assert_eq!(
            outcome_from_response(&value),
            CheckOutcome::Unchanged(envelope())
        );
    }

    #[test]
    fn unchanged_verdict_missing_reference_fails_open() {
        let value = serde_json::json!({ "verdict": "unchanged" });
        assert_eq!(
            outcome_from_response(&value),
            CheckOutcome::FailOpen("unchanged_verdict_missing_reference")
        );
    }

    #[test]
    fn first_send_and_changed_and_inactive_and_unknown_doc_allow() {
        for verdict in ["first_send", "changed", "inactive", "unknown_doc"] {
            let value = serde_json::json!({ "verdict": verdict });
            assert_eq!(
                outcome_from_response(&value),
                CheckOutcome::Allow,
                "verdict {verdict:?} should allow"
            );
        }
    }

    #[test]
    fn no_session_verdict_fails_open() {
        let value = serde_json::json!({ "verdict": "no_session" });
        assert_eq!(
            outcome_from_response(&value),
            CheckOutcome::FailOpen("no_session_token")
        );
    }

    #[test]
    fn unrecognized_future_verdict_fails_open_never_denies() {
        let value = serde_json::json!({ "verdict": "something_v3.4.11_invents" });
        assert_eq!(
            outcome_from_response(&value),
            CheckOutcome::FailOpen("unrecognized_verdict")
        );
    }

    #[test]
    fn missing_verdict_field_fails_open() {
        let value = serde_json::json!({});
        assert_eq!(
            outcome_from_response(&value),
            CheckOutcome::FailOpen("unrecognized_verdict")
        );
    }

    // ── Pure outcome→decision (exit code + stdout) mapping ─────────────────

    #[test]
    fn decide_maps_every_outcome_variant() {
        assert_eq!(decide(CheckOutcome::Allow), Decision::Allow);
        assert_eq!(
            decide(CheckOutcome::Unchanged(envelope())),
            Decision::Deny(envelope())
        );
        assert_eq!(
            decide(CheckOutcome::FailOpen("no_daemon")),
            Decision::FailOpen("no_daemon")
        );
    }

    // ── `run` end-to-end: every fail-open branch, each asserting the
    //    `hook_failopen` gain event lands (plan 5.1) ────────────────────────

    /// A vault with a search collection configured, plus an isolated
    /// `ONEBRAIN_CACHE_DIR` — enough for `record_gain`'s `collection_for` to
    /// resolve without touching the real cache dir.
    fn test_vault() -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: token-check-test\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        (vault, cache)
    }

    fn gain_events(cache: &Path) -> Vec<GainEvent> {
        // `ONEBRAIN_CACHE_DIR` overrides `search_cache_root()`'s PARENT
        // (`default_state_dir()`); the actual collection cache dir is
        // `<ONEBRAIN_CACHE_DIR>/search/<collection>/...` — see
        // `search_common::search_cache_root`.
        let dir = cache
            .join("search")
            .join("token-check-test")
            .join("token")
            .join("gain");
        JsonlGainWriter::new(&dir).read_all().unwrap_or_default()
    }

    #[test]
    fn empty_path_fails_open_and_records_event() {
        let (vault, cache) = test_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()), // no daemon.json under here
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        let code = run(Some(vault.path().to_path_buf()), "   ").expect("run should not error");
        assert_eq!(code, 0);
        let events = gain_events(cache.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cache, CacheKind::HookFailopen);
        assert_eq!(events[0].transform, "empty_path");
        assert_eq!(events[0].surface, Surface::ReadHook);
    }

    #[test]
    fn no_daemon_fails_open_and_records_event() {
        // No daemon.json under this HOME → `discover_matching` sees nothing.
        let (vault, cache) = test_vault();
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(code, 0);
        let events = gain_events(cache.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cache, CacheKind::HookFailopen);
        assert_eq!(events[0].transform, "no_daemon");
    }

    /// Writes a `daemon.json` under `home/.onebrain/run/` pointing at
    /// `port`/`token`, bound to `vault` — the exact record format
    /// `discover_matching` reads (mirrors the daemon's own `DaemonInfo::write`
    /// but skipped here since the test doesn't need the private perms path).
    fn write_daemon_json(home: &Path, vault: &Path, port: u16, token: &str) {
        let run_dir = home.join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let info = daemon_client::DaemonInfo {
            port,
            token: token.to_string(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            vault: daemon_client::canonical_vault_id(vault),
        };
        std::fs::write(
            run_dir.join("daemon.json"),
            serde_json::to_vec_pretty(&info).unwrap(),
        )
        .unwrap();
    }

    /// A minimal hand-rolled HTTP/1.1 responder — good enough for the two
    /// GET/POST routes `query_daemon` hits. Avoids pulling a mock-HTTP dev-dep
    /// just to control response TIMING (mockito doesn't expose per-response
    /// delay), which the timeout test below needs. `ledger_body` is served
    /// (as 200 JSON) for `/api/token/ledger/check`; `ledger_status` overrides
    /// the status line when non-200 (e.g. 404 for version-skew); `ledger_delay`
    /// sleeps before responding, to simulate a wedged daemon.
    fn start_fake_daemon(
        ledger_status: u16,
        ledger_body: &'static str,
        ledger_delay: Duration,
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.starts_with("GET /api/health") {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        );
                    } else {
                        if !ledger_delay.is_zero() {
                            std::thread::sleep(ledger_delay);
                        }
                        let status_line = match ledger_status {
                            200 => "HTTP/1.1 200 OK",
                            404 => "HTTP/1.1 404 Not Found",
                            _ => "HTTP/1.1 500 Internal Server Error",
                        };
                        let resp = format!(
                            "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            ledger_body.len(),
                            ledger_body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                });
            }
        });
        port
    }

    #[test]
    fn daemon_predates_token_routes_fails_open_and_records_event() {
        let (vault, cache) = test_vault();
        let port = start_fake_daemon(404, "not found", Duration::ZERO);
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "daemon-check-token-123");

        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(code, 0);
        let events = gain_events(cache.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cache, CacheKind::HookFailopen);
        assert_eq!(events[0].transform, "daemon_version_skew");
    }

    #[test]
    fn no_session_response_fails_open_and_records_event() {
        let (vault, cache) = test_vault();
        let port = start_fake_daemon(200, r#"{"verdict":"no_session"}"#, Duration::ZERO);
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "daemon-check-token-123");

        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(code, 0);
        let events = gain_events(cache.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cache, CacheKind::HookFailopen);
        assert_eq!(events[0].transform, "no_session_token");
    }

    #[test]
    fn timeout_fails_open_fast_and_records_event() {
        let (vault, cache) = test_vault();
        // The fake daemon answers the ledger check but only after 2s — far
        // longer than the 200ms budget. `run` must return well before that.
        let port = start_fake_daemon(200, r#"{"verdict":"first_send"}"#, Duration::from_secs(2));
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "daemon-check-token-123");

        let started = std::time::Instant::now();
        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        let elapsed = started.elapsed();
        assert_eq!(code, 0);
        assert!(
            elapsed < Duration::from_millis(1500),
            "expected the 200ms budget to cut this off well under the daemon's \
             2s delay, took {elapsed:?}"
        );
        let events = gain_events(cache.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cache, CacheKind::HookFailopen);
        assert_eq!(events[0].transform, "timeout_200ms");
    }

    #[test]
    fn unchanged_verdict_denies_with_reference_json_on_stdout() {
        let (vault, cache) = test_vault();
        let body = r#"{"verdict":"unchanged","reference":{"doc_path":"notes/a.md","hash":"deadbeef","sent_earlier":true,"bytes_saved":123,"rematerialize":"onebrain search get notes/a.md --force"}}"#;
        let port = start_fake_daemon(200, body, Duration::ZERO);
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "daemon-check-token-123");

        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(code, 2);
        // Deny never records a gain event from `check` itself — the ledger
        // route doesn't meter (only `search get`/MCP `get` deliveries do).
        assert!(gain_events(cache.path()).is_empty());
    }

    #[test]
    fn allow_verdict_produces_no_gain_event() {
        let (vault, cache) = test_vault();
        let port = start_fake_daemon(200, r#"{"verdict":"first_send"}"#, Duration::ZERO);
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        write_daemon_json(vault.path(), vault.path(), port, "daemon-check-token-123");

        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(code, 0);
        assert!(gain_events(cache.path()).is_empty());
    }

    #[test]
    fn no_vault_fails_open_without_panicking() {
        // A directory with no onebrain.yml anywhere above it — vault_ctx::require
        // errors, so `run` has nowhere to record a gain event but must still
        // allow the read.
        let empty = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("HOME", empty.path());
        let code = run(Some(empty.path().to_path_buf()), "a.md").expect("run should not error");
        assert_eq!(code, 0);
    }
}
