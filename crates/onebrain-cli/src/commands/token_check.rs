//! `onebrain token check <path>` — the CLI a PreToolUse read-hook (design
//! §5b, Track 8) calls to gate a repeat vault-doc `Read`. Exit-code protocol
//! (RTK-inspired, frozen by design):
//!
//!   - exit 0 = ALLOW (stdout empty) — let the read proceed untouched.
//!   - exit 2 = DENY  — the doc was already delivered unchanged this
//!     session; the [`ReferenceEnvelope`] JSON (T4's frozen shape) is on
//!     stdout so the hook can hand the agent the `--force` recovery path.
//!
//! **Daemon → Direct, then fail-open.** When a warm, vault-bound daemon holds
//! the engine the check routes to it (under the 200ms budget below); when there
//! is NO such daemon the check opens the engine + token ledger **directly**
//! in-process ([`check_direct`], mirroring `search get`'s daemon→Direct
//! fallback) and gates from there — so the read-hook works whether or not a
//! daemon is running (issue #248: before, no daemon meant an unconditional
//! `no_daemon` fail-open, i.e. the gate silently never fired without a warm
//! daemon).
//!
//! **Fail-open on trouble.** ANY trouble getting a trustworthy verdict — no
//! vault, a daemon too old to have the route, a transport error, an unopenable
//! engine or token cache, no resolvable session token, or the daemon
//! round-trip exceeding the 200ms budget — exits 0 (allow) and records a
//! `hook_failopen` [`GainEvent`] (design §5c-5: "fail-open is visible in gain
//! data, never silent") so `/doctor` can surface degradation instead of it
//! being silent. A read is NEVER blocked by infrastructure trouble.
//!
//! The 200ms budget is enforced by THIS command, not by the daemon client's
//! own (much longer, 2-30s) HTTP timeouts — the daemon round-trip runs on a
//! detached thread; the caller only waits up to [`CHECK_TIMEOUT`] on a
//! channel and fails open the instant that elapses, regardless of what the
//! background thread is still doing (the process exits right after `run`
//! returns anyway, via the dispatcher's `std::process::exit`). The Direct leg
//! runs synchronously (no artificial deadline): with no daemon there is no
//! redb-lock contention, so the engine opens promptly — and a lock genuinely
//! held elsewhere errors at once (non-blocking flock), never hangs.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use onebrain_core::path::ResolvedVault;
use onebrain_token::{CacheKind, GainEvent, OptLevel, ReferenceEnvelope, Surface};

use crate::commands::daemon_client::{self, DaemonHandle};
use crate::commands::search_common::{daemon_routing_disabled, normalize_doc_path, open_engine};
use crate::commands::token_runner::{record_gain, resolve_session_token_opt, token_db_path};

/// Default wall-clock budget for the whole daemon round-trip (design §5b:
/// "decision target <200ms") when the vault config can't be read. Enforced
/// client-side — the daemon client's own timeouts (2-30s, sized for cold
/// embeds) are all longer than this and would otherwise let a wedged daemon
/// block the very read it's supposed to gate. The operative budget is
/// `token_optimization.check_timeout_ms` (#264, default 200) — pinned equal to
/// this constant by a test — so iCloud / networked vaults can raise it above
/// 200ms rather than fail open silently every time (the #264 field finding).
const DEFAULT_CHECK_TIMEOUT: Duration =
    Duration::from_millis(onebrain_core::config::default_check_timeout_ms() as u64);

/// The daemon round-trip budget for this vault: `token_optimization
/// .check_timeout_ms` (default 200ms). An unreadable config, or a configured
/// `0` (which would be an instant-timeout that silently disables the gate),
/// both fall back to [`DEFAULT_CHECK_TIMEOUT`] — a gate must never lose its
/// budget, and never self-disable via a degenerate value.
fn check_timeout(resolved: &ResolvedVault) -> Duration {
    let ms = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.token_optimization.check_timeout_ms)
        .unwrap_or_else(|_| onebrain_core::config::default_check_timeout_ms());
    if ms == 0 {
        DEFAULT_CHECK_TIMEOUT
    } else {
        Duration::from_millis(ms as u64)
    }
}

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

/// Result of the warm-daemon leg of [`resolve_outcome`]: either an adopted
/// daemon produced a verdict (or a daemon-side fail-open), or there is no
/// vault-bound daemon to route to and the caller must fall through to the
/// in-process Direct check ([`check_direct`]).
enum DaemonLeg {
    /// The daemon answered — or a daemon-side error / the 200ms timeout fails
    /// open. Authoritative: an adopted daemon holds the engine, so a Direct
    /// open would only hit the redb lock it holds — never second-guess it.
    Verdict(CheckOutcome),
    /// No usable daemon — a missing discovery record, or a version/vault
    /// mismatch, or the run dir couldn't be located. All resolved from local
    /// files with no network, so falling through to [`check_direct`] stays fast.
    FallThrough,
}

