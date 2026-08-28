//! Persisted gateway auth state: `~/.onebrain/gateway/{clients,codes,tokens,pairing}.json`.
//!
//! **Persistence pattern mirrors [`crate::commands::daemon_client::DaemonInfo`]
//! exactly** — owner-only files (0600) in an owner-only directory (0700),
//! atomic tmp-sibling + rename writes, and a re-assert-then-`tracing::warn!`
//! (never a silent swallow) on a chmod failure after create. See
//! `DaemonInfo::write`/`read` and `ensure_private_run_dir` for the precedent
//! this file's [`write_json_atomic`]/[`read_json_or_default`]/
//! [`ensure_private_dir`] copy.
//!
//! **Design ruling — opaque tokens, not JWT.** Every credential this store
//! mints (auth codes, access/refresh tokens, pairing codes) is a random
//! opaque string from [`super::core::mint_secret_32`] /
//! [`super::core::mint_pairing_code`], looked up by exact key in one of the
//! four JSON maps below. Nothing here is a signed, self-describing token —
//! this workspace carries no JWT/HMAC crate (and won't gain one just for
//! this), and opaque tokens give EXACT revocation for free (flip
//! `revoked`/delete the map entry) where a JWT would need a parallel
//! denylist to match.
//!
//! **The security-critical invariant this file must get right: refresh
//! rotation + reuse detection.** [`AuthStore::rotate_refresh`] implements
//! OAuth 2.1's mandated refresh-token-rotation reuse detection (RFC 6819
//! §5.2.2.3 / OAuth 2.1 draft §4.14.3): a refresh token is single-use — each
//! successful rotation marks the presented token `revoked` and stamps
//! `rotated_to` with the new refresh token's value. If that SAME
//! already-rotated token is presented again (`rotated_to.is_some()`), that
//! can only mean a copy of it leaked (a legitimate client always moves
//! forward to the newest token) — so every token sharing its `family` id,
//! INCLUDING the pair minted by the legitimate rotation that already
//! happened, is revoked. We cannot tell the attacker's copy from the
//! legitimate holder's at that point, so neither gets to keep going; the
//! legitimate client discovers this on its next call and must re-auth from
//! scratch. See the `reuse_detection_revokes_whole_family_both_new_tokens_die`
//! test below for the end-to-end proof.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::core;

// ── Domain types ─────────────────────────────────────────────────────────

/// OAuth dynamic-client-registration application type — affects which token
/// endpoint auth methods a later HTTP task will require (public native apps
/// can't hold a client secret; confidential web apps can).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppType {
    Native,
    Web,
}

/// A dynamically registered OAuth client. Persisted in `clients.json`, keyed
/// by `client_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub application_type: AppType,
    pub created: u64,
}

/// A single-use authorization code minted by the `/authorize` step (lands in
/// a later HTTP task) and redeemed by the `/token` step. Persisted in
/// `codes.json`, keyed by `code`.
///
/// `Debug` is hand-written (not derived) to redact `code` — see the impl
/// below and the module-level rationale on [`TokenRecord`]'s own redacted
/// `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCode {
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: String,
    pub scope: String,
    pub expires: u64,
    pub used: bool,
}

/// Redacts `code` (the bearer secret redeemable at `/token`) — every other
/// field is either non-secret (`client_id`, `redirect_uri`, `resource`,
/// `scope`, `expires`, `used`) or, in `code_challenge`'s case, a PKCE S256
/// hash that is INTENDED to be sent openly in the `/authorize` URL (RFC 7636
/// — the secret is the verifier, which this store never persists), so it
/// stays visible for debugging. See [`TokenRecord`]'s `Debug` impl for the
/// full rationale (this exists so a later `{:?}` in a log path can't leak a
/// redeemable code).
impl std::fmt::Debug for AuthCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCode")
            .field("code", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("code_challenge", &self.code_challenge)
            .field("resource", &self.resource)
            .field("scope", &self.scope)
            .field("expires", &self.expires)
            .field("used", &self.used)
            .finish()
    }
}

/// Auth codes are short-lived by design (RFC 6749 §4.1.2 recommends a code
/// live only long enough to complete one redirect round-trip) — 10 minutes.
const AUTH_CODE_TTL_SECS: u64 = 600;

/// Access vs. refresh — same [`TokenRecord`] shape, different TTL and
/// rotation behavior (only `Refresh` tokens rotate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Access,
    Refresh,
}

/// One opaque bearer token (access or refresh). Persisted in `tokens.json`,
/// keyed by `token`.
///
/// `family` groups every token descended from one `issue_token_pair` call —
/// an access/refresh pair minted together share a family, and every
/// subsequent rotation's new pair keeps that SAME family id. This is what
/// lets [`AuthStore::rotate_refresh`]'s reuse detection burn "every token
/// that ever descended from this login" in one pass. `rotated_to` is `None`
/// until this exact token is exchanged during a rotation, at which point it
/// holds the new refresh token's value — that's the reuse-detection tripwire
/// (see module docs).
///
/// `Debug` is hand-written (not derived) — see the impl below: a bare
/// `#[derive(Debug)]` here would print the raw `token`/`rotated_to` secret
/// verbatim, and this type is exactly the kind of thing that ends up in a
/// `tracing::debug!(?record, ...)` somewhere down the line. Redacting at the
/// `Debug` level (rather than trusting every call site to remember not to
/// log the field directly) means that mistake can't leak a credential no
/// matter where it's made (Task 1 review finding, binding Task 2 requirement
/// B).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    pub token: String,
    pub kind: TokenKind,
    pub family: String,
    pub client_id: String,
    pub scope: String,
    pub expires: u64,
    pub revoked: bool,
    pub rotated_to: Option<String>,
}

