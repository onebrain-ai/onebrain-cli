//! Gateway policy engine (Gateway PR 4, Task 2): risk classification of tool
//! calls, per-class approval modes, TTL-bounded consent grants, and the
//! pack-scope check that closes the gap `auth::middleware::Principal`'s own
//! doc comment flags — `Principal.scope` is issued at `/authorize`, stored on
//! `TokenRecord`, returned by `check_access`, and then dropped on the floor:
//! nothing compared it to anything at dispatch. [`decide`] is that
//! comparison.
//!
//! [`decide`] is pure, synchronous logic with no I/O — [`Grants`] is the only
//! stateful piece, and it's a plain [`std::sync::Mutex`] around an in-memory
//! map (no persistence: every fresh `gateway run` starts with zero grants,
//! same as the spec's "session-scoped, not persisted" TTL-grant design).
//! `server.rs`'s tool handlers call `decide` synchronously (never across an
//! `.await`), so there is no "hold the mutex across an await" hazard here —
//! see `Grants::has`/`Grants::record`'s own doc comments.
//!
//! ## Scope-vs-pack: the seam for future packs
//!
//! Today there is exactly one capability pack (`"brain"` — see
//! `server::capability_packs`), and `/authorize` normalizes every issued
//! scope to exactly `"brain"` (`oauth_routes.rs`'s `resolve_scope`), so the
//! only reachable outcome of the scope check right now is a match. `decide`
//! still performs a REAL comparison — [`scope_covers_pack`], a token-set
//! membership test against [`CURRENT_PACK`] — rather than hardcoding `true`,
//! specifically so that adding a second pack later is a matter of widening
//! what `decide` compares against (thread a `pack: &str` derived from the
//! tool being called, in place of the fixed [`CURRENT_PACK`] constant), not
//! rediscovering that the scope check was never real. `scope_covers_pack`
//! already treats `scope` as an OAuth2-style space-separated SET of granted
//! tokens (`scope.split_whitespace().any(...)`), not a single string
//! compared with `==`, so a future `"brain files"` scope is handled
//! correctly the day a `files` pack ships — no change needed to the
//! membership test itself, only to what gets passed in as `pack`.
//!
//! ## Decision table
//!
//! `decide` checks scope-vs-pack FIRST — a mismatch is `Deny` regardless of
//! mode, including the otherwise-unconditional `auto` — then dispatches on
//! the [`PolicyMode`] configured for the call's [`RiskClass`]:
//!
//! | mode | grant | outcome |
//! |---|---|---|
//! | `auto` | (ignored) | `Allow` |
//! | `deny` | (ignored) | `Deny` |
//! | `ask_always` | (ignored) | `NeedApproval` — grants never satisfy `ask_always`, by design (it means "ask every time") |
//! | `ask_once` | none | `NeedApproval` |
//! | `ask_once` | live | `Allow` |
//! | `ask_once` | expired | `NeedApproval` |
//!
//! Every read-only tool (`capabilities`/`brain_tasks`/`brain_get`/
//! `brain_search`) only ever reaches `mode = auto` under the DEFAULT
//! `PolicyConfig` — the `ask_once`/`ask_always`/`deny` paths are exercised
//! here by this module's own unit tests, and against a customized
//! `gateway.yml` `policy:` block by `server.rs`'s own tests. `brain_capture`
//! (Gateway PR 4, Task 5) is the first `Mutating` tool, so it's also the
//! first to reach `mode = ask_once` under the DEFAULT config. When `decide`
//! returns `NeedApproval`, `server.rs`'s `await_approval` registers a
//! [`super::approval::PendingApproval`] and blocks on a human decision (the
//! native macOS dialog and/or the `/approvals` HTTP surface) — never a
//! silent allow, and never an unbounded hang (`PolicyConfig::approval_wait_seconds`
//! bounds the wait).

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::auth::core::now_epoch_secs;
use super::auth::Principal;

/// The only capability pack that exists today (see `server::capability_packs`).
/// [`decide`] compares [`Principal::scope`] against this constant — see the
/// module doc's "Scope-vs-pack" section for why that's a real membership
/// test and not a hardcoded `true`, and what changes when a second pack
/// ships.
const CURRENT_PACK: &str = "brain";