/// Query a warm, vault-bound daemon for `path`'s ledger verdict — the part of
/// [`resolve_outcome`] that touches the network, isolated so it can run on a
/// background thread under the configured budget. `vault_root` scopes daemon
/// discovery to the SAME vault (a mismatched-vault daemon is never adopted —
/// same rule every other daemon-client caller follows); a non-match yields
/// [`DaemonLeg::FallThrough`] so the caller opens the engine directly (the #248
/// fix), rather than the old unconditional `no_daemon` fail-open.
///
/// **#264 — adopt a same-vault daemon REGARDLESS of version**
/// ([`daemon_client::discover_same_vault_any_version`], the same route
/// `token gain` uses for the same reason). The prior version-EXACT
/// [`daemon_client::discover_matching`] was the root cause of the field
/// finding: after a `brew upgrade`, the still-running same-vault daemon is
/// version-skewed, so `discover_matching` returned `FallThrough` → the Direct
/// leg cold-opened the engine — but the skewed daemon STILL held both
/// `token.redb` and the engine lock, so the open collided and collapsed into
/// the opaque `engine_open_error` (the 25× majority in `token gain --history`).
/// Adopting the warm daemon serves the verdict from its HELD engine, lock-free
/// (`post_ledger_check`) — no cold open, no collision. A version-skewed daemon
/// too old to have the route 404s → `daemon_version_skew` fail-open (clean, and
/// it self-heals once the daemon restarts at v3.4.13+); an older daemon that
/// HAS the route but predates #255's path normalization simply misses on the
/// absolute path → `first_send`/`unknown_doc` → Allow (also clean — the gate
/// fires once the warm daemon is v3.4.13+ too). Either way, no engine_open_error.
fn query_daemon_leg(vault_root: &Path, path: &str) -> DaemonLeg {
    let handle: DaemonHandle =
        match daemon_client::discover_same_vault_any_version(Some(vault_root)) {
            Ok(Some(handle)) => handle,
            // No same-vault record, a different-vault daemon, or a daemon that
            // vanished mid-discovery (no longer holds the lock) → nothing same-vault
            // holds `token.redb`, so falling through to the Direct in-process check
            // is safe (self-healing, mirrors `token gain`'s #258 Gap 8 handling).
            Ok(None) => return DaemonLeg::FallThrough,
            // Can't even locate the run dir → treat as "no daemon" and let the
            // local engine (authoritative when nothing else holds it) answer.
            Err(_) => return DaemonLeg::FallThrough,
        };
    DaemonLeg::Verdict(match handle.token_ledger_check(path, None, None, true) {
        Ok(Some(value)) => outcome_from_response(&value),
        // 404 — daemon predates the token routes (design §8 version skew). It
        // still holds the engine, so a Direct open would only hit its lock;
        // fail open rather than fall through.
        Ok(None) => CheckOutcome::FailOpen("daemon_version_skew"),
        Err(_) => CheckOutcome::FailOpen("ledger_check_error"),
    })
}

/// Run [`query_daemon_leg`] on a detached thread and wait up to `budget`
/// (`token_optimization.check_timeout_ms`, #264). A timeout fails open (a
/// wedged daemon holds the engine — a Direct open would only hit its lock, so
/// we do NOT fall through); the background thread is simply abandoned (the
/// process exits right after `run` returns anyway, taking it down with it).
fn query_daemon_leg_with_deadline(
    vault_root: PathBuf,
    path: String,
    budget: Duration,
) -> DaemonLeg {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(query_daemon_leg(&vault_root, &path));
    });
    rx.recv_timeout(budget)
        .unwrap_or(DaemonLeg::Verdict(CheckOutcome::FailOpen("daemon_timeout")))
}

/// True when an `open_engine` failure is the redb-lock-held-elsewhere case
/// (`EngineBusy`, exit-77 territory), as opposed to a genuine cold/slow open
/// failure. `search_common::map_engine_open_error` re-wraps a redb lock
/// collision as [`onebrain_core::CoreError::EngineBusy`] (dropping the original
/// typed `onebrain_search::EngineBusy` head), so the primary check is on
/// `CoreError`; the raw search-layer predicate is kept as a belt-and-suspenders
/// for any path that surfaces the untyped-mapped variant. Lets `check_direct`
/// record `engine_busy` vs `engine_open_error` distinctly (#264 A#5).
fn is_engine_busy_error(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<onebrain_core::CoreError>()
            .is_some_and(|e| matches!(e, onebrain_core::CoreError::EngineBusy(_)))
    }) || onebrain_search::error::is_engine_busy(err)
}