/// Redacts `token` (the bearer credential itself) and `rotated_to` (which,
/// when present, holds the NEXT refresh token's raw value — just as much a
/// live credential as `token`). `family` stays visible: it's an internal
/// correlation id used only for the reuse-detection cascade (see module
/// docs), never accepted anywhere as a credential, so printing it doesn't
/// hand out anything an attacker could authenticate with.
impl std::fmt::Debug for TokenRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenRecord")
            .field("token", &"<redacted>")
            .field("kind", &self.kind)
            .field("family", &self.family)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .field("expires", &self.expires)
            .field("revoked", &self.revoked)
            .field(
                "rotated_to",
                &self.rotated_to.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// 1 hour — a conventional OAuth access-token lifetime; short enough that a
/// leaked access token self-expires quickly, long enough to avoid rotating
/// on every request.
const ACCESS_TTL_SECS: u64 = 60 * 60;
/// 30 days — refresh tokens are long-lived by design (that's the point of
/// having them); rotation + reuse detection is what keeps that safe.
const REFRESH_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// The current device-pairing code + when it was (re)minted. Persisted in
/// `pairing.json` as a single record (not a map) — a gateway has exactly one
/// active pairing code at a time; minting a new one (via
/// [`AuthStore::rotate_pairing_code`]) replaces it outright, invalidating the
/// old one.
///
/// `Debug` is hand-written (not derived) to redact `code` — see
/// [`TokenRecord`]'s `Debug` impl for the full rationale.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingState {
    pub code: String,
    pub created: u64,
}

impl std::fmt::Debug for PairingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingState")
            .field("code", &"<redacted>")
            .field("created", &self.created)
            .finish()
    }
}

/// Outcome of [`AuthStore::rotate_refresh`]. See the module docs for the
/// full reuse-detection rationale.
#[derive(Debug, PartialEq, Eq)]
pub enum RotateOutcome {
    /// The presented refresh token was valid and unused; here is the fresh
    /// pair minted in its place (same `family`). Boxed (clippy
    /// `large_enum_variant`) so the `ReuseDetected`/`Invalid` variants don't
    /// pay for two `TokenRecord`s' worth of space in every `RotateOutcome`.
    Rotated {
        access: Box<TokenRecord>,
        refresh: Box<TokenRecord>,
    },
    /// The presented refresh token had ALREADY been rotated once before
    /// (`rotated_to.is_some()`) — a stolen/replayed token. Every token in its
    /// `family` (including the pair from the legitimate rotation) is now
    /// revoked.
    ReuseDetected,
    /// Not found, not a refresh token, expired, or explicitly revoked
    /// (without having been rotated) — nothing to rotate, no family-wide
    /// action taken.
    Invalid,
}

// ── Store ────────────────────────────────────────────────────────────────

/// Handle onto the four JSON files under `root` (normally
/// `~/.onebrain/gateway/`). Cheap to construct — holds only the root path;
/// every op re-reads its file fresh (single-process, low-frequency local
/// auth traffic; no in-memory cache to keep coherent with the on-disk
/// source of truth).
pub struct AuthStore {
    root: PathBuf,
}

impl AuthStore {
    /// Open the real gateway auth store at `~/.onebrain/gateway/` (created
    /// 0700 if absent). Same home resolution as
    /// [`crate::commands::daemon_client::run_dir`] /
    /// [`crate::commands::gateway::config::gateway_config_path`]:
    /// [`crate::home::home_dir`], which honours `$HOME`/`%USERPROFILE%` on
    /// both platforms (plain `dirs::home_dir()` does not on Windows).
    pub fn open() -> Result<AuthStore> {
        let home =
            crate::home::home_dir().context("resolve home directory for gateway auth store")?;
        Self::open_at(home.join(".onebrain").join("gateway"))
    }

    /// Open (creating 0700 if absent) the auth store at an arbitrary `root`.
    /// `pub(crate)` — the real entry point is [`Self::open`]; this exists so
    /// tests can point the store at a tempdir instead of the real home.
    pub(crate) fn open_at(root: PathBuf) -> Result<AuthStore> {
        ensure_private_dir(&root)?;
        Ok(AuthStore { root })
    }

    fn clients_path(&self) -> PathBuf {
        self.root.join("clients.json")
    }
    fn codes_path(&self) -> PathBuf {
        self.root.join("codes.json")
    }
    fn tokens_path(&self) -> PathBuf {
        self.root.join("tokens.json")
    }
    fn pairing_path(&self) -> PathBuf {
        self.root.join("pairing.json")
    }

    fn load_clients(&self) -> Result<BTreeMap<String, RegisteredClient>> {
        read_json_or_default(&self.clients_path())
    }
    fn save_clients(&self, clients: &BTreeMap<String, RegisteredClient>) -> Result<()> {
        write_json_atomic(&self.clients_path(), clients)
    }

