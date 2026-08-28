//! `onebrain gateway` OAuth 2.1 auth core (Gateway PR 3): opaque
//! server-side bearer tokens, PKCE S256, and device pairing.
//!
//! Task 1 shipped PURE LOGIC only ([`core`] + [`store`], no HTTP). Task 2
//! added the first HTTP consumer — [`middleware::require_bearer`], the
//! resource-server gate wrapping the gateway's `/mcp` nest, which calls
//! [`store::AuthStore::check_access`] through the shared
//! [`crate::commands::gateway::oauth_routes::AuthCtx`]. Tasks 3-5 landed the
//! rest of the authorization-server surface against that same `AuthCtx`:
//! `POST /register` (RFC 7591), `GET`/`POST /authorize` (the consent flow),
//! and `POST /token` (code exchange + refresh rotation, including the RFC
//! 6749 §4.1.2 replay hardening that added
//! [`store::AuthStore::mark_code_minted_family`]/
//! [`store::AuthStore::find_code_record`]/[`store::AuthStore::revoke_family`]).
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
//! `#![allow(dead_code)]`. Tasks 1-5 wired an HTTP (or startup-line) caller
//! for nearly every method here, but a few remain genuinely unreached until
//! a LATER task: [`store::AuthStore::rotate_pairing_code`] (planned CLI
//! subcommand to re-mint a pairing code) and
//! [`store::AuthStore::purge_expired`] (planned periodic sweep — see its own
//! doc comment). `base64url_nopad`/`constant_time_str_eq` are also re-exported
//! but never directly imported outside this module (their only external
//! callers go through [`core::pkce_s256_matches`] instead). Until every
//! re-exported item has an external caller, the rest of this module reads as
//! dead/unused code to a plain `cargo build`/`clippy`, so we allow it here —
//! once, with this explanation — rather than sprinkle per-item `#[allow]`s or
//! gate the API behind a feature. The unit tests in `core.rs`, `store.rs`,
//! and `middleware.rs` exercise every path, so none of it is untested dead
//! code.
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
// `pub(crate)` (not `pub use` above) — `ACCESS_TTL_SECS` is `pub(crate)` in
// `store`, so re-exporting it any more broadly than crate-visible would be
// widening its visibility, which `pub use` doesn't allow. `oauth_routes.rs`
// (the one crate-internal consumer, for `/token`'s `expires_in`) reaches it
// through this same curated import list either way.
pub(crate) use store::ACCESS_TTL_SECS;