/// In-process (Direct-mode) ledger check — the #248 fix. When no vault-bound
/// daemon holds the engine, `token check` used to fail open unconditionally
/// (`no_daemon`), so the read-hook silently never gated. This opens the engine
/// and token ledger directly and runs the SAME verdict logic the daemon's
/// `/api/token/ledger/check` route runs (mirroring `search get`'s daemon→Direct
/// fallback):
///
/// - **M3 level gate** — the ledger only activates at `balanced`/`aggressive`
///   (`level >= OptLevel::Balanced`, the same gate `search get` applies); below
///   that it's `Allow` (the route's `inactive` verdict).
/// - **hash from the engine** — the read-hook has only a path, so the current
///   content hash is [`onebrain_search::engine::Engine::doc_hash`]; an
///   unknown/unindexed doc (`None`) is `Allow` (the route's `unknown_doc`),
///   never a false "unchanged".
/// - **record on first send** — `record = true` (as [`query_daemon_leg`]'s
///   daemon call passes), so the NEXT repeat read is gated; best-effort (a
///   record error still allows this read).
///
/// Fail-open, always: an unreadable config, an unopenable engine (e.g. the
/// index is locked by another process → prompt `EngineBusy`, never a hang) or
/// token cache, no resolvable session token, or a ledger error all resolve to a
/// tagged `FailOpen`. A gate must never block a read on infrastructure trouble.
fn check_direct(resolved: &ResolvedVault, path: &str) -> CheckOutcome {
    // Normalize the incoming path to the index's vault-relative key form BEFORE
    // any engine/ledger lookup. The read-hook is invoked with an ABSOLUTE path
    // (Claude Code's Read tool always passes absolute), but the index and the
    // ledger key both use the vault-RELATIVE path `search get` records. Without
    // this, `engine.doc_hash`/`engine.get` miss the relative key (→ Allow: the
    // gate silently never fires in production) AND the ledger key never collides
    // across surfaces (#255). Idempotent on an already-relative path.
    let normalized = normalize_doc_path(path, resolved.root.as_path());
    let path = normalized.as_str();

    // M3 gate: read the vault's configured optimization level. An unreadable
    // config defaults to `off` (< balanced → ledger inactive → safe), matching
    // the daemon route's own conservative-on-error fallback.
    let level = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.token_optimization.level)
        .unwrap_or_default();
    if level < OptLevel::Balanced {
        return CheckOutcome::Allow; // ledger inactive at off/conservative
    }

    // No resolvable session token → ledger inactive for this call (never guess
    // a token — design §3b). Fails open like the daemon's `no_session` verdict.
    let Some(session) = resolve_session_token_opt() else {
        return CheckOutcome::FailOpen("no_session_token");
    };

    // Open the engine to derive the doc's current content hash (the read-hook
    // supplies none). redb's exclusive flock is non-blocking, so a lock held by
    // another process errors promptly (never hangs) → fail open.
    //
    // #264 — classify the open failure instead of collapsing every cause into
    // one opaque `engine_open_error` (v3.4.12's 25× majority). A redb lock held
    // elsewhere (`open_engine` maps it to `CoreError::EngineBusy`) is recorded
    // as the distinct `engine_busy` reason; a genuine cold/slow open keeps
    // `engine_open_error`. With the warm-daemon route now adopting a same-vault
    // daemon regardless of version, a held lock should almost never reach the
    // Direct leg anymore — so a future `engine_busy` spike self-diagnoses as a
    // discovery-race rather than hiding as generic open failure.
    let engine = match open_engine(Some(resolved.root.as_path().to_path_buf())) {
        Ok((engine, _)) => engine,
        Err(e) => {
            return CheckOutcome::FailOpen(if is_engine_busy_error(&e) {
                "engine_busy"
            } else {
                "engine_open_error"
            });
        }
    };
    let Some(current_hash) = engine.doc_hash(path) else {
        // Unindexed / unknown doc — can't compare, so never claim "unchanged".
        return CheckOutcome::Allow;
    };

    // Open the Direct-mode token ledger (`token.redb`) and classify the send.
    let Some(db_path) = token_db_path(resolved) else {
        return CheckOutcome::FailOpen("no_collection");
    };
    let Ok(cache) = onebrain_token::TokenCache::open(&db_path) else {
        return CheckOutcome::FailOpen("token_cache_open_error");
    };
    let ledger = cache.ledger();
    match ledger.check(&session, path, &current_hash) {
        Ok(onebrain_token::LedgerVerdict::Unchanged { sent_hash }) => {
            // Credit the inline body size we avoided — a best-effort engine read
            // (0 on error, never fail the decision on a size estimate), exactly
            // as the daemon route does when the caller supplies no `bytes`.
            let bytes_saved = engine.get(path).map(|b| b.len() as u64).unwrap_or(0);
            CheckOutcome::Unchanged(ReferenceEnvelope::new(path, sent_hash, bytes_saved))
        }
        Ok(onebrain_token::LedgerVerdict::FirstSend | onebrain_token::LedgerVerdict::Changed) => {
            // Mark the send (record = true, as the daemon call does) so the NEXT
            // repeat read is gated. Best-effort: a record failure still allows.
            let _ = ledger.record(&session, path, &current_hash);
            CheckOutcome::Allow
        }
        Err(_) => CheckOutcome::FailOpen("ledger_check_error"),
    }
}