    fn load_codes(&self) -> Result<BTreeMap<String, AuthCode>> {
        read_json_or_default(&self.codes_path())
    }
    fn save_codes(&self, codes: &BTreeMap<String, AuthCode>) -> Result<()> {
        write_json_atomic(&self.codes_path(), codes)
    }

    fn load_tokens(&self) -> Result<BTreeMap<String, TokenRecord>> {
        read_json_or_default(&self.tokens_path())
    }
    fn save_tokens(&self, tokens: &BTreeMap<String, TokenRecord>) -> Result<()> {
        write_json_atomic(&self.tokens_path(), tokens)
    }

    fn load_pairing(&self) -> Result<Option<PairingState>> {
        read_json_or_default(&self.pairing_path())
    }
    fn save_pairing(&self, state: &PairingState) -> Result<()> {
        write_json_atomic(&self.pairing_path(), state)
    }

    // ── Clients ──────────────────────────────────────────────────────────

    /// Insert or overwrite a client registration, keyed by its `client_id`.
    pub fn register_client(&self, client: RegisteredClient) -> Result<()> {
        let mut clients = self.load_clients()?;
        clients.insert(client.client_id.clone(), client);
        self.save_clients(&clients)
    }

    /// Look up a registered client by id, or `None` if never registered.
    pub fn get_client(&self, client_id: &str) -> Result<Option<RegisteredClient>> {
        Ok(self.load_clients()?.remove(client_id))
    }

    // ── Authorization codes ─────────────────────────────────────────────

    /// Mint and persist a fresh, single-use auth code (>= 32 random bytes,
    /// [`AUTH_CODE_TTL_SECS`] lifetime) carrying the PKCE challenge + the
    /// rest of the `/authorize` request's parameters, to be redeemed once by
    /// [`Self::consume_code`].
    pub fn issue_code(
        &self,
        client_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
        resource: &str,
        scope: &str,
    ) -> Result<AuthCode> {
        let code_value = core::mint_secret_32();
        let auth_code = AuthCode {
            code: code_value.clone(),
            client_id: client_id.to_string(),
            redirect_uri: redirect_uri.to_string(),
            code_challenge: code_challenge.to_string(),
            resource: resource.to_string(),
            scope: scope.to_string(),
            expires: core::now_epoch_secs() + AUTH_CODE_TTL_SECS,
            used: false,
        };
        let mut codes = self.load_codes()?;
        codes.insert(code_value, auth_code.clone());
        self.save_codes(&codes)?;
        Ok(auth_code)
    }

    /// Redeem `code` exactly once: unknown, already-`used`, or expired codes
    /// are rejected as `Ok(None)` (not an error — a caller treats this as
    /// "invalid_grant"); a fresh, unexpired code is marked `used` (so a
    /// second redemption of the SAME code always fails, even mid-expiry
    /// window) and returned.
    pub fn consume_code(&self, code: &str) -> Result<Option<AuthCode>> {
        let mut codes = self.load_codes()?;
        let now = core::now_epoch_secs();
        let Some(entry) = codes.get_mut(code) else {
            return Ok(None);
        };
        if entry.used || entry.expires <= now {
            return Ok(None);
        }
        entry.used = true;
        let consumed = entry.clone();
        self.save_codes(&codes)?;
        Ok(Some(consumed))
    }

    // ── Tokens ───────────────────────────────────────────────────────────

    /// Mint a fresh access+refresh pair (>= 32 random bytes each) sharing a
    /// new random `family` id, with [`ACCESS_TTL_SECS`]/[`REFRESH_TTL_SECS`]
    /// lifetimes. Persists both before returning them.
    pub fn issue_token_pair(
        &self,
        client_id: &str,
        scope: &str,
    ) -> Result<(TokenRecord, TokenRecord)> {
        let family = core::mint_secret_32();
        let now = core::now_epoch_secs();
        let access = TokenRecord {
            token: core::mint_secret_32(),
            kind: TokenKind::Access,
            family: family.clone(),
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            expires: now + ACCESS_TTL_SECS,
            revoked: false,
            rotated_to: None,
        };
        let refresh = TokenRecord {
            token: core::mint_secret_32(),
            kind: TokenKind::Refresh,
            family,
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            expires: now + REFRESH_TTL_SECS,
            revoked: false,
            rotated_to: None,
        };

        let mut tokens = self.load_tokens()?;
        tokens.insert(access.token.clone(), access.clone());
        tokens.insert(refresh.token.clone(), refresh.clone());
        self.save_tokens(&tokens)?;
        Ok((access, refresh))
    }

