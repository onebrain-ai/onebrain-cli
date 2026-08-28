//! In-memory human-approval registry for gateway tool calls gated by
//! [`super::policy::PolicyOutcome::NeedApproval`] (Gateway PR 4, Task 3) —
//! the register/wait/resolve machinery `server.rs`'s `policy_gate` doc
//! comment names as "Task 3+'s interactive approval flow", plus the
//! operator-facing HTTP surface in [`super::approval_routes`] a human uses
//! to actually decide. `server.rs`'s `policy_gate` itself is NOT rewired to
//! call `register`/`wait` by this task — that remains the "later task"
//! its own doc comment already names; this task ships the registry and its
//! HTTP surface on their own, fully tested in isolation.
//!
//! ## Two resolution channels, one registry (Gateway PR 4, Task 4)
//!
//! [`super::approval_routes`]'s `/approvals` HTTP surface is not the only
//! way a pending approval gets resolved: [`super::approval_native`] adds a
//! SECOND channel — a native macOS `display dialog` — that calls the exact
//! same [`Approvals::resolve`] below. Both channels race harmlessly by
//! construction, purely because `resolve` is first-response-wins (see its
//! own doc comment) — no coordination between the two channels exists or is
//! needed anywhere in this module.
//!
//! ## Privilege separation — why `/approvals` is a SEPARATE, differently
//! gated surface (see [`super::approval_routes`] for the actual mounting)
//!
//! A connector reaches the gateway over `/mcp`, authenticated by an OAuth
//! bearer token it obtained for ITSELF
//! (`auth::middleware::require_bearer`). If that SAME bearer token could
//! also approve a pending request, a connector could silently self-approve
//! anything policy would otherwise ask a human about — defeating the entire
//! point of `ask_once`/`ask_always`, the human checkpoint they exist to
//! enforce. So `/approvals` is mounted WITHOUT the connector Bearer layer at
//! all, gated instead by the gateway's own pairing code — a secret only a
//! human operator at the machine possesses (it's the SAME code that pairs a
//! brand-new connector in the first place, `AuthStore::verify_pairing`),
//! never a client-issued bearer token. `approval_routes.rs`'s own tests
//! prove a connector's live bearer token is explicitly REJECTED on
//! `/approvals` — the privilege-separation property this whole gate rests
//! on.
//!
//! ## In-memory, per-process — same lifetime discipline as [`super::policy::Grants`]
//!
//! `Approvals` holds no persisted state: a gateway restart drops every
//! pending approval (the connector that was waiting simply times out and
//! must retry). This mirrors `Grants`'s own "session-scoped, not persisted"
//! design (see that module's doc comment) — an approval, like a grant, is
//! consent for THIS running gateway, not a standing credential written to
//! disk.
//!
//! ## The no-mutex-across-await discipline
//!
//! [`Approvals::register`]/[`Approvals::list`]/[`Approvals::resolve`] are
//! all plain, synchronous, lock-do-one-thing-drop-the-guard operations —
//! same shape as [`super::policy::Grants::has`]/[`super::policy::Grants::record`].
//! [`Approvals::wait`] is the one `async fn` here, and it is written so the
//! `std::sync::Mutex` guard is NEVER held across the `.await`: the lock is
//! only ever taken (a) inside `register`, before returning the
//! `oneshot::Receiver` to the caller — no `.await` in that function's body
//! at all — and (b) inside `wait`, strictly AFTER
//! `tokio::time::timeout(..).await` has already resolved, purely to clean
//! up. Holding a `std::sync::Mutex` guard across an `.await` would risk
//! deadlocking the executor (a task blocked trying to acquire the lock
//! can't yield back to let the lock holder's own await resolve) — see
//! `Grants`'s own doc comment for the same rule applied elsewhere in this
//! crate, and `tests::wait_does_not_hold_the_lock_across_the_await` below
//! for the end-to-end proof (not just a read-the-code assertion).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::policy::RiskClass;

