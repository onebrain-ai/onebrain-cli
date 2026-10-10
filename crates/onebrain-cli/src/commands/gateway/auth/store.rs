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
use std::time::Duration;

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
    /// The token `family` id minted when this code was successfully
    /// redeemed — `None` until then (and forever, if this code is never
    /// successfully redeemed at all). Stamped by [`AuthStore::exchange_code`]
    /// in the same `auth.lock` hold that spends the code and mints the pair,
    /// and read back by it when that SAME code is presented again — a replay
    /// of an already-`used` code (RFC 6749 §4.1.2 SHOULD) — to revoke
    /// everything that code ever produced. `#[serde(default)]` so an on-disk
    /// `codes.json` written before this field existed still deserializes
    /// (as `None`, the correct "nothing minted from this yet" value).
    #[serde(default)]
    pub minted_family: Option<String>,
}

/// Redacts `code` (the bearer secret redeemable at `/token`) — every other
/// field is either non-secret (`client_id`, `redirect_uri`, `resource`,
/// `scope`, `expires`, `used`) or, in `code_challenge`'s case, a PKCE S256
/// hash that is INTENDED to be sent openly in the `/authorize` URL (RFC 7636
/// — the secret is the verifier, which this store never persists), so it
/// stays visible for debugging. See [`TokenRecord`]'s `Debug` impl for the
/// full rationale (this exists so a later `{:?}` in a log path can't leak a
/// redeemable code). `minted_family` also stays visible — like
/// [`TokenRecord::family`], it's an internal correlation id, never accepted
/// anywhere as a credential.
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
            .field("minted_family", &self.minted_family)
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
    /// RFC 8707 resource this token was issued for (`{issuer}/mcp` today).
    /// `None` on tokens minted before #404 — `#[serde(default)]` keeps every
    /// existing `tokens.json` loading. Recorded, not yet enforced by
    /// `require_bearer` (one resource today; see the #404 plan's hub notes).
    #[serde(default)]
    pub resource: Option<String>,
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
            .field("resource", &self.resource)
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
/// on every request. `pub(crate)` (not private) so `/token`'s RFC 6749 §5.1
/// `expires_in` response field can reference this SAME constant directly
/// instead of duplicating the number and risking the two drifting apart.
///
/// ⚠ `TokenView::issued` (operator CLI, #406) is DERIVED as
/// `expires − <this TTL>` — no issue time is stored. Changing this value
/// makes every already-stored record of this kind report a wrong `issued`
/// in `onebrain gateway tokens list` (expiry and validity are unaffected).
pub(crate) const ACCESS_TTL_SECS: u64 = 60 * 60;
/// 30 days — refresh tokens are long-lived by design (that's the point of
/// having them); rotation + reuse detection is what keeps that safe.
///
/// ⚠ `TokenView::issued` (operator CLI, #406) is DERIVED as
/// `expires − <this TTL>` — no issue time is stored. Changing this value
/// makes every already-stored record of this kind report a wrong `issued`
/// in `onebrain gateway tokens list` (expiry and validity are unaffected).
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

/// Outcome of [`AuthStore::exchange_code`] — the whole RFC 6749 §4.1.3
/// authorization_code redemption, decided in ONE `auth.lock` hold.
#[derive(Debug, PartialEq, Eq)]
pub enum CodeExchange {
    /// The code was fresh, its bindings matched and its client is still
    /// registered: here is the pair minted for it (boxed, as in
    /// [`RotateOutcome::Rotated`]). The code is spent and links to the pair's
    /// family.
    Issued {
        access: Box<TokenRecord>,
        refresh: Box<TokenRecord>,
    },
    /// `invalid_grant`, whatever the cause: unknown or expired code; a
    /// replay of a spent code (whose minted family, if any, is now revoked);
    /// a binding/PKCE mismatch (the code is spent anyway); or a client that
    /// was removed (the code is spent, nothing minted).
    Invalid,
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

// ── Operator views (T2 / #406: `onebrain gateway tokens|clients`) ────────

/// Hex chars in a [`display_id`] (6 bytes of SHA-256).
pub(crate) const DISPLAY_ID_LEN: usize = 12;
/// Shortest id prefix `tokens revoke <id>` / `--family` accept.
pub(crate) const MIN_ID_PREFIX_LEN: usize = 4;

/// Stable, non-reversible operator-facing id for a secret-bearing string (a
/// token value or a family id): the first [`DISPLAY_ID_LEN`] lowercase hex
/// chars of `SHA-256(value)`. This is the ONLY form in which the operator CLI
/// ever names a token or a family. The raw values are bearer credentials (or,
/// for `family`, correlate them) and are never printed.
pub(crate) fn display_id(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(secret.as_bytes());
    let mut out = String::with_capacity(DISPLAY_ID_LEN);
    for &b in &digest[..DISPLAY_ID_LEN / 2] {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Normalize an operator-typed id prefix: trim, lowercase, and accept only
/// [`MIN_ID_PREFIX_LEN`]..=[`DISPLAY_ID_LEN`] hex chars. `None` for anything
/// else. That notably includes a pasted raw token (43 base64url chars), which
/// callers must reject WITHOUT echoing it back.
pub(crate) fn normalize_id_prefix(input: &str) -> Option<String> {
    let s = input.trim().to_ascii_lowercase();
    let ok = (MIN_ID_PREFIX_LEN..=DISPLAY_ID_LEN).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then_some(s)
}

/// Lifecycle state shown by `tokens list`. `Revoked` wins over `Expired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenStatus {
    Live,
    Expired,
    Revoked,
}

/// Secret-free projection of a [`TokenRecord`], and the ONLY token shape the
/// operator CLI ever handles. It deliberately has no field that could carry
/// `token` or `rotated_to`, so no rendering bug downstream can print a
/// credential. `issued` is derived as `expires − TTL(kind)`, which is exact
/// for every record this store mints (`expires = now + TTL`), because
/// `TokenRecord` stores no issue time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenView {
    pub id: String,
    pub kind: TokenKind,
    pub client_id: String,
    pub family_id: String,
    pub scope: String,
    pub issued: u64,
    pub expires: u64,
    pub status: TokenStatus,
    pub rotated: bool,
}

impl TokenView {
    fn from_record(rec: &TokenRecord, now: u64) -> TokenView {
        let ttl = match rec.kind {
            TokenKind::Access => ACCESS_TTL_SECS,
            TokenKind::Refresh => REFRESH_TTL_SECS,
        };
        let status = if rec.revoked {
            TokenStatus::Revoked
        } else if rec.expires <= now {
            TokenStatus::Expired
        } else {
            TokenStatus::Live
        };
        TokenView {
            id: display_id(&rec.token),
            kind: rec.kind,
            client_id: rec.client_id.clone(),
            family_id: display_id(&rec.family),
            scope: rec.scope.clone(),
            issued: rec.expires.saturating_sub(ttl),
            expires: rec.expires,
            status,
            rotated: rec.rotated_to.is_some(),
        }
    }
}

/// A registered client plus how many LIVE tokens it currently holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientView {
    pub client_id: String,
    pub client_name: Option<String>,
    pub application_type: AppType,
    pub redirect_uris: Vec<String>,
    pub created: u64,
    pub live_tokens: usize,
}

fn kind_rank(kind: TokenKind) -> u8 {
    match kind {
        TokenKind::Access => 0,
        TokenKind::Refresh => 1,
    }
}

/// Which tokens [`AuthStore::revoke_tokens`] targets. `Id`/`Family` carry a
/// prefix ALREADY normalized by [`normalize_id_prefix`]. `Client` is an exact
/// `client_id`. The CLI's clap `ArgGroup` guarantees exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSelector {
    Id(String),
    Client(String),
    Family(String),
}

impl TokenSelector {
    /// `"id" | "client" | "family"`: the JSON `selector` field and the noun
    /// used in operator messages.
    pub fn kind(&self) -> &'static str {
        match self {
            TokenSelector::Id(_) => "id",
            TokenSelector::Client(_) => "client",
            TokenSelector::Family(_) => "family",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            TokenSelector::Id(v) | TokenSelector::Client(v) | TokenSelector::Family(v) => v,
        }
    }
}

/// Result of [`AuthStore::revoke_tokens`]. Every id here is a [`display_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// Matched. `newly_revoked` (sorted) were flipped by THIS call, and
    /// `already_revoked` matched but were revoked before (idempotent re-run).
    Revoked {
        newly_revoked: Vec<String>,
        already_revoked: usize,
    },
    /// Nothing matched. Nothing was written.
    NotFound,
    /// The prefix matched more than one distinct id (sorted, deduped).
    /// Nothing was written.
    Ambiguous(Vec<String>),
}

/// Result of [`AuthStore::remove_client`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedClient {
    pub client_id: String,
    pub tokens_revoked: usize,
    pub codes_removed: usize,
}

/// `hits` = (display id, `tokens.json` key). Unique iff every hit shares ONE
/// display id. A family prefix legitimately matches several tokens of the
/// same family. Returns the keys, or `Err(Ambiguous)` naming the distinct ids.
fn resolve_unique(hits: Vec<(String, String)>) -> std::result::Result<Vec<String>, RevokeOutcome> {
    let mut ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
    ids.sort();
    ids.dedup();
    if ids.len() > 1 {
        return Err(RevokeOutcome::Ambiguous(ids));
    }
    Ok(hits.into_iter().map(|(_, key)| key).collect())
}

// ── Store ────────────────────────────────────────────────────────────────

/// Handle onto the four JSON files under `root` (normally
/// `~/.onebrain/gateway/`). Cheap to construct — holds only the root path;
/// every op re-reads its file fresh (no in-memory cache), so a change made by
/// another process (e.g. `onebrain gateway tokens revoke` while `gateway run`
/// is up) is seen on the very next call. Every read-modify-write op holds the
/// store-wide `auth.lock` (see [`Self::lock_exclusive`]) so two processes can
/// never lose each other's writes.
///
/// The gateway shares ONE `AuthStore` across threads with no in-process
/// mutex (#428): each `lock_exclusive` call opens its own handle on
/// `auth.lock`, so that lock serializes threads exactly as it serializes
/// processes (proven by `two_threads_sharing_one_store_serialize_on_auth_lock`).
pub struct AuthStore {
    root: PathBuf,
    /// How long [`Self::lock_exclusive`] waits before giving up with
    /// [`StoreBusy`]. [`LOCK_WAIT`] everywhere except unit tests.
    lock_wait: Duration,
}