/// Resolve `path`'s ledger verdict: route to a warm, vault-bound daemon when one
/// exists (under the 200ms network budget), else check the ledger directly
/// in-process ([`check_direct`]). Before #248 there was no Direct leg — no
/// daemon meant an unconditional `no_daemon` fail-open, so the read-hook never
/// gated without a warm daemon. `ONEBRAIN_NO_DAEMON` forces the Direct leg (the
/// operator kill-switch + deterministic tests), matching every other search
/// surface.
fn resolve_outcome(resolved: &ResolvedVault, path: &str) -> CheckOutcome {
    if daemon_routing_disabled() {
        return check_direct(resolved, path);
    }
    let budget = check_timeout(resolved);
    match query_daemon_leg_with_deadline(
        resolved.root.as_path().to_path_buf(),
        path.to_string(),
        budget,
    ) {
        DaemonLeg::Verdict(outcome) => outcome,
        DaemonLeg::FallThrough => check_direct(resolved, path),
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serialize the reference envelope and write it (newline-terminated) to `w`,
/// returning `false` if either step fails — a serialize error or a closed /
/// broken sink. Extracted from [`run`] so the deny path's write-failure
/// branch, which must fail OPEN rather than panic, is unit-testable without
/// touching the process-global stdout. `writeln!` (unlike `println!`) surfaces
/// the `io::Error` instead of panicking on a broken pipe.
fn emit_reference(w: &mut impl std::io::Write, reference: &ReferenceEnvelope) -> bool {
    serde_json::to_string(reference)
        .ok()
        .and_then(|json| writeln!(w, "{json}").ok())
        .is_some()
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

/// Record the read-hook DENY [`GainEvent`] (#264 sub-fix B). This is the fix
/// for the field finding that a real deny never appeared in telemetry: the deny
/// arm returned `exit 2` with no `record_gain`, so the very savings the ledger
/// produced were invisible in `token gain`.
///
/// Metered HERE — at the read-hook client — for BOTH the Direct and the
/// warm-daemon leg, because `run` is the single place the ReadHook surface
/// finalizes a deny. Deliberately NOT metered inside the daemon's
/// `post_ledger_check`: that route is shared with `search get` / MCP `get`,
/// which already meter their own `LedgerRef` deliveries, so a daemon-side meter
/// would double-count those surfaces (and mislabel them ReadHook). Metering at
/// the client keeps the design §5d invariant "metering never depends on the
/// daemon" and records each read-hook deny exactly once.
///
/// Parallels `mcp.rs`'s `LedgerRef` recording: `bytes_before` = the original
/// doc body avoided (the envelope's `bytes_saved`), `bytes_after` = the
/// reference envelope actually delivered, `cache = LedgerDeny`, `surface =
/// ReadHook`. Best-effort — no collection → no sink → dropped, like every other
/// surface. `level` reflects the vault's configured rung (the ledger only gates
/// at `balanced`+, so an unreadable config defaults to `balanced`).
fn record_ledger_deny(
    resolved: Option<&ResolvedVault>,
    reference: &ReferenceEnvelope,
    session_token: Option<String>,
) {
    let Some(resolved) = resolved else {
        return;
    };
    let ref_bytes = serde_json::to_string(reference)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let level = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.token_optimization.level)
        .unwrap_or(OptLevel::Balanced);
    record_gain(
        resolved,
        GainEvent {
            ts: now_ts(),
            surface: Surface::ReadHook,
            transform: "ledger_deny".to_string(),
            level,
            bytes_before: reference.bytes_saved,
            bytes_after: ref_bytes,
            cache: CacheKind::LedgerDeny,
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
        Some(r) => resolve_outcome(r, path),
        None => CheckOutcome::FailOpen("no_vault"),
    };

    match decide(outcome) {
        Decision::Allow => Ok(0),
        Decision::Deny(reference) => {
            // Deliver the reference envelope on stdout, then exit 2 (deny). A
            // raw `println!` here would PANIC if stdout is already closed — the
            // hook harness hung up or its own (5s) timeout fired while we were
            // still resolving the vault — and under the release build's
            // `panic = "abort"` that panic is a SIGABRT: an exit code outside
            // the documented {0,2} contract, with no `hook_failopen` recorded.
            // A gating hook must NEVER block a read on infrastructure trouble,
            // so a stdout write failure fails OPEN, mirroring the codebase's
            // own BrokenPipe-as-exit-0 convention (`exit.rs`).
            if emit_reference(&mut std::io::stdout().lock(), &reference) {
                // #264 sub-fix B: the deny actually saved the doc body — record
                // it so `token gain` surfaces the read-hook's savings (before,
                // a deny recorded nothing and the ledger's whole point was
                // invisible). Only after a SUCCESSFUL emit — a write failure
                // fails open below and is metered as such, never as a deny.
                record_ledger_deny(resolved.as_ref(), &reference, resolve_session_token_opt());
                Ok(2)
            } else {
                record_failopen(
                    resolved.as_ref(),
                    "reference_emit_error",
                    resolve_session_token_opt(),
                );
                Ok(0)
            }
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
    // Daemon-backed test infra is Unix-only: these tests isolate daemon
    // discovery via $HOME, which `dirs::home_dir()` honours only on Unix
    // (Windows reads %USERPROFILE%). Matches the repo-wide `#[cfg(unix)]`
    // convention for HOME-based daemon tests (see daemon_client.rs). The
    // production code is cross-platform; only the home-isolation is Unix-only.
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::net::TcpListener;

    // ── Pure verdict→outcome mapping (plan 5.1's primary TDD target) ───────

    fn envelope() -> ReferenceEnvelope {
        ReferenceEnvelope::new("a.md", "deadbeef", 4096)
    }

    /// A sink that fails every write — stands in for a stdout whose reader has
    /// already hung up (closed pipe).
    struct BrokenSink;
    impl std::io::Write for BrokenSink {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }
    }

    #[test]
    fn emit_reference_writes_newline_terminated_json_to_a_live_sink() {
        let mut buf = Vec::new();
        assert!(emit_reference(&mut buf, &envelope()));
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("\"doc_path\":\"a.md\""),
            "envelope JSON: {out}"
        );
        assert!(out.ends_with('\n'), "must be newline-terminated: {out:?}");
    }

    #[test]
    fn emit_reference_on_broken_stdout_reports_failure_never_panics() {
        // The deny path relies on this: a `false` return makes `run` fail OPEN
        // (exit 0 + `hook_failopen`) instead of the old `println!` panicking —
        // which under release `panic = "abort"` was a SIGABRT breaking the
        // {0,2} contract. A broken sink must NOT panic here.
        assert!(!emit_reference(&mut BrokenSink, &envelope()));
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
            decide(CheckOutcome::FailOpen("engine_open_error")),
            Decision::FailOpen("engine_open_error")
        );
    }

    // ── #264 A#4/A#5: configurable budget + open-failure classification ────

    #[test]
    fn default_check_timeout_pins_the_config_default() {
        // The read-hook's fallback `Duration` must never drift from the config's
        // documented default — both derive from `default_check_timeout_ms`.
        assert_eq!(
            DEFAULT_CHECK_TIMEOUT,
            Duration::from_millis(onebrain_core::config::default_check_timeout_ms() as u64)
        );
        assert_eq!(DEFAULT_CHECK_TIMEOUT, Duration::from_millis(200));
    }

    fn vault_with_timeout(yaml: &str) -> (tempfile::TempDir, ResolvedVault) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("onebrain.yml"), yaml).unwrap();
        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        (vault, resolved)
    }

    #[test]
    fn check_timeout_reads_configured_budget() {
        // #264: iCloud/network vaults raise the budget past 200ms.
        let (_v, resolved) = vault_with_timeout(
            "search:\n  collection: t\ntoken_optimization:\n  check_timeout_ms: 750\n",
        );
        assert_eq!(check_timeout(&resolved), Duration::from_millis(750));
    }

    #[test]
    fn check_timeout_defaults_when_absent() {
        let (_v, resolved) = vault_with_timeout("search:\n  collection: t\n");
        assert_eq!(check_timeout(&resolved), DEFAULT_CHECK_TIMEOUT);
    }

    #[test]
    fn check_timeout_zero_falls_back_to_default_never_instant() {
        // A degenerate `0` would recv_timeout(0) → instant fail-open every call,
        // silently disabling the gate. Fall back to the default instead.
        let (_v, resolved) = vault_with_timeout(
            "search:\n  collection: t\ntoken_optimization:\n  check_timeout_ms: 0\n",
        );
        assert_eq!(check_timeout(&resolved), DEFAULT_CHECK_TIMEOUT);
    }

    #[test]
    fn is_engine_busy_error_distinguishes_lock_from_cold_open() {
        let busy: anyhow::Error =
            anyhow::Error::new(onebrain_core::CoreError::EngineBusy("locked".to_string()));
        assert!(is_engine_busy_error(&busy));
        let cold = anyhow::anyhow!("opening search engine at /x: no such file");
        assert!(!is_engine_busy_error(&cold));
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
    fn no_daemon_at_conservative_goes_direct_and_allows_without_failopen() {
        // #248: no daemon.json under this HOME → `discover_matching` sees
        // nothing → the check now falls THROUGH to the in-process Direct leg (it
        // used to fail open with `no_daemon`). At the default `conservative`
        // level the ledger is inactive, so the Direct leg simply ALLOWS — exit 0
        // with NO fail-open event (a legitimate business allow, not
        // degradation). The pre-#248 code recorded a `no_daemon` hook_failopen
        // here; that event must now be gone.
        let (vault, cache) = test_vault(); // no token_optimization → conservative
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(code, 0);
        assert!(
            gain_events(cache.path()).is_empty(),
            "an inactive-level Direct allow records no event — and never a `no_daemon` fail-open"
        );
    }

    /// Writes a `daemon.json` under `home/.onebrain/run/` pointing at
    /// `port`/`token`, bound to `vault` — the exact record format
    /// `discover_matching` reads (mirrors the daemon's own `DaemonInfo::write`
    /// but skipped here since the test doesn't need the private perms path).
    #[cfg(unix)]
    fn write_daemon_json(home: &Path, vault: &Path, port: u16, token: &str) {
        write_daemon_json_versioned(home, vault, port, token, env!("CARGO_PKG_VERSION"));
    }

    /// Like [`write_daemon_json`] but pins an arbitrary `version` — lets a test
    /// stand up a VERSION-SKEWED same-vault daemon record (the post-`brew
    /// upgrade` field condition #264 fixes).
    #[cfg(unix)]
    fn write_daemon_json_versioned(
        home: &Path,
        vault: &Path,
        port: u16,
        token: &str,
        version: &str,
    ) {
        let run_dir = home.join(".onebrain").join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let info = daemon_client::DaemonInfo {
            port,
            token: token.to_string(),
            pid: std::process::id(),
            version: version.to_string(),
            vault: daemon_client::canonical_vault_id(vault),
        };
        std::fs::write(
            run_dir.join("daemon.json"),
            serde_json::to_vec_pretty(&info).unwrap(),
        )
        .unwrap();
    }

    /// A minimal hand-rolled HTTP/1.1 responder — good enough for the two
    /// GET/POST routes `query_daemon_leg` hits. Avoids pulling a mock-HTTP dev-dep
    /// just to control response TIMING (mockito doesn't expose per-response
    /// delay), which the timeout test below needs. `ledger_body` is served
    /// (as 200 JSON) for `/api/token/ledger/check`; `ledger_status` overrides
    /// the status line when non-200 (e.g. 404 for version-skew); `ledger_delay`
    /// sleeps before responding, to simulate a wedged daemon.
    #[cfg(unix)]
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
                    // Accumulate until the end of the HTTP headers (`\r\n\r\n`) so
                    // a request split across TCP segments isn't misread from a
                    // single partial `read`.
                    let mut req_bytes = Vec::with_capacity(8192);
                    let mut chunk = [0u8; 1024];
                    while req_bytes.len() < 8192 {
                        let n = stream.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req_bytes.extend_from_slice(&chunk[..n]);
                        if req_bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&req_bytes);
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    /// **#264 sub-fix A — the warm-daemon route adopts a same-vault daemon
    /// REGARDLESS of version.** A version-SKEWED (post-`brew upgrade`) same-vault
    /// daemon record answers `unchanged`; the read-hook must ROUTE to it and
    /// GATE (exit 2), NOT version-reject it and cold-open the engine. Under the
    /// prior `discover_matching` (version-exact) this skewed daemon was rejected
    /// → FallThrough → `check_direct` at conservative → Allow (exit 0): the RED
    /// state, and precisely why v3.4.12 collided on the held lock into
    /// `engine_open_error`. GREEN after switching to
    /// `discover_same_vault_any_version`.
    #[cfg(unix)]
    #[test]
    fn warm_same_vault_daemon_gates_even_across_version_skew() {
        let (vault, cache) = test_vault(); // conservative client config
        let body = r#"{"verdict":"unchanged","reference":{"doc_path":"notes/a.md","hash":"deadbeef","sent_earlier":true,"bytes_saved":6352,"rematerialize":"onebrain search get notes/a.md --force"}}"#;
        let port = start_fake_daemon(200, body, Duration::ZERO);
        let _env = crate::test_env::set_vars(&[
            ("HOME", vault.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
        ]);
        // A version DIFFERENT from ours — the post-upgrade skew condition.
        write_daemon_json_versioned(
            vault.path(),
            vault.path(),
            port,
            "daemon-check-token-123",
            "0.0.1-old",
        );

        let code =
            run(Some(vault.path().to_path_buf()), "notes/a.md").expect("run should not error");
        assert_eq!(
            code, 2,
            "a warm same-vault daemon must be adopted across version skew and GATE the read \
             (exit 2) via its held engine — never version-rejected into a colliding cold open"
        );
    }

    #[cfg(unix)]
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
        assert_eq!(events[0].transform, "daemon_timeout");
    }

    #[cfg(unix)]
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
        // #264 sub-fix B: a deny NOW records a `LedgerDeny` gain row (before, it
        // recorded nothing and the ledger's savings were invisible in
        // `token gain`). `bytes_before` credits the avoided body (the envelope's
        // `bytes_saved`); `bytes_after` is the reference envelope actually sent.
        let events = gain_events(cache.path());
        assert_eq!(events.len(), 1, "a deny must record exactly one gain row");
        assert_eq!(events[0].cache, CacheKind::LedgerDeny);
        assert_eq!(events[0].surface, Surface::ReadHook);
        assert_eq!(events[0].transform, "ledger_deny");
        assert_eq!(
            events[0].bytes_before, 123,
            "bytes_before credits the avoided doc body (envelope bytes_saved)"
        );
        assert!(
            events[0].bytes_after > 0,
            "bytes_after is the reference envelope size"
        );
    }

    #[cfg(unix)]
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

    // ── #248 Direct-mode (no-daemon) end-to-end across the seam ────────────

    /// Like [`test_vault`] but at `balanced` — the level that activates the
    /// already-sent ledger (design §3b M3 gate). The #248 Direct-mode e2e needs
    /// the ledger live; a fresh collection name keeps its cache isolated.
    #[cfg(unix)]
    fn balanced_vault() -> (tempfile::TempDir, tempfile::TempDir) {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: token-check-e2e\ntoken_optimization:\n  level: balanced\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        (vault, cache)
    }

    /// Index `rel` (lex-only, so no embedding model loads) into `vault`'s
    /// collection so [`onebrain_search::engine::Engine::doc_hash`] resolves,
    /// then DROP the engine so its redb lock is released before `run` /
    /// [`check_direct`] open their own. The Direct-mode setup for the seam e2e.
    #[cfg(unix)]
    fn index_doc_lex_only(vault: &Path, rel: &str, body: &str) {
        std::fs::write(vault.join(rel), body).unwrap();
        let (mut engine, resolved) =
            crate::commands::search_common::open_engine(Some(vault.to_path_buf())).unwrap();
        engine
            .reindex_paths_lex_only_with_progress(
                resolved.root.as_path(),
                &[rel.to_string()],
                &mut |_| {},
            )
            .unwrap();
        // engine dropped here → redb lock released for the Direct check below.
    }

    /// **The #248 e2e — read-hook gating across the daemon→Direct seam.**
    /// Scratch vault, NO daemon anywhere, `balanced` level, an indexed doc. The
    /// FIRST check first-sends → allow + record (exit 0); the SECOND check of
    /// the UNCHANGED doc must be GATED — exit 2 with a reference envelope — the
    /// read-hook behavior v3.4.10 silently never delivered without a warm
    /// daemon. This is precisely the path every v3.4.10 gate missed: no prior
    /// test drove the no-daemon route end to end. RED before the Direct fallback
    /// (both checks fail-open → exit 0); GREEN after.
    #[cfg(unix)]
    #[test]
    fn direct_read_hook_gates_repeat_unchanged_read_with_no_daemon() {
        let (vault, cache) = balanced_vault();
        let home = tempfile::tempdir().unwrap(); // no daemon.json under here
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            // Force the Direct leg deterministically (belt-and-suspenders with
            // the empty HOME) and pin a resolvable, stable session token so the
            // ledger key is identical across both checks.
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
            ("CLAUDE_CODE_SESSION_ID", std::ffi::OsStr::new("e2eSess1")),
        ]);
        index_doc_lex_only(vault.path(), "a.md", "# A\nbody text for the #248 seam e2e");

        // 1) First read of an indexed doc → first_send → allow + record.
        assert_eq!(
            run(Some(vault.path().to_path_buf()), "a.md").expect("run should not error"),
            0,
            "first read is a first_send → allow"
        );

        // 2) Repeat read of the UNCHANGED doc → the gate fires with NO daemon.
        assert_eq!(
            run(Some(vault.path().to_path_buf()), "a.md").expect("run should not error"),
            2,
            "a no-daemon repeat read of an unchanged doc must be GATED (exit 2) — the #248 fix"
        );

        // #264 sub-fix B — the DIRECT-leg deny also records a `LedgerDeny` row
        // (a real indexed body → non-zero bytes_before). The first_send allow
        // recorded nothing, so exactly one row exists after the deny. This vault
        // uses the `token-check-e2e` collection (not `test_vault`'s
        // `token-check-test`), so read that collection's gain dir directly.
        let e2e_gain = crate::commands::search_common::collection_cache_dir("token-check-e2e")
            .join("token")
            .join("gain");
        let events = JsonlGainWriter::new(&e2e_gain)
            .read_all()
            .unwrap_or_default();
        assert_eq!(events.len(), 1, "only the deny records; the allow does not");
        assert_eq!(events[0].cache, CacheKind::LedgerDeny);
        assert_eq!(events[0].surface, Surface::ReadHook);
        assert_eq!(events[0].level, OptLevel::Balanced);
        assert!(
            events[0].bytes_before > 0,
            "a real indexed body credits non-zero bytes_before"
        );

        // The deny carries the frozen reference envelope. `run` writes it to
        // stdout (proven byte-for-byte by the `emit_reference` tests); assert
        // its CONTENTS here via the same Direct verdict the deny path produced,
        // now that a.md is recorded in the ledger.
        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        match check_direct(&resolved, "a.md") {
            CheckOutcome::Unchanged(env) => {
                assert_eq!(env.doc_path, "a.md");
                assert!(
                    !env.hash.is_empty(),
                    "a real indexed doc has a content hash"
                );
                assert!(env.sent_earlier);
                assert!(
                    env.bytes_saved > 0,
                    "a real indexed body credits non-zero bytes_saved"
                );
                assert_eq!(env.rematerialize, "onebrain search get a.md --force");
            }
            other => panic!("expected an Unchanged reference envelope, got {other:?}"),
        }
    }

    // ── #255 cross-surface unification (get side ↔ read-hook) ──────────────

    /// **The #255 cross-surface e2e — realistic path skew.** In ONE session: the
    /// CLI `search get` records the send under the vault-RELATIVE key it
    /// normalizes to, then the read-hook `token check` is invoked with an
    /// ABSOLUTE path (exactly as Claude Code's Read tool passes it) and must
    /// STILL gate (exit 2). This needs BOTH halves of the key `(session,
    /// doc_path, hash)` to agree: the read-hook normalizes the absolute path to
    /// the same relative key AND both surfaces key the hash on `Engine::doc_hash`.
    ///
    /// Two pre-#255 gaps this pins, both → a first-send Allow (exit 0):
    /// 1. HASH skew — the get side recorded `content_hash(engine.get(path))`
    ///    (reconstructed body, headings stripped) while `token check` keyed on
    ///    `doc_hash` (raw file bytes) — different hashes.
    /// 2. PATH skew — the read-hook keyed on the raw ABSOLUTE path, which misses
    ///    the relative index key entirely (`doc_hash` → `None` → Allow) and never
    ///    matches `search get`'s relative ledger path.
    ///
    /// RED before the fix (exit 0); GREEN after (shared relative key + shared
    /// `doc_hash` → exit 2).
    #[cfg(unix)]
    #[test]
    fn search_get_then_read_hook_share_one_ledger_entry() {
        let (vault, cache) = balanced_vault();
        let home = tempfile::tempdir().unwrap(); // no daemon.json under here
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
            ("CLAUDE_CODE_SESSION_ID", std::ffi::OsStr::new("xSurf255")),
        ]);
        // A heading + body: the chunker drops the heading line from the chunk
        // text, so the reconstructed body (`engine.get`) is provably ≠ the raw
        // file bytes — the exact pre-#255 hash divergence this test pins.
        index_doc_lex_only(
            vault.path(),
            "a.md",
            "# A\nshared-ledger cross-surface body text",
        );

        // Build the read-hook's absolute path from the RESOLVED root (what the
        // CLI resolves the vault to) so it is a genuine child of the same root
        // `normalize_doc_path` strips against — mirroring the real Read tool,
        // which passes an absolute path under the actual vault root.
        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();
        let abs = resolved.root.as_path().join("a.md");
        let abs = abs.to_str().unwrap();

        // Surface A — CLI `search get --output json` records the send under the
        // relative key `a.md`, keyed on the doc's `doc_hash`.
        crate::commands::search_get::run(
            Some(vault.path().to_path_buf()),
            &crate::output::OutputMode::Json { pretty: false },
            &crate::cli::SearchGetArgs {
                doc_path: "a.md".to_string(),
                force: false,
                opt_level: None,
            },
        )
        .expect("search get should succeed");

        // Surface B — the read-hook, invoked with the ABSOLUTE path, must
        // normalize to the same relative key, find A's entry, and GATE the read.
        assert_eq!(
            run(Some(vault.path().to_path_buf()), abs).expect("run should not error"),
            2,
            "search get (relative) + read-hook (ABSOLUTE) of the same unchanged doc \
             must share one ledger entry and GATE the read (exit 2) — the #255 fix"
        );

        // The deny carries the frozen reference envelope (design contract: a deny
        // ALWAYS offers a `--force` re-materialize path). Re-derive the same
        // Direct verdict the deny produced — passing the ABSOLUTE path again — and
        // assert the envelope reports the NORMALIZED relative key.
        match check_direct(&resolved, abs) {
            CheckOutcome::Unchanged(env) => {
                assert_eq!(
                    env.doc_path, "a.md",
                    "envelope must report the relative key"
                );
                assert!(
                    !env.hash.is_empty(),
                    "a real indexed doc has a content hash"
                );
                assert!(env.sent_earlier);
                assert_eq!(env.rematerialize, "onebrain search get a.md --force");
            }
            other => panic!("expected an Unchanged reference envelope, got {other:?}"),
        }
    }

    /// A read-hook `token check` invoked with an ABSOLUTE path must resolve the
    /// SAME index identity as the relative key — normalization maps the absolute
    /// Read-tool path onto the vault-relative index key. Proves the path half of
    /// the #255 unification directly at the engine level (and that the raw
    /// absolute path misses — the reason normalization is required).
    #[cfg(unix)]
    #[test]
    fn absolute_path_resolves_same_doc_hash_as_relative_key() {
        let (vault, cache) = balanced_vault();
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
            ("CLAUDE_CODE_SESSION_ID", std::ffi::OsStr::new("xAbs255")),
        ]);
        index_doc_lex_only(vault.path(), "a.md", "# A\nbody for the abs-path test");

        let (engine, resolved) =
            crate::commands::search_common::open_engine(Some(vault.path().to_path_buf())).unwrap();
        let root = resolved.root.as_path();
        let abs = root.join("a.md");
        let abs = abs.to_str().unwrap();

        let rel_hash = engine.doc_hash("a.md");
        assert!(rel_hash.is_some(), "the relative key resolves a doc_hash");
        // Normalizing the absolute path resolves the SAME identity.
        let normalized = normalize_doc_path(abs, root);
        assert_eq!(
            engine.doc_hash(&normalized),
            rel_hash,
            "a normalized absolute path must resolve the SAME index identity as the relative key"
        );
        // The RAW absolute path misses — index keys are vault-relative. This is
        // precisely why the read-hook must normalize (#255).
        assert!(
            engine.doc_hash(abs).is_none(),
            "the raw absolute path must NOT resolve — this is the miss #255 fixes"
        );
    }

    /// `doc_hash == None` (unindexed doc) must make `direct_ledger_reference`
    /// SKIP the ledger — inline the full body, record NOTHING, never unwrap. Proof
    /// it recorded nothing: a subsequent read-hook check of the same doc is still
    /// a first-send Allow (exit 0), not a gate.
    #[cfg(unix)]
    #[test]
    fn direct_ledger_reference_skips_when_doc_hash_none() {
        let (vault, cache) = balanced_vault();
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
            ("CLAUDE_CODE_SESSION_ID", std::ffi::OsStr::new("xNone255")),
        ]);
        index_doc_lex_only(vault.path(), "a.md", "# A\nbody for the none-fallback test");
        let resolved = crate::vault_ctx::require(Some(vault.path().to_path_buf())).unwrap();

        // None doc_hash → skip: returns None (inline the body) without touching
        // the ledger.
        assert!(
            crate::commands::token_runner::direct_ledger_reference(
                &resolved,
                "a.md",
                None,
                "body for the none-fallback test",
            )
            .is_none(),
            "doc_hash=None must skip the ledger and inline, never unwrap"
        );

        // It must not have RECORDED anything: the very next read-hook check of the
        // real, indexed doc is a fresh first-send Allow, not a gate.
        assert_eq!(
            run(Some(vault.path().to_path_buf()), "a.md").expect("run should not error"),
            0,
            "a skipped (doc_hash=None) call must record no ledger entry"
        );
    }

    /// Safe-failure invariant (#255): when the doc's indexed content CHANGES
    /// after a send was recorded, the read-hook must re-deliver (verdict
    /// `Changed` → Allow, exit 0) — the ledger NEVER hard-denies content the
    /// agent hasn't seen. Record H0, reindex the doc to a new `doc_hash` H1, then
    /// check: H1 ≠ recorded H0 → Changed → Allow, not a stale deny.
    #[cfg(unix)]
    #[test]
    fn changed_indexed_content_re_delivers_never_denies_unseen() {
        let (vault, cache) = balanced_vault();
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("ONEBRAIN_CACHE_DIR", cache.path().as_os_str()),
            ("ONEBRAIN_NO_DAEMON", std::ffi::OsStr::new("1")),
            ("CLAUDE_CODE_SESSION_ID", std::ffi::OsStr::new("xChg255")),
        ]);
        index_doc_lex_only(vault.path(), "a.md", "# A\noriginal body content");

        // First read-hook check records the ORIGINAL doc_hash (H0) → Allow.
        assert_eq!(
            run(Some(vault.path().to_path_buf()), "a.md").expect("run should not error"),
            0,
            "first check is a first-send Allow (records H0)"
        );

        // Reindex the doc with entirely new bytes → its doc_hash flips to H1.
        index_doc_lex_only(
            vault.path(),
            "a.md",
            "# A\nCOMPLETELY DIFFERENT body — reindexed",
        );

        // The read-hook now sees H1 ≠ recorded H0 → Changed → ALLOW (re-deliver),
        // never a stale deny of content the agent has not been shown.
        assert_eq!(
            run(Some(vault.path().to_path_buf()), "a.md").expect("run should not error"),
            0,
            "changed indexed content must re-deliver (Changed→Allow), never deny unseen content"
        );
    }
}