/// One tool call awaiting a human operator's decision — the record surfaced
/// by `GET /approvals` and released by `POST /approvals/{id}`.
///
/// Every field here is safe to hand back to the OPERATOR verbatim over HTTP
/// (see [`Approvals::list`]'s doc comment): nothing here is a token, a full
/// note body, or a host filesystem path. `summary` is the SAME kind of
/// redacted, human-readable one-liner `audit::AuditEntry::args_summary`
/// already is — built by the CALLER before constructing this struct, never
/// raw input, and never further scrubbed here.
#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
    pub id: String,
    pub client_id: String,
    pub tool: String,
    pub summary: String,
    pub created: u64,
    pub expires: u64,
    /// The [`RiskClass`] this call was gated at — recorded so a
    /// `Decision::Approve` resolution knows which `(client, class)` grant to
    /// record via [`super::policy::Grants::record`] (Task 2 review's binding
    /// requirement A — see `approval_routes::resolve_approval`'s doc
    /// comment for the actual wiring). Not a secret: an operator reviewing
    /// the pending list benefits from seeing exactly what class of access is
    /// being asked for, same as `tool`/`summary`.
    pub class: RiskClass,
}

/// A human operator's response to one [`PendingApproval`] — deliberately a
/// DIFFERENT type from `audit::Decision` (`Auto`/`Approved`/`Denied`/
/// `TimedOut`/`Blocked`, an OUTCOME record) even though the names echo each
/// other: this is the human's raw INPUT (`Approve`/`Deny`), which
/// `approval_routes::resolve_approval` translates into effects (waking the
/// waiter, and on `Approve`, recording a [`super::policy::Grants`] entry).
/// Always qualify as `approval::Decision` at any use site that also sees
/// `audit::Decision`, so the two can't be confused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Deny,
}

/// Outcome of [`Approvals::wait`]: either a human [`Decision`], or the TTL
/// elapsed with no response.
///
/// No production caller yet (see [`Approvals::wait`]'s own doc comment for
/// why — same "Dead-code allow" situation as `audit::Decision::Approved`/
/// `TimedOut`, which this eventually feeds into once a later task wires
/// `server.rs`'s `policy_gate` to actually call [`Approvals::register`]/
/// [`Approvals::wait`]). Exercised directly by this module's own unit
/// tests until then.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    Decided(Decision),
    TimedOut,
}

/// In-memory, per-process registry of pending approvals, keyed by
/// [`PendingApproval::id`]. See the module docs for the full lifecycle and
/// locking discipline.
pub struct Approvals {
    pending: Mutex<HashMap<String, (PendingApproval, oneshot::Sender<Decision>)>>,
}

impl Approvals {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register `p` as pending and return the receiver half a caller
    /// `.await`s (via [`Self::wait`]) for the eventual decision. Locks,
    /// inserts, drops the guard — there is no `.await` anywhere in this
    /// function's body, so the lock is never even in the neighborhood of an
    /// await point.
    ///
    /// First given a production caller by Gateway PR 4, Task 5:
    /// `server::await_approval` calls this from `policy_gate`'s
    /// `NeedApproval` arm, before firing the native dialog channel and
    /// `.await`ing [`Self::wait`] on the returned receiver.
    pub fn register(&self, p: PendingApproval) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.insert(p.id.clone(), (p, tx));
        rx
    }

    /// Every currently-pending approval, in arbitrary (`HashMap` iteration)
    /// order — exactly [`PendingApproval`]'s own fields and nothing else;
    /// the `oneshot::Sender` half never leaves this module. This is the
    /// ENTIRE body of `GET /approvals`'s response.
    pub fn list(&self) -> Vec<PendingApproval> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.values().map(|(p, _)| p.clone()).collect()
    }

    /// Resolve `id` with decision `d`. `true` iff `id` was pending AND the
    /// waiting receiver was still live to accept it.
    ///
    /// First responder wins: resolving removes the entry from `pending`
    /// immediately, so a second `resolve` call for the same `id` — whether
    /// it raced concurrently or arrives after — always finds nothing left
    /// and returns `false`; the decision the first call sent stands,
    /// untouched by the second. An `id` that was never registered, was
    /// already resolved, or already timed out and was cleaned up by
    /// [`Self::wait`] all collapse to the same "nothing to resolve" `false`
    /// — there is no different recovery action a caller could safely take
    /// for any of those, so distinguishing them isn't worth the extra
    /// surface.
    ///
    /// Called from BOTH resolution channels (Gateway PR 4, Task 4) —
    /// `approval_routes::resolve_approval` (the operator HTTP surface) and
    /// [`super::approval_native::prompt`] (the native macOS dialog) — with
    /// no coordination between them beyond this method's own
    /// first-response-wins removal-before-send behavior: whichever channel
    /// answers first wins, and a later answer from the other channel for
    /// the same `id` simply finds nothing left to resolve.
    pub fn resolve(&self, id: &str, d: Decision) -> bool {
        let removed = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(id)
        };
        match removed {
            Some((_, tx)) => tx.send(d).is_ok(),
            None => false,
        }
    }

    /// Wait up to `ttl` for `id`'s registered receiver `rx` to resolve.
    ///
    /// Never holds the `pending` lock across the `.await`:
    /// `tokio::time::timeout` runs first with NO lock held at all, and the
    /// lock is only (re)acquired afterward — strictly to clean up, once the
    /// await has already resolved one way or the other. On a timeout (or
    /// the sender being dropped without ever sending — which, given
    /// [`Self::resolve`] only ever removes-then-sends, means nothing
    /// meaningfully different happened), `id`'s entry is removed from
    /// `pending` so it doesn't linger forever waiting for a decision that
    /// will never come, and [`WaitOutcome::TimedOut`] is returned.
    ///
    /// First given a production caller by Gateway PR 4, Task 5 — see
    /// [`Self::register`]'s doc comment.
    pub async fn wait(
        &self,
        id: &str,
        rx: oneshot::Receiver<Decision>,
        ttl: Duration,
    ) -> WaitOutcome {
        match tokio::time::timeout(ttl, rx).await {
            Ok(Ok(decision)) => WaitOutcome::Decided(decision),
            Ok(Err(_)) | Err(_) => {
                let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.remove(id);
                WaitOutcome::TimedOut
            }
        }
    }
}