/// Upper bound on one wait for `auth.lock` (#428). A CLI suspended with
/// Ctrl-Z while holding the lock must not stall `/token` forever.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Who holds `auth.lock`, as recorded in the `auth.lock.holder` sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockHolder {
    pub pid: u32,
    pub version: String,
}

/// `auth.lock` stayed held past [`LOCK_WAIT`]. Returned (inside
/// `anyhow::Error`) by every locked mutator; detect it with
/// [`is_store_busy`]. `holder` is `None` when the sidecar is missing or
/// unreadable (e.g. the holder predates v3.5.1, or is not `onebrain`).
///
/// Accepted risk: the sidecar is removed when the guard drops, so a holder
/// killed with SIGKILL leaves it behind. If the NEXT holder is pre-3.5.1 or
/// foreign (it never rewrites the sidecar), a waiter names that dead pid.
/// There is no liveness check.
#[derive(Debug)]
pub struct StoreBusy {
    pub holder: Option<LockHolder>,
}

impl std::fmt::Display for StoreBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = LOCK_WAIT.as_secs();
        match &self.holder {
            Some(h) => write!(
                f,
                "the gateway auth store is busy — onebrain {} (pid {}) has held auth.lock for over {secs}s",
                h.version, h.pid
            ),
            None => write!(
                f,
                "the gateway auth store is busy — another process has held auth.lock for over {secs}s"
            ),
        }
    }
}

impl std::error::Error for StoreBusy {}

/// The [`StoreBusy`] in `e`'s chain, if `auth.lock` was what failed.
pub fn store_busy(e: &anyhow::Error) -> Option<&StoreBusy> {
    e.downcast_ref::<StoreBusy>()
}