/// Risk classification of a gateway tool call — the axis [`PolicyConfig`]'s
/// three modes (`read_only`/`mutating`/`destructive`) key off of.
/// `capabilities`/`brain_tasks`/`brain_get`/`brain_search` are `ReadOnly`;
/// `brain_capture` (Task 5) is `Mutating`. No tool is `Destructive` yet —
/// the variant exists so `PolicyConfig` has a slot ready for one (e.g. a
/// future delete/overwrite tool) without another config-shape change.
///
/// `Serialize`s lowercase/snake_case (matching [`PolicyMode`]'s own
/// convention) — Gateway PR 4, Task 3 gave this its first outbound-JSON
/// caller: `super::approval::PendingApproval::class`, so an operator
/// reviewing `GET /approvals` can see exactly what class of access a
/// pending call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    ReadOnly,
    /// `brain_capture` (Gateway PR 4, Task 5) is the first — and, as of
    /// this task, only — tool classified `Mutating`.
    Mutating,
    /// No production tool is classified `Destructive` yet. Exercised by
    /// this module's own unit tests; not by any tool handler until a
    /// future task ships one.
    #[allow(dead_code)]
    Destructive,
}

/// How the gateway treats calls of one [`RiskClass`]. `Serialize`/
/// `Deserialize` as lowercase/snake_case (`auto`, `ask_once`, `ask_always`,
/// `deny`) so `gateway.yml`'s `policy:` block reads the same vocabulary this
/// module docs use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Always allow — no approval, no grant involved.
    Auto,
    /// Require approval once per `(client, RiskClass)` pair; a live grant
    /// (see [`Grants`]) satisfies subsequent calls until it expires.
    AskOnce,
    /// Always require approval, even with a live grant — "ask every time".
    AskAlways,
    /// Always refuse.
    Deny,
}

/// `gateway.yml`'s `policy:` block. Defaults (`auto` / `ask_once` /
/// `ask_always` / `30`) are chosen so a zero-config gateway keeps today's
/// read-only-tools-just-work behavior (`read_only: auto`) while any future
/// write tool defaults to SAFE — asked about at least once
/// (`mutating: ask_once`) and destructive actions asked about EVERY time
/// (`destructive: ask_always`), never silently auto-allowed by a config a
/// user never wrote.
///
/// Added to [`super::GatewayConfig`] as `#[serde(default)] pub policy:
/// PolicyConfig` — `GatewayConfig`'s own `Default` impl is hand-written (it
/// does not derive `Default`), so it constructs this via
/// [`PolicyConfig::default`] explicitly rather than relying on `derive` to
/// wire the two together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default = "default_read_only")]
    pub read_only: PolicyMode,
    #[serde(default = "default_mutating")]
    pub mutating: PolicyMode,
    #[serde(default = "default_destructive")]
    pub destructive: PolicyMode,
    #[serde(default = "default_grant_ttl_minutes")]
    pub grant_ttl_minutes: u64,
    /// How long a `NeedApproval` call blocks waiting for a human decision
    /// (`server::await_approval`, Gateway PR 4, Task 5) before giving up —
    /// deliberately a SEPARATE knob from `grant_ttl_minutes` above: that one
    /// governs how long an ALREADY-GRANTED consent lasts for FUTURE calls
    /// (minutes-to-hours scale), while this one bounds how long the
    /// CURRENT, still-synchronous MCP tool call waits for a first decision
    /// (seconds-to-minutes scale) — conflating the two into one field would
    /// make it impossible for an operator to want "ask me and wait up to 5
    /// minutes" independently of "then remember it for a day". Default 300s
    /// (5 minutes): long enough for a human to notice the native macOS
    /// dialog or the `/approvals` HTTP surface and respond, short enough
    /// that a client isn't left hanging indefinitely.
    #[serde(default = "default_approval_wait_seconds")]
    pub approval_wait_seconds: u64,
}

fn default_read_only() -> PolicyMode {
    PolicyMode::Auto
}
fn default_mutating() -> PolicyMode {
    PolicyMode::AskOnce
}
fn default_destructive() -> PolicyMode {
    PolicyMode::AskAlways
}
fn default_grant_ttl_minutes() -> u64 {
    30
}
fn default_approval_wait_seconds() -> u64 {
    300
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            read_only: default_read_only(),
            mutating: default_mutating(),
            destructive: default_destructive(),
            grant_ttl_minutes: default_grant_ttl_minutes(),
            approval_wait_seconds: default_approval_wait_seconds(),
        }
    }
}

impl PolicyConfig {
    /// The [`PolicyMode`] configured for one [`RiskClass`].
    fn mode_for(&self, class: RiskClass) -> PolicyMode {
        match class {
            RiskClass::ReadOnly => self.read_only,
            RiskClass::Mutating => self.mutating,
            RiskClass::Destructive => self.destructive,
        }
    }
}

/// Key identifying one `(client, risk class)` consent grant inside
/// [`Grants`]. Fields are deliberately private — build one via [`Self::new`]
/// (Task 5's approval flow is the one caller expected to construct these
/// outside this module, once it records a fresh grant after an `ask_once`
/// approval).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantKey {
    client_id: String,
    class: RiskClass,
}