impl Default for Approvals {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> PendingApproval {
        PendingApproval {
            id: id.to_string(),
            client_id: "client-1".to_string(),
            tool: "brain_capture".to_string(),
            summary: "note: Quarterly Plan".to_string(),
            created: 1_700_000_000,
            expires: 1_700_000_300,
            class: RiskClass::Mutating,
        }
    }

    // ── Step 1: register -> resolve delivers the decision ──────────────

    #[tokio::test]
    async fn register_then_resolve_delivers_the_decision() {
        let approvals = Approvals::new();
        let rx = approvals.register(sample("a1"));
        assert!(approvals.resolve("a1", Decision::Approve));
        assert_eq!(rx.await.unwrap(), Decision::Approve);
    }

    // ── resolve of an unknown id -> false ───────────────────────────────

    #[test]
    fn resolve_of_an_unknown_id_returns_false() {
        let approvals = Approvals::new();
        assert!(!approvals.resolve("nope", Decision::Approve));
    }

    // ── double resolve: second returns false, first decision stands ────

    #[tokio::test]
    async fn double_resolve_second_call_returns_false_first_decision_stands() {
        let approvals = Approvals::new();
        let rx = approvals.register(sample("a1"));
        assert!(approvals.resolve("a1", Decision::Approve));
        assert!(
            !approvals.resolve("a1", Decision::Deny),
            "a second resolve of an already-resolved id must return false"
        );
        assert_eq!(
            rx.await.unwrap(),
            Decision::Approve,
            "the FIRST decision must stand, unaffected by the second call"
        );
    }

    // ── timeout path: TimedOut + entry dropped from pending ─────────────

    #[tokio::test]
    async fn wait_returns_the_decision_when_resolved_before_the_ttl() {
        let approvals = Approvals::new();
        let rx = approvals.register(sample("a1"));
        assert!(approvals.resolve("a1", Decision::Deny));
        let outcome = approvals.wait("a1", rx, Duration::from_secs(5)).await;
        assert_eq!(outcome, WaitOutcome::Decided(Decision::Deny));
    }

    #[tokio::test]
    async fn wait_times_out_and_drops_the_entry_from_pending() {
        let approvals = Approvals::new();
        let rx = approvals.register(sample("a1"));
        assert_eq!(approvals.list().len(), 1, "must be pending before the wait");

        let outcome = approvals.wait("a1", rx, Duration::from_millis(20)).await;
        assert_eq!(outcome, WaitOutcome::TimedOut);
        assert!(
            approvals.list().is_empty(),
            "a timed-out entry must be dropped from `pending`"
        );

        // Genuinely gone, not just invisible to `list` — a later resolve for
        // the same id also fails.
        assert!(!approvals.resolve("a1", Decision::Approve));
    }

