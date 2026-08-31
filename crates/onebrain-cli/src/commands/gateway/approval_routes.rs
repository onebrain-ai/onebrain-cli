//! Operator HTTP surface for the [`super::approval`] registry: `GET
//! /approvals` (list pending) and `POST /approvals/{id}` (resolve one) —
//! Gateway PR 4, Task 3.
//!
//! ## Design decision: this is the OPERATOR's surface, not the connector's
//!
//! These routes are deliberately NOT part of the `/mcp` nest and are
//! deliberately NOT gated by `auth::middleware::require_bearer` (the
//! connector Bearer check `server::build_gateway_router` wraps `/mcp`
//! with). A client that could approve its own pending request would defeat
//! the entire point of `ask_once`/`ask_always` policy modes — the human
//! checkpoint they exist to enforce (see [`super::approval`]'s module docs
//! for the full rationale). So `/approvals` is built as its own,
//! self-contained router here, gated instead by [`require_pairing_header`]:
//! the SAME human-typed pairing code
//! (`X-OneBrain-Pairing` header — bearer-style placement, but a DIFFERENT
//! credential entirely from an OAuth access token) that pairs a brand-new
//! connector in the first place, verified constant-time via
//! [`super::oauth_routes::AuthCtx::check_pairing_code`] — which wraps
//! `AuthStore::verify_pairing` (itself already calling
//! `auth::core::constant_time_str_eq`) in the same brute-force limiter
//! `POST /authorize` uses, so no new compare logic and no second failure
//! budget is written here at all; this module just reuses both, exactly as
//! instructed. A connector's live OAuth bearer token is never even
//! inspected by this gate (it only ever reads one header,
//! `X-OneBrain-Pairing`), so it can never satisfy it — proven explicitly by
//! `tests::connector_bearer_token_is_rejected_on_approvals` below, the
//! privilege-separation property this whole design rests on.
//!
//! `server::build_gateway_router` merges [`approval_router`]'s output in
//! alongside the OAuth discovery/registration/consent/token routers — after
//! the `/mcp` Bearer layer is applied to the `/mcp` sub-router, so (exactly
//! like those routers) the Bearer layer never wraps it either. See
//! `server::tests::approvals_route_is_merged_into_the_real_router_and_ignores_a_connector_bearer_token`
//! for the end-to-end proof against the REAL assembled router.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::approval::{self, PendingApproval};
use super::oauth_routes::{AuthCtx, PairingCheck};
use super::policy::{GrantKey, PolicyMode};
use super::server::GatewayState;

/// The operator credential header this whole surface is gated on — see the
/// module docs' "Design decision" section. Lower-case constant purely for
/// readability at the call site; `HeaderMap` lookups are already
/// case-insensitive (the `http` crate normalizes header names), so this is
/// not itself part of the security property.
const PAIRING_HEADER: &str = "x-onebrain-pairing";