impl GrantKey {
    pub fn new(client_id: impl Into<String>, class: RiskClass) -> Self {
        Self {
            client_id: client_id.into(),
            class,
        }
    }
}

/// In-memory, per-process, TTL-bounded consent grants: `(client, RiskClass)
/// -> expires-at (Unix epoch seconds)`. Never persisted — a gateway restart
/// clears every grant, which is the intended behavior (a grant is consent
/// for THIS running gateway, not a standing credential).
///
/// Both methods lock, do one `HashMap` operation, and drop the guard —
/// never held across an `.await` (there is no `.await` inside either body),
/// matching the boundary discipline `commands/mcp.rs::with_engine` and
/// `auth::middleware::require_bearer` both follow for their own
/// `std::sync::Mutex`-guarded state.
pub struct Grants {
    inner: Mutex<HashMap<GrantKey, u64>>,
}

impl Grants {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// `true` iff `key` has a grant recorded AND it has not yet expired.
    /// An expired entry is treated as absent (not cleaned up here — a stale
    /// entry costs a few bytes in the map and is harmlessly overwritten by
    /// the next [`Self::record`] for the same key; adding eviction now would
    /// be speculative complexity with no caller that needs it yet).
    pub fn has(&self, key: &GrantKey) -> bool {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.get(key)
            .is_some_and(|&expires| expires > now_epoch_secs())
    }

    /// Record (or replace) a grant for `key`, expiring `ttl_secs` from now.
    ///
    /// First given a real production caller by Gateway PR 4, Task 3:
    /// `approval_routes::resolve_approval` calls this on every
    /// `approval::Decision::Approve` resolution, using a config-derived TTL
    /// (`PolicyConfig::grant_ttl_minutes * 60`) rather than a test's
    /// hardcoded value — see that function's doc comment. `decide` only
    /// ever READS grants via [`Self::has`]; this is the only writer.
    ///
    /// Uses `saturating_add`, not a bare `+` (Task 2 review, binding
    /// requirement A) — now that a production caller can pass an
    /// operator-configured `ttl_secs`, the arithmetic must not be able to
    /// panic (debug builds) or silently wrap to a tiny/past expiry (release
    /// builds) on a pathological config value. Saturating to `u64::MAX`
    /// instead just means "this grant effectively never expires" — a safe,
    /// inert failure mode compared to either alternative.
    pub fn record(&self, key: GrantKey, ttl_secs: u64) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(key, now_epoch_secs().saturating_add(ttl_secs));
    }
}

impl Default for Grants {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a policy check for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// Proceed — no approval needed (or already granted).
    Allow,
    /// The call needs interactive approval before it may proceed.
    NeedApproval,
    /// The call is refused outright (policy `deny`, or a scope/pack
    /// mismatch).
    Deny,
}

/// Decide whether a tool call of risk class `class`, made by `principal`,
/// may proceed. See the module docs for the full decision table and the
/// scope-vs-pack rationale.
pub fn decide(
    cfg: &PolicyConfig,
    grants: &Grants,
    principal: &Principal,
    class: RiskClass,
) -> PolicyOutcome {
    if !scope_covers_pack(&principal.scope, CURRENT_PACK) {
        return PolicyOutcome::Deny;
    }
    match cfg.mode_for(class) {
        PolicyMode::Auto => PolicyOutcome::Allow,
        PolicyMode::Deny => PolicyOutcome::Deny,
        // Always ask, even with a live grant — a grant satisfies `ask_once`
        // (its entire point) but must never silently satisfy `ask_always`,
        // or "always" would be a lie.
        PolicyMode::AskAlways => PolicyOutcome::NeedApproval,
        PolicyMode::AskOnce => {
            let key = GrantKey::new(principal.client_id.clone(), class);
            if grants.has(&key) {
                PolicyOutcome::Allow
            } else {
                PolicyOutcome::NeedApproval
            }
        }
    }
}