    // ── list never exposes anything beyond PendingApproval's own fields ─

    #[test]
    fn list_is_empty_for_a_fresh_registry() {
        assert!(Approvals::new().list().is_empty());
    }

    #[test]
    fn list_exposes_exactly_pending_approvals_own_fields() {
        let approvals = Approvals::new();
        let _rx = approvals.register(sample("a1"));
        let listed = approvals.list();
        assert_eq!(listed.len(), 1);
        let p = &listed[0];
        assert_eq!(p.id, "a1");
        assert_eq!(p.client_id, "client-1");
        assert_eq!(p.tool, "brain_capture");
        assert_eq!(p.summary, "note: Quarterly Plan");
        assert_eq!(p.created, 1_700_000_000);
        assert_eq!(p.expires, 1_700_000_300);
        assert_eq!(p.class, RiskClass::Mutating);

        // Serializes to EXACTLY these 7 fields — a JSON object with no extra
        // keys, so no token / full note body / host path could sneak in
        // through a field this struct doesn't have.
        let value = serde_json::to_value(p).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "class",
                "client_id",
                "created",
                "expires",
                "id",
                "summary",
                "tool"
            ]
        );
    }

    #[tokio::test]
    async fn multiple_pending_approvals_are_independent() {
        let approvals = Approvals::new();
        let rx1 = approvals.register(sample("a1"));
        let mut second = sample("a2");
        second.client_id = "client-2".to_string();
        let rx2 = approvals.register(second);

        assert_eq!(approvals.list().len(), 2);
        assert!(approvals.resolve("a2", Decision::Deny));
        assert_eq!(approvals.list().len(), 1, "resolving a2 must not touch a1");
        assert_eq!(rx2.await.unwrap(), Decision::Deny);

        assert!(approvals.resolve("a1", Decision::Approve));
        assert_eq!(rx1.await.unwrap(), Decision::Approve);
        assert!(approvals.list().is_empty());
    }

    // ── wire shape ───────────────────────────────────────────────────────

    #[test]
    fn decision_deserializes_lowercase_snake_case() {
        assert_eq!(
            serde_json::from_str::<Decision>("\"approve\"").unwrap(),
            Decision::Approve
        );
        assert_eq!(
            serde_json::from_str::<Decision>("\"deny\"").unwrap(),
            Decision::Deny
        );
    }

    // ── no-mutex-across-await, proven rather than merely asserted ───────

    /// If [`Approvals::wait`] held the `pending` lock across its
    /// `tokio::time::timeout(..).await`, a concurrent [`Approvals::list`]
    /// call made WHILE that wait is still outstanding would block on the
    /// same `std::sync::Mutex` for the full wait duration (a blocking
    /// `Mutex::lock()` doesn't yield to the async runtime the way an
    /// `.await` does, so a real regression here would hang, not just run
    /// slow). Runs `list()` via `spawn_blocking` so a genuine deadlock
    /// surfaces as a clean `tokio::time::timeout` failure on the
    /// `JoinHandle` await instead of hanging the whole test binary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_does_not_hold_the_lock_across_the_await() {
        let approvals = std::sync::Arc::new(Approvals::new());
        let rx = approvals.register(sample("a1"));

        let waiter = {
            let approvals = approvals.clone();
            tokio::spawn(async move { approvals.wait("a1", rx, Duration::from_millis(300)).await })
        };

        // Give the waiter a moment to actually enter the timeout future.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let listed = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking({
                let approvals = approvals.clone();
                move || approvals.list()
            }),
        )
        .await
        .expect("list() must not hang while an unrelated wait() is still pending")
        .expect("spawn_blocking task panicked");
        assert_eq!(listed.len(), 1, "the entry is still pending mid-wait");

        assert!(approvals.resolve("a1", Decision::Approve));
        let outcome = waiter.await.unwrap();
        assert_eq!(outcome, WaitOutcome::Decided(Decision::Approve));
    }
}