    /// Validate a presented bearer token as an in-date, unrevoked ACCESS
    /// token; `None` for anything else (unknown token, a refresh token
    /// presented where an access token is expected, expired, or revoked).
    ///
    /// This is a plain map lookup by the token's exact value, not a
    /// constant-time scan — that's a deliberate choice, not an oversight.
    /// Constant-time compare (used elsewhere in this module for the pairing
    /// code and, in `core::pkce_s256_matches`, for the PKCE challenge)
    /// matters when a SHORT or attacker-influenced secret is compared
    /// byte-by-byte against the one correct value, because an early-exit
    /// compare then leaks "how many leading bytes were right" through
    /// timing. A `BTreeMap`/hash lookup for a 256-bit random token doesn't
    /// have that shape: the comparisons made while walking to (or missing)
    /// the matching entry are against OTHER stored keys, not incremental
    /// byte-by-byte feedback on the ONE correct token, so there is no
    /// partial-credit signal for an attacker to accumulate. See `check_access`
    /// callers for where this token travels (always loopback / a paired
    /// device in later tasks), and `core.rs`'s module docs for the constant-
    /// time cases that DO apply.
    pub fn check_access(&self, token: &str) -> Result<Option<TokenRecord>> {
        let tokens = self.load_tokens()?;
        let now = core::now_epoch_secs();
        Ok(tokens.get(token).and_then(|rec| {
            (rec.kind == TokenKind::Access && !rec.revoked && rec.expires > now)
                .then(|| rec.clone())
        }))
    }

    /// Rotate a refresh token, enforcing single-use + reuse detection. See
    /// the module docs for the full invariant and rationale; short version:
    ///
    /// - Unknown / wrong-kind / expired / (explicitly, never-rotated) revoked
    ///   → [`RotateOutcome::Invalid`].
    /// - Already rotated once before (`rotated_to.is_some()`) → burn the
    ///   whole `family` (every token sharing it, revoked) →
    ///   [`RotateOutcome::ReuseDetected`].
    /// - Fresh and valid → mark it spent (`revoked = true`,
    ///   `rotated_to = Some(new_refresh)`), mint a new pair in the SAME
    ///   family → [`RotateOutcome::Rotated`].
    pub fn rotate_refresh(&self, refresh: &str) -> Result<RotateOutcome> {
        let mut tokens = self.load_tokens()?;
        let now = core::now_epoch_secs();

        let Some(rec) = tokens.get(refresh).cloned() else {
            return Ok(RotateOutcome::Invalid);
        };
        if rec.kind != TokenKind::Refresh {
            return Ok(RotateOutcome::Invalid);
        }

        // Reuse: this exact refresh token was already exchanged once
        // (`rotated_to` was stamped by a prior successful rotation below) and
        // is being presented again. A legitimate client always moves forward
        // to the newest refresh token, so a repeat presentation of a
        // superseded one can only mean a copy leaked. Burn the family: we
        // cannot tell the attacker's copy from the legitimate holder's, so
        // neither gets to keep going — this includes the pair minted by the
        // rotation that already happened.
        if rec.rotated_to.is_some() {
            for t in tokens.values_mut() {
                if t.family == rec.family {
                    t.revoked = true;
                }
            }
            self.save_tokens(&tokens)?;
            return Ok(RotateOutcome::ReuseDetected);
        }

        if rec.revoked || rec.expires <= now {
            return Ok(RotateOutcome::Invalid);
        }

        let new_access = TokenRecord {
            token: core::mint_secret_32(),
            kind: TokenKind::Access,
            family: rec.family.clone(),
            client_id: rec.client_id.clone(),
            scope: rec.scope.clone(),
            expires: now + ACCESS_TTL_SECS,
            revoked: false,
            rotated_to: None,
        };
        let new_refresh = TokenRecord {
            token: core::mint_secret_32(),
            kind: TokenKind::Refresh,
            family: rec.family.clone(),
            client_id: rec.client_id.clone(),
            scope: rec.scope.clone(),
            expires: now + REFRESH_TTL_SECS,
            revoked: false,
            rotated_to: None,
        };

        if let Some(old) = tokens.get_mut(refresh) {
            old.revoked = true;
            old.rotated_to = Some(new_refresh.token.clone());
        }
        tokens.insert(new_access.token.clone(), new_access.clone());
        tokens.insert(new_refresh.token.clone(), new_refresh.clone());
        self.save_tokens(&tokens)?;
        Ok(RotateOutcome::Rotated {
            access: Box::new(new_access),
            refresh: Box::new(new_refresh),
        })
    }

    /// Revoke exactly the one named token (access OR refresh). Does NOT
    /// cascade to its `family` — that cascading behavior is reserved for
    /// [`Self::rotate_refresh`]'s reuse-detection path, which has a specific
    /// "a copy of this exact token leaked" signal to act on. A plain,
    /// intentional revoke (e.g. a future logout route) only has the caller's
    /// say-so for the ONE token it names; a no-op on an unknown token.
    pub fn revoke_token(&self, token: &str) -> Result<()> {
        let mut tokens = self.load_tokens()?;
        if let Some(rec) = tokens.get_mut(token) {
            rec.revoked = true;
            self.save_tokens(&tokens)?;
        }
        Ok(())
    }

    // ── Pairing ──────────────────────────────────────────────────────────

    /// The current pairing code, minting one (and persisting it) on first
    /// call. Idempotent after that — repeated calls return the SAME code
    /// until [`Self::rotate_pairing_code`] replaces it.
    pub fn pairing_code(&self) -> Result<String> {
        if let Some(state) = self.load_pairing()? {
            return Ok(state.code);
        }
        let code = core::mint_pairing_code();
        self.save_pairing(&PairingState {
            code: code.clone(),
            created: core::now_epoch_secs(),
        })?;
        Ok(code)
    }

