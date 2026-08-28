//! `onebrain gateway` OAuth 2.1 auth core (Gateway PR 3): opaque
//! server-side bearer tokens, PKCE S256, and device pairing.
//!
//! Task 1 shipped PURE LOGIC only ([`core`] + [`store`], no HTTP). Task 2
//! adds the first HTTP consumer — [`middleware::require_bearer`], the
//! resource-server gate wrapping the gateway's `/mcp` nest, which calls
//! [`store::AuthStore::check_access`] through the shared
//! [`crate::commands::gateway::oauth_routes::AuthCtx`]. The remaining
//! authorization-server routes (`/authorize`/`/token`/`/register`, and the
//! rest of `AuthStore`'s domain ops: `issue_code`/`consume_code`/
//! `rotate_refresh`/`revoke_token`/`rotate_pairing_code`/`register_client`/
//! `get_client`) land in Tasks 3-5 against the exact public names re-exported
//! here.
//!
//! Design ruling (not JWT): tokens are random opaque strings looked up in
//! [`store::AuthStore`], not signed/self-describing JWTs — this workspace
//! carries no JWT/HMAC crate, and opaque tokens give exact, immediate
//! revocation (a JWT would need shadow-revocation-list bookkeeping to match
//! that, i.e. the exact `TokenRecord.revoked` field this store already
//! provides directly).
//!
//! Zero new crate dependencies (hard constraint): see [`core`]'s module docs
//! for the hand-rolled base64url/constant-time-compare rationale, and
//! [`store`]'s for the persistence pattern this mirrors.
//!
//! ## Dead-code allow
//! Same situation as [`crate::commands::daemon_client`]'s own
//! `#![allow(dead_code)]`: most of `AuthStore`'s methods (and several
//! `core::` primitives) still have no HTTP caller until Tasks 3-5 wire in
//! `/authorize`/`/token`/`/register` — only `check_access` (via the Bearer
//! gate) and `pairing_code` (via `gateway::run`'s startup line) are reachable
//! as of Task 2. Until every method has a caller, the rest of this module
//! (and its re-exports below) reads as dead/unused code to a plain `cargo
//! build`/`clippy`, so we allow it here — once, with this explanation —
//! rather than sprinkle per-item `#[allow]`s or gate the API behind a
//! feature. The unit tests in `core.rs`, `store.rs`, and `middleware.rs`
//! exercise every path, so none of it is untested dead code.
#![allow(dead_code, unused_imports)]

pub mod core;
pub mod middleware;
pub mod store;

pub use core::{
    base64url_nopad, constant_time_str_eq, mint_pairing_code, mint_secret_32, now_epoch_secs,
    pkce_s256_matches,
};
pub use middleware::{require_bearer, Principal};
pub use store::{
    AppType, AuthCode, AuthStore, PairingState, RegisteredClient, RotateOutcome, TokenKind,
    TokenRecord,
};