/// Axum middleware guarding the `/approvals` router: requires the gateway's
/// current pairing code, presented via [`PAIRING_HEADER`], to
/// constant-time-match — checked through [`AuthCtx::check_pairing_code`],
/// which wraps `AuthStore::verify_pairing` in the SHARED brute-force
/// limiter. No token, missing
/// header, wrong code, a lockout, or an `AuthStore` I/O error (e.g. a corrupt
/// `pairing.json`) all collapse to the same bare 401 — there is no
/// different recovery action for an operator to take (re-read the pairing
/// code and try again) and no reason to hand out which failure mode
/// occurred. In particular a lockout is deliberately indistinguishable from
/// a wrong code here: telling a guesser they have tripped the limiter tells
/// them their earlier guesses were being counted.
///
/// **Why the shared limiter and not a local one.** This surface is gated on
/// the very same secret `POST /authorize` checks, and it is the last thing
/// standing between a caller and self-approval — an attacker needs no OAuth
/// token at all to reach it, just the port (a second local account, any
/// unsandboxed process on the box, or anything that reaches a tunnel once
/// `public_url` is in play). Calling `verify_pairing` directly, as an earlier
/// revision did, left this surface accepting unlimited, unthrottled, unlogged
/// guesses at a code `/authorize` rate-limits to five. A SECOND,
/// separately-counted limiter would have been its own defect: two budgets
/// over one secret doubles the guess rate an attacker gets, so this shares
/// `/authorize`'s exact counter, lockout, and log line — see
/// [`super::oauth_routes::AttemptState`].
///
/// Deliberately NOT `auth::middleware::require_bearer` — see the module
/// docs. This function reads ONLY [`PAIRING_HEADER`]; the `Authorization`
/// header, if present at all, is never even looked at.
///
/// No `std::sync::Mutex` guard is held across the `next.run(req).await`
/// below: `check_pairing_code` is synchronous and releases both of its locks
/// before returning its verdict.
async fn require_pairing_header(
    State(ctx): State<Arc<AuthCtx>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(code) = req
        .headers()
        .get(PAIRING_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let checked = ctx.check_pairing_code(code);

    match checked {
        PairingCheck::Accepted => next.run(req).await,
        PairingCheck::Rejected { .. } | PairingCheck::LockedOut => {
            StatusCode::UNAUTHORIZED.into_response()
        }
        PairingCheck::StoreError(e) => {
            // Operator-facing only; the client still gets the same bare 401.
            tracing::error!(error = %e, "pairing store I/O error during an /approvals request");
            StatusCode::UNAUTHORIZED.into_response()
        }
    }
}

/// `GET /approvals` — every currently pending approval. See
/// [`super::approval::Approvals::list`]'s own doc comment for why this can
/// never expose more than [`PendingApproval`]'s own fields.
async fn list_approvals(State(state): State<Arc<GatewayState>>) -> Json<Vec<PendingApproval>> {
    Json(state.approvals.list())
}

/// `POST /approvals/{id}` request body: `{"decision":"approve"|"deny"}`.
#[derive(Debug, Deserialize)]
struct ResolveRequest {
    decision: approval::Decision,
}

/// `POST /approvals/{id}` response body — deliberately minimal: an operator
/// already has the full [`PendingApproval`] from the `GET /approvals` call
/// that showed them this `id`, so there is nothing more to echo back here.
#[derive(Debug, Serialize)]
struct ResolveResponse {
    id: String,
    resolved: bool,
}

/// `POST /approvals/{id}`: resolve one pending approval.
///
/// On a `Decision::Approve` that actually resolved something, this is also
/// the gateway's first PRODUCTION caller of
/// [`super::policy::Grants::record`] (Task 2 review, binding requirement A)
/// — using a config-derived TTL (`PolicyConfig::grant_ttl_minutes * 60`),
/// not a test's hardcoded value. Approving one call grants the SAME
/// `(client, vault, class)` triple every subsequent `ask_once` call until
/// that grant expires — that is the entire point of `ask_once` vs.
/// `ask_always` (see `policy.rs`'s decision table doc comment). A
/// `Decision::Deny` records nothing: denial is never "ask less often next
/// time."
///
/// **Nothing is recorded under `ask_always` either**, whichever channel the
/// approval arrived through. `decide` already ignores grants in that mode,
/// so today this only avoids writing an entry nothing reads — but "always
/// ask" must never be capable of producing standing consent, and leaving a
/// live grant in the map for a mode whose whole meaning is "ask every time"
/// is a trap for the next refactor of `decide`. `server::await_approval`
/// applies the identical guard on the waiter side.
///
/// The pending entry's `client_id`/`vault`/`class` are snapshotted from
/// [`super::approval::Approvals::list`] BEFORE calling
/// [`super::approval::Approvals::resolve`], because `resolve` REMOVES the
/// entry as part of its own first-responder-wins contract (see that
/// method's doc comment) — this is the last point that information is still
/// available. If a concurrent resolve or a timeout wins the race instead,
/// `resolve` below simply returns `false` and nothing is recorded: never a
/// grant for a call that was actually denied, timed out, or already handled
/// by someone else.
async fn resolve_approval(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Json(body): Json<ResolveRequest>,
) -> Response {
    let snapshot = state.approvals.list().into_iter().find(|p| p.id == id);

    // This IS the operator HTTP surface — see the module docs' "Design
    // decision" section — so every resolution through here is, by
    // definition, `ResolvedVia::Http`. `server::record_audit` reads this
    // straight off `approval::WaitOutcome::Decided` on the waiter side
    // (Gateway PR 5, Task 3), so the audit log names this exact channel.
    let resolved = state
        .approvals
        .resolve(&id, body.decision, approval::ResolvedVia::Http);
    if !resolved {
        return (
            StatusCode::NOT_FOUND,
            Json(ResolveResponse {
                id,
                resolved: false,
            }),
        )
            .into_response();
    }

    if body.decision == approval::Decision::Approve {
        if let Some(p) = snapshot {
            if state.config.policy.mode_for(p.class) != PolicyMode::AskAlways {
                let ttl_secs = state.config.policy.grant_ttl_minutes.saturating_mul(60);
                state
                    .grants
                    .record(GrantKey::new(p.client_id, p.vault, p.class), ttl_secs);
            }
        }
    }

    (StatusCode::OK, Json(ResolveResponse { id, resolved: true })).into_response()
}

/// Build the `/approvals` router: `GET /approvals` + `POST /approvals/{id}`,
/// both gated by [`require_pairing_header`]. `state` gives the handlers
/// access to [`super::approval::Approvals`] (`state.approvals`) and
/// [`super::policy::Grants`] (`state.grants`); `auth_ctx` is the pairing
/// gate's own state, applied as a `.layer` — see `require_bearer`'s
/// identical `from_fn_with_state` shape in `auth/middleware.rs` for the
/// precedent this mirrors (a middleware-level state, independent of the
/// router's own handler state).
pub fn approval_router(state: Arc<GatewayState>, auth_ctx: Arc<AuthCtx>) -> Router {
    Router::new()
        .route("/approvals", get(list_approvals))
        .route("/approvals/{id}", post(resolve_approval))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_ctx,
            require_pairing_header,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::commands::gateway::audit::AuditLog;
    use crate::commands::gateway::auth::AuthStore;
    use crate::commands::gateway::policy::RiskClass;
    use crate::commands::gateway::GatewayConfig;

    /// A pending entry with a LIVE TTL — `Approvals::register` prunes
    /// already-expired entries (see its doc comment), so a fixture with a
    /// hardcoded past `expires` would vanish the moment a second entry was
    /// registered.
    fn sample_pending(id: &str) -> PendingApproval {
        let now = crate::commands::gateway::auth::core::now_epoch_secs();
        PendingApproval {
            id: id.to_string(),
            client_id: "client-1".to_string(),
            tool: "brain_capture".to_string(),
            vault: Some("t1".to_string()),
            summary: "note: Quarterly Plan".to_string(),
            created: now,
            expires: now + 300,
            class: RiskClass::Mutating,
        }
    }

    /// Builds a fresh `GatewayState` + `AuthCtx` pair (own tempdir for both
    /// the audit log and the auth store — mirrors `server.rs`'s own
    /// `fixture_router`/`test_auth_ctx` helpers) and the `/approvals` router
    /// ALONE — not the full `build_gateway_router`; that end-to-end wiring
    /// proof lives in `server.rs`'s own test module instead (see this
    /// file's module doc). Returns the pairing code already minted on the
    /// SAME store the router's `AuthCtx` shares, so a test can present it
    /// directly.
    fn fixture() -> (tempfile::TempDir, Router, Arc<GatewayState>, String) {
        let (dir, router, state, _ctx, code) = fixture_with_ctx();
        (dir, router, state, code)
    }

    /// [`fixture`] plus the shared [`AuthCtx`] itself, for the tests that
    /// need to build a SECOND router (the OAuth `/authorize` surface) over
    /// the very same context — the only way to prove the two surfaces share
    /// one pairing-attempt budget rather than each keeping their own.
    fn fixture_with_ctx() -> (
        tempfile::TempDir,
        Router,
        Arc<GatewayState>,
        Arc<AuthCtx>,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open_at(dir.path().join("gateway-audit")).unwrap();
        let state = Arc::new(GatewayState::new(GatewayConfig::default(), audit));

        let auth_store = AuthStore::open_at(dir.path().join("gateway-auth")).unwrap();
        let pairing_code = auth_store.pairing_code().unwrap();
        let auth_ctx = Arc::new(AuthCtx::new(auth_store));
        auth_ctx
            .issuer
            .set("http://127.0.0.1:7717".to_string())
            .unwrap();

        let router = approval_router(state.clone(), auth_ctx.clone());
        (dir, router, state, auth_ctx, pairing_code)
    }

    /// A pairing code guaranteed NOT to be `real` — same shape (so nothing
    /// short-circuits on length), different content.
    fn wrong_code(real: &str) -> &'static str {
        if real.starts_with('A') {
            "BBBB-BBBB"
        } else {
            "AAAA-AAAA"
        }
    }

    /// Mints a live connector access token against the SAME on-disk auth
    /// store `fixture()` already opened at `<dir>/gateway-auth` — reopening
    /// is cheap (`AuthStore` just holds a path; see its own doc comment),
    /// mirroring `server.rs`'s `test_auth_ctx`/second-token pattern.
    fn connector_bearer_token(dir: &std::path::Path) -> String {
        let store = AuthStore::open_at(dir.join("gateway-auth")).unwrap();
        let (access, _refresh) = store.issue_token_pair("connector-1", "brain").unwrap();
        access.token
    }

    async fn get_approvals(router: &Router, pairing: Option<&str>) -> axum::http::Response<Body> {
        get_approvals_with(router, pairing, None).await
    }

    async fn get_approvals_with(
        router: &Router,
        pairing: Option<&str>,
        bearer: Option<&str>,
    ) -> axum::http::Response<Body> {
        let mut builder = HttpRequest::builder().method("GET").uri("/approvals");
        if let Some(code) = pairing {
            builder = builder.header(PAIRING_HEADER, code);
        }
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let req = builder.body(Body::empty()).unwrap();
        router.clone().oneshot(req).await.unwrap()
    }

    async fn post_resolve(
        router: &Router,
        id: &str,
        pairing: Option<&str>,
        decision: &str,
    ) -> axum::http::Response<Body> {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri(format!("/approvals/{id}"))
            .header("content-type", "application/json");
        if let Some(code) = pairing {
            builder = builder.header(PAIRING_HEADER, code);
        }
        let body = serde_json::json!({ "decision": decision }).to_string();
        let req = builder.body(Body::from(body)).unwrap();
        router.clone().oneshot(req).await.unwrap()
    }

    async fn json_body(resp: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "response was not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    // ── Step 2: unauthenticated / wrong pairing code -> 401 ─────────────

    #[tokio::test]
    async fn approvals_without_pairing_header_401s() {
        let (_dir, router, _state, _code) = fixture();
        let resp = get_approvals(&router, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn approvals_with_wrong_pairing_code_401s() {
        let (_dir, router, _state, code) = fixture();
        let resp = get_approvals(&router, Some(wrong_code(&code))).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn resolve_without_pairing_header_401s_and_leaves_the_entry_pending() {
        let (_dir, router, state, _code) = fixture();
        let _rx = state.approvals.register(sample_pending("a1")).unwrap();

        let resp = post_resolve(&router, "a1", None, "approve").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            state.approvals.list().len(),
            1,
            "an unauthenticated resolve attempt must not have resolved anything"
        );
    }

    // ── Step 2: correct pairing code -> 200 with the pending list ───────

    #[tokio::test]
    async fn approvals_with_correct_pairing_code_lists_pending() {
        let (_dir, router, state, code) = fixture();
        let _rx = state.approvals.register(sample_pending("a1")).unwrap();

        let resp = get_approvals(&router, Some(&code)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{body}");
        assert_eq!(arr[0]["id"], "a1");
        assert_eq!(arr[0]["client_id"], "client-1");
        assert_eq!(arr[0]["class"], "mutating");
    }

    #[tokio::test]
    async fn approvals_lists_empty_when_nothing_pending() {
        let (_dir, router, _state, code) = fixture();
        let resp = get_approvals(&router, Some(&code)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    // ── Step 2: resolve -> 200 and the waiter wakes ─────────────────────

    #[tokio::test]
    async fn resolve_wakes_the_waiter_and_returns_200() {
        let (_dir, router, state, code) = fixture();
        let rx = state.approvals.register(sample_pending("a1")).unwrap();

        let resp = post_resolve(&router, "a1", Some(&code), "approve").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["resolved"], true, "{body}");

        assert_eq!(
            rx.await.unwrap(),
            (approval::Decision::Approve, approval::ResolvedVia::Http),
            "the /approvals HTTP surface must resolve as ResolvedVia::Http"
        );
        assert!(
            state.approvals.list().is_empty(),
            "a resolved approval must no longer be pending"
        );
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_not_found() {
        let (_dir, router, _state, code) = fixture();
        let resp = post_resolve(&router, "does-not-exist", Some(&code), "deny").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── Step 2: a connector's bearer token must NOT be accepted here ───

    /// The privilege-separation property this whole design rests on (see
    /// the module docs): a connector's own, perfectly live OAuth bearer
    /// token — freshly minted against the SAME auth store, exactly the kind
    /// of token that legitimately passes `/mcp`'s `require_bearer` gate —
    /// must be flatly ignored here. `require_pairing_header` never even
    /// reads the `Authorization` header, so presenting one (with no
    /// `X-OneBrain-Pairing`) is exactly as unauthenticated as presenting
    /// nothing at all.
    #[tokio::test]
    async fn connector_bearer_token_is_rejected_on_approvals() {
        let (dir, router, _state, _code) = fixture();
        let token = connector_bearer_token(dir.path());

        let resp = get_approvals_with(&router, None, Some(&token)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "a connector's own bearer token must never satisfy the operator pairing gate"
        );
    }

    #[tokio::test]
    async fn connector_bearer_token_alone_cannot_resolve_an_approval() {
        let (dir, router, state, _code) = fixture();
        let _rx = state.approvals.register(sample_pending("a1")).unwrap();
        let token = connector_bearer_token(dir.path());

        let req = HttpRequest::builder()
            .method("POST")
            .uri("/approvals/a1")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "decision": "approve" }).to_string(),
            ))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            state.approvals.list().len(),
            1,
            "a connector token must not be able to self-approve its own pending call"
        );
    }

    // ── The pairing gate is brute-force limited, on ONE shared budget ────

    /// The operator surface is the last thing between a caller and
    /// self-approval, and it needs no OAuth token to reach — so it must not
    /// accept unlimited guesses at the pairing code. Five wrong codes trip
    /// the lockout; the sixth submission is rejected even though the code is
    /// CORRECT, which is the observable difference between "limited" and
    /// "not limited" (without a limiter the correct code would be accepted).
    #[tokio::test]
    async fn repeated_wrong_pairing_codes_lock_out_the_approvals_surface() {
        let (_dir, router, _state, code) = fixture();
        let wrong = wrong_code(&code);

        for attempt in 1..=5 {
            let resp = get_approvals(&router, Some(wrong)).await;
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "attempt {attempt} must be refused"
            );
        }

        let resp = get_approvals(&router, Some(&code)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "the CORRECT code inside the lockout window must still be refused — \
             without a limiter this call would have succeeded"
        );
    }

    /// …and it is the SAME budget `POST /authorize` spends, not a second one
    /// of its own — two independent limiters over one secret would simply
    /// double the guess rate available to an attacker.
    ///
    /// Proven in both directions over one shared [`AuthCtx`]: four failures
    /// spent on `/approvals` leave `/authorize` one away from its lockout
    /// (so the FIFTH failure, made on `/authorize`, is the one that trips
    /// it), and the lockout that `/authorize` then reports also rejects a
    /// correct code back on `/approvals`.
    #[tokio::test]
    async fn the_approvals_gate_shares_one_failure_budget_with_authorize() {
        use crate::commands::gateway::auth::{mint_secret_32, AppType, RegisteredClient};
        use crate::commands::gateway::oauth_routes::{authorize_router, LOCKED_OUT_MESSAGE};

        let (_dir, approvals, _state, ctx, code) = fixture_with_ctx();
        let authorize = authorize_router(ctx.clone());
        let wrong = wrong_code(&code);

        // A registered client, so `POST /authorize` gets past its own
        // request validation and actually reaches the pairing check.
        let redirect_uri = "https://claude.ai/cb";
        let client_id = mint_secret_32();
        ctx.store
            .lock()
            .unwrap()
            .register_client(RegisteredClient {
                client_id: client_id.clone(),
                client_name: None,
                redirect_uris: vec![redirect_uri.to_string()],
                application_type: AppType::Web,
                created: crate::commands::gateway::auth::core::now_epoch_secs(),
            })
            .unwrap();

        let post_authorize = |pairing: String| {
            let router = authorize.clone();
            let client_id = client_id.clone();
            async move {
                let form = format!(
                    "response_type=code&client_id={client_id}\
                     &redirect_uri=https%3A%2F%2Fclaude.ai%2Fcb\
                     &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
                     &code_challenge_method=S256&state=s1&pairing_code={pairing}"
                );
                let req = HttpRequest::builder()
                    .method("POST")
                    .uri("/authorize")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap();
                let resp = router.oneshot(req).await.unwrap();
                let bytes = resp.into_body().collect().await.unwrap().to_bytes();
                String::from_utf8_lossy(&bytes).into_owned()
            }
        };

        // Four failures, all spent on the OPERATOR surface.
        for attempt in 1..=4 {
            let resp = get_approvals(&approvals, Some(wrong)).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "attempt {attempt}");
        }

        // The fifth failure lands on /authorize — and trips the lockout,
        // which can only happen if the four above counted against the same
        // budget.
        let body = post_authorize(wrong.to_string()).await;
        assert!(
            body.contains(LOCKED_OUT_MESSAGE),
            "four /approvals failures plus one /authorize failure must reach the \
             shared five-failure threshold; a per-surface budget would still be at 1"
        );

        // And the lockout is likewise shared in the other direction.
        let resp = get_approvals(&approvals, Some(&code)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "a lockout tripped on /authorize must also close the /approvals surface"
        );
    }

    /// A lockout must not be distinguishable from a plain wrong code here:
    /// a different status (or body) would tell a guesser their attempts are
    /// being counted, and would confirm which of their guesses were wrong.
    #[tokio::test]
    async fn a_locked_out_approvals_request_is_indistinguishable_from_a_wrong_code() {
        let (_dir, router, _state, code) = fixture();
        let wrong = wrong_code(&code);

        let first = get_approvals(&router, Some(wrong)).await;
        let first_status = first.status();
        let first_body = first.into_body().collect().await.unwrap().to_bytes();

        for _ in 0..5 {
            let _ = get_approvals(&router, Some(wrong)).await;
        }

        let locked = get_approvals(&router, Some(&code)).await;
        assert_eq!(locked.status(), first_status);
        assert_eq!(
            locked.into_body().collect().await.unwrap().to_bytes(),
            first_body
        );
    }

    // ── Requirement A: approving wires a real, config-derived-TTL grant ──

    #[tokio::test]
    async fn approving_records_a_grant_using_the_config_derived_ttl() {
        let (_dir, router, state, code) = fixture();
        let mut pending = sample_pending("a1");
        pending.client_id = "client-x".to_string();
        pending.class = RiskClass::Mutating;
        let _rx = state.approvals.register(pending).unwrap();

        let key = GrantKey::new("client-x", Some("t1".to_string()), RiskClass::Mutating);
        assert!(!state.grants.has(&key), "no grant before approval");

        let resp = post_resolve(&router, "a1", Some(&code), "approve").await;
        assert_eq!(resp.status(), StatusCode::OK);

        assert!(
            state.grants.has(&key),
            "approving must record a grant for the SAME (client, vault, class) triple"
        );
    }

    /// "Always ask" must never produce standing consent. `decide` already
    /// ignores grants under `ask_always`, so this is defence in depth — but
    /// a live grant sitting in the map for that mode is exactly the trap a
    /// later refactor of `decide` would fall into. The default config has
    /// `destructive: ask_always`, so a `Destructive` pending entry exercises
    /// it without a custom config.
    #[tokio::test]
    async fn approving_under_ask_always_records_no_grant() {
        let (_dir, router, state, code) = fixture();
        assert_eq!(
            state.config.policy.mode_for(RiskClass::Destructive),
            PolicyMode::AskAlways,
            "fixture precondition: the default config must make Destructive ask_always"
        );
        let mut pending = sample_pending("a1");
        pending.client_id = "client-z".to_string();
        pending.class = RiskClass::Destructive;
        let _rx = state.approvals.register(pending).unwrap();

        let resp = post_resolve(&router, "a1", Some(&code), "approve").await;
        assert_eq!(resp.status(), StatusCode::OK);

        assert!(
            !state.grants.has(&GrantKey::new(
                "client-z",
                Some("t1".to_string()),
                RiskClass::Destructive
            )),
            "an approval under ask_always must never leave standing consent behind"
        );
    }

    /// The DENY condition, isolated.
    ///
    /// This deliberately uses `Mutating`, which the default config maps to
    /// `ask_once` — a class where an APPROVE would record a grant (proven
    /// directly above by
    /// `approving_records_a_grant_using_the_config_derived_ttl`, which uses
    /// the same class and the same fixture). An earlier revision used
    /// `Destructive`, which the default config maps to `ask_always`: the
    /// inner "never record under ask_always" guard then suppressed the grant
    /// on its own, so the test passed identically with the decision check
    /// deleted and proved nothing about denial. With `Mutating`, the
    /// `decision == Approve` condition is the ONLY thing left that can keep
    /// this grant out of the map — verified by deleting that condition and
    /// watching this test, and only this test, fail.
    #[tokio::test]
    async fn denying_does_not_record_a_grant() {
        let (_dir, router, state, code) = fixture();
        assert_eq!(
            state.config.policy.mode_for(RiskClass::Mutating),
            PolicyMode::AskOnce,
            "fixture precondition: the class under test must NOT be ask_always, or the \
             ask_always guard would suppress the grant regardless of the decision"
        );
        let mut pending = sample_pending("a1");
        pending.client_id = "client-y".to_string();
        pending.class = RiskClass::Mutating;
        let _rx = state.approvals.register(pending).unwrap();

        let resp = post_resolve(&router, "a1", Some(&code), "deny").await;
        assert_eq!(resp.status(), StatusCode::OK);

        assert!(
            !state.grants.has(&GrantKey::new(
                "client-y",
                Some("t1".to_string()),
                RiskClass::Mutating
            )),
            "denying must never record a grant"
        );
    }
}