/// `true` iff `pack` appears as one whitespace-separated token in `scope` —
/// the OAuth2 convention (RFC 6749 §3.3: scope is a space-delimited set of
/// case-sensitive strings), not a single-string `==` comparison. See the
/// module doc's "Scope-vs-pack" section.
fn scope_covers_pack(scope: &str, pack: &str) -> bool {
    scope.split_whitespace().any(|tok| tok == pack)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(scope: &str) -> Principal {
        Principal {
            client_id: "client-1".to_string(),
            scope: scope.to_string(),
        }
    }

    fn cfg_with(
        read_only: PolicyMode,
        mutating: PolicyMode,
        destructive: PolicyMode,
    ) -> PolicyConfig {
        PolicyConfig {
            read_only,
            mutating,
            destructive,
            grant_ttl_minutes: 30,
            approval_wait_seconds: 300,
        }
    }

    // ── PolicyConfig defaults ────────────────────────────────────────────

    #[test]
    fn default_policy_config_is_auto_ask_once_ask_always_30() {
        let cfg = PolicyConfig::default();
        assert_eq!(cfg.read_only, PolicyMode::Auto);
        assert_eq!(cfg.mutating, PolicyMode::AskOnce);
        assert_eq!(cfg.destructive, PolicyMode::AskAlways);
        assert_eq!(cfg.grant_ttl_minutes, 30);
        assert_eq!(cfg.approval_wait_seconds, 300);
    }

    #[test]
    fn policy_mode_serializes_lowercase_snake_case() {
        assert_eq!(
            serde_json::to_string(&PolicyMode::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&PolicyMode::AskOnce).unwrap(),
            "\"ask_once\""
        );
        assert_eq!(
            serde_json::to_string(&PolicyMode::AskAlways).unwrap(),
            "\"ask_always\""
        );
        assert_eq!(
            serde_json::to_string(&PolicyMode::Deny).unwrap(),
            "\"deny\""
        );
    }

    #[test]
    fn policy_config_parses_from_yaml_and_fills_missing_fields_with_defaults() {
        let full: PolicyConfig = serde_yaml::from_str(
            "read_only: deny\nmutating: auto\ndestructive: ask_once\ngrant_ttl_minutes: 5\n\
             approval_wait_seconds: 10\n",
        )
        .unwrap();
        assert_eq!(full.read_only, PolicyMode::Deny);
        assert_eq!(full.mutating, PolicyMode::Auto);
        assert_eq!(full.destructive, PolicyMode::AskOnce);
        assert_eq!(full.grant_ttl_minutes, 5);
        assert_eq!(full.approval_wait_seconds, 10);

        let sparse: PolicyConfig = serde_yaml::from_str("mutating: deny\n").unwrap();
        assert_eq!(
            sparse.read_only,
            PolicyMode::Auto,
            "missing field must default"
        );
        assert_eq!(sparse.mutating, PolicyMode::Deny);
        assert_eq!(sparse.destructive, PolicyMode::AskAlways);
        assert_eq!(sparse.grant_ttl_minutes, 30);
        assert_eq!(
            sparse.approval_wait_seconds, 300,
            "missing field must default"
        );
    }

    // ── Step 1: decide's table — mode × grant-present × scope-match ─────

    #[test]
    fn auto_allows_regardless_of_grant() {
        let cfg = cfg_with(PolicyMode::Auto, PolicyMode::Deny, PolicyMode::Deny);
        let grants = Grants::new();
        let p = principal("brain");
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::ReadOnly),
            PolicyOutcome::Allow,
            "auto must allow with no grant"
        );
        grants.record(GrantKey::new("client-1", RiskClass::ReadOnly), 3600);
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::ReadOnly),
            PolicyOutcome::Allow,
            "auto must allow even with a (irrelevant) live grant present"
        );
    }

    #[test]
    fn ask_once_with_no_grant_needs_approval() {
        let cfg = cfg_with(PolicyMode::Deny, PolicyMode::AskOnce, PolicyMode::Deny);
        let grants = Grants::new();
        let p = principal("brain");
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::Mutating),
            PolicyOutcome::NeedApproval
        );
    }

    #[test]
    fn ask_once_with_a_live_grant_allows() {
        let cfg = cfg_with(PolicyMode::Deny, PolicyMode::AskOnce, PolicyMode::Deny);
        let grants = Grants::new();
        let p = principal("brain");
        grants.record(GrantKey::new("client-1", RiskClass::Mutating), 3600);
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::Mutating),
            PolicyOutcome::Allow
        );
    }

    #[test]
    fn ask_once_with_an_expired_grant_needs_approval() {
        let cfg = cfg_with(PolicyMode::Deny, PolicyMode::AskOnce, PolicyMode::Deny);
        let grants = Grants::new();
        let p = principal("brain");
        // A "grant" that expired in the past (ttl wraps `now` backward by
        // recording it, then overwriting with an already-past expiry via a
        // second `record` at ttl_secs = 0 is not quite "expired" — 0 means
        // "expires now", and `has` requires `expires > now`, which is false
        // the instant it's recorded. That IS an expired grant for `has`'s
        // purposes, so ttl_secs: 0 is the direct way to construct one here.
        grants.record(GrantKey::new("client-1", RiskClass::Mutating), 0);
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::Mutating),
            PolicyOutcome::NeedApproval,
            "an expired grant must not satisfy ask_once"
        );
    }

    #[test]
    fn ask_always_needs_approval_even_with_a_live_grant() {
        let cfg = cfg_with(PolicyMode::Deny, PolicyMode::Deny, PolicyMode::AskAlways);
        let grants = Grants::new();
        let p = principal("brain");
        grants.record(GrantKey::new("client-1", RiskClass::Destructive), 3600);
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::Destructive),
            PolicyOutcome::NeedApproval,
            "ask_always must ignore a live grant — it means \"ask every time\""
        );
    }

    #[test]
    fn deny_mode_always_denies() {
        let cfg = cfg_with(PolicyMode::Deny, PolicyMode::Deny, PolicyMode::Deny);
        let grants = Grants::new();
        let p = principal("brain");
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::ReadOnly),
            PolicyOutcome::Deny
        );
        // Even with a grant present (grants are irrelevant to `deny`).
        grants.record(GrantKey::new("client-1", RiskClass::ReadOnly), 3600);
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::ReadOnly),
            PolicyOutcome::Deny
        );
    }

    #[test]
    fn scope_mismatch_denies_even_under_auto() {
        let cfg = PolicyConfig::default(); // read_only: auto
        let grants = Grants::new();
        let p = principal("other-pack");
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::ReadOnly),
            PolicyOutcome::Deny,
            "a scope that doesn't cover the \"brain\" pack must deny even under the most permissive mode"
        );
    }

    #[test]
    fn empty_scope_denies() {
        let cfg = PolicyConfig::default();
        let grants = Grants::new();
        let p = principal("");
        assert_eq!(
            decide(&cfg, &grants, &p, RiskClass::ReadOnly),
            PolicyOutcome::Deny
        );
    }

    #[test]
    fn scope_check_is_token_membership_not_exact_equality() {
        // Forward-compat proof for the "seam for future packs": a scope
        // carrying MULTIPLE space-separated tokens, one of which is
        // "brain", must still cover the "brain" pack — not just a scope
        // that is EXACTLY the string "brain".
        assert!(scope_covers_pack("brain files", "brain"));
        assert!(scope_covers_pack("files brain", "brain"));
        assert!(!scope_covers_pack("files", "brain"));
        assert!(!scope_covers_pack("brainstorm", "brain"));
    }

    // ── GrantKey / Grants ─────────────────────────────────────────────────

    #[test]
    fn grants_has_is_false_for_an_unrecorded_key() {
        let grants = Grants::new();
        assert!(!grants.has(&GrantKey::new("nobody", RiskClass::ReadOnly)));
    }

    #[test]
    fn grants_are_scoped_by_both_client_and_risk_class() {
        let grants = Grants::new();
        grants.record(GrantKey::new("client-a", RiskClass::Mutating), 3600);
        assert!(grants.has(&GrantKey::new("client-a", RiskClass::Mutating)));
        assert!(
            !grants.has(&GrantKey::new("client-b", RiskClass::Mutating)),
            "a grant must not leak to a different client"
        );
        assert!(
            !grants.has(&GrantKey::new("client-a", RiskClass::Destructive)),
            "a grant must not leak to a different risk class for the same client"
        );
    }

    #[test]
    fn recording_a_grant_again_replaces_its_expiry() {
        let grants = Grants::new();
        let key = GrantKey::new("client-a", RiskClass::Mutating);
        grants.record(key.clone(), 0); // expires immediately
        assert!(!grants.has(&key));
        grants.record(key.clone(), 3600); // re-grant, now live
        assert!(grants.has(&key));
    }

    /// Task 2 review, binding requirement A: `now_epoch_secs() + ttl_secs`
    /// (a bare `+`) would panic in a debug build the moment a caller passes
    /// a `ttl_secs` anywhere near `u64::MAX` — exactly the kind of value a
    /// pathological (or merely very large) `grant_ttl_minutes` config could
    /// produce once a real caller (Gateway PR 4, Task 3's
    /// `approval_routes::resolve_approval`) exists. `saturating_add` must
    /// instead clamp to `u64::MAX` — "never expires" — and the grant must
    /// still read as live.
    #[test]
    fn record_with_a_massive_ttl_saturates_instead_of_overflowing() {
        let grants = Grants::new();
        let key = GrantKey::new("client-1", RiskClass::Mutating);
        grants.record(key.clone(), u64::MAX);
        assert!(
            grants.has(&key),
            "a saturated (still enormous) expiry must still be live"
        );
    }
}