/// True when `e` is a [`StoreBusy`] (possibly under added context).
pub fn is_store_busy(e: &anyhow::Error) -> bool {
    store_busy(e).is_some()
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
        Ok(AuthStore {
            root,
            lock_wait: LOCK_WAIT,
        })
    }

    /// Test-only: a shorter [`LOCK_WAIT`] so busy-path tests don't sleep 5 s.
    #[cfg(test)]
    pub(crate) fn with_lock_wait(mut self, wait: Duration) -> AuthStore {
        self.lock_wait = wait;
        self
    }

    /// Take the store-wide advisory EXCLUSIVE lock (`<root>/auth.lock`,
    /// created 0600 on first use), waiting at most [`LOCK_WAIT`] (#428) and
    /// then failing with a typed [`StoreBusy`] that names the holder from
    /// the `auth.lock.holder` sidecar when it can. Blocking: async callers
    /// run it on `spawn_blocking`, never on a runtime worker. Every method
    /// that does load → modify → save holds this for its whole critical
    /// section, so a `onebrain gateway tokens revoke` in one process can
    /// never be lost to a concurrent `rotate_refresh_for_client`/
    /// `issue_token_pair_for_resource` in the running gateway (both used to
    /// read the same JSON, modify their copy, and atomically rename it back —
    /// last rename silently won).
    ///
    /// Advisory: only `AuthStore` honours it, which is all that matters
    /// because nothing else writes these files. Read-only methods do NOT take
    /// it: writers replace files by atomic tmp+rename, so a reader always
    /// sees one whole file. The lock is NOT re-entrant (a second handle on
    /// the lock file conflicts even in-process), so a locked method must
    /// never call another locked public method. It must use the private
    /// `load_*`/`save_*` helpers instead. Corollary: the delegating wrappers
    /// `issue_token_pair` / `rotate_refresh` take NO lock — only the inner
    /// `*_for_resource` / `*_for_client` bodies do.
    pub(crate) fn lock_exclusive(&self) -> Result<StoreLock> {
        // The gateway dir may have been deleted while `gateway run` is up;
        // recreate it (as `write_json_atomic` does) rather than failing every
        // mutator until a restart.
        ensure_private_dir(&self.root)?;
        let path = self.lock_path();
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&path)
            .with_context(|| format!("open gateway auth lock {}", path.display()))?;
        let deadline = std::time::Instant::now() + self.lock_wait;
        let mut pause = Duration::from_millis(5);
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return Err(StoreBusy {
                            holder: self.read_holder(),
                        }
                        .into());
                    }
                    std::thread::sleep(pause.min(deadline - now));
                    pause = (pause * 2).min(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e)
                        .with_context(|| format!("lock gateway auth store ({})", path.display()));
                }
            }
        }
        // The holder lives in a sidecar, not in `auth.lock` itself: a
        // Windows whole-file lock can stop other handles reading the locked
        // file. Best-effort — a failure only makes a waiter's message generic.
        let holder = self.holder_path();
        let record = LockHolder {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        if let Err(e) = serde_json::to_vec(&record)
            .map_err(anyhow::Error::from)
            .and_then(|b| std::fs::write(&holder, b).map_err(anyhow::Error::from))
        {
            tracing::debug!(error = %e, "could not write auth.lock.holder");
        }
        Ok(StoreLock {
            holder,
            _file: file,
        })
    }

    /// Best-effort read of the `auth.lock.holder` sidecar.
    fn read_holder(&self) -> Option<LockHolder> {
        let bytes = std::fs::read(self.holder_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
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
    fn lock_path(&self) -> PathBuf {
        self.root.join("auth.lock")
    }
    fn holder_path(&self) -> PathBuf {
        self.root.join("auth.lock.holder")
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
        let _guard = self.lock_exclusive()?;
        let mut clients = self.load_clients()?;
        clients.insert(client.client_id.clone(), client);
        self.save_clients(&clients)
    }

    /// Look up a registered client by id, or `None` if never registered.
    pub fn get_client(&self, client_id: &str) -> Result<Option<RegisteredClient>> {
        Ok(self.load_clients()?.remove(client_id))
    }

    /// Insert `client` unless that would push the store past `max`
    /// registrations: returns `Ok(false)` and writes nothing when
    /// `clients.len() >= max` AND the `client_id` is not already present.
    /// Overwriting an existing id always succeeds, matching
    /// [`Self::register_client`]. The count and the insert happen under one
    /// hold of the store lock, so a concurrent remove/register in another
    /// process cannot slip between them (`POST /register`'s
    /// `MAX_REGISTERED_CLIENTS` cap, #404 item 3).
    pub fn register_client_capped(&self, client: RegisteredClient, max: usize) -> Result<bool> {
        let _guard = self.lock_exclusive()?;
        let mut clients = self.load_clients()?;
        if clients.len() >= max && !clients.contains_key(&client.client_id) {
            return Ok(false);
        }
        clients.insert(client.client_id.clone(), client);
        self.save_clients(&clients)?;
        Ok(true)
    }

    /// Number of registered clients. A plain read — NOT a cap check (use
    /// [`Self::register_client_capped`], which counts and inserts under the
    /// store lock).
    pub fn client_count(&self) -> Result<usize> {
        Ok(self.load_clients()?.len())
    }

    // ── Authorization codes ─────────────────────────────────────────────

    /// Mint and persist a fresh, single-use auth code (>= 32 random bytes,
    /// [`AUTH_CODE_TTL_SECS`] lifetime) carrying the PKCE challenge + the
    /// rest of the `/authorize` request's parameters, to be redeemed once by
    /// [`Self::exchange_code`].
    pub fn issue_code(
        &self,
        client_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
        resource: &str,
        scope: &str,
    ) -> Result<AuthCode> {
        let _guard = self.lock_exclusive()?;
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
            minted_family: None,
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
        let _guard = self.lock_exclusive()?;
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

    /// Redeem `code` for a token pair (RFC 6749 §4.1.3) in ONE hold of
    /// `auth.lock`: consume → `bindings_ok` (client_id / redirect_uri /
    /// resource / PKCE, decided by the caller) → client still registered →
    /// mint the pair → link the code to the pair's family. A replay of an
    /// already-spent code revokes the family it minted (RFC 6749 §4.1.2
    /// SHOULD) in the same hold. That revoke is NOT best-effort any more: if
    /// it cannot be saved, the call returns `Err` (→ 500) rather than
    /// reporting a clean `invalid_grant` over a live family.
    ///
    /// One hold is what makes the replay hardening sound without an
    /// in-process mutex (#428 review): a replay can never run between the
    /// first redemption's consume and its family link, because both happen
    /// before the lock is released — so it always finds the family to
    /// revoke. It also means a busy lock fails BEFORE anything is spent: the
    /// client can retry with the same code.
    ///
    /// Write order is `codes.json` first (spent + linked), then
    /// `tokens.json`: a crash between the two leaves a spent code and no
    /// tokens (fail closed), never live tokens beside a redeemable code.
    pub fn exchange_code(
        &self,
        code: &str,
        bindings_ok: impl FnOnce(&AuthCode) -> bool,
    ) -> Result<CodeExchange> {
        let _guard = self.lock_exclusive()?;
        let mut codes = self.load_codes()?;
        let now = core::now_epoch_secs();
        let Some(entry) = codes.get_mut(code) else {
            return Ok(CodeExchange::Invalid);
        };
        if entry.used {
            if let Some(family) = entry.minted_family.clone() {
                self.revoke_family_locked(&family)?;
            }
            return Ok(CodeExchange::Invalid);
        }
        if entry.expires <= now {
            return Ok(CodeExchange::Invalid);
        }
        entry.used = true;
        let auth_code = entry.clone();
        #[cfg(test)]
        exchange_pause::fire();

        // A code is spent on presentation, not only on success — a wrong
        // verifier or a removed client still kills it.
        if !bindings_ok(&auth_code) || !self.load_clients()?.contains_key(&auth_code.client_id) {
            self.save_codes(&codes)?;
            return Ok(CodeExchange::Invalid);
        }

        let (access, refresh) = self.new_pair(
            &auth_code.client_id,
            &auth_code.scope,
            Some(&auth_code.resource),
        );
        if let Some(entry) = codes.get_mut(code) {
            entry.minted_family = Some(refresh.family.clone());
        }
        self.save_codes(&codes)?;
        let mut tokens = self.load_tokens()?;
        tokens.insert(access.token.clone(), access.clone());
        tokens.insert(refresh.token.clone(), refresh.clone());
        self.save_tokens(&tokens)?;
        Ok(CodeExchange::Issued {
            access: Box::new(access),
            refresh: Box::new(refresh),
        })
    }

    // ── Tokens ───────────────────────────────────────────────────────────

    /// Mint a fresh access+refresh pair with no bound `resource` — see
    /// [`Self::issue_token_pair_for_resource`]. Test fixture only: in
    /// production every pair comes from [`Self::exchange_code`] or
    /// [`Self::rotate_refresh_for_client`].
    #[cfg(test)]
    pub fn issue_token_pair(
        &self,
        client_id: &str,
        scope: &str,
    ) -> Result<(TokenRecord, TokenRecord)> {
        self.issue_token_pair_for_resource(client_id, scope, None)
    }

    /// Mint a fresh access+refresh pair (>= 32 random bytes each) sharing a
    /// new random `family` id, with [`ACCESS_TTL_SECS`]/[`REFRESH_TTL_SECS`]
    /// lifetimes and `resource` bound onto both (RFC 8707, #404). Persists
    /// both before returning them. Test fixture only (see
    /// [`Self::issue_token_pair`]).
    #[cfg(test)]
    pub fn issue_token_pair_for_resource(
        &self,
        client_id: &str,
        scope: &str,
        resource: Option<&str>,
    ) -> Result<(TokenRecord, TokenRecord)> {
        let _guard = self.lock_exclusive()?;
        let (access, refresh) = self.new_pair(client_id, scope, resource);
        let mut tokens = self.load_tokens()?;
        tokens.insert(access.token.clone(), access.clone());
        tokens.insert(refresh.token.clone(), refresh.clone());
        self.save_tokens(&tokens)?;
        Ok((access, refresh))
    }

    /// Build (not persist) a fresh access+refresh pair in a new family.
    fn new_pair(
        &self,
        client_id: &str,
        scope: &str,
        resource: Option<&str>,
    ) -> (TokenRecord, TokenRecord) {
        let family = core::mint_secret_32();
        let now = core::now_epoch_secs();
        let resource = resource.map(str::to_string);
        let access = TokenRecord {
            token: core::mint_secret_32(),
            kind: TokenKind::Access,
            family: family.clone(),
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            resource: resource.clone(),
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
            resource,
            expires: now + REFRESH_TTL_SECS,
            revoked: false,
            rotated_to: None,
        };
        (access, refresh)
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

    /// [`Self::rotate_refresh_for_client`] with no client check.
    pub fn rotate_refresh(&self, refresh: &str) -> Result<RotateOutcome> {
        self.rotate_refresh_for_client(refresh, None)
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
    /// - `client_id` is `Some` and differs from the token's →
    ///   [`RotateOutcome::Invalid`], WITHOUT spending the token (OAuth 2.1
    ///   §4.3.1). Checked AFTER reuse detection — a replayed, already-rotated
    ///   token still burns its family whatever client_id it arrives with —
    ///   and before any state change otherwise.
    pub fn rotate_refresh_for_client(
        &self,
        refresh: &str,
        client_id: Option<&str>,
    ) -> Result<RotateOutcome> {
        let _guard = self.lock_exclusive()?;
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

        if let Some(presented) = client_id {
            if presented != rec.client_id {
                return Ok(RotateOutcome::Invalid);
            }
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
            resource: rec.resource.clone(),
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
            resource: rec.resource.clone(),
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
        let _guard = self.lock_exclusive()?;
        let mut tokens = self.load_tokens()?;
        if let Some(rec) = tokens.get_mut(token) {
            rec.revoked = true;
            self.save_tokens(&tokens)?;
        }
        Ok(())
    }

    /// Revoke every token sharing `family` (idempotent: already-revoked
    /// tokens are left alone, and nothing is written back if `family`
    /// matches no token at all). This is the SAME "burn the whole family"
    /// action [`Self::rotate_refresh`]'s reuse-detection branch takes
    /// inline, and the one [`Self::exchange_code`]'s replay branch takes
    /// (via [`Self::revoke_family_locked`], inside its own lock hold).
    /// Test-only entry point to that locked body.
    #[cfg(test)]
    pub fn revoke_family(&self, family: &str) -> Result<()> {
        let _guard = self.lock_exclusive()?;
        self.revoke_family_locked(family)
    }

    /// [`Self::revoke_family`]'s body, for callers already holding the lock.
    fn revoke_family_locked(&self, family: &str) -> Result<()> {
        let mut tokens = self.load_tokens()?;
        let mut changed = false;
        for t in tokens.values_mut() {
            if t.family == family && !t.revoked {
                t.revoked = true;
                changed = true;
            }
        }
        if changed {
            self.save_tokens(&tokens)?;
        }
        Ok(())
    }

    // ── Pairing ──────────────────────────────────────────────────────────

    /// The current pairing code, minting one (and persisting it) on first
    /// call. Idempotent after that — repeated calls return the SAME code
    /// until [`Self::rotate_pairing_code`] replaces it.
    pub fn pairing_code(&self) -> Result<String> {
        let _guard = self.lock_exclusive()?;
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
        let _guard = self.lock_exclusive()?;
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
    /// collection, called once at `gateway::run()` startup right after
    /// `AuthStore::open()` and before the listener starts accepting
    /// connections (best-effort: a failure there is logged and does NOT
    /// abort startup); safe to call any time otherwise.
    ///
    /// **`codes.json` retention is NOT simply "past its own `expires`
    /// field."** A USED code that recorded a [`AuthCode::minted_family`] is a
    /// durable security artifact, not disposable state: the `/token`
    /// handler's RFC 6749 §4.1.2 replay hardening (inside
    /// [`Self::exchange_code`]) depends on that record still being on disk
    /// to catch a LATE replay of the code, and a refresh token from that
    /// family can legitimately still be presented for rotation up to
    /// [`REFRESH_TTL_SECS`] (30 days) after it was minted — far longer than
    /// the code's own 10-minute [`AUTH_CODE_TTL_SECS`]. If this sweep deleted
    /// a used, family-linked code the moment its OWN `expires` passed, replay
    /// hardening would silently degrade to only the ~10-minute window between
    /// issuance and expiry — with nothing (no test, no error) ever signaling
    /// that regression once this method gains a caller. So: a used code that
    /// minted a family is retained until `expires + REFRESH_TTL_SECS`, long
    /// enough to outlive every token that family could still be rotating. An
    /// unused code, or a used code that never actually minted a family (e.g.
    /// one redeemed by a request that failed its bindings/PKCE check before
    /// `issue_token_pair` ran), keeps the original behavior: purged the
    /// moment `expires` passes.
    ///
    /// `tokens.json` retention is unaffected by any of this — each token
    /// already carries its own correct TTL ([`ACCESS_TTL_SECS`] /
    /// [`REFRESH_TTL_SECS`]) directly in its `expires` field.
    ///
    /// Returns the total number of dropped records (codes + tokens) so the
    /// startup caller can log a debug line naming the count.
    pub fn purge_expired(&self) -> Result<usize> {
        let _guard = self.lock_exclusive()?;
        let now = core::now_epoch_secs();
        let mut dropped = 0usize;

        let mut codes = self.load_codes()?;
        let before = codes.len();
        codes.retain(|_, c| {
            if c.expires > now {
                return true;
            }
            // Naturally expired by its own TTL — but a used code that
            // minted a family stays around until that family's refresh
            // token could no longer legitimately be presented for
            // rotation. See the doc comment above.
            c.used && c.minted_family.is_some() && now <= c.expires.saturating_add(REFRESH_TTL_SECS)
        });
        dropped += before - codes.len();
        if codes.len() != before {
            self.save_codes(&codes)?;
        }

        let mut tokens = self.load_tokens()?;
        let before = tokens.len();
        tokens.retain(|_, t| t.expires > now);
        dropped += before - tokens.len();
        if tokens.len() != before {
            self.save_tokens(&tokens)?;
        }

        Ok(dropped)
    }

    // ── Operator views (T2 / #406) ───────────────────────────────────────

    /// Every token on disk as a secret-free [`TokenView`], sorted by
    /// `(client_id, family_id, kind, expires, id)`. Read-only, so it takes
    /// no lock (see [`Self::lock_exclusive`]).
    pub fn list_tokens(&self) -> Result<Vec<TokenView>> {
        let now = core::now_epoch_secs();
        let mut views: Vec<TokenView> = self
            .load_tokens()?
            .values()
            .map(|r| TokenView::from_record(r, now))
            .collect();
        views.sort_by(|a, b| {
            (
                &a.client_id,
                &a.family_id,
                kind_rank(a.kind),
                a.expires,
                &a.id,
            )
                .cmp(&(
                    &b.client_id,
                    &b.family_id,
                    kind_rank(b.kind),
                    b.expires,
                    &b.id,
                ))
        });
        Ok(views)
    }

    /// Every registered client (sorted by `client_id`) with its count of
    /// live tokens (`!revoked && expires > now`, the same predicate as
    /// [`Self::check_access`]). Read-only, so it takes no lock.
    pub fn list_clients(&self) -> Result<Vec<ClientView>> {
        let now = core::now_epoch_secs();
        let tokens = self.load_tokens()?;
        Ok(self
            .load_clients()?
            .into_values()
            .map(|c| {
                let live_tokens = tokens
                    .values()
                    .filter(|t| t.client_id == c.client_id && !t.revoked && t.expires > now)
                    .count();
                ClientView {
                    client_id: c.client_id,
                    client_name: c.client_name,
                    application_type: c.application_type,
                    redirect_uris: c.redirect_uris,
                    created: c.created,
                    live_tokens,
                }
            })
            .collect())
    }

    /// Revoke the tokens `selector` names (operator `tokens revoke`).
    /// Resolution happens INSIDE the store lock, so the set revoked is
    /// exactly the set resolved. Like [`Self::revoke_token`], `Id` does NOT
    /// cascade to the family. `Family`/`Client` are the bulk cut-offs.
    pub fn revoke_tokens(&self, selector: &TokenSelector) -> Result<RevokeOutcome> {
        debug_assert!(
            matches!(selector, TokenSelector::Client(_))
                || selector.value().len() >= MIN_ID_PREFIX_LEN,
            "Id/Family selectors must be normalized by normalize_id_prefix"
        );
        let _guard = self.lock_exclusive()?;
        let mut tokens = self.load_tokens()?;
        let keys: Vec<String> = match selector {
            TokenSelector::Client(client_id) => tokens
                .iter()
                .filter(|(_, t)| &t.client_id == client_id)
                .map(|(k, _)| k.clone())
                .collect(),
            TokenSelector::Id(prefix) => {
                let hits = tokens
                    .keys()
                    .map(|k| (display_id(k), k.clone()))
                    .filter(|(id, _)| id.starts_with(prefix.as_str()))
                    .collect();
                match resolve_unique(hits) {
                    Ok(keys) => keys,
                    Err(ambiguous) => return Ok(ambiguous),
                }
            }
            TokenSelector::Family(prefix) => {
                let hits = tokens
                    .iter()
                    .map(|(k, t)| (display_id(&t.family), k.clone()))
                    .filter(|(id, _)| id.starts_with(prefix.as_str()))
                    .collect();
                match resolve_unique(hits) {
                    Ok(keys) => keys,
                    Err(ambiguous) => return Ok(ambiguous),
                }
            }
        };
        if keys.is_empty() {
            return Ok(RevokeOutcome::NotFound);
        }
        let mut newly_revoked = Vec::new();
        let mut already_revoked = 0usize;
        for key in &keys {
            if let Some(t) = tokens.get_mut(key) {
                if t.revoked {
                    already_revoked += 1;
                } else {
                    t.revoked = true;
                    newly_revoked.push(display_id(key));
                }
            }
        }
        if !newly_revoked.is_empty() {
            self.save_tokens(&tokens)?;
        }
        newly_revoked.sort();
        Ok(RevokeOutcome::Revoked {
            newly_revoked,
            already_revoked,
        })
    }

    /// Remove `client_id`'s registration AND cut it off. Every token it holds
    /// is revoked (kept on disk, so `tokens list --all` still shows them until
    /// [`Self::purge_expired`] sweeps them). Every auth code issued to it is
    /// deleted. This only covers what exists when it runs: the `/authorize`
    /// and `/token` handlers re-check registration after minting a code or a
    /// pair, which closes the window where a mint races this call.
    /// `Ok(None)` (nothing written) if no such client is registered.
    ///
    /// Write order is tokens → codes → registration. A crash part-way leaves
    /// the client still LISTED (re-run `clients remove`), never a client that
    /// looks removed but still holds live tokens.
    pub fn remove_client(&self, client_id: &str) -> Result<Option<RemovedClient>> {
        let _guard = self.lock_exclusive()?;
        let mut clients = self.load_clients()?;
        if clients.remove(client_id).is_none() {
            return Ok(None);
        }

        let mut tokens = self.load_tokens()?;
        let mut tokens_revoked = 0usize;
        for t in tokens.values_mut() {
            if t.client_id == client_id && !t.revoked {
                t.revoked = true;
                tokens_revoked += 1;
            }
        }
        let mut codes = self.load_codes()?;
        let before = codes.len();
        codes.retain(|_, c| c.client_id != client_id);
        let codes_removed = before - codes.len();

        if tokens_revoked > 0 {
            self.save_tokens(&tokens)?;
        }
        if codes_removed > 0 {
            self.save_codes(&codes)?;
        }
        self.save_clients(&clients)?;
        Ok(Some(RemovedClient {
            client_id: client_id.to_string(),
            tokens_revoked,
            codes_removed,
        }))
    }
}

/// Read-only health probe for `doctor`: parse every store file WITHOUT
/// creating the directory (unlike [`AuthStore::open_at`]), taking `auth.lock`,
/// or writing anything.
pub(crate) fn check_files_parse(root: &Path) -> Result<()> {
    let store = AuthStore {
        root: root.to_path_buf(),
        lock_wait: LOCK_WAIT,
    };
    store.load_clients()?;
    store.load_codes()?;
    store.load_tokens()?;
    store.load_pairing()?;
    Ok(())
}

/// RAII guard returned by [`AuthStore::lock_exclusive`]. The OS releases the
/// lock when the file handle closes, i.e. when this guard drops. Bind it as
/// `let _guard = …` — `let _ = …` would drop (and unlock) immediately.
#[must_use = "the auth store lock is released as soon as this guard is dropped"]
pub(crate) struct StoreLock {
    holder: PathBuf,
    _file: std::fs::File,
}

impl Drop for StoreLock {
    /// Remove the holder sidecar BEFORE the lock is released (`_file` drops
    /// after this body), so it never names a process that no longer holds it.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.holder);
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
/// real path. Mirrors `daemon_client::DaemonInfo::write`, plus durability
/// (#429): the temp file is fsynced before the rename and, on unix, the
/// parent directory after it, so a power cut cannot lose a write the caller
/// was told succeeded. Windows has no directory fsync; there the rename's
/// own metadata durability is NTFS's.
///
/// Any failure after the temp file is opened (write, fsync, rename) removes
/// it. Once the rename has succeeded the write IS committed and visible, so a
/// failed directory fsync only warns: returning `Err` there would tell the
/// caller a visible write failed — e.g. a refresh whose `rotated_to` is
/// already saved would answer 500, and the client's retry would trip reuse
/// detection and burn the family.
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
        let written = write_fault::hit(write_fault::Stage::Write)
            .and_then(|()| f.write_all(&bytes))
            .with_context(|| format!("write {}", tmp.display()))
            .and_then(|()| {
                write_fault::hit(write_fault::Stage::SyncFile)
                    .and_then(|()| f.sync_all())
                    .with_context(|| format!("fsync {}", tmp.display()))
            });
        if let Err(e) = written {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(error = %e, path = %tmp.display(),
                "could not re-assert 0600 on gateway auth store file (may be readable)");
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()));
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Err(e) = write_fault::hit(write_fault::Stage::SyncDir)
            .and_then(|()| std::fs::File::open(parent))
            .and_then(|d| d.sync_all())
        {
            tracing::warn!(error = %e, path = %parent.display(),
                "gateway auth store write committed, but its directory fsync failed; \
                 the change may not survive a power cut");
        }
    }
    Ok(())
}

/// Fault injection for [`write_json_atomic`]'s I/O steps. Tests arm one
/// stage on their own thread; production builds compile to a no-op.
mod write_fault {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) enum Stage {
        Write,
        SyncFile,
        #[cfg_attr(not(unix), allow(dead_code))]
        SyncDir,
    }

    #[cfg(test)]
    thread_local! {
        static ARMED: std::cell::Cell<Option<Stage>> = const { std::cell::Cell::new(None) };
    }

    /// Fail the next `stage` on this thread (once).
    #[cfg(test)]
    pub(super) fn arm(stage: Stage) {
        ARMED.with(|a| a.set(Some(stage)));
    }

    #[cfg(test)]
    pub(super) fn hit(stage: Stage) -> std::io::Result<()> {
        if ARMED.with(|a| a.get()) == Some(stage) {
            ARMED.with(|a| a.set(None));
            return Err(std::io::Error::other("injected write fault"));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn hit(_stage: Stage) -> std::io::Result<()> {
        Ok(())
    }
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

/// Test-only pause point inside [`AuthStore::exchange_code`], right after
/// the code is marked spent and before the pair is minted — where the
/// pre-#428-review split released `auth.lock`. Thread-local, so only the
/// thread that armed it pauses.
#[cfg(test)]
pub(crate) mod exchange_pause {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(crate) fn arm(hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn fire() {
        if let Some(hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook();
        }
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

    #[test]
    fn client_count_tracks_registrations() {
        let (_dir, store) = open_temp();
        assert_eq!(store.client_count().unwrap(), 0);
        store.register_client(client("a")).unwrap();
        store.register_client(client("b")).unwrap();
        store.register_client(client("a")).unwrap(); // overwrite, not a new entry
        assert_eq!(store.client_count().unwrap(), 2);
    }

    #[test]
    fn check_files_parse_is_read_only_and_names_the_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("gateway");
        check_files_parse(&root).unwrap();
        assert!(!root.exists(), "a health check must not create the store");
        std::fs::create_dir_all(&root).unwrap();
        for file in ["clients.json", "codes.json", "tokens.json", "pairing.json"] {
            std::fs::write(root.join(file), b"{ broken").unwrap();
            assert!(
                format!("{:#}", check_files_parse(&root).unwrap_err()).contains(file),
                "{file}"
            );
            std::fs::remove_file(root.join(file)).unwrap();
        }
        assert!(
            !root.join("auth.lock").exists(),
            "a health check must not take the lock"
        );
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
            minted_family: None,
        };
        let mut codes = BTreeMap::new();
        codes.insert(expired.code.clone(), expired.clone());
        store.save_codes(&codes).unwrap();

        assert!(store.consume_code(&expired.code).unwrap().is_none());
    }

    // ── Auth-code replay hardening: minted_family linkage (Task 5) ──────

    #[test]
    fn revoke_family_kills_every_token_sharing_it_and_is_idempotent() {
        let (_dir, store) = open_temp();
        let (access, refresh) = store.issue_token_pair("client1", "scope").unwrap();
        assert!(store.check_access(&access.token).unwrap().is_some());

        store.revoke_family(&access.family).unwrap();
        assert!(store.check_access(&access.token).unwrap().is_none());
        assert_eq!(
            store.rotate_refresh(&refresh.token).unwrap(),
            RotateOutcome::Invalid,
            "the refresh half of the family must be dead too"
        );

        // Calling it again is a harmless no-op (nothing left to flip).
        store.revoke_family(&access.family).unwrap();
    }

    #[test]
    fn revoke_family_does_not_touch_an_unrelated_family() {
        let (_dir, store) = open_temp();
        let (access1, _refresh1) = store.issue_token_pair("client1", "scope").unwrap();
        let (access2, _refresh2) = store.issue_token_pair("client2", "scope").unwrap();

        store.revoke_family(&access1.family).unwrap();
        assert!(store.check_access(&access1.token).unwrap().is_none());
        assert!(
            store.check_access(&access2.token).unwrap().is_some(),
            "an unrelated family must be untouched"
        );
    }

    #[test]
    fn revoke_family_of_unknown_family_is_a_noop() {
        let (_dir, store) = open_temp();
        let (access, _refresh) = store.issue_token_pair("client1", "scope").unwrap();
        store.revoke_family("no-such-family").unwrap();
        assert!(
            store.check_access(&access.token).unwrap().is_some(),
            "an unrelated (nonexistent) family name must touch nothing"
        );
    }

    /// The replay-hardening flow end to end, sequentially: a redeemed code
    /// is linked to the family it minted; replaying it is `invalid_grant`
    /// AND revokes that family.
    #[test]
    fn a_replayed_code_revokes_the_family_it_minted() {
        let (_dir, store) = open_temp();
        store.register_client(client("client1")).unwrap();
        let issued = store
            .issue_code("client1", "https://cb", "chal", "res", "scope")
            .unwrap();
        assert!(issued.minted_family.is_none());

        let CodeExchange::Issued { access, refresh } =
            store.exchange_code(&issued.code, |_| true).unwrap()
        else {
            panic!("a fresh code must redeem");
        };
        let record = store.load_codes().unwrap()[&issued.code].clone();
        assert!(record.used);
        assert_eq!(
            record.minted_family.as_deref(),
            Some(access.family.as_str())
        );
        assert!(store.check_access(&access.token).unwrap().is_some());

        assert_eq!(
            store.exchange_code(&issued.code, |_| true).unwrap(),
            CodeExchange::Invalid
        );
        assert!(
            store.check_access(&access.token).unwrap().is_none(),
            "the access token minted from the replayed code must now be dead"
        );
        assert_eq!(
            store.rotate_refresh(&refresh.token).unwrap(),
            RotateOutcome::Invalid
        );
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
                minted_family: None,
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
                resource: None,
                expires: core::now_epoch_secs().saturating_sub(1),
                revoked: false,
                rotated_to: None,
            },
        );
        store.save_tokens(&tokens).unwrap();

        assert_eq!(
            store.purge_expired().unwrap(),
            2,
            "must report exactly the 1 expired code + 1 expired token it dropped"
        );

        let codes_after = store.load_codes().unwrap();
        assert!(!codes_after.contains_key("expired"));
        assert!(codes_after.contains_key(&live_code.code));

        let tokens_after = store.load_tokens().unwrap();
        assert!(!tokens_after.contains_key("expired-tok"));
        assert!(tokens_after.contains_key(&live_access.token));
        assert!(tokens_after.contains_key(&live_refresh.token));
    }

    /// Pins the fix for a code-review finding: `purge_expired` must NOT
    /// destroy the RFC 6749 §4.1.2 replay-revoke linkage
    /// ([`AuthCode::minted_family`]) the moment a USED code's own
    /// [`AUTH_CODE_TTL_SECS`] passes — the family it minted can still be
    /// rotating for up to [`REFRESH_TTL_SECS`] after that. Three codes,
    /// three different fates:
    /// - a used, family-linked code just past its own `expires` → SURVIVES
    ///   (equivalent to "a purge run at `expires + 1`").
    /// - the same shape, but far enough past `expires` that
    ///   `expires + REFRESH_TTL_SECS` has ALSO elapsed → purged
    ///   (equivalent to "a purge run at `expires + REFRESH_TTL_SECS + 1`").
    /// - an unused, naturally-expired code → purged immediately, exactly the
    ///   pre-fix behavior (unaffected by this change).
    #[test]
    fn purge_expired_retains_a_used_replay_linked_code_until_the_family_could_no_longer_rotate() {
        let (_dir, store) = open_temp();
        let now = core::now_epoch_secs();

        let survives = AuthCode {
            code: "used-with-family-just-expired".to_string(),
            client_id: "c1".to_string(),
            redirect_uri: "https://cb".to_string(),
            code_challenge: "chal".to_string(),
            resource: "res".to_string(),
            scope: "scope".to_string(),
            expires: now.saturating_sub(1),
            used: true,
            minted_family: Some("fam-1".to_string()),
        };
        let expired_for_good = AuthCode {
            code: "used-with-family-long-gone".to_string(),
            expires: now.saturating_sub(REFRESH_TTL_SECS + 1),
            ..survives.clone()
        };
        let unused_expired = AuthCode {
            code: "unused-expired".to_string(),
            used: false,
            minted_family: None,
            expires: now.saturating_sub(1),
            ..survives.clone()
        };

        let mut codes = BTreeMap::new();
        for c in [&survives, &expired_for_good, &unused_expired] {
            codes.insert(c.code.clone(), c.clone());
        }
        store.save_codes(&codes).unwrap();

        assert_eq!(
            store.purge_expired().unwrap(),
            2,
            "must report exactly the 2 purged codes (expired_for_good + unused_expired)"
        );

        let after = store.load_codes().unwrap();
        assert!(
            after.contains_key(&survives.code),
            "a used, family-linked code must survive a purge run just past its own expiry"
        );
        assert!(
            !after.contains_key(&expired_for_good.code),
            "a used, family-linked code must still be purged once expires + REFRESH_TTL_SECS has passed"
        );
        assert!(
            !after.contains_key(&unused_expired.code),
            "an unused expired code must be purged immediately, unaffected by this change"
        );
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

    // ── #404 folds: resource binding + refresh client binding ─────────────

    #[test]
    fn issue_token_pair_for_resource_binds_it_and_rotation_carries_it() {
        let (_dir, store) = open_temp();
        let res = "http://127.0.0.1:7717/mcp";
        let (access, refresh) = store
            .issue_token_pair_for_resource("client1", "brain", Some(res))
            .unwrap();
        assert_eq!(access.resource.as_deref(), Some(res));
        assert_eq!(refresh.resource.as_deref(), Some(res));
        let RotateOutcome::Rotated {
            access: a2,
            refresh: r2,
        } = store.rotate_refresh(&refresh.token).unwrap()
        else {
            panic!("expected Rotated");
        };
        assert_eq!(a2.resource.as_deref(), Some(res));
        assert_eq!(r2.resource.as_deref(), Some(res));
    }

    #[test]
    fn issue_token_pair_without_resource_leaves_it_unset() {
        let (_dir, store) = open_temp();
        let (access, refresh) = store.issue_token_pair("client1", "brain").unwrap();
        assert!(access.resource.is_none());
        assert!(refresh.resource.is_none());
    }

    /// Back-compat: a tokens.json written before `resource` existed (no key
    /// at all) still loads, and its tokens still validate.
    #[test]
    fn token_record_written_before_resource_existed_still_loads() {
        let (dir, store) = open_temp();
        let expires = core::now_epoch_secs() + 3600;
        let legacy = format!(
            r#"{{"legacy-tok":{{"token":"legacy-tok","kind":"access","family":"fam","client_id":"c1","scope":"brain","expires":{expires},"revoked":false,"rotated_to":null}}}}"#
        );
        std::fs::write(dir.path().join("gateway").join("tokens.json"), legacy).unwrap();
        let rec = store
            .check_access("legacy-tok")
            .unwrap()
            .expect("a legacy token must still validate");
        assert_eq!(rec.client_id, "c1");
        assert!(rec.resource.is_none());
    }

    #[test]
    fn rotate_refresh_for_client_rejects_another_client_without_spending_the_token() {
        let (_dir, store) = open_temp();
        let (_access, refresh) = store.issue_token_pair("client1", "scope").unwrap();
        assert_eq!(
            store
                .rotate_refresh_for_client(&refresh.token, Some("client2"))
                .unwrap(),
            RotateOutcome::Invalid
        );
        assert!(matches!(
            store
                .rotate_refresh_for_client(&refresh.token, Some("client1"))
                .unwrap(),
            RotateOutcome::Rotated { .. }
        ));
    }

    #[test]
    fn rotate_refresh_for_client_still_burns_the_family_on_reuse_whatever_client_id_is_sent() {
        let (_dir, store) = open_temp();
        let (_a1, refresh1) = store.issue_token_pair("client1", "scope").unwrap();
        let RotateOutcome::Rotated { access: a2, .. } =
            store.rotate_refresh(&refresh1.token).unwrap()
        else {
            panic!("expected Rotated");
        };
        assert_eq!(
            store
                .rotate_refresh_for_client(&refresh1.token, Some("client2"))
                .unwrap(),
            RotateOutcome::ReuseDetected
        );
        assert!(store.check_access(&a2.token).unwrap().is_none());
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
            resource: Some("http://127.0.0.1:7717/mcp".to_string()),
            expires: 42,
            revoked: false,
            rotated_to: Some("SUPER-SECRET-NEXT-TOKEN".to_string()),
        };
        let debug = format!("{rec:?}");
        assert!(
            !debug.contains("SUPER-SECRET-TOKEN-VALUE"),
            "token leaked into Debug output"
        );
        assert!(
            !debug.contains("SUPER-SECRET-NEXT-TOKEN"),
            "rotated_to leaked into Debug output"
        );
        // Non-secret fields must still be visible — this is a redaction, not
        // a blackout.
        assert!(debug.contains("fam-123"), "{debug}");
        assert!(debug.contains("client-9"), "{debug}");
        assert!(debug.contains("brain"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(
            debug.contains("http://127.0.0.1:7717/mcp"),
            "resource is non-secret and should stay visible: {debug}"
        );
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
            minted_family: Some("fam-77".to_string()),
        };
        let debug = format!("{code:?}");
        assert!(
            !debug.contains("SUPER-SECRET-AUTH-CODE"),
            "code leaked into Debug output"
        );
        assert!(debug.contains("client-9"), "{debug}");
        assert!(debug.contains("not-actually-secret-challenge"), "{debug}");
        assert!(
            debug.contains("fam-77"),
            "minted_family is a non-secret correlation id and must stay visible: {debug}"
        );
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
            "pairing code leaked into Debug output"
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

        for name in [
            "clients.json",
            "codes.json",
            "tokens.json",
            "pairing.json",
            "auth.lock",
        ] {
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

    // ── Cross-process advisory lock (T2 / #406) ─────────────────────────

    /// #428 review blocker: a replay that arrives while the first
    /// redemption is between "code spent" and "family linked" must still
    /// end with that family revoked. Thread A pauses at exactly that point
    /// ([`exchange_pause`]) and waits for replay B to finish (up to 500 ms).
    /// With the exchange in ONE lock hold, B cannot run inside the pause: it
    /// waits on `auth.lock`, then finds the linked family and revokes it.
    /// With the old split (consume, mint, link as separate lock holds), B
    /// runs inside the pause, finds no family yet, and A's pair survives.
    #[test]
    fn a_replay_racing_the_first_redemption_still_revokes_its_family() {
        let (_dir, store) = open_temp();
        store.register_client(client("c1")).unwrap();
        let code = store
            .issue_code("c1", "https://cb", "chal", "res", "brain")
            .unwrap()
            .code;
        let store = std::sync::Arc::new(store);
        let (consumed_tx, consumed_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let first = {
            let store = store.clone();
            let code = code.clone();
            std::thread::spawn(move || {
                exchange_pause::arm(move || {
                    consumed_tx.send(()).unwrap();
                    let _ = done_rx.recv_timeout(std::time::Duration::from_millis(500));
                });
                store.exchange_code(&code, |_| true).unwrap()
            })
        };
        consumed_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the first redemption never reached the pause point");
        let replay = store.exchange_code(&code, |_| true).unwrap();
        let _ = done_tx.send(());
        assert_eq!(replay, CodeExchange::Invalid);

        let CodeExchange::Issued { access, refresh } = first.join().unwrap() else {
            panic!("the first redemption must mint a pair");
        };
        assert!(
            store.check_access(&access.token).unwrap().is_none(),
            "the replay raced the first redemption and its pair survived"
        );
        assert_eq!(
            store.rotate_refresh(&refresh.token).unwrap(),
            RotateOutcome::Invalid,
            "the replayed code's refresh token must be dead too"
        );
    }

    #[test]
    fn exchange_code_spends_the_code_on_a_binding_mismatch_and_mints_nothing() {
        let (_dir, store) = open_temp();
        store.register_client(client("c1")).unwrap();
        let code = store
            .issue_code("c1", "https://cb", "chal", "res", "brain")
            .unwrap()
            .code;
        assert_eq!(
            store.exchange_code(&code, |_| false).unwrap(),
            CodeExchange::Invalid
        );
        assert!(store.load_tokens().unwrap().is_empty());
        assert_eq!(
            store.exchange_code(&code, |_| true).unwrap(),
            CodeExchange::Invalid,
            "a code that failed its bindings is spent"
        );
    }

    #[test]
    fn exchange_code_on_a_busy_store_spends_nothing() {
        let (dir, store) = open_temp();
        store.register_client(client("c1")).unwrap();
        let code = store
            .issue_code("c1", "https://cb", "chal", "res", "brain")
            .unwrap()
            .code;
        let busy = AuthStore::open_at(dir.path().join("gateway"))
            .unwrap()
            .with_lock_wait(std::time::Duration::from_millis(100));
        let guard = store.lock_exclusive().unwrap();
        assert!(is_store_busy(
            &busy.exchange_code(&code, |_| true).unwrap_err()
        ));
        drop(guard);
        assert!(
            matches!(
                store.exchange_code(&code, |_| true).unwrap(),
                CodeExchange::Issued { .. }
            ),
            "a busy lock must not burn the code; the retry succeeds"
        );
    }

    /// #428 premise: with no in-process `Mutex<AuthStore>`, the gateway's
    /// threads share ONE `AuthStore` and rely on `auth.lock` alone to
    /// serialize writers. That only holds because every `lock_exclusive`
    /// call opens its OWN file handle (flock is per open-file-description,
    /// `LockFileEx` per handle). Two threads, one shared store: the second
    /// lock must wait until the first guard drops.
    #[test]
    fn two_threads_sharing_one_store_serialize_on_auth_lock() {
        let (_dir, store) = open_temp();
        let store = std::sync::Arc::new(store);
        let first = store.lock_exclusive().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = store.clone();
        let handle = std::thread::spawn(move || {
            let guard = shared.lock_exclusive().unwrap();
            tx.send(()).unwrap();
            drop(guard);
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a second thread acquired auth.lock while the first still held it"
        );
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(4))
            .expect("the second thread must acquire auth.lock once it is released");
        handle.join().unwrap();
    }

    /// #428: a mutator gives up after the lock wait with a typed
    /// [`StoreBusy`] naming the holder from the sidecar, writes nothing,
    /// and the holder's sidecar disappears with its guard.
    #[test]
    fn a_mutator_gives_up_with_store_busy_naming_the_holder() {
        let (dir, store) = open_temp();
        let (access, _r) = store.issue_token_pair("c1", "brain").unwrap();
        let other = AuthStore::open_at(dir.path().join("gateway"))
            .unwrap()
            .with_lock_wait(std::time::Duration::from_millis(200));
        let guard = store.lock_exclusive().unwrap();

        let started = std::time::Instant::now();
        let err = other.revoke_token(&access.token).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let busy = store_busy(&err).expect("lock timeout must be a typed StoreBusy");
        assert_eq!(
            busy.holder,
            Some(LockHolder {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
        );
        assert!(
            err.to_string().contains(&std::process::id().to_string()),
            "{err}"
        );
        assert!(store.check_access(&access.token).unwrap().is_some());

        drop(guard);
        assert!(
            !store.root.join("auth.lock.holder").exists(),
            "the holder sidecar must go away with the guard"
        );
        other.revoke_token(&access.token).unwrap();
        assert!(store.check_access(&access.token).unwrap().is_none());
    }

    /// A holder that leaves no sidecar (an older CLI, a foreign process)
    /// still yields `StoreBusy`, with a generic message.
    #[test]
    fn store_busy_without_a_sidecar_is_generic() {
        let (dir, store) = open_temp();
        let other = AuthStore::open_at(dir.path().join("gateway"))
            .unwrap()
            .with_lock_wait(std::time::Duration::from_millis(100));
        let guard = store.lock_exclusive().unwrap();
        std::fs::remove_file(store.root.join("auth.lock.holder")).unwrap();
        let err = other.register_client(client("x")).unwrap_err();
        let busy = store_busy(&err).expect("typed StoreBusy");
        assert_eq!(busy.holder, None);
        assert!(err.to_string().contains("another process"), "{err}");
        drop(guard);
    }

    /// #429: a rename that fails (the target is a non-empty directory)
    /// must not leave the credential-bearing `.json.tmp` behind.
    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("tokens.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"x").unwrap();
        let err = write_json_atomic(&target, &BTreeMap::<String, String>::new());
        assert!(err.is_err(), "renaming over a non-empty dir must fail");
        assert!(
            !dir.path().join("tokens.json.tmp").exists(),
            "a failed rename left tokens.json.tmp behind"
        );
    }

    /// #429 review F2: a failure at ANY step after the temp file is opened
    /// removes it — not only a failed rename.
    #[test]
    fn a_failed_write_or_fsync_removes_the_temp_file() {
        for stage in [write_fault::Stage::Write, write_fault::Stage::SyncFile] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("tokens.json");
            write_fault::arm(stage);
            let err = write_json_atomic(&target, &BTreeMap::<String, String>::new());
            assert!(err.is_err(), "{stage:?}: the injected fault must surface");
            assert!(
                !dir.path().join("tokens.json.tmp").exists(),
                "{stage:?}: a failed write left tokens.json.tmp behind"
            );
            assert!(!target.exists(), "{stage:?}: nothing may be committed");
        }
    }

    /// #429 review F1: once the rename has committed the write, a failed
    /// directory fsync must NOT turn into `Err`. Shown on the path the
    /// reviewer hit: a refresh rotation whose `rotated_to` is already saved
    /// must report `Rotated`, or the client's retry trips reuse detection.
    #[cfg(unix)]
    #[test]
    fn a_failed_dir_fsync_after_a_committed_rename_is_not_an_error() {
        let (_dir, store) = open_temp();
        let (_access, refresh) = store.issue_token_pair("c1", "brain").unwrap();
        write_fault::arm(write_fault::Stage::SyncDir);
        let outcome = store
            .rotate_refresh(&refresh.token)
            .expect("a committed rotation must not report an error");
        let RotateOutcome::Rotated { refresh: next, .. } = outcome else {
            panic!("expected Rotated, got {outcome:?}");
        };
        assert!(matches!(
            store.rotate_refresh(&next.token).unwrap(),
            RotateOutcome::Rotated { .. }
        ));
    }

    /// A SECOND `AuthStore` handle on the same root stands in for a second
    /// process (the CLI vs. a running gateway): `flock`/`LockFileEx` locks
    /// conflict across distinct open file handles even inside one process,
    /// so two handles reproduce the cross-process race faithfully.
    /// Also covers the INNER token mutators directly (red-team blocker 1):
    /// `rotate_refresh_for_client` must wait on a lock held elsewhere.
    #[test]
    fn a_mutator_blocks_while_another_handle_holds_the_store_lock() {
        let (dir, store) = open_temp();
        let (access, _refresh) = store.issue_token_pair("c1", "brain").unwrap();
        let other = AuthStore::open_at(dir.path().join("gateway")).unwrap();

        let guard = store.lock_exclusive().unwrap();
        let token = access.token.clone();
        let handle = std::thread::spawn(move || other.revoke_token(&token).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "revoke_token must wait while another handle holds the store lock"
        );
        assert!(
            store.check_access(&access.token).unwrap().is_some(),
            "nothing may be written while another handle holds the lock"
        );
        drop(guard);
        handle.join().unwrap();
        assert!(
            store.check_access(&access.token).unwrap().is_none(),
            "the revoke must land once the lock is released"
        );

        let (_a2, refresh2) = store
            .issue_token_pair_for_resource("c1", "brain", None)
            .unwrap();
        let other = AuthStore::open_at(dir.path().join("gateway")).unwrap();
        let guard = store.lock_exclusive().unwrap();
        let handle = std::thread::spawn(move || {
            other
                .rotate_refresh_for_client(&refresh2.token, Some("c1"))
                .unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "rotate_refresh_for_client must wait while another handle holds the store lock"
        );
        drop(guard);
        assert!(matches!(
            handle.join().unwrap(),
            RotateOutcome::Rotated { .. }
        ));
    }

    #[test]
    fn a_mutator_recreates_a_deleted_gateway_dir() {
        let (_dir, store) = open_temp();
        std::fs::remove_dir_all(&store.root).unwrap();
        store
            .issue_token_pair("c1", "brain")
            .expect("mutator must heal a deleted gateway dir");
        assert!(store.root.join("auth.lock").exists());
    }

    /// The wrappers delegate to locked inner fns and must NOT lock
    /// themselves: the lock is not re-entrant, so a locked wrapper would
    /// block forever on its own inner call. A watchdog turns that hang into
    /// a failure instead of a stuck test run.
    #[test]
    fn token_wrappers_do_not_deadlock_on_the_inner_lock() {
        let (_dir, store) = open_temp();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (_a, r) = store.issue_token_pair("c1", "brain").unwrap();
            let outcome = store.rotate_refresh(&r.token).unwrap();
            let _ = tx.send(matches!(outcome, RotateOutcome::Rotated { .. }));
        });
        let rotated = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("issue_token_pair/rotate_refresh deadlocked on auth.lock");
        assert!(rotated);
    }

    /// The lost-update race #406 is about: one handle hammers the INNER
    /// `issue_token_pair_for_resource` (load → insert → save of the whole `tokens.json`)
    /// while another revokes tokens one by one. Without the lock, a revoke
    /// landing between the issuer's load and save is overwritten (last rename
    /// wins), and an issuer insert can likewise be lost to the revoker's
    /// save. Both effects are asserted.
    #[test]
    fn concurrent_mutators_on_two_handles_never_lose_a_revoke_or_an_insert() {
        const TARGETS: usize = 40;
        const ISSUES: usize = 150;
        let (dir, store) = open_temp();
        let targets: Vec<String> = (0..TARGETS)
            .map(|_| {
                store
                    .issue_token_pair_for_resource("victim", "brain", None)
                    .unwrap()
                    .0
                    .token
            })
            .collect();
        let root = dir.path().join("gateway");
        let issuer = AuthStore::open_at(root.clone()).unwrap();
        let revoker = AuthStore::open_at(root).unwrap();

        let a = std::thread::spawn(move || {
            for _ in 0..ISSUES {
                issuer
                    .issue_token_pair_for_resource("busy", "brain", None)
                    .unwrap();
            }
        });
        let to_revoke = targets.clone();
        let b = std::thread::spawn(move || {
            for t in &to_revoke {
                revoker.revoke_token(t).unwrap();
            }
        });
        a.join().unwrap();
        b.join().unwrap();

        let tokens = store.load_tokens().unwrap();
        assert_eq!(
            tokens.len(),
            TARGETS * 2 + ISSUES * 2,
            "an issue_token_pair_for_resource insert was lost to a concurrent write"
        );
        let lost = targets.iter().filter(|t| !tokens[*t].revoked).count();
        assert_eq!(
            lost, 0,
            "{lost} revoke(s) were lost to a concurrent read-modify-write"
        );
    }

    // ── register_client_capped (T2 / #406, hub Ruling 1) ────────────────

    #[test]
    fn register_client_capped_refuses_a_new_client_at_the_cap_and_writes_nothing() {
        let (_dir, store) = open_temp();
        store.register_client(client("a")).unwrap();
        store.register_client(client("b")).unwrap();
        assert!(!store.register_client_capped(client("c"), 2).unwrap());
        assert_eq!(store.client_count().unwrap(), 2);
        assert!(store.get_client("c").unwrap().is_none());
    }

    #[test]
    fn register_client_capped_still_overwrites_an_existing_id_at_the_cap() {
        let (_dir, store) = open_temp();
        store.register_client(client("a")).unwrap();
        store.register_client(client("b")).unwrap();
        let mut again = client("a");
        again.client_name = Some("renamed".to_string());
        assert!(store.register_client_capped(again, 2).unwrap());
        assert_eq!(store.client_count().unwrap(), 2);
        assert_eq!(
            store
                .get_client("a")
                .unwrap()
                .unwrap()
                .client_name
                .as_deref(),
            Some("renamed")
        );
    }

    #[test]
    fn register_client_capped_inserts_under_the_cap() {
        let (_dir, store) = open_temp();
        store.register_client(client("a")).unwrap();
        assert!(store.register_client_capped(client("b"), 2).unwrap());
        assert_eq!(store.client_count().unwrap(), 2);
    }

    #[test]
    fn register_client_capped_blocks_while_another_handle_holds_the_lock() {
        let (dir, store) = open_temp();
        let other = AuthStore::open_at(dir.path().join("gateway")).unwrap();
        let guard = store.lock_exclusive().unwrap();
        let handle =
            std::thread::spawn(move || other.register_client_capped(client("x"), 5).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "capped register must wait on the lock"
        );
        drop(guard);
        assert!(handle.join().unwrap());
    }

    // ── Operator views (T2 / #406) ───────────────────────────────────────

    fn live_record(token: &str, family: &str, client_id: &str) -> TokenRecord {
        TokenRecord {
            token: token.to_string(),
            kind: TokenKind::Access,
            family: family.to_string(),
            client_id: client_id.to_string(),
            scope: "brain".to_string(),
            resource: None,
            expires: core::now_epoch_secs() + 3600,
            revoked: false,
            rotated_to: None,
        }
    }

    fn plant(store: &AuthStore, records: &[TokenRecord]) {
        let mut tokens = store.load_tokens().unwrap();
        for r in records {
            tokens.insert(r.token.clone(), r.clone());
        }
        store.save_tokens(&tokens).unwrap();
    }

    #[test]
    fn display_id_is_12_lowercase_hex_stable_and_not_the_input() {
        let id = display_id("some-secret-token-value");
        assert_eq!(id.len(), DISPLAY_ID_LEN);
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(
            id,
            display_id("some-secret-token-value"),
            "must be deterministic"
        );
        assert_ne!(id, display_id("some-secret-token-valuf"));
        // Pinned vector: sha256("abc") = ba7816bf8f01…
        assert_eq!(display_id("abc"), "ba7816bf8f01");
    }

    #[test]
    fn normalize_id_prefix_accepts_4_to_12_hex_any_case_and_rejects_everything_else() {
        assert_eq!(normalize_id_prefix("ABCD").as_deref(), Some("abcd"));
        assert_eq!(normalize_id_prefix("  a1b2c3 ").as_deref(), Some("a1b2c3"));
        assert_eq!(
            normalize_id_prefix("0123456789ab").as_deref(),
            Some("0123456789ab")
        );
        assert_eq!(normalize_id_prefix("abc"), None, "too short");
        assert_eq!(normalize_id_prefix("0123456789abc"), None, "too long");
        assert_eq!(normalize_id_prefix("wxyz"), None, "not hex");
        assert_eq!(normalize_id_prefix(""), None);
        // A pasted raw token (43 base64url chars) must never be accepted.
        assert_eq!(normalize_id_prefix(&core::mint_secret_32()), None);
    }

    #[test]
    fn list_tokens_on_a_fresh_store_is_empty() {
        let (_dir, store) = open_temp();
        assert!(store.list_tokens().unwrap().is_empty());
        assert!(store.list_clients().unwrap().is_empty());
    }

    #[test]
    fn list_tokens_never_exposes_a_token_rotated_to_or_family_value() {
        let (_dir, store) = open_temp();
        let (_access, refresh) = store.issue_token_pair("c1", "brain").unwrap();
        let RotateOutcome::Rotated { .. } = store.rotate_refresh(&refresh.token).unwrap() else {
            panic!("rotation should succeed");
        };
        let raw = store.load_tokens().unwrap();
        let views = store.list_tokens().unwrap();
        assert_eq!(views.len(), 4);
        let json = serde_json::to_string(&views).unwrap();
        let debug = format!("{views:?}");
        for rec in raw.values() {
            let mut secrets = vec![rec.token.as_str(), rec.family.as_str()];
            if let Some(next) = &rec.rotated_to {
                secrets.push(next);
            }
            for s in secrets {
                assert!(
                    !json.contains(s),
                    "a raw secret leaked into list_tokens JSON"
                );
                assert!(
                    !debug.contains(s),
                    "a raw secret leaked into list_tokens Debug"
                );
            }
            assert!(views.iter().any(|v| v.id == display_id(&rec.token)));
        }
    }

    #[test]
    fn list_tokens_reports_status_issued_and_rotation() {
        let (_dir, store) = open_temp();
        let now = core::now_epoch_secs();
        let live = live_record("live-tok", "fam-a", "c1");
        let mut revoked = live_record("revoked-tok", "fam-a", "c1");
        revoked.revoked = true;
        revoked.expires = now.saturating_sub(5); // revoked wins over expired
        let mut expired = live_record("expired-tok", "fam-b", "c1");
        expired.expires = now.saturating_sub(1);
        let mut rotated = live_record("rotated-tok", "fam-b", "c2");
        rotated.kind = TokenKind::Refresh;
        rotated.revoked = true;
        rotated.rotated_to = Some("next-tok".to_string());
        plant(&store, &[live.clone(), revoked, expired, rotated]);

        let views = store.list_tokens().unwrap();
        let by = |t: &str| {
            views
                .iter()
                .find(|v| v.id == display_id(t))
                .unwrap()
                .clone()
        };
        assert_eq!(by("live-tok").status, TokenStatus::Live);
        assert_eq!(by("live-tok").issued, live.expires - ACCESS_TTL_SECS);
        assert_eq!(by("live-tok").family_id, display_id("fam-a"));
        assert_eq!(by("revoked-tok").status, TokenStatus::Revoked);
        assert_eq!(by("expired-tok").status, TokenStatus::Expired);
        let r = by("rotated-tok");
        assert!(r.rotated);
        assert_eq!(r.kind, TokenKind::Refresh);
        assert_eq!(r.issued, r.expires.saturating_sub(REFRESH_TTL_SECS));
        // Sorted by client_id first.
        assert_eq!(views.last().unwrap().client_id, "c2");
    }

    #[test]
    fn list_clients_counts_only_live_tokens_per_client() {
        let (_dir, store) = open_temp();
        store.register_client(client("c1")).unwrap();
        store.register_client(client("c2")).unwrap();
        store.issue_token_pair("c1", "brain").unwrap(); // 2 live
        let (a, _r) = store.issue_token_pair("c1", "brain").unwrap();
        store.revoke_token(&a.token).unwrap(); // 1 more live (its refresh)
        let clients = store.list_clients().unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].client_id, "c1");
        assert_eq!(clients[0].live_tokens, 3);
        assert_eq!(clients[1].client_id, "c2");
        assert_eq!(clients[1].live_tokens, 0);
    }

    /// Two distinct strings `"{tag}-{i}"` whose display ids share their first
    /// MIN_ID_PREFIX_LEN hex chars, plus that shared prefix. Deterministic
    /// (SHA-256) birthday search; pigeonhole guarantees a hit by 65 537.
    fn colliding_pair(tag: &str) -> (String, String, String) {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for i in 0..70_000u32 {
            let s = format!("{tag}-{i}");
            let p = display_id(&s)[..MIN_ID_PREFIX_LEN].to_string();
            if let Some(prev) = seen.get(&p) {
                return (prev.clone(), s, p);
            }
            seen.insert(p, s);
        }
        unreachable!("pigeonhole: 65 536 four-hex-char prefixes");
    }

    #[test]
    fn revoke_tokens_by_full_or_prefix_id_revokes_exactly_that_token() {
        let (_dir, store) = open_temp();
        let (a1, _r1) = store.issue_token_pair("c1", "brain").unwrap();
        let (a2, _r2) = store.issue_token_pair("c1", "brain").unwrap();
        let id = display_id(&a1.token);
        let out = store
            .revoke_tokens(&TokenSelector::Id(id[..6].to_string()))
            .unwrap();
        assert_eq!(
            out,
            RevokeOutcome::Revoked {
                newly_revoked: vec![id.clone()],
                already_revoked: 0
            }
        );
        assert!(store.check_access(&a1.token).unwrap().is_none());
        assert!(store.check_access(&a2.token).unwrap().is_some());
        // Idempotent: the same id again reports it as already revoked.
        assert_eq!(
            store.revoke_tokens(&TokenSelector::Id(id)).unwrap(),
            RevokeOutcome::Revoked {
                newly_revoked: vec![],
                already_revoked: 1
            }
        );
    }

    #[test]
    fn revoke_tokens_by_id_does_not_cascade_to_the_family() {
        let (_dir, store) = open_temp();
        let (access, refresh) = store.issue_token_pair("c1", "brain").unwrap();
        store
            .revoke_tokens(&TokenSelector::Id(display_id(&access.token)))
            .unwrap();
        match store.rotate_refresh(&refresh.token).unwrap() {
            RotateOutcome::Rotated { .. } => {}
            other => panic!("the sibling refresh token must still rotate: {other:?}"),
        }
    }

    #[test]
    fn revoke_tokens_with_an_ambiguous_id_prefix_changes_nothing() {
        let (_dir, store) = open_temp();
        let (t1, t2, prefix) = colliding_pair("tok");
        plant(
            &store,
            &[
                live_record(&t1, "fam-1", "c1"),
                live_record(&t2, "fam-2", "c1"),
            ],
        );
        let before = store.load_tokens().unwrap();
        match store.revoke_tokens(&TokenSelector::Id(prefix)).unwrap() {
            RevokeOutcome::Ambiguous(ids) => {
                let mut want = vec![display_id(&t1), display_id(&t2)];
                want.sort();
                assert_eq!(ids, want);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        assert_eq!(store.load_tokens().unwrap(), before, "nothing may change");
    }

    #[test]
    fn revoke_tokens_unknown_selector_is_not_found() {
        let (_dir, store) = open_temp();
        store.issue_token_pair("c1", "brain").unwrap();
        assert_eq!(
            store
                .revoke_tokens(&TokenSelector::Client("nobody".into()))
                .unwrap(),
            RevokeOutcome::NotFound
        );
        let (_d, empty) = open_temp();
        assert_eq!(
            empty
                .revoke_tokens(&TokenSelector::Id("abcd".into()))
                .unwrap(),
            RevokeOutcome::NotFound
        );
    }

    #[test]
    fn revoke_tokens_by_client_and_by_family_are_scoped() {
        let (_dir, store) = open_temp();
        let (c1a, c1r) = store.issue_token_pair("c1", "brain").unwrap();
        let (c2a, _c2r) = store.issue_token_pair("c2", "brain").unwrap();
        let (c2b, _) = store.issue_token_pair("c2", "brain").unwrap();

        // --family: one family (2 tokens) is NOT ambiguous even though the
        // prefix matches several tokens; they share one family id.
        let fam = display_id(&c1a.family);
        match store
            .revoke_tokens(&TokenSelector::Family(fam[..5].to_string()))
            .unwrap()
        {
            RevokeOutcome::Revoked {
                newly_revoked,
                already_revoked: 0,
            } => {
                let mut want = vec![display_id(&c1a.token), display_id(&c1r.token)];
                want.sort();
                assert_eq!(newly_revoked, want);
            }
            other => panic!("expected Revoked, got {other:?}"),
        }
        assert!(store.check_access(&c2a.token).unwrap().is_some());

        // --client: every token of c2, nothing else.
        match store
            .revoke_tokens(&TokenSelector::Client("c2".into()))
            .unwrap()
        {
            RevokeOutcome::Revoked { newly_revoked, .. } => assert_eq!(newly_revoked.len(), 4),
            other => panic!("expected Revoked, got {other:?}"),
        }
        assert!(store.check_access(&c2b.token).unwrap().is_none());
    }

    #[test]
    fn revoke_tokens_with_an_ambiguous_family_prefix_changes_nothing() {
        let (_dir, store) = open_temp();
        let (f1, f2, prefix) = colliding_pair("fam");
        plant(
            &store,
            &[
                live_record("tok-x", &f1, "c1"),
                live_record("tok-y", &f2, "c1"),
            ],
        );
        assert!(matches!(
            store.revoke_tokens(&TokenSelector::Family(prefix)).unwrap(),
            RevokeOutcome::Ambiguous(ids) if ids.len() == 2
        ));
        assert!(store.check_access("tok-x").unwrap().is_some());
    }

    #[test]
    fn revoke_tokens_waits_for_the_store_lock() {
        let (dir, store) = open_temp();
        let (access, _r) = store.issue_token_pair("c1", "brain").unwrap();
        let other = AuthStore::open_at(dir.path().join("gateway")).unwrap();
        let guard = store.lock_exclusive().unwrap();
        let sel = TokenSelector::Id(display_id(&access.token));
        let handle = std::thread::spawn(move || other.revoke_tokens(&sel).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "revoke_tokens must wait for the lock"
        );
        drop(guard);
        handle.join().unwrap();
        assert!(store.check_access(&access.token).unwrap().is_none());
    }

    #[test]
    fn remove_client_revokes_its_tokens_deletes_its_codes_and_unregisters_it() {
        let (_dir, store) = open_temp();
        store.register_client(client("c1")).unwrap();
        store.register_client(client("c2")).unwrap();
        let (a1, r1) = store.issue_token_pair("c1", "brain").unwrap();
        let (a2, _r2) = store.issue_token_pair("c2", "brain").unwrap();
        let pending = store
            .issue_code("c1", "https://cb", "chal", "res", "brain")
            .unwrap();
        let other_code = store
            .issue_code("c2", "https://cb", "chal", "res", "brain")
            .unwrap();

        let removed = store.remove_client("c1").unwrap().unwrap();
        assert_eq!(
            removed,
            RemovedClient {
                client_id: "c1".into(),
                tokens_revoked: 2,
                codes_removed: 1
            }
        );
        assert!(store.get_client("c1").unwrap().is_none());
        assert!(store.get_client("c2").unwrap().is_some());
        assert!(store.check_access(&a1.token).unwrap().is_none());
        assert_eq!(
            store.rotate_refresh(&r1.token).unwrap(),
            RotateOutcome::Invalid,
            "a removed client's refresh token must not mint a new pair"
        );
        assert!(store.consume_code(&pending.code).unwrap().is_none());
        assert!(store.check_access(&a2.token).unwrap().is_some());
        assert!(store.load_codes().unwrap().contains_key(&other_code.code));
    }

    #[test]
    fn remove_client_of_an_unknown_client_is_none_and_writes_nothing() {
        let (_dir, store) = open_temp();
        let (a, _r) = store.issue_token_pair("ghost", "brain").unwrap();
        assert!(store.remove_client("ghost").unwrap().is_none());
        assert!(
            store.check_access(&a.token).unwrap().is_some(),
            "an unregistered client id must not revoke anything"
        );
    }

    #[test]
    fn remove_client_waits_for_the_store_lock() {
        let (dir, store) = open_temp();
        store.register_client(client("c1")).unwrap();
        let other = AuthStore::open_at(dir.path().join("gateway")).unwrap();
        let guard = store.lock_exclusive().unwrap();
        let handle = std::thread::spawn(move || other.remove_client("c1").unwrap());
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "remove_client must wait for the lock"
        );
        drop(guard);
        assert!(handle.join().unwrap().is_some());
        assert!(store.get_client("c1").unwrap().is_none());
    }
}
