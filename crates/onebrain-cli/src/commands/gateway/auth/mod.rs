//! `onebrain gateway` OAuth 2.1 auth core (Gateway PR 3, Task 1): opaque
//! server-side bearer tokens, PKCE S256, and device pairing — PURE LOGIC,
//! no HTTP. The HTTP routes that wrap these primitives (authorize/token/
//! register/pairing endpoints) land in later tasks against the exact public
//! names re-exported here.
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
//! `#![allow(dead_code)]`: this task (Gateway PR 3, Task 1) ships the auth
//! CORE only — no HTTP routes call `AuthStore`'s methods or the `core::`
//! primitives yet, those land in later tasks against these exact `pub`
//! names. Until they wire in, this whole module (and its re-exports below)
//! reads as dead/unused code to a plain `cargo build`/`clippy`, so we allow
//! it here — once, with this explanation — rather than sprinkle per-item
//! `#[allow]`s or gate the API behind a feature. The unit tests in `core.rs`
//! and `store.rs` exercise every path, so none of it is untested dead code.
#![allow(dead_code, unused_imports)]

pub mod core;
pub mod store;

pub use core::{
    base64url_nopad, constant_time_str_eq, mint_pairing_code, mint_secret_32, now_epoch_secs,
    pkce_s256_matches,
};
pub use store::{
    AppType, AuthCode, AuthStore, PairingState, RegisteredClient, RotateOutcome, TokenKind,
    TokenRecord,
};