    /// Mint a brand-new pairing code and persist it in place of whatever was
    /// there — the old code stops verifying immediately (verification only
    /// ever checks the CURRENT record).
    pub fn rotate_pairing_code(&self) -> Result<String> {
        let code = core::mint_pairing_code();
        self.save_pairing(&PairingState {
            code: code.clone(),
            created: core::now_epoch_secs(),
        })?;
        Ok(code)
    }

    /// Does `code` match the current pairing code? Constant-time (via
    /// [`core::constant_time_str_eq`]) — unlike [`Self::check_access`]'s
    /// token map lookup, this compares a SHORT (8-char, 34-symbol-alphabet)
    /// human-typed secret against the ONE stored correct value, which is
    /// exactly the byte-by-byte-leak shape constant-time compare exists to
    /// close. `false` (not an error) when no pairing code has ever been
    /// minted.
    pub fn verify_pairing(&self, code: &str) -> Result<bool> {
        match self.load_pairing()? {
            Some(state) => Ok(core::constant_time_str_eq(&state.code, code)),
            None => Ok(false),
        }
    }

    // ── Housekeeping ─────────────────────────────────────────────────────

    /// Drop every expired auth code and token from disk. Best-effort garbage
    /// collection (nothing calls this automatically yet — a later task
    /// wires it into a periodic sweep); safe to call any time.
    pub fn purge_expired(&self) -> Result<()> {
        let now = core::now_epoch_secs();

        let mut codes = self.load_codes()?;
        let before = codes.len();
        codes.retain(|_, c| c.expires > now);
        if codes.len() != before {
            self.save_codes(&codes)?;
        }

        let mut tokens = self.load_tokens()?;
        let before = tokens.len();
        tokens.retain(|_, t| t.expires > now);
        if tokens.len() != before {
            self.save_tokens(&tokens)?;
        }

        Ok(())
    }
}

// ── File I/O helpers (mirrors `daemon_client::DaemonInfo`) ────────────────

/// Create `dir` with owner-only (0700) permissions on Unix, re-asserting the
/// mode (and warning, never silently swallowing) if it already existed with
/// looser bits. Plain recursive create on non-Unix. Mirrors
/// `daemon_client::ensure_private_run_dir` exactly.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create gateway auth store dir {}", dir.display()))?;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = %dir.display(),
                "could not re-assert 0700 on gateway auth store dir");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create gateway auth store dir {}", dir.display()))
    }
}

/// Serialize `value` and atomically replace `path` with it: write to a
/// `.tmp` sibling with owner-only (0600) perms, re-assert 0600 (warn, don't
/// swallow, on failure — this is a credential file), then rename over the
/// real path. Mirrors `daemon_client::DaemonInfo::write` exactly.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).context("serialize gateway auth store json")?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::fs::OpenOptions;
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        use std::io::Write;
        f.write_all(&bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(error = %e, path = %tmp.display(),
                "could not re-assert 0600 on gateway auth store file (may be readable)");
        }
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Read + parse `path` as JSON; a missing file yields `T::default()` (empty
/// map / `None`), a present-but-corrupt file is a hard `Err` (never silently
/// treated as empty — a corrupt store must not look like "nothing here yet").
/// Mirrors `daemon_client::DaemonInfo::read`'s missing-vs-corrupt contract.
fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).context(format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, AuthStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("gateway")).unwrap();
        (dir, store)
    }

    fn client(id: &str) -> RegisteredClient {
        RegisteredClient {
            client_id: id.to_string(),
            client_name: Some("Test Client".to_string()),
            redirect_uris: vec!["https://example.test/cb".to_string()],
            application_type: AppType::Native,
            created: core::now_epoch_secs(),
        }
    }

    // ── Clients ──────────────────────────────────────────────────────────

    #[test]
    fn register_and_get_client_round_trips() {
        let (_dir, store) = open_temp();
        assert!(store.get_client("c1").unwrap().is_none());
        store.register_client(client("c1")).unwrap();
        let got = store.get_client("c1").unwrap().unwrap();
        assert_eq!(got.client_id, "c1");
        assert_eq!(got.application_type, AppType::Native);
    }

    // ── Auth codes: single-use + expiry ─────────────────────────────────

    #[test]
    fn code_is_single_use_second_consume_fails() {
        let (_dir, store) = open_temp();
        let issued = store
            .issue_code("client1", "https://cb", "challenge", "res", "scope")
            .unwrap();

        let consumed = store.consume_code(&issued.code).unwrap();
        assert_eq!(
            consumed.as_ref().map(|c| &c.client_id),
            Some(&"client1".to_string())
        );

        // Second redemption of the SAME code must fail — that's the whole
        // point of a single-use auth code (RFC 6749 §4.1.2: a replayed code
        // must cause the server to revoke everything issued from it; here,
        // simply refusing the replay is step one and is what this store
        // guarantees).
        let replay = store.consume_code(&issued.code).unwrap();
        assert!(replay.is_none(), "a used code must not be consumable twice");
    }

    #[test]
    fn unknown_code_is_not_consumable() {
        let (_dir, store) = open_temp();
        assert!(store.consume_code("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn expired_code_is_rejected() {
        let (_dir, store) = open_temp();
        // Insert an already-expired code directly (bypassing `issue_code`'s
        // fixed TTL) so the test doesn't need to sleep past a real 10-minute
        // window.
        let expired = AuthCode {
            code: "expired-code".to_string(),
            client_id: "client1".to_string(),
            redirect_uri: "https://cb".to_string(),
            code_challenge: "challenge".to_string(),
            resource: "res".to_string(),
            scope: "scope".to_string(),
            expires: core::now_epoch_secs().saturating_sub(1),
            used: false,
        };
        let mut codes = BTreeMap::new();
        codes.insert(expired.code.clone(), expired.clone());
        store.save_codes(&codes).unwrap();

        assert!(store.consume_code(&expired.code).unwrap().is_none());
    }

    // ── Tokens: issuance + check_access on expired/revoked ──────────────

    #[test]
    fn issued_access_token_passes_check_access() {
        let (_dir, store) = open_temp();
        let (access, refresh) = store.issue_token_pair("client1", "read write").unwrap();
        assert_eq!(access.kind, TokenKind::Access);
        assert_eq!(refresh.kind, TokenKind::Refresh);
        assert_eq!(access.family, refresh.family, "pair must share one family");
        assert_ne!(access.token, refresh.token);

        let checked = store.check_access(&access.token).unwrap().unwrap();
        assert_eq!(checked.token, access.token);
        assert_eq!(checked.client_id, "client1");
    }

    #[test]
    fn check_access_rejects_a_refresh_token() {
        let (_dir, store) = open_temp();
        let (_access, refresh) = store.issue_token_pair("client1", "scope").unwrap();
        assert!(
            store.check_access(&refresh.token).unwrap().is_none(),
            "a refresh token must not pass as an access token"
        );
    }

    #[test]
    fn check_access_rejects_unknown_token() {
        let (_dir, store) = open_temp();
        assert!(store.check_access("nope").unwrap().is_none());
    }

    #[test]
    fn check_access_rejects_expired_token() {
        let (_dir, store) = open_temp();
        let (access, _refresh) = store.issue_token_pair("client1", "scope").unwrap();

        // Directly age the persisted record past expiry rather than sleeping
        // out a real 1-hour TTL.
        let mut tokens = store.load_tokens().unwrap();
        tokens.get_mut(&access.token).unwrap().expires = core::now_epoch_secs().saturating_sub(1);
        store.save_tokens(&tokens).unwrap();

        assert!(store.check_access(&access.token).unwrap().is_none());
    }

    #[test]
    fn check_access_rejects_revoked_token() {
        let (_dir, store) = open_temp();
        let (access, _refresh) = store.issue_token_pair("client1", "scope").unwrap();
        store.revoke_token(&access.token).unwrap();
        assert!(store.check_access(&access.token).unwrap().is_none());
    }

    #[test]
    fn revoke_token_does_not_cascade_to_the_rest_of_the_family() {
        let (_dir, store) = open_temp();
        let (access, refresh) = store.issue_token_pair("client1", "scope").unwrap();
        store.revoke_token(&access.token).unwrap();

        assert!(store.check_access(&access.token).unwrap().is_none());
        // The refresh token is untouched — `revoke_token` is deliberately
        // scoped to exactly the token it names (see its doc comment).
        match store.rotate_refresh(&refresh.token).unwrap() {
            RotateOutcome::Rotated { .. } => {}
            other => panic!("sibling refresh token should still rotate fine: {other:?}"),
        }
    }

    // ── Refresh rotation: happy path ─────────────────────────────────────

    #[test]
    fn rotate_refresh_happy_path_mints_new_pair_in_same_family() {
        let (_dir, store) = open_temp();
        let (access1, refresh1) = store.issue_token_pair("client1", "scope").unwrap();

        let outcome = store.rotate_refresh(&refresh1.token).unwrap();
        let (access2, refresh2) = match outcome {
            RotateOutcome::Rotated { access, refresh } => (access, refresh),
            other => panic!("expected Rotated, got {other:?}"),
        };

        assert_ne!(access2.token, access1.token);
        assert_ne!(refresh2.token, refresh1.token);
        assert_eq!(
            access2.family, access1.family,
            "rotation keeps the same family"
        );
        assert_eq!(refresh2.family, access1.family);

        // New pair is live.
        assert!(store.check_access(&access2.token).unwrap().is_some());
        // Old access token from before rotation is untouched by rotation
        // itself (rotation only spends the REFRESH token that was presented).
        assert!(store.check_access(&access1.token).unwrap().is_some());
    }

    #[test]
    fn rotate_refresh_on_unknown_token_is_invalid() {
        let (_dir, store) = open_temp();
        assert_eq!(
            store.rotate_refresh("nope").unwrap(),
            RotateOutcome::Invalid
        );
    }

    #[test]
    fn rotate_refresh_on_an_access_token_is_invalid() {
        let (_dir, store) = open_temp();
        let (access, _refresh) = store.issue_token_pair("client1", "scope").unwrap();
        assert_eq!(
            store.rotate_refresh(&access.token).unwrap(),
            RotateOutcome::Invalid
        );
    }

    #[test]
    fn rotate_refresh_on_expired_refresh_is_invalid() {
        let (_dir, store) = open_temp();
        let (_access, refresh) = store.issue_token_pair("client1", "scope").unwrap();

        let mut tokens = store.load_tokens().unwrap();
        tokens.get_mut(&refresh.token).unwrap().expires = core::now_epoch_secs().saturating_sub(1);
        store.save_tokens(&tokens).unwrap();

        assert_eq!(
            store.rotate_refresh(&refresh.token).unwrap(),
            RotateOutcome::Invalid
        );
    }

    // ── Refresh rotation: reuse detection revokes the whole family ──────

    #[test]
    fn reuse_detection_revokes_whole_family_both_new_tokens_die() {
        let (_dir, store) = open_temp();
        let (access1, refresh1) = store.issue_token_pair("client1", "scope").unwrap();

        // Legitimate rotation #1.
        let (access2, refresh2) = match store.rotate_refresh(&refresh1.token).unwrap() {
            RotateOutcome::Rotated { access, refresh } => (access, refresh),
            other => panic!("expected Rotated, got {other:?}"),
        };
        assert!(store.check_access(&access2.token).unwrap().is_some());

        // Attacker (or the original client after losing a race) replays the
        // NOW-SPENT refresh1 token.
        let replay = store.rotate_refresh(&refresh1.token).unwrap();
        assert_eq!(replay, RotateOutcome::ReuseDetected);

        // The whole family is burned: the legitimately-issued access2/refresh2
        // — which were perfectly valid a moment ago — are now BOTH dead, not
        // just refresh1's direct descendants being blocked from further use.
        assert!(
            store.check_access(&access2.token).unwrap().is_none(),
            "access token from the legitimate rotation must die on reuse detection"
        );
        assert_eq!(
            store.rotate_refresh(&refresh2.token).unwrap(),
            RotateOutcome::Invalid,
            "refresh token from the legitimate rotation must also die on reuse detection"
        );
        // The original (pre-rotation) access token dies too — same family.
        assert!(store.check_access(&access1.token).unwrap().is_none());
    }

    #[test]
    fn reuse_detection_does_not_touch_a_different_family() {
        let (_dir, store) = open_temp();
        let (_a1, refresh1) = store.issue_token_pair("client1", "scope").unwrap();
        let (access_other, refresh_other) = store.issue_token_pair("client2", "scope").unwrap();

        // Rotate + replay to trigger reuse detection on family #1.
        let (_a2, refresh2) = match store.rotate_refresh(&refresh1.token).unwrap() {
            RotateOutcome::Rotated { access, refresh } => (access, refresh),
            other => panic!("expected Rotated, got {other:?}"),
        };
        let _ = refresh2;
        assert_eq!(
            store.rotate_refresh(&refresh1.token).unwrap(),
            RotateOutcome::ReuseDetected
        );

        // An entirely unrelated family is untouched.
        assert!(store.check_access(&access_other.token).unwrap().is_some());
        match store.rotate_refresh(&refresh_other.token).unwrap() {
            RotateOutcome::Rotated { .. } => {}
            other => panic!("unrelated family must be unaffected: {other:?}"),
        }
    }

    // ── Pairing ──────────────────────────────────────────────────────────

    #[test]
    fn pairing_code_is_created_on_first_call_and_stable_after() {
        let (_dir, store) = open_temp();
        let first = store.pairing_code().unwrap();
        let second = store.pairing_code().unwrap();
        assert_eq!(first, second, "repeated calls must not mint a new code");
        assert!(store.verify_pairing(&first).unwrap());
    }

    #[test]
    fn verify_pairing_rejects_wrong_code_and_absent_code() {
        let (_dir, store) = open_temp();
        // No pairing code minted yet.
        assert!(!store.verify_pairing("ANYX-CODE").unwrap());

        let code = store.pairing_code().unwrap();
        let wrong = if code.starts_with('A') {
            "BBBB-BBBB"
        } else {
            "AAAA-AAAA"
        };
        assert!(!store.verify_pairing(wrong).unwrap());
        assert!(store.verify_pairing(&code).unwrap());
    }

    #[test]
    fn rotate_pairing_code_invalidates_the_old_code() {
        let (_dir, store) = open_temp();
        let old = store.pairing_code().unwrap();
        let new = store.rotate_pairing_code().unwrap();
        assert_ne!(old, new);
        assert!(
            !store.verify_pairing(&old).unwrap(),
            "old pairing code must stop verifying"
        );
        assert!(store.verify_pairing(&new).unwrap());
    }

    // ── purge_expired ────────────────────────────────────────────────────

    #[test]
    fn purge_expired_drops_expired_codes_and_tokens_keeps_live_ones() {
        let (_dir, store) = open_temp();
        let live_code = store
            .issue_code("c1", "https://cb", "chal", "res", "scope")
            .unwrap();
        let (live_access, live_refresh) = store.issue_token_pair("c1", "scope").unwrap();

        // Force-expire one code and one token record directly.
        let mut codes = store.load_codes().unwrap();
        codes.insert(
            "expired".to_string(),
            AuthCode {
                code: "expired".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "https://cb".to_string(),
                code_challenge: "chal".to_string(),
                resource: "res".to_string(),
                scope: "scope".to_string(),
                expires: core::now_epoch_secs().saturating_sub(1),
                used: false,
            },
        );
        store.save_codes(&codes).unwrap();

        let mut tokens = store.load_tokens().unwrap();
        tokens.get_mut(&live_refresh.token).unwrap();
        tokens.insert(
            "expired-tok".to_string(),
            TokenRecord {
                token: "expired-tok".to_string(),
                kind: TokenKind::Access,
                family: "fam".to_string(),
                client_id: "c1".to_string(),
                scope: "scope".to_string(),
                expires: core::now_epoch_secs().saturating_sub(1),
                revoked: false,
                rotated_to: None,
            },
        );
        store.save_tokens(&tokens).unwrap();

        store.purge_expired().unwrap();

        let codes_after = store.load_codes().unwrap();
        assert!(!codes_after.contains_key("expired"));
        assert!(codes_after.contains_key(&live_code.code));

        let tokens_after = store.load_tokens().unwrap();
        assert!(!tokens_after.contains_key("expired-tok"));
        assert!(tokens_after.contains_key(&live_access.token));
        assert!(tokens_after.contains_key(&live_refresh.token));
    }

    // ── Corrupt file → error, not a silent default ──────────────────────

    #[test]
    fn corrupt_clients_file_is_an_error_not_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        let store = AuthStore::open_at(root.clone()).unwrap();
        std::fs::write(root.join("clients.json"), b"not json at all {{{").unwrap();
        let err = store.get_client("x").unwrap_err();
        assert!(
            format!("{err:#}").contains("clients.json"),
            "error should name the offending file: {err:#}"
        );
    }

    #[test]
    fn corrupt_codes_file_is_an_error_not_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        let store = AuthStore::open_at(root.clone()).unwrap();
        std::fs::write(root.join("codes.json"), b"{ broken").unwrap();
        assert!(store.consume_code("anything").is_err());
    }

    #[test]
    fn corrupt_tokens_file_is_an_error_not_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        let store = AuthStore::open_at(root.clone()).unwrap();
        std::fs::write(root.join("tokens.json"), b"[1,2,").unwrap();
        assert!(store.check_access("anything").is_err());
    }

    #[test]
    fn corrupt_pairing_file_is_an_error_not_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        let store = AuthStore::open_at(root.clone()).unwrap();
        std::fs::write(root.join("pairing.json"), b"\"unterminated").unwrap();
        assert!(store.verify_pairing("ANYX-CODE").is_err());
    }

    // ── Redacting Debug (requirement B, Task 2) ──────────────────────────

    #[test]
    fn token_record_debug_redacts_token_and_rotated_to_but_keeps_other_fields() {
        let rec = TokenRecord {
            token: "SUPER-SECRET-TOKEN-VALUE".to_string(),
            kind: TokenKind::Refresh,
            family: "fam-123".to_string(),
            client_id: "client-9".to_string(),
            scope: "brain".to_string(),
            expires: 42,
            revoked: false,
            rotated_to: Some("SUPER-SECRET-NEXT-TOKEN".to_string()),
        };
        let debug = format!("{rec:?}");
        assert!(
            !debug.contains("SUPER-SECRET-TOKEN-VALUE"),
            "token leaked into Debug output: {debug}"
        );
        assert!(
            !debug.contains("SUPER-SECRET-NEXT-TOKEN"),
            "rotated_to leaked into Debug output: {debug}"
        );
        // Non-secret fields must still be visible — this is a redaction, not
        // a blackout.
        assert!(debug.contains("fam-123"), "{debug}");
        assert!(debug.contains("client-9"), "{debug}");
        assert!(debug.contains("brain"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn auth_code_debug_redacts_code_but_keeps_other_fields() {
        let code = AuthCode {
            code: "SUPER-SECRET-AUTH-CODE".to_string(),
            client_id: "client-9".to_string(),
            redirect_uri: "https://cb.example/cb".to_string(),
            code_challenge: "not-actually-secret-challenge".to_string(),
            resource: "http://127.0.0.1:7717/mcp".to_string(),
            scope: "brain".to_string(),
            expires: 42,
            used: false,
        };
        let debug = format!("{code:?}");
        assert!(
            !debug.contains("SUPER-SECRET-AUTH-CODE"),
            "code leaked into Debug output: {debug}"
        );
        assert!(debug.contains("client-9"), "{debug}");
        assert!(debug.contains("not-actually-secret-challenge"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn pairing_state_debug_redacts_code_but_keeps_created() {
        let state = PairingState {
            code: "ABCD-2345".to_string(),
            created: 42,
        };
        let debug = format!("{state:?}");
        assert!(
            !debug.contains("ABCD-2345"),
            "pairing code leaked into Debug output: {debug}"
        );
        assert!(debug.contains('4'), "{debug}"); // `created: 42` still present
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    // ── Unix permission asserts ──────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn store_dir_and_files_are_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        let store = AuthStore::open_at(root.clone()).unwrap();

        // Materialize all four files.
        store.register_client(client("c1")).unwrap();
        store
            .issue_code("c1", "https://cb", "chal", "res", "scope")
            .unwrap();
        store.issue_token_pair("c1", "scope").unwrap();
        store.pairing_code().unwrap();

        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "gateway auth dir must be 0700, was {dir_mode:o}"
        );

        for name in ["clients.json", "codes.json", "tokens.json", "pairing.json"] {
            let mode = std::fs::metadata(root.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name} must be 0600, was {mode:o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn reopening_an_existing_looser_dir_reasserts_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _store = AuthStore::open_at(root.clone()).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "open_at must re-assert 0700 on a pre-existing looser dir"
        );
    }
}
