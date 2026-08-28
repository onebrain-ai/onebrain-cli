//! `GatewayServer` — the MCP streamable-HTTP handler for `onebrain gateway
//! run` (Gateway PR 2). Mirrors `commands/mcp.rs`'s `#[tool_router]` /
//! `#[tool_handler]` structure, but serves the gateway's machine-level
//! `GatewayConfig` (multi-vault) over Streamable HTTP instead of one vault
//! over stdio.
//!
//! Protocol is PINNED to `2026-07-28` (SEP-2567 stateless streamable HTTP):
//! `get_info`'s `with_protocol_version` sets the negotiation FALLBACK for any
//! client-requested version rmcp doesn't recognise, and `build_gateway_router`
//! forces `legacy_session_mode(false)` — sessions don't exist in this design,
//! grants/TTL land in PR 4 as their replacement. A client that legitimately
//! requests an older KNOWN version (e.g. `2025-11-25`) still gets it echoed
//! back (`negotiate_protocol_version` in the vendored crate) — the pin only
//! changes the FALLBACK, not the negotiation.
//!
//! Gateway PR 2, Task 4 shipped four READ-ONLY tools: `capabilities`
//! (self-description), `brain_tasks` (open task listing, reusing
//! `task_list.rs`'s scan/filter composition verbatim), `brain_get`
//! (traversal-guarded single-file read), and `brain_search` (daemon-routed
//! hybrid search — see the `brain_search` doc comment for why it never
//! falls back to a direct engine). Gateway PR 4, Task 5 added the first
//! WRITE tool, `brain_capture` (create-safe-guarded inbox note creation,
//! gated by [`policy_gate`]'s now-wired `NeedApproval` arm — see
//! [`await_approval`]'s doc comment for the full approval flow).
//!
//! `build_gateway_router`/`GatewayServer::new` are called from `onebrain
//! gateway run` (Task 4, `gateway/mod.rs::run`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};

use onebrain_core::{
    load_vault_config, require_vault, CoreError, ResolvedVault, VaultResolveInputs,
};
use onebrain_fs::task::{visit_tasks, TaskHit, TaskScanOptions};

use crate::commands::daemon_client;
use crate::commands::gateway::approval::{self, Approvals, PendingApproval, WaitOutcome};
use crate::commands::gateway::approval_native;
use crate::commands::gateway::approval_routes::approval_router;
use crate::commands::gateway::audit::{AuditEntry, AuditLog, Decision, Outcome};
use crate::commands::gateway::auth::core::{mint_secret_32, now_epoch_secs};
use crate::commands::gateway::auth::middleware::require_bearer;
use crate::commands::gateway::auth::Principal;
use crate::commands::gateway::oauth_routes::{
    authorize_router, register_router, token_router, well_known_router, AuthCtx,
};
use crate::commands::gateway::policy::{
    self, GrantKey, Grants, PolicyMode, PolicyOutcome, RiskClass,
};
use crate::commands::gateway::GatewayConfig;
use crate::commands::task_list::{resolve_due_by, resolve_prefixes, TaskCollector};

/// Machine-level gateway state shared across every request. Sessionless
/// (§ above), so this is the only state a tool call can read — no per-session
/// data exists.
///
/// `grants`/`audit`/`approvals` are per-PROCESS: a fresh `gateway run`
/// always starts with zero grants and zero pending approvals (see
/// [`Grants`]'s and [`Approvals`]'s own doc comments — both are
/// session-scoped, never persisted) and opens the audit log fresh
/// (append-only — [`GatewayState::new`] takes an already-opened
/// [`AuditLog`] rather than opening it itself, so tests can point it at a
/// tempdir instead of the real `~/.onebrain/gateway/audit/`).
pub struct GatewayState {
    pub config: GatewayConfig,
    pub grants: Grants,
    pub audit: AuditLog,
    /// Pending human approvals (Gateway PR 4, Task 3). Reached three ways:
    /// the operator-facing `/approvals` HTTP surface (`approval_routes.rs`)
    /// reads/writes it via `Arc<GatewayState>`; [`policy_gate`]'s
    /// `NeedApproval` arm (`await_approval`, Gateway PR 4, Task 5) registers
    /// and waits on it directly off `&Arc<GatewayState>`; and that same
    /// wiring hands [`approval_native::prompt`] an owned
    /// `Arc<Approvals>` (`state.approvals.clone()`) — `prompt` spawns a
    /// `tokio::task::spawn_blocking` closure that outlives the tool call and
    /// so needs a `'static` handle onto `Approvals` alone, not the whole
    /// `GatewayState`. Wrapping `Approvals` itself in an `Arc` here (rather
    /// than changing `prompt`'s already-correct signature) is the
    /// reconciliation Task 5's brief calls out by name: the minimal thing
    /// that gives every caller an owned handle when it needs one, while
    /// every OTHER caller (`Approvals`'s own methods all take `&self`) keeps
    /// working unchanged through `Deref`.
    pub approvals: Arc<Approvals>,
}

impl GatewayState {
    pub fn new(config: GatewayConfig, audit: AuditLog) -> Self {
        Self {
            config,
            grants: Grants::new(),
            audit,
            approvals: Arc::new(Approvals::new()),
        }
    }
}

#[derive(Clone)]
pub struct GatewayServer {
    state: Arc<GatewayState>,
    tool_router: ToolRouter<Self>,
}

/// Output of the `capabilities` tool.
///
/// Gateway PR 4, Task 6 widened this from a bare pack/tool-name listing to a
/// TRUTHFUL description of what calling each tool would actually do right
/// now: [`PackInfo::tools`] carries each tool's [`RiskClass`] and the
/// EFFECTIVE [`PolicyMode`] the live `gateway.yml` resolves it to (see
/// [`ToolInfo`]), and [`approval_channels`] reports which channels can
/// actually deliver an interactive approval prompt on THIS running gateway
/// process — the binding requirement being that a caller must never be told
/// a write CAN be approved when no channel exists to carry that prompt to a
/// human.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CapabilitiesOut {
    pub gateway_version: String,
    pub protocol_version: String,
    pub packs: Vec<PackInfo>,
    /// `config.vaults` keys.
    pub vaults: Vec<String>,
    /// Name of the vault serving `vault`-omitted calls, when resolvable to a
    /// `config.vaults` entry; otherwise the fixed marker `"(configured)"` —
    /// NEVER the raw configured path, which would leak a host filesystem
    /// path to the client. `None` when no `default_vault` is configured (a
    /// call would then fall through to the env/walk-up chain — see
    /// [`resolve_vault_arg`]).
    pub default_vault: Option<String>,
    /// Which channels can deliver an interactive approval prompt to a human
    /// on THIS running gateway process — see [`ApprovalChannels`] and
    /// [`approval_channels`].
    pub approval_channels: ApprovalChannels,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct PackInfo {
    pub name: String,
    pub enabled: bool,
    pub tools: Vec<ToolInfo>,
    pub note: String,
}

/// One tool's entry inside [`PackInfo::tools`] — its [`RiskClass`] (the same
/// classification every `#[tool]` handler passes to [`policy_gate`]) and the
/// EFFECTIVE [`PolicyMode`] that class resolves to under the gateway's live
/// `gateway.yml` (via [`policy::PolicyConfig::mode_for`] — the identical
/// lookup [`policy::decide`] itself performs, so this can never drift from
/// what actually happens on the next real call). First given a real caller
/// by Gateway PR 4, Task 6's [`brain_pack_tools`]/[`capability_packs`] —
/// before this task, `capabilities` reported only a bare tool-name list with
/// no way to tell whether calling one would need approval at all.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ToolInfo {
    pub name: String,
    pub risk_class: RiskClass,
    pub policy_mode: PolicyMode,
}

/// Which channels can actually resolve a [`policy::PolicyOutcome::NeedApproval`]
/// call on THIS running gateway process (Gateway PR 4, Task 6) — see
/// [`approval_channels`]'s doc comment for exactly how each field is
/// determined and why `http` is unconditionally `true` in this build while
/// `native`/`telegram` are not.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ApprovalChannels {
    pub native: bool,
    pub http: bool,
    pub telegram: bool,
    /// Human-readable caveats a caller needs to correctly interpret
    /// `native`/`http`/`telegram` above — e.g. why `telegram` is always
    /// `false` today. Never a raw host detail (matches every other
    /// client-facing field in this struct/file).
    pub note: String,
}

/// Params for the `brain_tasks` tool.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BrainTasksParams {
    /// Named vault from the gateway config; omit for the default vault.
    pub vault: Option<String>,
    /// "today" or YYYY-MM-DD; omit for no due-date cutoff.
    pub due_by: Option<String>,
    /// Max tasks returned (default 20). `total` always reflects the full filtered count.
    pub limit: Option<usize>,
}

/// Output of the `brain_tasks` tool.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct BrainTasksOut {
    pub tasks: Vec<GatewayTaskHit>,
    pub total: usize,
    pub vault: String,
}

/// Mirrors `onebrain_fs::task::TaskHit` (which has no `JsonSchema` derive) for
/// the tool's structured output schema.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct GatewayTaskHit {
    pub file: String,
    pub line: u32,
    pub text: String,
    pub done: bool,
    pub due: Option<String>,
}

impl From<TaskHit> for GatewayTaskHit {
    fn from(hit: TaskHit) -> Self {
        Self {
            file: hit.file,
            line: hit.line,
            text: hit.text,
            done: hit.done,
            due: hit.due,
        }
    }
}

/// Params for the `brain_get` tool.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BrainGetParams {
    /// Vault-relative path to the note to read.
    pub file: String,
    /// Named vault from the gateway config; omit for the default vault.
    pub vault: Option<String>,
}

/// Params for the `brain_search` tool.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BrainSearchParams {
    /// Search text.
    pub query: String,
    /// Named vault from the gateway config; omit for the default vault.
    pub vault: Option<String>,
    /// Max hits returned; omit for the daemon's own default.
    pub top_k: Option<usize>,
}

/// Params for the `brain_capture` tool — the gateway's first WRITE tool.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BrainCaptureParams {
    /// Note body/content to capture.
    pub text: String,
    /// Optional title. Drives the derived filename's slug; when omitted (or
    /// when it has no usable alphanumeric content at all) the slug is
    /// derived from the first words of `text` instead. Any script works —
    /// a Thai, Japanese, or Cyrillic title yields a Thai, Japanese, or
    /// Cyrillic filename, not a fallback.
    pub title: Option<String>,
    /// Named vault from the gateway config; omit for the default vault.
    pub vault: Option<String>,
}

/// Output of the `brain_capture` tool.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct BrainCaptureOut {
    /// Vault-relative path of the newly created note.
    pub path: String,
}

/// Resolves a vault-relative path to an absolute, canonicalized path that is
/// guaranteed to live under `vault_root` — the traversal guard for
/// `brain_get`. Canonicalizing both sides (not just comparing the joined
/// path textually) means `..` segments AND symlinks that point outside the
/// vault are both caught: `starts_with` runs against the fully resolved
/// target, so a symlink inside the vault pointing at `/etc/passwd` resolves
/// to `/etc/passwd` before the check and is rejected exactly like a literal
/// `../etc/passwd` would be.
///
/// Replicated — not called — from `commands/mcp.rs::resolve_under_vault`
/// (`crates/onebrain-cli/src/commands/mcp.rs:107-124`) per the Task 3 brief:
/// that function is a plain (non-`pub`) `fn` private to the `mcp` module, so
/// it isn't reachable from `gateway::server` (a sibling module under
/// `crate::commands::gateway`, not `crate::commands::mcp`). Kept logically
/// identical to the source; only this doc comment differs.
fn resolve_under_vault(vault_root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let root = vault_root
        .canonicalize()
        .context("canonicalize vault root")?;
    // Absolute `rel` inputs (e.g. `/etc/passwd`) are rejected too: `Path::join`
    // with an absolute path discards `root` entirely and returns the absolute
    // path unchanged, so the `starts_with(&root)` check below still catches it
    // (the canonicalized absolute path won't start with the vault root) — but
    // only for paths outside the vault; an absolute path that happens to fall
    // *inside* the vault would incorrectly pass, which is why callers should
    // always pass vault-relative paths, not attacker-controlled absolute ones.
    let joined = root.join(rel);
    let canon = joined
        .canonicalize()
        .with_context(|| format!("not found: {rel}"))?;
    anyhow::ensure!(canon.starts_with(&root), "path escapes the vault: {rel}");
    Ok(canon)
}

/// Create-safe confinement check for `brain_capture` (Gateway PR 4, Task 5)
/// — the sibling [`resolve_under_vault`] itself calls for rather than being
/// reused verbatim: that function canonicalizes the TARGET, which only
/// works once the target already exists, and `brain_capture` is about to
/// CREATE one. Recon for this task confirmed `onebrain_fs::note::new_note`
/// (and its siblings `append_note`/`write_note`) do a bare
/// `vault_root.join(rel_path)` with ZERO confinement of their own — an
/// absolute `rel_path` silently discards `vault_root` entirely
/// (`Path::join`'s documented behavior) and writes wherever it points. This
/// function is the only thing standing between a crafted `title`/derived
/// slug and a write outside the vault.
///
/// Two independent layers, so a bug in either alone still leaves the other
/// standing:
///
/// 1. **Syntactic, before any filesystem call**: every component of `rel`
///    must be a plain [`std::path::Component::Normal`] segment. This
///    rejects an absolute path outright (its first component is a
///    `RootDir`/`Prefix`, never `Normal`) and rejects any `.`/`..` segment
///    — so `../../etc/cron.d/x` and `/etc/x` are both refused before
///    `create_dir_all` ever touches a filesystem, regardless of how deep
///    the escape attempt is buried. In `brain_capture`'s real call path this
///    check is never actually the thing that fires — [`derive_slug`]'s
///    alphanumerics-only sanitization already strips every `/`, `\` and `.`
///    before a path is ever built (see [`sanitize_slug`]'s own doc comment
///    for why that still holds now that the kept charset is Unicode
///    alphanumerics rather than ASCII ones) — but it stands on its own as a
///    second, independent line of defense against a hypothetical future
///    caller (or a bug in that sanitization) passing an unsanitized `rel`
///    straight through. Widening that charset neither weakened this layer
///    nor was allowed to lean on it: the two are independent by design.
/// 2. **Canonicalization, once the syntax is known-safe**: the PARENT
///    directory of `rel` (never `rel` itself — the whole reason this
///    function exists instead of reusing [`resolve_under_vault`]) is
///    created if it doesn't exist yet, then canonicalized, and asserted to
///    still live under the canonicalized vault root — the same
///    `starts_with` check [`resolve_under_vault`] uses, catching a
///    SYMLINKED parent (e.g. the inbox folder itself replaced by a symlink
///    pointing outside the vault) even though layer 1 already ruled out a
///    textual `..` escape.
/// 3. **Transfer to the actual write**: `new_note`/`append_note` do NOT
///    take the confined path — they take `(vault_root, rel)` and join them
///    with zero confinement of their own. So the returned path is finally
///    asserted to equal `canonical_root.join(rel)`: the write's own join,
///    with only the root resolved. When those differ, some component of
///    `rel` was rewritten by symlink resolution — the guard proved a
///    DIFFERENT path from the one the write will open, so its proof does
///    not transfer and the call is refused. (Concretely: an inbox that is
///    a symlink to another directory *inside* the same vault passes layer 2
///    but is refused here. That is deliberate — fail closed rather than
///    write through a link the guard did not actually vouch for.)
///
/// Returns that path: it is the exact file the subsequent write will
/// create, so `capture_note` uses it directly for its same-day collision
/// check rather than re-deriving one.
///
/// **What this does NOT cover, stated plainly:** the parent is canonicalized
/// BEFORE the write, so a parent swapped for an escaping symlink in the
/// window between this check and `new_note`'s `create_dir_all`/write is not
/// caught. Closing that needs `openat`-style handle-relative I/O in
/// `onebrain-fs`, which this layer cannot reach; it is tracked as fs-layer
/// hardening. The window requires an attacker who already has write access
/// inside the vault, which is a strictly larger capability than anything
/// this gateway grants.
fn resolve_create_under_vault(vault_root: &Path, rel: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        rel.components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "derived note path is not a plain vault-relative path: {}",
        rel.display()
    );
    let file_name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow::anyhow!("derived note path has no file name: {}", rel.display()))?;

    let root = vault_root
        .canonicalize()
        .context("canonicalize vault root")?;
    let rel_parent = rel.parent().unwrap_or_else(|| Path::new(""));
    let parent_abs = root.join(rel_parent);
    std::fs::create_dir_all(&parent_abs)
        .with_context(|| format!("create parent directory for {}", rel.display()))?;
    let parent_canon = parent_abs
        .canonicalize()
        .with_context(|| format!("canonicalize parent directory for {}", rel.display()))?;
    anyhow::ensure!(
        parent_canon.starts_with(&root),
        "path escapes the vault: {}",
        rel.display()
    );

    // Layer 3 — see the doc comment. `new_note`/`append_note` will open
    // `vault_root.join(rel)`; this is that same join with the root resolved.
    // Making it an ENFORCED equality rather than an inference is what lets
    // the caller hand those functions the raw `rel` and still know the
    // guard's proof applies to the file they will actually touch.
    let confined = parent_canon.join(file_name);
    anyhow::ensure!(
        confined == root.join(rel),
        "derived note path does not resolve to its own vault-relative location: {}",
        rel.display()
    );
    Ok(confined)
}

/// Env var that, when set to any NON-EMPTY value, disables `brain_capture`'s
/// best-effort daemon reindex — the SECOND half of the pair
/// [`approval_native::DISABLE_NATIVE_APPROVAL_ENV`] opened, and it exists
/// for the same reason: to switch off, FROM OUTSIDE THE PROCESS, a side
/// effect that is fine in production and unacceptable in a test.
///
/// The side effect here is a real subprocess. `capture_note`'s reindex step
/// calls `daemon_client::ensure_running`, which — when no warm daemon is
/// already up — spawns `onebrain daemon start`, leaving a genuine `onebrain
/// daemon __run` process alive on the machine long after the test that
/// caused it has finished. That was observed for real, not theorized:
/// `tests/gateway_approval_e2e.rs` left one behind on a developer machine
/// before this switch existed. A test binary must not leave background
/// processes running on a developer's box or on CI.
///
/// Why not the pre-existing `ONEBRAIN_NO_DAEMON`: that one gates
/// `search_common::route_to_daemon`, the CLI's PASSIVE "route to a daemon
/// that already exists" path. It never reaches `ensure_running`, the ACTIVE
/// spawn-if-absent path this call site (and `brain_search`) uses, so setting
/// it changes nothing here.
///
/// Scope, deliberately: this covers ONLY the BEST-EFFORT reindex. It does
/// not touch `brain_search`, whose `ensure_running` call is load-bearing —
/// disabling that would break the tool rather than degrade it, and no test
/// wants a `brain_search` that silently returns nothing. `tests/gateway_http.rs`
/// deliberately drives a real daemon for exactly that reason.
///
/// Same presence-switch semantics as its sibling (`super::env_switch_on`):
/// any non-empty value is ON, a set-but-empty value counts as unset.
pub const DISABLE_DAEMON_REINDEX_ENV: &str = "ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX";

/// `true` iff `capture_note` may dispatch its best-effort daemon reindex.
///
/// Two independent off-switches, matching the shape `await_approval` already
/// uses for the native dialog channel (`!cfg!(test) && is_available()`):
///
/// - `cfg!(test)` covers THIS crate's own unit tests. Compile-time, so it is
///   `false` in the shipped binary and cannot be turned back on by accident.
/// - [`DISABLE_DAEMON_REINDEX_ENV`] covers everything `cfg!(test)` cannot:
///   a separately compiled integration-test binary that spawns the real
///   `onebrain` executable (`tests/gateway_approval_e2e.rs`), where this
///   crate's own `cfg(test)` is irrelevant because the gateway is a
///   different, non-test process.
///
/// Read fresh on every call, never cached — same discipline as
/// `approval_native::is_available`, so a test that sets the var mid-process
/// observes it immediately.
fn reindex_channel_enabled() -> bool {
    !cfg!(test) && !super::env_switch_on(DISABLE_DAEMON_REINDEX_ENV)
}

/// The client-facing same-day collision message for `brain_capture`.
///
/// Written here rather than forwarding `onebrain_fs::note::new_note`'s own
/// `InvalidTarget` text, which reads "file exists: <path> (use --force)" —
/// `--force` is a CLI flag, and an MCP client has no CLI and no way to pass
/// one, so that sentence tells a caller to do something impossible. `rel` is
/// vault-RELATIVE (never a host path), so naming it is safe and is the one
/// genuinely useful detail: it tells the caller which note already holds
/// today's slug.
fn capture_collision_message(rel: &Path) -> String {
    format!(
        "capture failed: a note already exists at {} — captures are one note per title per day, \
         so use a different title",
        rel.display()
    )
}

/// Max characters kept in [`derive_slug`]'s output — bounds the derived note
/// filename's length regardless of how long a caller's `title`/`text` is.
/// Comfortably under any filesystem's per-component limit even after the
/// `YYYY-MM-DD-` prefix and `.md` suffix are added back.
const MAX_SLUG_CHARS: usize = 60;

/// Max BYTES kept in [`derive_slug`]'s output, applied ALONGSIDE
/// [`MAX_SLUG_CHARS`] — whichever bites first wins.
///
/// A character cap alone is not a length bound on a UTF-8 filename:
/// filesystem name limits are counted in bytes (255 on APFS, ext4, and
/// NTFS's UTF-8 equivalent), and one `char` can be up to four of them. At
/// [`MAX_SLUG_CHARS`] four-byte characters the component would reach 254
/// bytes once `YYYY-MM-DD-` and `.md` are added back — right on the edge of
/// `ENAMETOOLONG` for input the caller controls. This cap keeps the worst
/// case at 134 bytes instead, with no effect at all on ASCII titles (60
/// chars is 60 bytes, well under it).
const MAX_SLUG_BYTES: usize = 120;

/// Fallback slug [`derive_slug`] falls back to when NEITHER `title` nor
/// `text` yields any alphanumeric content to build one from (e.g. an
/// empty or punctuation-only title with empty or punctuation-only text, or
/// one made entirely of emoji) — so `brain_capture` always has a valid
/// filename to write, never an error, no matter how degenerate its input.
///
/// Never used bare: [`derive_slug`] appends
/// [`FALLBACK_DISAMBIGUATOR_CHARS`] random characters to it, so two
/// fallback captures on the same day get different filenames instead of the
/// second one failing on a same-day collision. See [`derive_slug`].
const FALLBACK_SLUG: &str = "capture";

/// How many random characters [`derive_slug`] appends to [`FALLBACK_SLUG`].
/// Six lowercase base32-ish characters is ~10^9 combinations — far more than
/// enough to keep a day's fallback captures apart, and short enough that the
/// filename still reads as a fallback rather than as a hash.
const FALLBACK_DISAMBIGUATOR_CHARS: usize = 6;

/// How many leading characters of `text` [`derive_slug`] considers when no
/// usable `title` is given — bounds the cost of scanning an arbitrarily
/// long `text` before it's even sanitized (sanitization itself is O(n), but
/// there's no reason to scan more of `text` than could ever survive
/// [`MAX_SLUG_CHARS`] truncation anyway).
const TEXT_SLUG_SOURCE_CHARS: usize = 80;

/// Sanitize `raw` into a slug: every alphanumeric character is lowercased
/// and kept, and every run of one-or-more anything else — punctuation,
/// whitespace, `/`, `\`, `.`, control characters, Unicode format characters
/// — collapses to a single `-`. Leading and trailing `-` are trimmed.
///
/// **Unicode alphanumerics, not just ASCII.** An earlier revision kept only
/// `char::is_ascii_alphanumeric`, which made a Thai, Japanese, Korean, or
/// Cyrillic title sanitize to the EMPTY string — so every such capture on a
/// given day derived the identical `YYYY-MM-DD-capture.md` filename and all
/// but the first failed on a same-day collision. That is unusable for anyone
/// who does not write in a Latin script, this vault's owner included.
///
/// **The confinement argument, re-established for the wider charset** (it
/// does not simply carry over from the ASCII one):
///
/// - `char::is_alphanumeric` is `Alphabetic || Nd || Nl || No`. Every
///   character this function can EMIT is either `-` or satisfies that
///   predicate — including after lowercasing, see the next paragraph. So the
///   output cannot contain `/` or `\` (both `Po`), `.` (`Po`), NUL or any
///   other control character (`Cc`), a newline (`Cc`), or ANY Unicode format
///   character (`Cf`) — bidi overrides `U+202A`–`U+202E` and isolates
///   `U+2066`–`U+2069` in particular are `Cf`, not alphabetic, so they never
///   survive; like every other non-alphanumeric they collapse into the
///   separator run. `tests::` below asserts each of those
///   category claims directly rather than taking them on trust.
/// - A whole path component of `.` or `..` is therefore unreachable twice
///   over: `.` cannot be emitted at all, and the component this slug lands
///   in is always `YYYY-MM-DD-<slug>.md`.
/// - Lowercasing is applied through `char::to_lowercase` (Unicode, not
///   `to_ascii_lowercase`) and its OUTPUT is filtered again, because a few
///   mappings expand into a non-alphanumeric character — `U+0130` (`İ`)
///   lowercases to `i` + `U+0307`, a combining mark (`Mn`). Filtering after
///   the mapping is what keeps "every emitted character is alphanumeric" an
///   invariant rather than an approximation.
///
/// `resolve_create_under_vault`'s `Component::Normal` check remains exactly
/// as it was. It is defense in depth against a future caller passing an
/// unsanitized path — this widening neither weakens nor replaces it.
fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase().filter(|c| c.is_alphanumeric()));
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Truncate `s` to at most `max_chars` characters AND at most `max_bytes`
/// bytes, always on a `char` boundary. Both bounds matter: see
/// [`MAX_SLUG_BYTES`] for why a character cap alone does not bound a UTF-8
/// filename.
fn cap_slug(s: &str, max_chars: usize, max_bytes: usize) -> &str {
    let mut end = 0;
    for (i, (offset, ch)) in s.char_indices().enumerate() {
        if i >= max_chars || offset + ch.len_utf8() > max_bytes {
            break;
        }
        end = offset + ch.len_utf8();
    }
    &s[..end]
}

/// Derive the slug portion of `brain_capture`'s note filename
/// (`<inbox>/YYYY-MM-DD-<slug>.md`): from `title` when it [`sanitize_slug`]s
/// to something non-empty, else from the first [`TEXT_SLUG_SOURCE_CHARS`]
/// characters of `text`, else [`FALLBACK_SLUG`] plus a random
/// disambiguator — so every call produces SOME valid filename, never an
/// error, regardless of how degenerate `title`/`text` are (empty,
/// punctuation-only, emoji-only, arbitrarily long, ...). The result is
/// always non-empty, always [`MAX_SLUG_CHARS`] characters or fewer, and
/// always [`MAX_SLUG_BYTES`] bytes or fewer.
///
/// **The fallback is disambiguated, and only the fallback.** A slug derived
/// from real input is deterministic, so re-capturing the same title on the
/// same day still surfaces the deliberate one-note-per-title-per-day
/// collision error (`capture_collision_message`). But a slug that carries no
/// caller information — the emoji-only title with an emoji-only body — would
/// otherwise make EVERY such capture in a day collide with the first, which
/// is a filename artifact rather than the "you already wrote this note"
/// signal that error is meant to convey. Appending
/// [`FALLBACK_DISAMBIGUATOR_CHARS`] random characters (drawn from the same
/// [`mint_secret_32`] CSPRNG every other opaque id in this crate uses,
/// filtered to lowercase ASCII alphanumerics so the slug charset is
/// unchanged) keeps those apart. Deliberately NOT a general
/// collision-retry loop: what a same-day, same-title recapture should do is
/// a product question this fix wave should not settle.
fn derive_slug(title: Option<&str>, text: &str) -> String {
    let from_title = title.map(sanitize_slug).filter(|s| !s.is_empty());
    let candidate = from_title.unwrap_or_else(|| {
        let head: String = text.chars().take(TEXT_SLUG_SOURCE_CHARS).collect();
        sanitize_slug(&head)
    });
    let capped = cap_slug(&candidate, MAX_SLUG_CHARS, MAX_SLUG_BYTES);
    let trimmed = capped.trim_end_matches('-');
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    let disambiguator: String = mint_secret_32()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .take(FALLBACK_DISAMBIGUATOR_CHARS)
        .collect();
    if disambiguator.is_empty() {
        // Unreachable in practice (62 of base64url's 64 characters are
        // alphanumeric, over 43 of them), but a bare fallback is still a
        // valid filename, so this degrades rather than failing.
        return FALLBACK_SLUG.to_string();
    }
    format!("{FALLBACK_SLUG}-{disambiguator}")
}

/// Maps a vault-resolution [`CoreError`] to an MCP `invalid_params` error.
/// The client-facing message is a FIXED phrase plus the stable `E_*` code —
/// never `err`'s own `Display` text, which for some variants embeds a host
/// path (`VaultNotFound { cwd }` names the process cwd, `NotAVault { path }`
/// names the configured path) that must not reach a network client. The
/// full error, path included, is logged server-side via `tracing::warn!`
/// (same pattern as [`sanitized_internal`]).
fn core_error(err: CoreError) -> ErrorData {
    let code = err.error_code();
    tracing::warn!(error = ?err, "vault resolution failed [{code}]");
    let message = match &err {
        CoreError::VaultNotFound { .. } => {
            format!("no OneBrain vault resolved for this call [{code}]")
        }
        CoreError::NotAVault { .. } => {
            format!("configured path is not a OneBrain vault [{code}]")
        }
        _ => format!("vault resolution failed [{code}]"),
    };
    ErrorData::invalid_params(message, None)
}

/// Client-facing internal error: full detail goes to the server log only —
/// daemon errors embed absolute host paths (e.g. the slot log path
/// `daemon_client::ensure_running` names on a start timeout), which must
/// never reach a network client. `context` becomes the ONLY thing the caller
/// sees on the wire; `err`'s full chain is logged via `tracing` instead.
fn sanitized_internal(context: &str, err: anyhow::Error) -> ErrorData {
    tracing::warn!(error = ?err, "{context}");
    ErrorData::internal_error(format!("{context} — see gateway logs"), None)
}

/// Resolve which vault a tool call operates on.
///
/// `vault` names an entry in `config.vaults` (unknown name → `invalid_params`
/// listing the known names). `None` resolves through the standard
/// flag/env/walk-up chain, with `config.default_vault` standing in for the
/// flag (so an explicit default always wins over `$ONEBRAIN_VAULT` / cwd,
/// exactly like a CLI `--vault` flag would).
fn resolve_vault_arg(
    state: &GatewayState,
    vault: Option<&str>,
) -> Result<ResolvedVault, ErrorData> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let inputs = match vault {
        Some(name) => {
            let Some(path) = state.config.vaults.get(name) else {
                let known: Vec<&str> = state.config.vaults.keys().map(String::as_str).collect();
                return Err(ErrorData::invalid_params(
                    format!(
                        "unknown vault \"{name}\" — known vaults: [{}]",
                        known.join(", ")
                    ),
                    None,
                ));
            };
            VaultResolveInputs {
                flag: Some(path.clone()),
                env: None,
                cwd,
            }
        }
        None => VaultResolveInputs {
            flag: state.config.default_vault.clone(),
            env: std::env::var_os("ONEBRAIN_VAULT").map(Into::into),
            cwd,
        },
    };
    require_vault(&inputs).map_err(core_error)
}

/// The `default_vault` value to report from `capabilities`: the matching
/// `config.vaults` name when the configured path is a named entry, else the
/// fixed marker `"(configured)"` — the raw path is NEVER returned, since
/// that would leak a host filesystem path to the client. Purely a display
/// convenience — `resolve_vault_arg` re-derives resolution from
/// `config.default_vault` itself, not from this string.
fn default_vault_display(config: &GatewayConfig) -> Option<String> {
    let path = config.default_vault.as_ref()?;
    let name = config
        .vaults
        .iter()
        .find(|(_, v)| paths_match(path, v))
        .map(|(name, _)| name.clone());
    Some(name.unwrap_or_else(|| "(configured)".to_string()))
}

/// Compares two paths for the purpose of matching `default_vault` against a
/// `config.vaults` entry. Canonicalizes both sides so a trailing-slash or
/// `.`-segment mismatch between an otherwise-identical path still matches;
/// falls back to plain equality when either side can't be canonicalized
/// (e.g. the path doesn't exist on disk yet) rather than treating that as a
/// hard mismatch.
fn paths_match(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// The brain pack's own tools, paired with each one's [`RiskClass`].
///
/// **This is a PARALLEL list, kept in sync by hand.** Each `#[tool]` handler
/// hardcodes its own `RiskClass` in its `policy_gate` call; nothing in the
/// type system ties the two together, so adding a tool (or reclassifying
/// one) means editing both places. `tests::brain_pack_tools_matches_the_tools_the_router_registers`
/// pins half of that — the NAME set here against the names `#[tool_router]`
/// actually registers, so a tool added to the router and forgotten here (or
/// vice versa) fails a test rather than shipping a `capabilities` response
/// that lies about what exists. The CLASS half stays conventional: a
/// handler's class is a literal inside an `async fn` body, not reachable
/// from any test without parsing source, and inventing a registry to make it
/// reachable would cost more than the drift it prevents. Verify by reading
/// when you touch either side.
///
/// Gateway PR 4, Task 6 widened this from a bare name list to also resolve
/// each class's EFFECTIVE [`PolicyMode`] via `policy.mode_for` — the exact
/// lookup [`policy::decide`] performs — so `capabilities` reports what would
/// actually happen on the NEXT real call to each tool, not just which risk
/// bucket it falls into.
fn brain_pack_tools(policy: &policy::PolicyConfig) -> Vec<ToolInfo> {
    [
        ("capabilities", RiskClass::ReadOnly),
        ("brain_tasks", RiskClass::ReadOnly),
        ("brain_get", RiskClass::ReadOnly),
        ("brain_search", RiskClass::ReadOnly),
        ("brain_capture", RiskClass::Mutating),
    ]
    .into_iter()
    .map(|(name, risk_class)| ToolInfo {
        name: name.to_string(),
        risk_class,
        policy_mode: policy.mode_for(risk_class),
    })
    .collect()
}

/// The full pack list `capabilities` reports. Only `brain` is enabled this
/// task; `developer`/`files`/`mac` are the roadmapped packs (later PRs),
/// listed disabled so a caller can see what's coming without probing for
/// tools that don't exist yet.
fn capability_packs(policy: &policy::PolicyConfig) -> Vec<PackInfo> {
    vec![
        PackInfo {
            name: "brain".to_string(),
            enabled: true,
            tools: brain_pack_tools(policy),
            // Must stay truthful about `brain_capture`: this prose sits in
            // the same `capabilities` payload as a `tools` array reporting
            // `brain_capture` with `risk_class: mutating`, and the two must
            // not contradict each other. Pinned by
            // `tests::brain_pack_note_does_not_claim_the_pack_is_read_only`.
            note: "Vault search, retrieval, and task listing, plus one write tool \
                   (brain_capture) that may require approval per gateway policy."
                .to_string(),
        },
        PackInfo {
            name: "developer".to_string(),
            enabled: false,
            tools: vec![],
            note: "Planned — not yet implemented.".to_string(),
        },
        PackInfo {
            name: "files".to_string(),
            enabled: false,
            tools: vec![],
            note: "Planned — not yet implemented.".to_string(),
        },
        PackInfo {
            name: "mac".to_string(),
            enabled: false,
            tools: vec![],
            note: "Planned — not yet implemented.".to_string(),
        },
    ]
}

/// Which channels can actually resolve a
/// [`policy::PolicyOutcome::NeedApproval`] call on THIS running gateway
/// process (Gateway PR 4, Task 6) — the truthfulness contract this whole
/// type exists for: a caller must never be told a write CAN be approved when
/// no channel exists to carry that prompt to a human.
///
/// - `native`: [`approval_native::is_available`] — `true` iff this build
///   targets macOS, `osascript` resolves on `$PATH`, AND the channel has not
///   been explicitly disabled via
///   `ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL` (set by this crate's own
///   `tests/gateway_approval_e2e.rs` in the environment of the gateway
///   subprocess it spawns, so that process never pops a real, unattended GUI
///   dialog on a macOS CI runner — see `approval_native`'s own module docs,
///   "Disabling the channel from outside the process").
/// - `http`: the operator `GET`/`POST /approvals` surface
///   ([`approval_routes::approval_router`]) — unconditionally mounted by
///   [`build_gateway_router`] on every platform this binary targets, so this
///   is always `true` in this build. Using it still requires a human who
///   knows the gateway's pairing code (printed once, to stdout, at `gateway
///   run` startup) — the same precondition OAuth pairing itself already
///   assumes; `capabilities` reporting `true` here is not a claim that a
///   human is actively watching, only that the channel exists and is
///   reachable.
/// - `telegram`: not implemented yet — deferred to Gateway PR 5. Always
///   `false` today; see `note` below and `docs/gateway.md`.
fn approval_channels() -> ApprovalChannels {
    ApprovalChannels {
        native: approval_native::is_available(),
        http: true,
        telegram: false,
        note: "http is always available given the gateway's pairing code (printed once to \
               stdout at `gateway run` startup); native requires macOS with osascript on \
               PATH and is unavailable when explicitly disabled; telegram is not \
               implemented yet — planned for Gateway PR 5."
            .to_string(),
    }
}

/// Extract the [`Principal`] `auth::middleware::require_bearer` inserted
/// into the raw HTTP request's extensions before this call ever reached an
/// rmcp tool handler. `parts` itself is obtained via the
/// `Extension<http::request::Parts>` extractor — see the vendored crate's
/// own "Accessing HTTP request data from tool handlers" doc
/// (`rmcp-3.0.1/src/transport/streamable_http_server/tower.rs`): the
/// streamable-HTTP service stashes the consumed request's `Parts` into the
/// rmcp-level context extensions, and axum's OWN `Extension` middleware
/// layer (a different map — `parts.extensions`, not the rmcp one) is where
/// `require_bearer` put the `Principal`. Two nested extension maps, two
/// different `Extension` types with the same name — easy to conflate, so
/// spelled out here once.
///
/// `Principal` should always be present for `/mcp`: `build_gateway_router`
/// wraps the WHOLE `/mcp` nest in `require_bearer`, which either 401s
/// (never reaching a tool handler) or inserts `Principal` before calling
/// `next`. A missing `Principal` here means a route bypassed that layer —
/// not reachable through `build_gateway_router` today, but this fails
/// closed with `internal_error` rather than panicking on it.
fn extract_principal(parts: &Parts) -> Result<Principal, ErrorData> {
    match parts.extensions.get::<Principal>() {
        Some(p) => Ok(p.clone()),
        None => {
            tracing::warn!(
                "gateway tool handler ran with no Principal in request extensions — \
                 require_bearer should make this unreachable for /mcp"
            );
            Err(ErrorData::internal_error(
                "internal error — see gateway logs",
                None,
            ))
        }
    }
}

/// Fixed placeholder `client_id` recorded by [`extract_principal_audited`]
/// when a gateway tool handler somehow runs with no [`Principal`] in the
/// request's extensions — there is no real principal to name. Deliberately
/// NOT a value any real OAuth client registration could ever produce (RFC
/// 7591 client ids are opaque random secrets from `core::mint_secret_32`,
/// never literal angle-bracketed text), so it reads unambiguously as the
/// marker it is when grepping the audit log.
const UNKNOWN_PRINCIPAL_CLIENT_ID: &str = "<no-principal>";

/// [`extract_principal`], plus one more thing: on failure, records a
/// minimal audit entry BEFORE returning the error (Task 3 review, binding
/// requirement B). The bare `extract_principal(&parts)?` this replaces
/// (Task 2's original shape, one call per tool handler) returned via `?`
/// before `record_audit` ever had a `principal` to build an entry from — a
/// genuine "can't happen" path (see [`extract_principal`]'s own doc
/// comment: not reachable through `build_gateway_router` today, since
/// `require_bearer` wraps the WHOLE `/mcp` nest) that nonetheless left ZERO
/// audit trail if it ever somehow did happen. Now it leaves exactly one
/// entry: `client_id` = [`UNKNOWN_PRINCIPAL_CLIENT_ID`] (there is no real
/// principal to name) and `decision` = [`Decision::Denied`] (the call was
/// refused outright, not asked-about-and-left-blocked).
async fn extract_principal_audited(
    state: &Arc<GatewayState>,
    tool: &'static str,
    started: Instant,
    parts: &Parts,
) -> Result<Principal, ErrorData> {
    match extract_principal(parts) {
        Ok(p) => Ok(p),
        Err(err) => {
            record_audit(
                state,
                UNKNOWN_PRINCIPAL_CLIENT_ID,
                CallMeta {
                    tool,
                    vault: None,
                    args_summary: format!("{tool}: (no Principal in request extensions)"),
                },
                Decision::Denied,
                started,
                Outcome::Error,
            )
            .await;
            Err(err)
        }
    }
}

/// Runs the policy check ([`policy::decide`]) for one tool call of risk
/// class `class`. `Ok(Decision::Auto)` or `Ok(Decision::Approved)` means the
/// call may proceed; `Err` carries BOTH the [`Decision`] to record in the
/// audit log and the client-facing [`ErrorData`] to return, for the three
/// ways a call may not proceed:
///
/// - `PolicyOutcome::Deny` (policy `deny`, or a scope/pack mismatch) →
///   `Decision::Denied`, immediately, no waiting.
/// - `PolicyOutcome::NeedApproval` → delegates to [`await_approval`], which
///   registers a [`PendingApproval`] and blocks on a human decision (or a
///   timeout) — see that function's own doc comment for the full flow.
///
/// Every read-only tool (`capabilities`/`brain_tasks`/`brain_get`/
/// `brain_search`) is `RiskClass::ReadOnly`, which defaults to
/// `PolicyMode::Auto` — so under the DEFAULT `gateway.yml`, a call to one of
/// those still takes the `Allow` path and this function is a no-op
/// pass-through for it. `brain_capture` is `RiskClass::Mutating`, which
/// defaults to `PolicyMode::AskOnce` — so under that SAME default config, a
/// `brain_capture` call reaches `NeedApproval` (absent a live grant) even
/// with zero operator customization.
///
/// `vault` is the call's own `vault` argument (`None` = the default
/// resolution chain). It is part of the consent scope, not just display:
/// `policy::decide` keys grants on `(client, vault, class)`, so approving a
/// write into one vault never authorizes writes into another — see
/// `policy.rs`'s "Grant scope" section.
///
/// `args_summary` is passed through verbatim to become
/// [`PendingApproval::summary`] if this call needs approval — the SAME
/// redacted, one-line string every caller already builds for its own audit
/// entry (never the raw tool arguments, never a note body — see
/// `audit::AuditEntry::args_summary`'s own doc comment), so there is no
/// second summary to keep in sync.
async fn policy_gate(
    state: &Arc<GatewayState>,
    principal: &Principal,
    tool: &'static str,
    class: RiskClass,
    vault: Option<&str>,
    args_summary: &str,
) -> Result<Decision, (Decision, ErrorData)> {
    match policy::decide(&state.config.policy, &state.grants, principal, class, vault) {
        PolicyOutcome::Allow => Ok(Decision::Auto),
        PolicyOutcome::Deny => Err((
            Decision::Denied,
            ErrorData::invalid_request(format!("gateway policy denies this call [{tool}]"), None),
        )),
        PolicyOutcome::NeedApproval => {
            await_approval(state, principal, tool, class, vault, args_summary).await
        }
    }
}

/// The `NeedApproval` arm of [`policy_gate`] (Gateway PR 4, Task 5): the
/// wiring that finally connects [`policy::decide`] to Task 3's
/// [`Approvals`] registry and Task 4's native macOS dialog channel.
///
/// 1. Build a [`PendingApproval`] (a fresh id via [`mint_secret_32`] — reused
///    rather than hand-rolled, matching every other opaque id/secret this
///    crate mints; `summary` is `args_summary` verbatim, `expires` derived
///    from [`policy::PolicyConfig::approval_wait_seconds`]) and
///    [`Approvals::register`] it — synchronous, no `.await` in its body, so
///    nothing is held across an await point here. Registration is BOUNDED
///    ([`approval::RegisterRejected`]): past the global or per-client cap
///    the call is refused with a policy error instead of queueing another
///    human prompt. Without that, a client looping `brain_capture` under
///    the default `ask_once` (before any grant exists) or under
///    `ask_always` would fan out one blocking `osascript` dialog — each
///    pinning a `spawn_blocking` thread — per in-flight call.
/// 2. Fire the native dialog channel via [`approval_native::prompt`] when
///    [`approval_native::is_available`] AND this isn't the test binary
///    (`cfg!(test)` — see the inline comment at the call site below for why)
///    — non-blocking (`prompt` hands the actual wait off to
///    `spawn_blocking` and returns immediately); a late or absent answer
///    from this channel is harmless by construction, since
///    [`Approvals::resolve`] is first-response-wins (see that method's own
///    doc comment).
/// 3. `.await` [`Approvals::wait`] for up to `approval_wait_seconds`. Per
///    that method's own doc comment, it never holds the `pending` lock
///    across this `.await` — the lock is only taken (inside `register`,
///    already returned by the time we get here) and, on timeout, briefly
///    again afterward purely to clean up. Nothing in THIS function holds a
///    `std::sync::Mutex` guard across the `.await` either: `Grants`/
///    `Approvals` are only ever touched through their own
///    lock-do-one-thing-drop-the-guard methods, called either strictly
///    BEFORE or strictly AFTER the `.await`, never straddling it.
/// 4. On `Decision::Approve`: record a [`Grants`] entry for `(client_id,
///    vault, class)` — config-derived TTL (`grant_ttl_minutes * 60`,
///    mirroring
///    `approval_routes::resolve_approval`'s own identical calculation for
///    the HTTP resolution channel) — so a second `ask_once` call from the
///    same client, same vault, same risk class, within that TTL, satisfies
///    `decide` via `Grants::has` and never reaches this function at all.
///    Nothing is recorded under `PolicyMode::AskAlways`: `decide` ignores
///    grants in that mode anyway, but "always ask" must never be capable of
///    leaving standing consent behind for a later refactor to start
///    honoring (`approval_routes::resolve_approval` carries the same
///    guard). Recording it
///    HERE (not only in `approval_routes::resolve_approval`) is deliberate:
///    that HTTP-channel recording only fires when an operator resolves
///    through `/approvals`, but an approval can equally arrive through the
///    native dialog channel, which never touches `approval_routes.rs` at
///    all — recording the grant in the WAITER, once, regardless of which
///    channel produced the decision, is the only way both channels reliably
///    honor "ask once". A second write to the same `(client, class)` key
///    from the HTTP channel (when that IS how it was resolved) is
///    idempotent — `Grants::record` replaces, never accumulates.
/// 5. On `Decision::Deny` or a timeout: no grant, no side effect beyond the
///    audit trail `policy_gate`'s caller records — a denied or timed-out
///    call never reaches the tool's own logic.
async fn await_approval(
    state: &Arc<GatewayState>,
    principal: &Principal,
    tool: &'static str,
    class: RiskClass,
    vault: Option<&str>,
    args_summary: &str,
) -> Result<Decision, (Decision, ErrorData)> {
    let wait_secs = state.config.policy.approval_wait_seconds;
    let now = now_epoch_secs();
    let pending = PendingApproval {
        id: mint_secret_32(),
        client_id: principal.client_id.clone(),
        tool: tool.to_string(),
        vault: vault.map(str::to_string),
        // Bounded here for the same reason `record_audit` bounds its own
        // copy: this string is shown to a human over `GET /approvals` and
        // handed to `osascript` as a command-line argument. See
        // [`bounded_summary`].
        summary: bounded_summary(args_summary.to_string()),
        created: now,
        expires: now.saturating_add(wait_secs),
        class,
    };
    let id = pending.id.clone();
    let rx = match state.approvals.register(pending.clone()) {
        Ok(rx) => rx,
        Err(rejected) => {
            // Operator-facing only: which cap was hit tells an operator
            // whether this is one runaway connector or a wedged gateway.
            // The client is told nothing beyond "at capacity" — the counts
            // and their split are not its business.
            tracing::warn!(
                ?rejected,
                client_id = %principal.client_id,
                tool,
                "refusing a tool call: the pending-approval registry is at capacity"
            );
            return Err((
                Decision::Denied,
                ErrorData::invalid_request(
                    format!("too many approval requests are already pending [{tool}]"),
                    None,
                ),
            ));
        }
    };

    // `cfg!(test)` guards this crate's OWN unit tests (this module's
    // `#[cfg(test)] mod tests` below) from ever actually reaching
    // `approval_native::prompt` on a macOS dev machine or macOS CI runner —
    // without it, EVERY test that drives a `NeedApproval` call through this
    // function would pop a real, blocking `osascript display dialog` GUI
    // prompt (`is_available()` is unconditionally true on any macOS box
    // with the stock `/usr/bin/osascript`, which every one is), exactly the
    // "never write a test that pops a real GUI dialog" hazard
    // `approval_native.rs`'s own module docs already call out — this task
    // is what FIRST gives that function a caller reachable from an ordinary
    // test run, so this guard is what keeps that hazard from becoming real.
    // `cfg!(test)` is false in the actually-shipped `onebrain` binary, so
    // production behavior (fire the dialog whenever available) is
    // unaffected; it does NOT protect a separate integration-test binary
    // that spawns the real compiled binary as a subprocess — Gateway PR 4,
    // Task 6's `tests/gateway_approval_e2e.rs` is exactly that: it spawns
    // the real binary and drives `brain_capture` through a real `ask_once`
    // policy over real HTTP. That test closes the gap a DIFFERENT way,
    // outside this function entirely: it sets
    // `ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL=1` in the spawned process's
    // environment, which makes `approval_native::is_available()` return
    // `false` — so the `&&` short-circuits and this line is never reached,
    // deliberately, not because nothing calls `await_approval` under
    // `ask_once` any more (something now does, for real, in a separate
    // process). See `approval_native.rs`'s own module docs ("Disabling the
    // channel from outside the process") for the full mechanism, and
    // `docs/coverage.md`'s "Gateway approval flow" residual entry for why
    // `approval_native::prompt`'s actual body stays uncovered by design —
    // this line included.
    if !cfg!(test) && approval_native::is_available() {
        approval_native::prompt(&pending, state.approvals.clone());
    }

    match state
        .approvals
        .wait(&id, rx, Duration::from_secs(wait_secs))
        .await
    {
        WaitOutcome::Decided(approval::Decision::Approve) => {
            // "Always ask" never leaves standing consent behind — see this
            // function's doc comment, step 4, and the identical guard in
            // `approval_routes::resolve_approval`.
            if state.config.policy.mode_for(class) != PolicyMode::AskAlways {
                let ttl_secs = state.config.policy.grant_ttl_minutes.saturating_mul(60);
                state.grants.record(
                    GrantKey::new(
                        principal.client_id.clone(),
                        vault.map(str::to_string),
                        class,
                    ),
                    ttl_secs,
                );
            }
            Ok(Decision::Approved)
        }
        WaitOutcome::Decided(approval::Decision::Deny) => Err((
            Decision::Denied,
            ErrorData::invalid_request(
                format!("this call was denied by the gateway operator [{tool}]"),
                None,
            ),
        )),
        WaitOutcome::TimedOut => Err((
            Decision::TimedOut,
            ErrorData::invalid_request(
                format!("approval request timed out with no response [{tool}]"),
                None,
            ),
        )),
    }
}

/// Hard ceiling, in BYTES, on a recorded `args_summary`
/// ([`bounded_summary`]). Generous for the "one-line, human-readable
/// description" `audit::AuditEntry::args_summary` documents itself as, and
/// far below anything that could grow the audit log or an approval prompt
/// out of proportion to the call it describes.
const MAX_ARGS_SUMMARY_BYTES: usize = 512;

/// Appended when [`bounded_summary`] actually cuts something, so a reader
/// never mistakes a truncated summary for a complete one — with the original
/// length, which is itself the interesting signal when a caller sends an
/// absurd argument.
const ARGS_SUMMARY_TRUNCATED_SUFFIX: &str = " [truncated";

/// Bound `summary` to [`MAX_ARGS_SUMMARY_BYTES`], marking it when truncation
/// happened.
///
/// Every tool handler interpolates RAW, caller-supplied parameters into its
/// `args_summary` (`params.file`, `params.query`, `params.title`, …), and
/// that string is then written verbatim to an audit log with no size cap and
/// no rotation, and shown verbatim to an operator as an approval prompt.
/// Without this, a client holding nothing but a valid connector token could
/// pass a multi-megabyte `file` to `brain_get` — an `auto`, read-only tool
/// that needs no approval and no grant — have the call fail immediately on
/// path resolution, and still land the whole payload on disk. Repeat to
/// fill it.
///
/// Applied at the two points where a summary is RECORDED — [`record_audit`]
/// (the audit entry) and [`await_approval`] ([`PendingApproval::summary`],
/// which reaches both the operator's `GET /approvals` list and the native
/// dialog's `osascript` argv, where a multi-megabyte argument would exceed
/// `ARG_MAX` and silently take that channel down for the call) — rather than
/// at each construction site. Two chokepoints, both structural: the next
/// tool added to this file inherits the bound without having to remember it.
///
/// Truncation lands on a `char` boundary, so the result is always valid
/// UTF-8 even when the cut falls inside a multi-byte character.
fn bounded_summary(summary: String) -> String {
    if summary.len() <= MAX_ARGS_SUMMARY_BYTES {
        return summary;
    }
    let total = summary.len();
    let mut end = MAX_ARGS_SUMMARY_BYTES;
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = summary;
    out.truncate(end);
    out.push_str(ARGS_SUMMARY_TRUNCATED_SUFFIX);
    out.push_str(&format!(", {total} bytes total]"));
    out
}

/// The parts of an audit entry a tool handler knows BEFORE calling
/// [`policy_gate`] — bundled into one struct purely to keep
/// [`record_audit`] under clippy's `too_many_arguments` threshold; `tool`/
/// `vault`/`args_summary` are always determined together, right after a
/// handler parses its params, so grouping them costs nothing at the call
/// site.
struct CallMeta {
    tool: &'static str,
    vault: Option<String>,
    args_summary: String,
}

/// Build and append one audit-log entry for a completed (or, via
/// [`extract_principal_audited`], a never-actually-started) tool call.
///
/// `client_id` is a plain `&str`, not `&Principal` — this function no
/// longer needs a real [`Principal`] to record an entry, since
/// [`extract_principal_audited`] is a caller that has none to give it (see
/// that function's doc comment); `outcome` is likewise passed directly
/// (`Outcome::Ok`/`Outcome::Error`) rather than derived from a generic
/// `Result<T, _>` reference, so every caller decides it once at the call
/// site instead of this function needing a type parameter just to call
/// `.is_ok()`.
///
/// Off-loads the actual blocking file write to
/// [`tokio::task::spawn_blocking`] (Task 3 review, binding requirement C —
/// `AuditLog::append` does synchronous filesystem I/O, and every other
/// filesystem operation in this file already runs off the async runtime the
/// same way: `brain_tasks`'s `visit_tasks` call and `brain_get`'s
/// `resolve_under_vault` call). `state` is cloned (an `Arc`, so cheap) into
/// the blocking closure, which needs `'static` data to hand to
/// `spawn_blocking`.
///
/// Still infallible from the caller's view, same as every
/// [`AuditLog::append`] caller (see that method's doc comment) — every call
/// site here `.await`s this to completion (so the entry is guaranteed
/// written before the tool call's own response goes out), but that await
/// can never surface an `Err` to unwind: the only NEW failure mode this
/// wrapping introduces is the spawned blocking task itself panicking, which
/// — per that same "must never block/fail the tool call it's recording"
/// contract — collapses to a `tracing::warn!` here too, never a propagated
/// error.
async fn record_audit(
    state: &Arc<GatewayState>,
    client_id: &str,
    meta: CallMeta,
    decision: Decision,
    started: Instant,
    outcome: Outcome,
) {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let entry = AuditEntry {
        ts: now_epoch_secs(),
        client_id: client_id.to_string(),
        tool: meta.tool.to_string(),
        vault: meta.vault,
        // The single point where a summary becomes a durable, unrotated
        // on-disk record — so the single point that bounds it. See
        // [`bounded_summary`].
        args_summary: bounded_summary(meta.args_summary),
        decision,
        channel: None,
        duration_ms,
        outcome,
    };
    let state = state.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || state.audit.append(&entry)).await {
        tracing::warn!(
            error = %e,
            "gateway audit-log spawn_blocking task panicked; entry not recorded"
        );
    }
}

#[tool_router]
impl GatewayServer {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "capabilities",
        description = "Report which capability packs and vaults this OneBrain gateway serves. Call this first to plan which brain_* tool fits the job.",
        annotations(read_only_hint = true)
    )]
    async fn capabilities(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<Json<CapabilitiesOut>, ErrorData> {
        let started = Instant::now();
        let principal =
            extract_principal_audited(&self.state, "capabilities", started, &parts).await?;
        let args_summary = "capabilities: (no arguments)".to_string();
        let (decision, result) = match policy_gate(
            &self.state,
            &principal,
            "capabilities",
            RiskClass::ReadOnly,
            None,
            &args_summary,
        )
        .await
        {
            Ok(decision) => {
                let config = &self.state.config;
                let out = CapabilitiesOut {
                    gateway_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: ProtocolVersion::V_2026_07_28.as_str().to_string(),
                    packs: capability_packs(&config.policy),
                    vaults: config.vaults.keys().cloned().collect(),
                    default_vault: default_vault_display(config),
                    approval_channels: approval_channels(),
                };
                (decision, Ok(Json(out)))
            }
            Err((decision, err)) => (decision, Err(err)),
        };
        record_audit(
            &self.state,
            &principal.client_id,
            CallMeta {
                tool: "capabilities",
                vault: None,
                args_summary,
            },
            decision,
            started,
            if result.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            },
        )
        .await;
        result
    }

    #[tool(
        name = "brain_tasks",
        description = "List open vault tasks (Obsidian checkbox lines, fence-aware). Use due_by=\"today\" for the daily view. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn brain_tasks(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<BrainTasksParams>,
    ) -> Result<Json<BrainTasksOut>, ErrorData> {
        let started = Instant::now();
        let principal =
            extract_principal_audited(&self.state, "brain_tasks", started, &parts).await?;
        let vault = params.vault.clone();
        let args_summary = format!(
            "tasks: due_by={:?} limit={:?} vault={:?}",
            params.due_by, params.limit, params.vault
        );

        let (decision, result) = match policy_gate(
            &self.state,
            &principal,
            "brain_tasks",
            RiskClass::ReadOnly,
            params.vault.as_deref(),
            &args_summary,
        )
        .await
        {
            Ok(decision) => {
                let result: Result<Json<BrainTasksOut>, ErrorData> = async {
                    let resolved = resolve_vault_arg(&self.state, params.vault.as_deref())?;
                    let vault_name = resolved.root.name();

                    let vault_config = load_vault_config(&resolved.root).map_err(core_error)?;

                    let cutoff = match params.due_by.as_deref() {
                        Some(raw) => Some(
                            resolve_due_by(raw)
                                .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?,
                        ),
                        None => None,
                    };
                    let limit = Some(params.limit.unwrap_or(20));
                    let include_prefixes = resolve_prefixes(&vault_config.folders, &[]);
                    let root = resolved.root.as_path().to_path_buf();

                    // `visit_tasks` walks the filesystem synchronously —
                    // off the async runtime, mirroring `mcp.rs`'s own
                    // filesystem-walk tools (`expand_multi_get_pattern`
                    // via `spawn_blocking`).
                    let (tasks, total) = tokio::task::spawn_blocking(move || {
                        let mut collector = TaskCollector::new(false, cutoff.as_deref(), limit);
                        let opts = TaskScanOptions {
                            include_prefixes,
                            max: usize::MAX,
                        };
                        visit_tasks(&root, &opts, |task| collector.consider(task));
                        collector.finish()
                    })
                    .await
                    .map_err(|e| sanitized_internal("internal task failure", e.into()))?;

                    Ok(Json(BrainTasksOut {
                        tasks: tasks.into_iter().map(GatewayTaskHit::from).collect(),
                        total,
                        vault: vault_name,
                    }))
                }
                .await;
                (decision, result)
            }
            Err((decision, err)) => (decision, Err(err)),
        };

        record_audit(
            &self.state,
            &principal.client_id,
            CallMeta {
                tool: "brain_tasks",
                vault,
                args_summary,
            },
            decision,
            started,
            if result.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            },
        )
        .await;
        result
    }

    #[tool(
        name = "brain_get",
        description = "Read one vault note by vault-relative path. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn brain_get(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<BrainGetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let principal =
            extract_principal_audited(&self.state, "brain_get", started, &parts).await?;
        let vault = params.vault.clone();
        let args_summary = format!("get: {} vault={:?}", params.file, params.vault);

        let (decision, result) = match policy_gate(
            &self.state,
            &principal,
            "brain_get",
            RiskClass::ReadOnly,
            params.vault.as_deref(),
            &args_summary,
        )
        .await
        {
            Ok(decision) => {
                let result: Result<CallToolResult, ErrorData> = async {
                    let resolved = resolve_vault_arg(&self.state, params.vault.as_deref())?;
                    let vault_root = resolved.root.as_path().to_path_buf();
                    let rel = params.file.clone();

                    let resolved_path = {
                        let vault_root = vault_root.clone();
                        let rel_for_resolve = rel.clone();
                        tokio::task::spawn_blocking(move || {
                            resolve_under_vault(&vault_root, &rel_for_resolve)
                        })
                        .await
                        .map_err(|e| sanitized_internal("internal task failure", e.into()))?
                        // Both of `resolve_under_vault`'s failure modes —
                        // canonicalize fails because nothing exists at the
                        // joined path, or it succeeds but the result
                        // escapes `vault_root` — collapse to the SAME
                        // generic message here, deliberately: a distinct
                        // "traversal blocked" vs. "genuinely missing"
                        // message would hand a caller an oracle for
                        // probing what exists outside the vault. Neither
                        // branch reaches the file's content either way.
                        .map_err(|_| {
                            ErrorData::invalid_params(
                                format!("file not found in vault: {rel}"),
                                None,
                            )
                        })?
                    };

                    tokio::fs::read_to_string(&resolved_path)
                        .await
                        .map(|text| CallToolResult::success(vec![ContentBlock::text(text)]))
                        .map_err(|e| ErrorData::invalid_params(format!("reading {rel}: {e}"), None))
                }
                .await;
                (decision, result)
            }
            Err((decision, err)) => (decision, Err(err)),
        };

        record_audit(
            &self.state,
            &principal.client_id,
            CallMeta {
                tool: "brain_get",
                vault,
                args_summary,
            },
            decision,
            started,
            if result.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            },
        )
        .await;
        result
    }

    #[tool(
        name = "brain_search",
        description = "Search vault notes (hybrid lexical + semantic via the warm daemon). Returns scored hits with paths and snippets.",
        annotations(read_only_hint = true)
    )]
    async fn brain_search(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<BrainSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let principal =
            extract_principal_audited(&self.state, "brain_search", started, &parts).await?;
        let vault = params.vault.clone();
        let args_summary = format!(
            "search: {:?} top_k={:?} vault={:?}",
            params.query, params.top_k, params.vault
        );

        let (decision, result) = match policy_gate(
            &self.state,
            &principal,
            "brain_search",
            RiskClass::ReadOnly,
            params.vault.as_deref(),
            &args_summary,
        )
        .await
        {
            Ok(decision) => {
                let result: Result<CallToolResult, ErrorData> = async {
                    let resolved = resolve_vault_arg(&self.state, params.vault.as_deref())?;
                    let vault_path = resolved.root.as_path().to_path_buf();
                    let query = params.query.clone();
                    let top_k = params.top_k;

                    // The gateway is a long-lived, multi-vault process: it
                    // must never open a direct `redb` engine itself,
                    // because that takes an exclusive per-vault file lock
                    // inside a process meant to serve MANY vaults
                    // concurrently — one vault's direct-engine open would
                    // starve every other vault's request against the same
                    // gateway. Search always routes through the warm
                    // per-vault daemon; a daemon-start or daemon-search
                    // failure is a hard `internal_error`, never a silent
                    // fallback to a direct engine (unlike
                    // `commands/mcp.rs`'s single-vault stdio server, which
                    // legitimately owns that fallback because it only
                    // ever holds one vault's lock for its whole process
                    // lifetime).
                    let hits = tokio::task::spawn_blocking(move || {
                        let handle = daemon_client::ensure_running(Some(&vault_path))?;
                        handle.search(&query, "hybrid", top_k, None)
                    })
                    .await
                    .map_err(|e| sanitized_internal("internal task failure", e.into()))?
                    .map_err(|e| sanitized_internal("search backend unavailable", e))?;

                    let body = serde_json::to_string_pretty(&hits)
                        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                    Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
                }
                .await;
                (decision, result)
            }
            Err((decision, err)) => (decision, Err(err)),
        };

        record_audit(
            &self.state,
            &principal.client_id,
            CallMeta {
                tool: "brain_search",
                vault,
                args_summary,
            },
            decision,
            started,
            if result.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            },
        )
        .await;
        result
    }

    #[tool(
        name = "brain_capture",
        description = "Capture a quick note into the vault inbox. The gateway's first WRITE tool — depending on gateway policy this may require interactive human approval before it proceeds.",
        annotations(read_only_hint = false)
    )]
    async fn brain_capture(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<BrainCaptureParams>,
    ) -> Result<Json<BrainCaptureOut>, ErrorData> {
        let started = Instant::now();
        let principal =
            extract_principal_audited(&self.state, "brain_capture", started, &parts).await?;
        let vault = params.vault.clone();
        // Never the note body — `text` is summarized by its length only, per
        // `audit::AuditEntry::args_summary`'s own doc comment (this string
        // also becomes `PendingApproval::summary` if the call needs
        // approval, an operator-facing field with the identical constraint).
        let args_summary = format!(
            "capture: title={:?} vault={:?} text_chars={}",
            params.title,
            params.vault,
            params.text.chars().count()
        );

        let (decision, result) = match policy_gate(
            &self.state,
            &principal,
            "brain_capture",
            RiskClass::Mutating,
            params.vault.as_deref(),
            &args_summary,
        )
        .await
        {
            Ok(decision) => (decision, capture_note(&self.state, &params).await),
            Err((decision, err)) => (decision, Err(err)),
        };

        record_audit(
            &self.state,
            &principal.client_id,
            CallMeta {
                tool: "brain_capture",
                vault,
                args_summary,
            },
            decision,
            started,
            if result.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            },
        )
        .await;
        result
    }
}

/// Do the actual `brain_capture` work once [`policy_gate`] has allowed the
/// call: resolve the vault, derive the inbox note's `YYYY-MM-DD-<slug>.md`
/// path, confine it ([`resolve_create_under_vault`] — see that function's
/// own doc comment for the two-layer argument), write it via
/// [`onebrain_fs::note::new_note`] (frontmatter + a title heading) followed
/// by [`onebrain_fs::note::append_note`] (the caller's actual `text` body —
/// `new_note` has no free-form body parameter of its own; append is the
/// existing, already-reused building block for landing arbitrary content
/// under a freshly created note, exactly like the CLI's own `note new` +
/// `note append` verbs compose), and best-effort reindex it so
/// `brain_search` can find it without waiting for the vault's next
/// scheduled/hook reindex. Split out of the `#[tool]` method itself purely
/// so [`GatewayServer::brain_capture`] keeps the same
/// policy_gate-then-inline-result-then-record_audit shape every other
/// handler here has, rather than growing one giant inline `async` block.
///
/// Every filesystem/daemon step runs off the async runtime via
/// `tokio::task::spawn_blocking`, matching every other handler in this file.
async fn capture_note(
    state: &Arc<GatewayState>,
    params: &BrainCaptureParams,
) -> Result<Json<BrainCaptureOut>, ErrorData> {
    // A capture with no body writes a titled, empty stub — never what a
    // caller meant, and afterwards indistinguishable from a capture whose
    // body was silently lost. This one genuinely IS a bad argument (unlike
    // the confinement failure below), so `invalid_params`.
    //
    // Checked HERE rather than before `policy_gate` so it stays consistent
    // with every other per-call validation in this file (an unknown `vault`
    // name is likewise only reported once policy has allowed the call) and
    // so the attempt still lands in the audit trail. The cost is that under
    // `ask_once` a human may be asked to approve a call that then fails
    // validation; moving every validation ahead of the gate would fix that
    // for all tools at once and is not this fix wave's scope.
    if params.text.trim().is_empty() {
        return Err(ErrorData::invalid_params(
            "capture failed: `text` is empty — a capture needs a note body".to_string(),
            None,
        ));
    }

    let resolved = resolve_vault_arg(state, params.vault.as_deref())?;
    let vault_root = resolved.root.as_path().to_path_buf();
    let vault_config = load_vault_config(&resolved.root).map_err(core_error)?;

    let slug = derive_slug(params.title.as_deref(), &params.text);
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let rel_path = PathBuf::from(vault_config.folders.inbox).join(format!("{today}-{slug}.md"));

    // Create-safe confinement BEFORE any write is attempted — see
    // `resolve_create_under_vault`'s own doc comment for why
    // `resolve_under_vault` (canonicalizes the TARGET) can't be reused here.
    // This is filesystem work (canonicalize + create_dir_all), so off the
    // async runtime like every other filesystem call in this file.
    //
    // The guard's returned path is BOUND and used, not discarded: layer 3 of
    // the guard makes "this is the file the write will open" an enforced
    // equality rather than an inference, so it is also the correct path to
    // test for the same-day filename collision — which is how this function
    // gets to phrase its own collision error instead of forwarding
    // `onebrain-fs`'s CLI-flavoured one.
    let already_exists = {
        let vault_root = vault_root.clone();
        let rel_path = rel_path.clone();
        tokio::task::spawn_blocking(move || {
            let confined = resolve_create_under_vault(&vault_root, &rel_path)?;
            Ok::<bool, anyhow::Error>(confined.exists())
        })
        .await
        .map_err(|e| sanitized_internal("internal task failure", e.into()))?
        // A guard rejection is deliberately `internal_error` here, NOT the
        // `invalid_params` collapse `brain_get` uses for its own traversal
        // guard. The two differ because the INPUT differs: `brain_get`'s
        // path comes straight from the caller, so distinguishing "blocked"
        // from "missing" would hand out an oracle for probing outside the
        // vault. `brain_capture`'s path is derived by `derive_slug`, which
        // strips every `/`, `\` and `.` — no caller input can select a path
        // component — so a failure here never means "your argument was
        // bad"; it means the vault's own inbox no longer resolves inside the
        // vault, a server-side fault. Both messages are sanitized, so
        // neither choice leaks an oracle; this one just reports the honest
        // kind of fault.
        .map_err(|e| sanitized_internal("failed to prepare note path", e))?
    };
    if already_exists {
        return Err(ErrorData::invalid_request(
            capture_collision_message(&rel_path),
            None,
        ));
    }

    // `tags: [capture]` — a proper YAML flow-sequence (matches the vault's
    // own `tags: [topic, type]` frontmatter convention) — plus `created`,
    // which `new_note` supplies on its own whenever the frontmatter pairs
    // given to it don't already carry one (see that function's doc
    // comment); this call site never provides one, so every captured note
    // gets today's date for free.
    let frontmatter = vec!["tags=[capture]".to_string()];
    let new_result = {
        let vault_root = vault_root.clone();
        let rel_for_error = rel_path.clone();
        let rel_path = rel_path.clone();
        tokio::task::spawn_blocking(move || {
            onebrain_fs::note::new_note(&vault_root, &rel_path, None, &frontmatter, false)
        })
        .await
        .map_err(|e| sanitized_internal("internal task failure", e.into()))?
        .map_err(|e| match &e {
            // `InvalidTarget` here is ALWAYS the same-day filename collision
            // (`new_note`'s only reachable `InvalidTarget` site under
            // `template: None` with this call's own well-formed
            // frontmatter — see `resolve_create_under_vault`'s doc comment).
            // The pre-check above normally catches it first; this arm is the
            // race (a note created between the check and the write). Either
            // way the client gets THIS crate's message, never `new_note`'s
            // own — that one ends in "(use --force)", a CLI flag no MCP
            // client has any way to pass.
            onebrain_fs::FsError::Core(CoreError::InvalidTarget(_)) => {
                ErrorData::invalid_request(capture_collision_message(&rel_for_error), None)
            }
            // Any OTHER `FsError` (e.g. `Io`, whose `Display` embeds an
            // ABSOLUTE host path) is sanitized instead.
            _ => sanitized_internal("failed to write note", e.into()),
        })?
    };

    // Land the caller's actual `text` at EOF, under the title heading
    // `new_note` just wrote. Unlike the reindex step below, a failure HERE
    // is a hard (sanitized) error, never best-effort: the note's body is
    // the entire point of a "capture" — silently leaving a titled-but-empty
    // stub behind would be a worse silent failure than a stale search
    // index. `append_note` targets the SAME already-guard-confined
    // `rel_path` (not a freshly derived one), so no second confinement
    // check is needed — `resolve_create_under_vault` already proved this
    // exact relative path is safe.
    {
        let vault_root = vault_root.clone();
        let rel_path = rel_path.clone();
        let text = params.text.clone();
        tokio::task::spawn_blocking(move || {
            onebrain_fs::note::append_note(&vault_root, &rel_path, &text, None)
        })
        .await
        .map_err(|e| sanitized_internal("internal task failure", e.into()))?
        .map_err(|e| sanitized_internal("failed to write note body", e.into()))?;
    }

    // Best-effort reindex so `brain_search` can find the note right away.
    // Never fails the capture itself — by this point the note is ALREADY
    // written, and a reindex hiccup is not a reason to tell the caller
    // nothing happened (which could prompt a retry straight into the
    // same-day filename collision above). A missed reindex here just means
    // `brain_search` lags until the vault's next scheduled/hook reindex.
    //
    // DETACHED: the `JoinHandle` is dropped without `.await`, so the tool
    // call returns as soon as the note is on disk. `ensure_running` spawns
    // `onebrain daemon start` and polls it for up to its own 10s
    // START_TIMEOUT — awaiting that would hold the MCP call open for a full
    // daemon cold start (or the whole timeout, on failure) AFTER the result
    // is already fully determined. "Best effort" has to mean best effort in
    // latency too, not only in error handling. Dropping a `spawn_blocking`
    // handle does not cancel the task, so the reindex still runs to
    // completion and its `tracing::warn!`s still reach the operator.
    if reindex_channel_enabled() {
        let vault_root = vault_root.clone();
        let doc_path = new_result.path.clone();
        drop(tokio::task::spawn_blocking(
            move || match daemon_client::ensure_running(Some(&vault_root)) {
                Ok(handle) => {
                    if let Err(e) = handle.reindex("paths", &[doc_path]) {
                        tracing::warn!(error = %e, "brain_capture: best-effort reindex request failed; note is written, brain_search may lag until the next reindex");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "brain_capture: could not reach daemon for best-effort reindex; note is written, brain_search may lag until the next reindex");
                }
            },
        ));
    }

    Ok(Json(BrainCaptureOut {
        path: new_result.path,
    }))
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for GatewayServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "OneBrain Gateway — Brain pack. Call `capabilities` first to see packs and \
                 vaults. `brain_search` finds notes, `brain_get` reads one, `brain_tasks` lists \
                 open tasks, `brain_capture` writes a new inbox note (may require interactive \
                 approval, per gateway policy). Vault-relative paths; select a vault by name via \
                 the `vault` argument or omit it for the default vault.",
            )
            .with_server_info(Implementation::new(
                "onebrain-gateway",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
    }
}

/// Assembles the gateway's full HTTP surface (Gateway PR 3, Task 2 adds
/// OAuth discovery, Task 3 adds registration, Task 4 adds the consent flow,
/// Task 5 adds the token endpoint, Gateway PR 4 Task 3 adds the operator
/// approval surface): a sessionless (SEP-2567) Streamable HTTP service
/// mounted at `/mcp`, gated by the [`require_bearer`] Bearer resource-server
/// check, plus the PUBLIC `/.well-known/*` OAuth discovery routes
/// ([`well_known_router`]), the PUBLIC `POST /register` RFC 7591
/// registration route ([`register_router`]), the PUBLIC `GET`/`POST
/// /authorize` consent flow ([`authorize_router`]), the PUBLIC `POST
/// /token` code-exchange/refresh-rotation endpoint ([`token_router`]), and
/// the OPERATOR-ONLY `/approvals` surface ([`approval_router`] —
/// deliberately NOT public and deliberately NOT behind [`require_bearer`]
/// either; see `approval_routes.rs`'s module docs for why it needs its OWN
/// third kind of gate). The `/mcp` factory closure builds a fresh
/// [`GatewayServer`] per request (cloning the shared `state` handle) —
/// sessionless mode never reuses a server instance across requests, so no
/// mutable per-connection state can leak between callers.
///
/// Layer scoping is load-bearing here: `.layer(from_fn_with_state(...))` is
/// called on the router BEFORE `well_known_router`/`register_router`/
/// `authorize_router`/`token_router`/`approval_router` are merged in, so the
/// Bearer gate wraps ONLY the `/mcp` nest — a client with no token yet can
/// still reach the discovery documents, register itself, complete the
/// consent flow, AND exchange its code (or rotate a refresh token) for a
/// bearer token, all of which it needs to do before it can obtain/renew
/// one; `/approvals` similarly stays OUTSIDE the Bearer layer, but for the
/// opposite reason — not because it's public (it isn't; `approval_router`
/// carries its own pairing-code gate), but because a connector's bearer
/// token must never be ABLE to satisfy it (see `approval_routes.rs`'s
/// module docs). See
/// `tests::well_known_routes_are_reachable_without_auth_while_mcp_stays_gated`,
/// `tests::register_is_reachable_without_auth_on_the_real_router`,
/// `tests::authorize_is_reachable_without_auth_on_the_real_router`,
/// `tests::token_is_reachable_without_auth_on_the_real_router`, and
/// `tests::approvals_route_is_merged_into_the_real_router_and_ignores_a_connector_bearer_token`
/// for the proof.
pub fn build_gateway_router(state: Arc<GatewayState>, auth_ctx: Arc<AuthCtx>) -> axum::Router {
    // Cloned BEFORE the `move` closure below takes ownership of `state` for
    // the `/mcp` factory's own per-request `state.clone()` — `approval_router`
    // needs its own handle on the SAME `Arc<GatewayState>` afterward.
    let approvals_state = state.clone();

    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    let service: StreamableHttpService<GatewayServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(GatewayServer::new(state.clone())),
            Default::default(),
            config,
        );
    let mcp_router = axum::Router::new().nest_service("/mcp", service).layer(
        axum::middleware::from_fn_with_state(auth_ctx.clone(), require_bearer),
    );
    mcp_router
        .merge(well_known_router(auth_ctx.clone()))
        .merge(register_router(auth_ctx.clone()))
        .merge(authorize_router(auth_ctx.clone()))
        .merge(token_router(auth_ctx.clone()))
        .merge(approval_router(approvals_state, auth_ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;
    use tower::ServiceExt;

    use crate::commands::gateway::auth::AuthStore;

    const PROTOCOL: &str = "2026-07-28";

    /// Builds an `AuthCtx` with one live access token (client "test-client",
    /// scope "brain") and a fixed test issuer, so a fixture router's `/mcp`
    /// can pass its Bearer gate. `root` only needs to be a fresh directory
    /// the auth store can use — the caller's own vault fixture files live
    /// alongside it in the SAME tempdir, under a distinct `gateway-auth/`
    /// subdirectory.
    fn test_auth_ctx(root: &Path) -> (Arc<AuthCtx>, String) {
        let store = AuthStore::open_at(root.join("gateway-auth")).unwrap();
        let (access, _refresh) = store.issue_token_pair("test-client", "brain").unwrap();
        let ctx = Arc::new(AuthCtx::new(store));
        ctx.issuer
            .set("http://127.0.0.1:7717".to_string())
            .expect("issuer set once on a fresh AuthCtx");
        (ctx, access.token)
    }

    /// Ruling B (Task 4 review): `sanitized_internal` must strip the daemon
    /// error's full detail — including any absolute host path it embeds,
    /// e.g. `ensure_running`'s "See <slot log path>" on a start timeout —
    /// down to a fixed context string plus a pointer at the server-side log,
    /// never echoing `err`'s own text back to the network client.
    #[test]
    fn sanitized_internal_strips_host_detail_from_the_client_facing_message() {
        let err = anyhow::anyhow!(
            "daemon did not become ready within 10s; last state: dead. \
             See /Users/keng/.onebrain/run/daemon-abc123.log"
        );
        let data = sanitized_internal("search backend unavailable", err);
        assert_eq!(
            data.message.as_ref(),
            "search backend unavailable — see gateway logs"
        );
        assert!(
            !data.message.contains("/Users/keng/.onebrain"),
            "client-facing message must not leak the host path: {}",
            data.message
        );
        assert!(
            !data.message.contains("daemon did not become ready"),
            "client-facing message must not echo the underlying error text: {}",
            data.message
        );
    }

    /// Final-review fix (Item 1): `core_error` must never forward a
    /// `CoreError`'s own `Display` text — `VaultNotFound { cwd }` embeds the
    /// process cwd and `NotAVault { path }` embeds the configured path, both
    /// host filesystem details that must not reach a network client. The
    /// stable `E_*` code must still be present (the binary integration test
    /// `gateway_run_outside_vault_brain_tasks_returns_vault_not_found_error`
    /// only substring-matches the code, so this stays compatible).
    #[test]
    fn core_error_drops_host_paths_but_keeps_the_e_code() {
        let cwd = PathBuf::from("/Users/keng/super-secret-cwd");
        let err = CoreError::VaultNotFound { cwd: cwd.clone() };
        let code = err.error_code();
        let data = core_error(err);
        assert_eq!(
            data.message.as_ref(),
            format!("no OneBrain vault resolved for this call [{code}]")
        );
        assert!(
            !data.message.contains("super-secret-cwd"),
            "must not leak the cwd: {}",
            data.message
        );

        let path = PathBuf::from("/Users/keng/another-secret-path");
        let err = CoreError::NotAVault { path: path.clone() };
        let code = err.error_code();
        let data = core_error(err);
        assert_eq!(
            data.message.as_ref(),
            format!("configured path is not a OneBrain vault [{code}]")
        );
        assert!(
            !data.message.contains("another-secret-path"),
            "must not leak the configured path: {}",
            data.message
        );
    }

    /// Where [`fixture_router`] (and the other fixture builders below) open
    /// the audit log — under the SAME tempdir the caller already gets back,
    /// at a fixed, well-known subpath (mirrors `test_auth_ctx`'s
    /// `"gateway-auth"` convention) so a test can reopen/read it back via
    /// `audit_log_path(dir.path())` without `fixture_router` needing to
    /// return a fourth value.
    fn audit_log_path(root: &Path) -> PathBuf {
        root.join("gateway-audit")
    }

    /// Builds a fixture vault (`onebrain.yml` + one dated task in
    /// `01-projects/x.md`, plus the same line fenced — which must NOT count)
    /// and a router whose gateway config names it `t1` and sets it default.
    fn fixture_router() -> (tempfile::TempDir, axum::Router, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("onebrain.yml"), "folders: {}\n").unwrap();
        std::fs::create_dir_all(root.join("01-projects")).unwrap();
        std::fs::write(
            root.join("01-projects/x.md"),
            "- [ ] gateway fixture task 📅 2026-01-01\n\n\
             ```\n\
             - [ ] gateway fixture task 📅 2026-01-01\n\
             ```\n",
        )
        .unwrap();

        let mut vaults = BTreeMap::new();
        vaults.insert("t1".to_string(), root.to_path_buf());
        let config = GatewayConfig {
            default_vault: Some(root.to_path_buf()),
            vaults,
            ..GatewayConfig::default()
        };
        let audit = AuditLog::open_at(audit_log_path(root)).unwrap();
        let state = Arc::new(GatewayState::new(config, audit));
        let (auth_ctx, token) = test_auth_ctx(root);
        (dir, build_gateway_router(state, auth_ctx), token)
    }

    /// POST `body` to `/mcp` with `token` as the `Authorization: Bearer`
    /// credential, plus the given extra headers (beyond the baseline
    /// content-type/accept/host/authorization every request needs — the
    /// Streamable HTTP service's DNS-rebinding guard 400s any request with
    /// no `Host` header and no URI authority, and `oneshot` supplies
    /// neither by default; `/mcp` now also 401s without a valid bearer
    /// token, per Gateway PR 3, Task 2).
    async fn post(
        router: &axum::Router,
        body: String,
        token: &str,
        extra: &[(&str, &str)],
    ) -> serde_json::Value {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {token}"));
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(Body::from(body)).unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "response was not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    fn init_body(id: u32, protocol_version: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"},
            },
        })
        .to_string()
    }

    fn call_body(id: u32, tool: &str, arguments: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string()
    }

    /// Headers required on every non-`initialize` request once the
    /// `MCP-Protocol-Version` header is `>= 2026-07-28` (SEP-2243): the
    /// vendored crate's own `validate_standard_headers` 400s a `tools/list`
    /// or `tools/call` with a missing/mismatched `Mcp-Method` (and, for
    /// `tools/call`, `Mcp-Name`) — see
    /// `rmcp-3.0.1/tests/test_streamable_http_standard_headers.rs`.
    fn standard_headers<'a>(method: &'a str, name: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut headers = vec![("MCP-Protocol-Version", PROTOCOL), ("Mcp-Method", method)];
        if let Some(name) = name {
            headers.push(("Mcp-Name", name));
        }
        headers
    }

    #[tokio::test]
    async fn initialize_pins_protocol_2026_07_28() {
        let (_dir, router, token) = fixture_router();
        let resp = post(
            &router,
            init_body(1, PROTOCOL),
            &token,
            &[("MCP-Protocol-Version", PROTOCOL)],
        )
        .await;
        assert_eq!(
            resp["result"]["protocolVersion"], PROTOCOL,
            "pin guard: {resp}"
        );
        assert_eq!(resp["result"]["serverInfo"]["name"], "onebrain-gateway");
    }

    #[tokio::test]
    async fn initialize_echoes_2025_11_25_dual_era_over_http() {
        let (_dir, router, token) = fixture_router();
        let resp = post(
            &router,
            init_body(1, "2025-11-25"),
            &token,
            &[("MCP-Protocol-Version", "2025-11-25")],
        )
        .await;
        assert_eq!(resp["result"]["protocolVersion"], "2025-11-25", "{resp}");
    }

    #[tokio::test]
    async fn tools_list_contains_capabilities_and_brain_tasks() {
        let (_dir, router, token) = fixture_router();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {},
        })
        .to_string();
        let resp = post(&router, body, &token, &standard_headers("tools/list", None)).await;
        let tools = resp["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tools array: {resp}"));
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"capabilities"), "{names:?}");
        assert!(names.contains(&"brain_tasks"), "{names:?}");

        // Final-review fix (Item 2): every READ-ONLY Brain-pack tool must
        // carry `annotations.readOnlyHint == true` — parsed as JSON (not
        // substring-matched) so a tool that carries the hint nested
        // somewhere unexpected, or omits it, fails clearly. `brain_capture`
        // is deliberately absent from this list: it is the pack's one
        // `Mutating` tool and carries `readOnlyHint: false`, pinned by
        // `brain_capture_is_listed_with_read_only_hint_false`.
        let expected = ["capabilities", "brain_tasks", "brain_get", "brain_search"];
        for name in expected {
            let tool = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("tools/list missing `{name}`: {names:?}"));
            assert_eq!(
                tool["annotations"]["readOnlyHint"],
                serde_json::Value::Bool(true),
                "`{name}` must carry readOnlyHint:true: {tool}"
            );
        }
    }

    #[tokio::test]
    async fn capabilities_reports_brain_enabled_developer_disabled() {
        let (_dir, router, token) = fixture_router();
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let packs = resp["result"]["structuredContent"]["packs"]
            .as_array()
            .unwrap_or_else(|| panic!("no structuredContent.packs: {resp}"));
        let brain = packs.iter().find(|p| p["name"] == "brain").unwrap();
        assert_eq!(brain["enabled"], true, "{resp}");
        let developer = packs.iter().find(|p| p["name"] == "developer").unwrap();
        assert_eq!(developer["enabled"], false, "{resp}");
    }

    // ── Gateway PR 4, Task 6: capabilities truthfulness ──────────────────

    /// Every `brain` pack tool must report its real [`RiskClass`] AND the
    /// EFFECTIVE [`PolicyMode`] the live `gateway.yml` resolves that class
    /// to — not just a bare name. Uses a custom policy (`mutating: deny`,
    /// `read_only` left at its `auto` default) so the two risk classes
    /// resolve to VISIBLY DIFFERENT modes, proving `capabilities` reports
    /// the per-tool lookup rather than one fixed value copy-pasted onto
    /// every entry.
    #[tokio::test]
    async fn capabilities_reports_risk_class_and_effective_policy_mode_per_tool() {
        let (_dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::Deny, 300, 30);
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let packs = resp["result"]["structuredContent"]["packs"]
            .as_array()
            .unwrap_or_else(|| panic!("no structuredContent.packs: {resp}"));
        let brain_tools = packs
            .iter()
            .find(|p| p["name"] == "brain")
            .unwrap_or_else(|| panic!("no brain pack: {resp}"))["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("brain pack has no tools array: {resp}"))
            .clone();

        let find = |name: &str| {
            brain_tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| {
                    panic!("tool {name:?} missing from capabilities: {brain_tools:?}")
                })
        };

        let capabilities_tool = find("capabilities");
        assert_eq!(capabilities_tool["risk_class"], "read_only", "{resp}");
        assert_eq!(
            capabilities_tool["policy_mode"], "auto",
            "read_only tools must report the default auto mode: {resp}"
        );

        let brain_capture_tool = find("brain_capture");
        assert_eq!(brain_capture_tool["risk_class"], "mutating", "{resp}");
        assert_eq!(
            brain_capture_tool["policy_mode"], "deny",
            "brain_capture must report THIS config's overridden deny mode, not the default: {resp}"
        );

        // Every listed tool name is present, so a caller can't be surprised
        // by a tool `tools/list` advertises that `capabilities` never
        // mentions.
        let names: Vec<&str> = brain_tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "capabilities",
            "brain_tasks",
            "brain_get",
            "brain_search",
            "brain_capture",
        ] {
            assert!(names.contains(&expected), "{names:?}");
        }
    }

    /// `http` is unconditionally `true` (the `/approvals` surface is always
    /// mounted by `build_gateway_router` in this build) and `telegram` is
    /// unconditionally `false` (not implemented yet — Gateway PR 5),
    /// regardless of policy configuration or platform.
    #[tokio::test]
    async fn capabilities_reports_http_channel_true_and_telegram_false() {
        let (_dir, router, token) = fixture_router();
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let channels = &resp["result"]["structuredContent"]["approval_channels"];
        assert_eq!(channels["http"], true, "{resp}");
        assert_eq!(channels["telegram"], false, "{resp}");
        assert!(
            channels["note"]
                .as_str()
                .unwrap_or_default()
                .contains("PR 5"),
            "the note must explain telegram's deferral: {resp}"
        );
    }

    /// `capabilities`'s reported `native` availability must match
    /// `approval_native::is_available()` exactly on THIS machine — the
    /// binding truthfulness requirement: a caller must never be told a
    /// channel is available when it isn't (or vice versa). Holds the
    /// crate-wide `test_env` lock across both reads (the direct call here
    /// and the one `capabilities`'s own handler makes internally) purely to
    /// serialize against `approval_native`'s own env-var-mutating tests —
    /// see that module's identical-purpose guard on
    /// `is_available_is_true_on_macos_with_osascript_on_path`.
    #[tokio::test]
    async fn capabilities_native_channel_matches_approval_native_is_available() {
        let _env = crate::test_env::set_vars(&[]);
        let (_dir, router, token) = fixture_router();
        let expected = approval_native::is_available();
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        assert_eq!(
            resp["result"]["structuredContent"]["approval_channels"]["native"], expected,
            "{resp}"
        );
    }

    /// The binding requirement in concrete form: explicitly disabling the
    /// native channel (the exact mechanism `tests/gateway_approval_e2e.rs`
    /// relies on to keep its spawned gateway subprocess headless) must make
    /// `capabilities` report `native: false` — regardless of what platform
    /// this test itself runs on, and regardless of whether `osascript` is
    /// really on `$PATH`. A caller must never be told a write CAN be
    /// approved through a channel that cannot actually deliver the prompt.
    #[tokio::test]
    async fn capabilities_reports_native_channel_false_when_explicitly_disabled() {
        let _env = crate::test_env::set_var(approval_native::DISABLE_NATIVE_APPROVAL_ENV, "1");
        let (_dir, router, token) = fixture_router();
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        assert_eq!(
            resp["result"]["structuredContent"]["approval_channels"]["native"], false,
            "{resp}"
        );
    }

    /// Final-review fix (Item 1): `fixture_router`'s `default_vault` is the
    /// SAME path as its `t1` entry, so `capabilities` must report the name
    /// `"t1"`, never the raw path — proves `default_vault_display` resolves
    /// through the name lookup rather than falling back to the path.
    #[tokio::test]
    async fn capabilities_default_vault_reports_the_matching_name_not_a_raw_path() {
        let (dir, router, token) = fixture_router();
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["default_vault"], "t1", "{resp}");
        assert!(
            !resp.to_string().contains(&dir.path().display().to_string()),
            "capabilities must not leak the raw vault path: {resp}"
        );
    }

    /// Final-review fix (Item 1): when `default_vault` does NOT match any
    /// named `config.vaults` entry, `capabilities` must report the fixed
    /// marker `"(configured)"` — never the raw configured path.
    #[tokio::test]
    async fn capabilities_default_vault_falls_back_to_configured_marker_when_unnamed() {
        let dir = tempfile::tempdir().unwrap();
        let default_root = dir.path().join("unnamed-default");
        std::fs::create_dir_all(&default_root).unwrap();
        std::fs::write(default_root.join("onebrain.yml"), "folders: {}\n").unwrap();

        let named_root = dir.path().join("named");
        std::fs::create_dir_all(&named_root).unwrap();
        std::fs::write(named_root.join("onebrain.yml"), "folders: {}\n").unwrap();

        let mut vaults = BTreeMap::new();
        vaults.insert("named".to_string(), named_root);
        let config = GatewayConfig {
            default_vault: Some(default_root.clone()),
            vaults,
            ..GatewayConfig::default()
        };
        let audit = AuditLog::open_at(audit_log_path(dir.path())).unwrap();
        let state = Arc::new(GatewayState::new(config, audit));
        let (auth_ctx, token) = test_auth_ctx(dir.path());
        let router = build_gateway_router(state, auth_ctx);

        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["default_vault"], "(configured)", "{resp}");
        assert!(
            !resp
                .to_string()
                .contains(&default_root.display().to_string()),
            "capabilities must not leak the raw default_vault path: {resp}"
        );
    }

    #[tokio::test]
    async fn brain_tasks_counts_the_unfenced_dated_task_only() {
        let (_dir, router, token) = fixture_router();
        let body = call_body(
            1,
            "brain_tasks",
            serde_json::json!({"due_by": "2026-12-31"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_tasks")),
        )
        .await;
        let out = &resp["result"]["structuredContent"];
        assert_eq!(out["total"], 1, "{resp}");
        let tasks = out["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{resp}");
        assert!(
            tasks[0]["text"]
                .as_str()
                .unwrap()
                .contains("gateway fixture task"),
            "{resp}"
        );
    }

    /// The SUCCESS half of `resolve_vault_arg`'s `Some(name)` branch (every
    /// other test either fails on an unknown name or omits `vault` entirely,
    /// falling through to `default_vault`) — proves a named-vault lookup
    /// actually resolves and serves the RIGHT vault, not just that an
    /// unrecognised name is rejected. Passing no `due_by` also exercises
    /// `brain_tasks`'s cutoff-omitted branch (every other `brain_tasks` test
    /// sets one).
    #[tokio::test]
    async fn brain_tasks_resolves_a_named_vault_with_no_due_by_filter() {
        let (_dir, router, token) = fixture_router();
        let body = call_body(1, "brain_tasks", serde_json::json!({"vault": "t1"}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_tasks")),
        )
        .await;
        let out = &resp["result"]["structuredContent"];
        assert_eq!(out["total"], 1, "{resp}");
        assert!(
            out["tasks"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("gateway fixture task"),
            "{resp}"
        );
    }

    #[tokio::test]
    async fn brain_tasks_unknown_vault_names_known_vaults_in_the_error() {
        let (_dir, router, token) = fixture_router();
        let body = call_body(1, "brain_tasks", serde_json::json!({"vault": "nope"}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_tasks")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("t1"), "{message}");
    }

    /// Sentinel content for the adversarial `brain_get` tests: a real file
    /// planted OUTSIDE the vault whose text must never appear in any
    /// response, no matter how the traversal is spelled.
    const OUTSIDE_SENTINEL: &str = "TOP-SECRET-OUTSIDE-VAULT-CONTENT-DO-NOT-LEAK";

    /// Like [`fixture_router`], but nests the vault one level inside the
    /// returned `TempDir` (`<tempdir>/vault/`) and writes a sentinel file at
    /// `<tempdir>/outside.md`, one level ABOVE the vault root — the fixture
    /// the brief's adversarial `brain_get` tests need to prove traversal
    /// attempts both error out AND never leak this file's content. Also
    /// creates an empty `vault/a/` subdirectory so `a/../../outside.md`
    /// exercises the same "canonicalizes fine, then escapes the vault" path
    /// as `../outside.md`, rather than failing earlier for an unrelated
    /// reason (a nonexistent `a/` component).
    fn fixture_with_outside_file() -> (tempfile::TempDir, axum::Router, String) {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("vault");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("onebrain.yml"), "folders: {}\n").unwrap();
        std::fs::write(root.join("hello.md"), "hello from inside the vault\n").unwrap();
        std::fs::write(workspace.path().join("outside.md"), OUTSIDE_SENTINEL).unwrap();

        let mut vaults = BTreeMap::new();
        vaults.insert("t1".to_string(), root.clone());
        let config = GatewayConfig {
            default_vault: Some(root),
            vaults,
            ..GatewayConfig::default()
        };
        let audit = AuditLog::open_at(audit_log_path(workspace.path())).unwrap();
        let state = Arc::new(GatewayState::new(config, audit));
        let (auth_ctx, token) = test_auth_ctx(workspace.path());
        (workspace, build_gateway_router(state, auth_ctx), token)
    }

    #[tokio::test]
    async fn brain_get_round_trips_fixture_note() {
        let (_dir, router, token) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "hello.md"}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content: {resp}"));
        assert!(text.contains("hello from inside the vault"), "{resp}");
    }

    #[tokio::test]
    async fn brain_get_unknown_file_is_invalid_params() {
        let (_dir, router, token) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "nope.md"}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("file not found in vault"), "{message}");
    }

    /// Adversarial: `../outside.md` — a single `..` segment climbing out of
    /// the vault root to a real, existing file. Must error AND must not leak
    /// the sentinel content anywhere in the JSON-RPC response.
    #[tokio::test]
    async fn brain_get_rejects_parent_traversal_without_leaking() {
        let (_dir, router, token) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "../outside.md"}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        assert!(
            resp.get("error").is_some(),
            "expected a JSON-RPC error: {resp}"
        );
        assert!(
            !resp.to_string().contains(OUTSIDE_SENTINEL),
            "sentinel content leaked: {resp}"
        );
    }

    /// Adversarial: an absolute path. `resolve_under_vault` (mirroring
    /// `mcp.rs`) discards the vault root when joining an absolute `rel`, so
    /// this must be caught by the post-canonicalize `starts_with` check —
    /// must error AND must not leak `/etc/hosts`'s actual contents.
    ///
    /// Hardened per Task 4 review: rather than assuming `/etc/hosts` contains
    /// a specific line (`127.0.0.1`, which isn't guaranteed on every CI
    /// runner/container base image), read the file's LIVE content at test
    /// setup and assert the response doesn't contain its actual first
    /// non-empty line. If the file is unreadable or empty (sandboxed CI, an
    /// unusual container), skip the content assertion gracefully — the
    /// traversal-rejection assertion (the actual security property under
    /// test) still runs unconditionally either way.
    #[tokio::test]
    async fn brain_get_rejects_absolute_path_without_leaking() {
        let (_dir, router, token) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "/etc/hosts"}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        assert!(
            resp.get("error").is_some(),
            "expected a JSON-RPC error: {resp}"
        );

        if let Ok(hosts) = std::fs::read_to_string("/etc/hosts") {
            if let Some(first_line) = hosts.lines().map(str::trim).find(|l| !l.is_empty()) {
                assert!(
                    !resp.to_string().contains(first_line),
                    "content leaked: {resp}"
                );
            }
        }
    }

    /// Adversarial: a nested `..` traversal through a real intermediate
    /// directory (`a/../../outside.md`) landing on the same outside file as
    /// the plain `../outside.md` case. Must error AND must not leak.
    #[tokio::test]
    async fn brain_get_rejects_nested_traversal_without_leaking() {
        let (_dir, router, token) = fixture_with_outside_file();
        let body = call_body(
            1,
            "brain_get",
            serde_json::json!({"file": "a/../../outside.md"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        assert!(
            resp.get("error").is_some(),
            "expected a JSON-RPC error: {resp}"
        );
        assert!(
            !resp.to_string().contains(OUTSIDE_SENTINEL),
            "sentinel content leaked: {resp}"
        );
    }

    /// `brain_search` with an unknown vault name errors out of
    /// `resolve_vault_arg` before any daemon interaction — exercises the
    /// shared resolver's error path without spawning a daemon (the
    /// daemon-backed happy path is Task 4's binary integration test).
    #[tokio::test]
    async fn brain_search_unknown_vault_names_known_vaults_in_the_error() {
        let (_dir, router, token) = fixture_router();
        let body = call_body(
            1,
            "brain_search",
            serde_json::json!({"query": "anything", "vault": "nope"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_search")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("t1"), "{message}");
    }

    // ── Gateway PR 3, Task 2: OAuth wiring on the REAL router ────────────
    //
    // `middleware.rs`'s own tests cover `require_bearer`'s exact 401 shapes
    // (no-token vs. bad-token, expired, revoked) against a synthetic `/mcp`
    // stand-in route; `oauth_routes.rs`'s own tests cover the well-known
    // documents' exact field sets against a bare `well_known_router()`. What
    // belongs HERE, against `build_gateway_router`'s real composition (the
    // actual rmcp `nest_service` + the actual merge with the well-known
    // routes), is the end-to-end proof that the two are wired together
    // correctly: the Bearer layer really does cover the real `/mcp` route,
    // and really does NOT cover the well-known routes once merged in.

    /// `/mcp` on the fully-assembled router 401s with no bearer token —
    /// confirms the gate is actually attached to the REAL rmcp-backed `/mcp`
    /// route, not just a synthetic stand-in.
    #[tokio::test]
    async fn mcp_without_bearer_token_401s_on_the_real_router() {
        let (_dir, router, _token) = fixture_router();
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(init_body(1, PROTOCOL)))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Layer-scoping proof (Step 2 of the brief): on ONE `build_gateway_router`
    /// output, every `/.well-known/*` discovery route answers 200 with no
    /// `Authorization` header at all, while `/mcp` on that SAME router
    /// instance stays gated — proving the `.merge()` in `build_gateway_router`
    /// adds the well-known routes WITHOUT the Bearer layer wrapping them
    /// (the layer is applied to the `/mcp` sub-router before the merge, so it
    /// can't leak onto routes merged in afterward — but this is the test that
    /// actually proves it, rather than just asserting it in a comment).
    #[tokio::test]
    async fn well_known_routes_are_reachable_without_auth_while_mcp_stays_gated() {
        let (_dir, router, _token) = fixture_router();

        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ] {
            let req = Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path} must be public");
        }

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(init_body(1, PROTOCOL)))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "/mcp must still be gated on the SAME router instance"
        );
    }

    /// Gateway PR 3, Task 3: `POST /register` on the fully-assembled router
    /// answers with no `Authorization` header at all — proves `register_router`
    /// really is merged the same way `well_known_router` is (after the Bearer
    /// layer is applied to `/mcp`, so the layer never wraps it), against the
    /// REAL merged router rather than `oauth_routes.rs`'s own bare
    /// `register_router()` fixture.
    #[tokio::test]
    async fn register_is_reachable_without_auth_on_the_real_router() {
        let (_dir, router, _token) = fixture_router();
        let req = Request::builder()
            .method("POST")
            .uri("/register")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
                })
                .to_string(),
            ))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "/register must be public — no Authorization header was sent"
        );
    }

    /// Gateway PR 3, Task 4: `GET /authorize` on the fully-assembled router
    /// answers with no `Authorization` header at all — proves
    /// `authorize_router` is merged the same way `well_known_router`/
    /// `register_router` are (after the Bearer layer is applied to `/mcp`,
    /// so the layer never wraps it). An unknown `client_id` still 400s (no
    /// client was registered against this fixture's store) — the point of
    /// this test is that the response is NOT a 401, proving the route is
    /// reachable with zero credentials, not that a specific client resolves.
    #[tokio::test]
    async fn authorize_is_reachable_without_auth_on_the_real_router() {
        let (_dir, router, _token) = fixture_router();
        let req = Request::builder()
            .method("GET")
            .uri("/authorize?response_type=code&client_id=nope&redirect_uri=https%3A%2F%2Fx.example%2Fcb&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "/authorize must be public — no Authorization header was sent"
        );
    }

    /// Gateway PR 3, Task 5: `POST /token` on the fully-assembled router
    /// answers with no `Authorization` header at all — proves `token_router`
    /// is merged the same way `well_known_router`/`register_router`/
    /// `authorize_router` are (after the Bearer layer is applied to `/mcp`,
    /// so the layer never wraps it). An unsupported `grant_type` still 400s
    /// (nothing was set up to succeed here) — the point of this test is that
    /// the response is NOT a 401, proving the route is reachable with zero
    /// credentials, not that a specific exchange succeeds.
    #[tokio::test]
    async fn token_is_reachable_without_auth_on_the_real_router() {
        let (_dir, router, _token) = fixture_router();
        let req = Request::builder()
            .method("POST")
            .uri("/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("grant_type=client_credentials"))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "/token must be public — no Authorization header was sent"
        );
    }

    // ── Gateway PR 4, Task 2: policy engine + Principal wiring ───────────

    /// Like [`fixture_router`], but lets the caller override
    /// `policy.read_only` — `fixture_router` always uses the DEFAULT policy
    /// (`read_only: auto`), which can never exercise the `Deny`/
    /// `NeedApproval` branches of the Task 2 wiring since all four
    /// originally-shipped tools are `RiskClass::ReadOnly`. `approval_wait_seconds`
    /// is also overridable — a `NeedApproval` test with nothing resolving it
    /// needs a SHORT one (e.g. `0`, an instant timeout), while a `Deny` test
    /// never reaches `await_approval` at all, so the value is irrelevant
    /// there.
    fn fixture_router_with_read_only_policy(
        read_only: policy::PolicyMode,
        approval_wait_seconds: u64,
    ) -> (tempfile::TempDir, axum::Router, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("onebrain.yml"), "folders: {}\n").unwrap();

        let mut vaults = BTreeMap::new();
        vaults.insert("t1".to_string(), root.to_path_buf());
        let config = GatewayConfig {
            default_vault: Some(root.to_path_buf()),
            vaults,
            policy: policy::PolicyConfig {
                read_only,
                approval_wait_seconds,
                ..policy::PolicyConfig::default()
            },
            ..GatewayConfig::default()
        };
        let audit = AuditLog::open_at(audit_log_path(root)).unwrap();
        let state = Arc::new(GatewayState::new(config, audit));
        let (auth_ctx, token) = test_auth_ctx(root);
        (dir, build_gateway_router(state, auth_ctx), token)
    }

    /// Reads every JSONL line back out of the audit log opened at
    /// `audit_log_path(root)`, across every month file present (fixture
    /// tests are short-lived, so in practice this is always exactly one
    /// file), parsed as loose `serde_json::Value`s in file (hence
    /// chronological, since month files sort lexically) then line order.
    fn read_audit_entries(root: &Path) -> Vec<serde_json::Value> {
        let dir = audit_log_path(root);
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .into_iter()
            .flat_map(|f| {
                std::fs::read_to_string(&f)
                    .unwrap_or_default()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// **Step 2 of the brief**: proves the `Extension<http::request::Parts>`
    /// seam actually delivers the RIGHT `Principal` to an rmcp `#[tool]`
    /// handler — not a shared/stale value, and not merely that the
    /// mechanism compiles. `middleware.rs`'s own
    /// `valid_access_token_passes_through_and_sets_principal` already
    /// proves `Extension<Principal>` reaches a PLAIN axum handler; this is
    /// the analogous proof for the REAL rmcp-backed `/mcp` route, which is
    /// the load-bearing case this whole PR's policy/audit wiring depends
    /// on.
    ///
    /// Mechanism: `capabilities` (the tool `Extension<http::request::Parts>`
    /// was added to first, per the brief's ordering) is called twice on the
    /// SAME router/service instance with two DIFFERENT bearer tokens minted
    /// for two different `client_id`s. Each call's audit-log entry records
    /// `principal.client_id` as read out of `parts.extensions` inside the
    /// live handler — if the seam were broken (returning a stale, default,
    /// or shared value), both entries would show the same `client_id`
    /// regardless of which token was presented. They don't.
    #[tokio::test]
    async fn capabilities_extension_seam_delivers_the_right_principal_per_request() {
        let (dir, router, token_a) = fixture_router();

        // Mint a second access token for a DIFFERENT client against the
        // SAME on-disk store `fixture_router`'s `test_auth_ctx` already
        // opened at `<root>/gateway-auth` — reopening it here is simpler
        // than threading `AuthCtx` itself out of `fixture_router`, and the
        // store's persistence is plain file I/O (no exclusive open lock),
        // same as `middleware.rs`'s own tests reaching under a live store
        // via its on-disk `tokens.json`.
        let token_b = {
            let store = AuthStore::open_at(dir.path().join("gateway-auth")).unwrap();
            let (access, _refresh) = store.issue_token_pair("client-b", "brain").unwrap();
            access.token
        };

        let body_a = call_body(1, "capabilities", serde_json::json!({}));
        let resp_a = post(
            &router,
            body_a,
            &token_a,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        assert!(resp_a.get("error").is_none(), "{resp_a}");

        let body_b = call_body(2, "capabilities", serde_json::json!({}));
        let resp_b = post(
            &router,
            body_b,
            &token_b,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        assert!(resp_b.get("error").is_none(), "{resp_b}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(
            entries[0]["client_id"], "test-client",
            "first call's audit entry must carry the FIRST token's own client_id: {entries:?}"
        );
        assert_eq!(
            entries[1]["client_id"], "client-b",
            "second call's audit entry must carry the SECOND token's own client_id, \
             proving the handler read THIS request's Principal, not a stale/shared one: {entries:?}"
        );
    }

    /// Step 3: every existing tool is `RiskClass::ReadOnly`, which defaults
    /// to `PolicyMode::Auto` — so under the DEFAULT `gateway.yml`, a call
    /// must behave EXACTLY as before (this task's "no behavior change"
    /// requirement) while still producing an audit entry with
    /// `decision: "auto"` and `outcome: "ok"`.
    #[tokio::test]
    async fn brain_tasks_under_default_policy_is_unchanged_and_audited_as_auto() {
        let (dir, router, token) = fixture_router();
        let body = call_body(
            1,
            "brain_tasks",
            serde_json::json!({"due_by": "2026-12-31"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_tasks")),
        )
        .await;
        let out = &resp["result"]["structuredContent"];
        assert_eq!(out["total"], 1, "{resp}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["tool"], "brain_tasks");
        assert_eq!(entries[0]["client_id"], "test-client");
        assert_eq!(entries[0]["decision"], "auto");
        assert_eq!(entries[0]["outcome"], "ok");
        assert!(
            entries[0]["args_summary"]
                .as_str()
                .unwrap()
                .contains("2026-12-31"),
            "{entries:?}"
        );
    }

    /// A `policy.read_only: deny` config must refuse `capabilities` outright
    /// — a JSON-RPC error, never the normal structured result — and the
    /// audit entry must record `decision: "denied"` / `outcome: "error"`.
    #[tokio::test]
    async fn capabilities_is_denied_when_policy_read_only_is_deny() {
        let (dir, router, token) =
            fixture_router_with_read_only_policy(policy::PolicyMode::Deny, 300);
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("policy denies"), "{message}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "denied");
        assert_eq!(entries[0]["outcome"], "error");
    }

    /// A `policy.read_only: ask_once` config with NO prior grant and NOTHING
    /// answering (`approval_wait_seconds: 0`, so `Approvals::wait` times out
    /// on its very first poll with no real waiting) must surface as a
    /// timeout client error and an audit `decision: "timedout"` — Gateway PR
    /// 4, Task 5 wired `policy_gate`'s `NeedApproval` arm to the real
    /// [`Approvals`] registry (previously a fixed "not yet supported"
    /// error, before this task existed); it must still never silently allow
    /// and never hang forever regardless.
    #[tokio::test]
    async fn capabilities_times_out_when_policy_needs_approval_and_nothing_answers() {
        let (dir, router, token) =
            fixture_router_with_read_only_policy(policy::PolicyMode::AskOnce, 0);
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("timed out"), "{message}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "timedout");
        assert_eq!(entries[0]["outcome"], "error");
    }

    /// End-to-end proof of the brief's scope-vs-pack requirement: a token
    /// whose scope does NOT cover the `"brain"` pack must be denied even
    /// though `policy.read_only` is the default, most-permissive `auto` —
    /// the scope check in `decide` runs BEFORE the mode dispatch and wins
    /// regardless of mode. `policy.rs`'s own `scope_mismatch_denies_even_under_auto`
    /// unit test proves this for `decide` in isolation; this is the same
    /// property proven through the real HTTP + rmcp tool-call path.
    #[tokio::test]
    async fn capabilities_is_denied_end_to_end_when_token_scope_does_not_cover_the_brain_pack() {
        let (dir, router, _token) = fixture_router();
        let store = AuthStore::open_at(dir.path().join("gateway-auth")).unwrap();
        let (access, _refresh) = store.issue_token_pair("client-c", "other-pack").unwrap();

        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
            &access.token,
            &standard_headers("tools/call", Some("capabilities")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("policy denies"), "{message}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "denied");
    }

    // ── Gateway PR 4, Task 3: /approvals router-merge + audit hardening ──

    /// `GET /approvals` on the fully-assembled router answers 401 with a
    /// connector's normal bearer token (no `X-OneBrain-Pairing` header) —
    /// proves `approval_routes::approval_router` really is merged into
    /// `build_gateway_router`'s output, and that the merge does NOT
    /// accidentally route it through `require_bearer` (the connector Bearer
    /// layer that wraps ONLY `/mcp`). The pairing-code-specific 401/200
    /// details are `approval_routes.rs`'s own tests' job; this is the
    /// wiring proof, the same role
    /// `well_known_routes_are_reachable_without_auth_while_mcp_stays_gated`
    /// plays for the OAuth discovery routes.
    #[tokio::test]
    async fn approvals_route_is_merged_into_the_real_router_and_ignores_a_connector_bearer_token() {
        let (_dir, router, token) = fixture_router();

        let req = Request::builder()
            .method("GET")
            .uri("/approvals")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "a connector's own bearer token must not satisfy /approvals' operator gate"
        );
    }

    /// Task 3 review, binding requirement B: a gateway tool handler running
    /// with no `Principal` in the request's extensions (believed
    /// unreachable through `build_gateway_router` today — `require_bearer`
    /// wraps the WHOLE `/mcp` nest — but a "can't happen" path is exactly
    /// what silently rots) must still leave an audit trail rather than
    /// vanishing via the early `?` return Task 2 originally shipped.
    /// Exercises `extract_principal_audited` directly, rather than trying
    /// to smuggle a request past `require_bearer` (which the real router
    /// makes genuinely impossible) — this is the unit-level proof that the
    /// HELPER itself does the right thing on the one path nothing upstream
    /// of it could ever protect against.
    #[tokio::test]
    async fn extract_principal_audited_records_an_audit_entry_on_a_missing_principal() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open_at(audit_log_path(dir.path())).unwrap();
        let state = Arc::new(GatewayState::new(GatewayConfig::default(), audit));

        // A bare `Parts` with nothing in `extensions` — no `require_bearer`
        // ran, so no `Principal` was ever inserted.
        let req = Request::builder().uri("/mcp").body(()).unwrap();
        let (parts, _) = req.into_parts();

        let started = Instant::now();
        let result = extract_principal_audited(&state, "capabilities", started, &parts).await;
        assert!(
            result.is_err(),
            "must still fail — this does not paper over the missing Principal"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(
            entries.len(),
            1,
            "the failure must still leave exactly one audit entry: {entries:?}"
        );
        assert_eq!(entries[0]["client_id"], UNKNOWN_PRINCIPAL_CLIENT_ID);
        assert_eq!(entries[0]["tool"], "capabilities");
        assert_eq!(entries[0]["decision"], "denied");
        assert_eq!(entries[0]["outcome"], "error");
    }

    /// Task 3 review, binding requirement C: `record_audit` must do its
    /// blocking file write via `tokio::task::spawn_blocking`, not directly
    /// on the async task — and awaiting it to completion must still
    /// guarantee the entry is on disk before the caller proceeds (the same
    /// guarantee every OTHER audit test in this file already relies on when
    /// it reads the log back immediately after an HTTP call returns).
    #[tokio::test]
    async fn record_audit_writes_via_spawn_blocking_and_completes_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open_at(audit_log_path(dir.path())).unwrap();
        let state = Arc::new(GatewayState::new(GatewayConfig::default(), audit));

        record_audit(
            &state,
            "client-x",
            CallMeta {
                tool: "capabilities",
                vault: None,
                args_summary: "capabilities: (no arguments)".to_string(),
            },
            Decision::Auto,
            Instant::now(),
            Outcome::Ok,
        )
        .await;

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["client_id"], "client-x");
        assert_eq!(entries[0]["decision"], "auto");
        assert_eq!(entries[0]["outcome"], "ok");
    }

    // ── args_summary is bounded before it is recorded (round-2 finding C) ─

    #[test]
    fn bounded_summary_leaves_an_ordinary_summary_untouched() {
        let s = "get: \"01-projects/x.md\" vault=None".to_string();
        assert_eq!(bounded_summary(s.clone()), s);
        // Exactly at the limit is still untouched — the bound is a ceiling,
        // not a target.
        let exact = "a".repeat(MAX_ARGS_SUMMARY_BYTES);
        assert_eq!(bounded_summary(exact.clone()), exact);
    }

    #[test]
    fn bounded_summary_cuts_an_oversized_summary_and_says_so() {
        let huge = "a".repeat(4 * 1024 * 1024);
        let bounded = bounded_summary(huge);
        assert!(
            bounded.len() < MAX_ARGS_SUMMARY_BYTES + 64,
            "bounded summary is still {} bytes",
            bounded.len()
        );
        assert!(
            bounded.contains(ARGS_SUMMARY_TRUNCATED_SUFFIX),
            "a truncated summary must not read as a complete one: {bounded}"
        );
        assert!(
            bounded.contains("4194304 bytes total"),
            "the original size is the interesting signal: {bounded}"
        );
    }

    /// The cut must land on a `char` boundary — a multi-byte character
    /// straddling the limit would otherwise produce invalid UTF-8. Built so
    /// the boundary falls INSIDE a 3-byte character rather than between two.
    #[test]
    fn bounded_summary_truncates_on_a_char_boundary() {
        let head = "a".repeat(MAX_ARGS_SUMMARY_BYTES - 1);
        let summary = format!("{head}{}", "ก".repeat(100));
        let bounded = bounded_summary(summary);
        assert!(bounded.starts_with(&head));
        assert!(
            bounded.len() <= MAX_ARGS_SUMMARY_BYTES + ARGS_SUMMARY_TRUNCATED_SUFFIX.len() + 32,
            "{}",
            bounded.len()
        );
        // Being a `String` at all already proves valid UTF-8; re-deriving it
        // from the bytes proves the cut itself did not split a character.
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }

    /// End to end: a caller passing a multi-megabyte parameter to an
    /// `auto`, read-only tool — no approval, no grant, the call fails on
    /// path resolution — must NOT land that payload in the audit log. This
    /// is the disk-fill path, driven through the real router rather than
    /// asserted at the helper.
    #[tokio::test]
    async fn a_huge_tool_argument_does_not_reach_the_audit_log_verbatim() {
        let (dir, router, token) = fixture_router();
        let huge = "A".repeat(1024 * 1024);
        let body = call_body(1, "brain_get", serde_json::json!({ "file": huge }));
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        assert!(
            resp.get("error").is_some() || resp["result"]["isError"] == serde_json::json!(true),
            "the call itself is expected to fail on path resolution: {resp}"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{}", entries.len());
        let recorded = entries[0]["args_summary"].as_str().unwrap();
        assert!(
            recorded.len() < MAX_ARGS_SUMMARY_BYTES + 64,
            "a 1 MiB argument landed {} bytes in the audit log",
            recorded.len()
        );
        assert!(
            recorded.contains(ARGS_SUMMARY_TRUNCATED_SUFFIX),
            "{recorded}"
        );
    }

    // ── Gateway PR 4, Task 5: brain_capture ──────────────────────────────
    //
    // Step 1: the create-safe guard (`resolve_create_under_vault`) and slug
    // derivation (`sanitize_slug`/`derive_slug`), unit-tested directly plus
    // one end-to-end pipeline test with a real sentinel file planted
    // outside the vault. Step 2: the tool itself under `policy.mutating:
    // auto`/`deny`. Step 3: the approval flow this task wires up.

    // ── Step 1a: sanitize_slug / derive_slug ─────────────────────────────

    #[test]
    fn sanitize_slug_lowercases_and_collapses_non_alnum_runs_to_a_single_dash() {
        assert_eq!(sanitize_slug("Hello, World!!"), "hello-world");
        assert_eq!(sanitize_slug("../../etc/cron.d/x"), "etc-cron-d-x");
        assert_eq!(sanitize_slug("/etc/x"), "etc-x");
        assert_eq!(
            sanitize_slug("  leading and trailing  "),
            "leading-and-trailing"
        );
    }

    #[test]
    fn sanitize_slug_of_empty_or_punctuation_only_input_is_empty() {
        assert_eq!(sanitize_slug(""), "");
        assert_eq!(sanitize_slug("!!!???..."), "");
    }

    /// Non-Latin scripts are KEPT, not stripped (round-2 finding E). The
    /// earlier ASCII-only charset collapsed every one of these to the empty
    /// string, so every Thai/Japanese/Cyrillic capture in a day derived the
    /// same fallback filename and all but the first failed on a collision.
    #[test]
    fn sanitize_slug_keeps_unicode_alphanumerics_from_any_script() {
        assert_eq!(sanitize_slug("日本語"), "日本語");
        assert_eq!(sanitize_slug("บันทึกการประชุม"), "บันทึกการประชุม");
        assert_eq!(sanitize_slug("Заметка о встрече"), "заметка-о-встрече");
        // Mixed scripts compose the same way ASCII words do.
        assert_eq!(sanitize_slug("日本語 Test"), "日本語-test");
    }

    /// Two different Thai titles must produce two DIFFERENT slugs — the
    /// actual defect, stated as the property that was broken.
    #[test]
    fn two_different_thai_titles_yield_two_different_slugs() {
        let a = derive_slug(Some("บันทึกการประชุม"), "ประชุมกับทีมเรื่องงบประมาณ");
        let b = derive_slug(Some("สรุปงบประมาณ"), "ประชุมกับทีมเรื่องงบประมาณ");
        assert_ne!(a, b, "a={a} b={b}");
        assert_ne!(a, FALLBACK_SLUG);
        assert_ne!(b, FALLBACK_SLUG);
    }

    /// A Thai `text` with no `title` must also derive a real slug rather
    /// than falling through to the marker.
    #[test]
    fn thai_text_without_a_title_still_derives_a_meaningful_slug() {
        let slug = derive_slug(None, "ประชุมกับทีมเรื่องงบประมาณ");
        assert!(slug.starts_with("ประชุม"), "{slug}");
        assert!(!slug.starts_with(FALLBACK_SLUG), "{slug}");
    }

    /// Thai TONE marks are `Mn` without `Other_Alphabetic`, so they are not
    /// alphanumeric and become a separator — the same treatment
    /// `onebrain_fs::note::new`'s own `slug` helper gives them, so a
    /// gateway capture and a `onebrain note new` produce the same filename
    /// for the same title. Pinned so the lossiness is a recorded, deliberate
    /// property rather than a surprise: the slug stays stable, distinct, and
    /// confined, which is what a filename needs to be. (Thai VOWEL signs
    /// carry `Other_Alphabetic` and ARE kept — the assertion above on
    /// "บันทึกการประชุม" round-tripping proves that half.)
    #[test]
    fn thai_tone_marks_become_separators_matching_the_vaults_own_slug_helper() {
        // U+0E37 (a vowel sign) survives; U+0E48 MAI EK (a tone mark) does
        // not and collapses to the separator.
        assert_eq!(sanitize_slug("เรื่อง"), "เรื-อง");
        assert!('\u{0E37}'.is_alphanumeric(), "vowel signs are kept");
        assert!(!'\u{0E48}'.is_alphanumeric(), "tone marks are not");
    }

    #[test]
    fn derive_slug_prefers_a_usable_title_over_text() {
        assert_eq!(
            derive_slug(Some("My Great Idea"), "this text is ignored"),
            "my-great-idea"
        );
    }

    #[test]
    fn derive_slug_falls_back_to_text_when_title_is_none_empty_or_punctuation_only() {
        let expected = "quick-note-about-coffee";
        assert_eq!(derive_slug(None, "Quick note about coffee"), expected);
        assert_eq!(derive_slug(Some(""), "Quick note about coffee"), expected);
        assert_eq!(
            derive_slug(Some("!!!"), "Quick note about coffee"),
            expected
        );
    }

    #[test]
    fn derive_slug_falls_back_to_the_marker_when_title_and_text_are_both_unusable() {
        // Punctuation-only and emoji-only input carry no alphanumerics in
        // ANY script, so they are the cases that genuinely have nothing to
        // build a name from. (Pure CJK/Thai/Cyrillic no longer land here —
        // see the unicode tests above.)
        for slug in [
            derive_slug(Some("!!!"), ""),
            derive_slug(None, "... ??? !!!"),
            derive_slug(Some("🎉🎊"), "🎈"),
        ] {
            assert!(
                slug.starts_with(&format!("{FALLBACK_SLUG}-")),
                "expected a disambiguated fallback, got {slug}"
            );
        }
    }

    /// Two fallback captures on the same day must not derive the same
    /// filename — otherwise the second one fails on a collision that says
    /// "you already captured this note" when nothing of the sort happened.
    /// Only the fallback is disambiguated: a slug derived from real input
    /// stays deterministic, so a genuine same-title recapture still
    /// collides deliberately (asserted below).
    #[test]
    fn repeated_fallback_slugs_do_not_collide_but_real_ones_stay_deterministic() {
        let a = derive_slug(Some("🎉"), "🎈");
        let b = derive_slug(Some("🎉"), "🎈");
        assert_ne!(a, b, "two fallback captures collided: {a}");
        assert!(a.starts_with(&format!("{FALLBACK_SLUG}-")), "{a}");
        assert_eq!(
            a.chars().count(),
            FALLBACK_SLUG.chars().count() + 1 + FALLBACK_DISAMBIGUATOR_CHARS
        );

        assert_eq!(
            derive_slug(Some("My Great Idea"), "x"),
            derive_slug(Some("My Great Idea"), "y"),
            "a slug derived from real input must stay deterministic"
        );
    }

    #[test]
    fn derive_slug_of_a_very_long_ascii_title_is_capped_and_never_ends_in_a_dash() {
        let long = "a".repeat(500);
        let slug = derive_slug(Some(&long), "unused");
        assert_eq!(slug.chars().count(), MAX_SLUG_CHARS, "{slug}");
        assert_eq!(slug, "a".repeat(MAX_SLUG_CHARS));

        // A long title made of WORDS (so the cap can land mid-dash) must
        // still never leave a trailing dash after truncation.
        let words = "word ".repeat(40);
        let slug2 = derive_slug(Some(&words), "unused");
        assert!(slug2.chars().count() <= MAX_SLUG_CHARS, "{slug2}");
        assert!(!slug2.ends_with('-'), "{slug2}");
    }

    /// The length cap must be BYTE-aware, not just char-aware: a filesystem
    /// name limit counts bytes (255 on APFS/ext4), and a multi-byte script
    /// at [`MAX_SLUG_CHARS`] characters would otherwise sail past it. Thai
    /// is three bytes per character, so this is not a hypothetical for this
    /// vault.
    #[test]
    fn a_long_non_ascii_title_is_capped_in_bytes_not_just_characters() {
        let long_thai = "ก".repeat(500);
        let slug = derive_slug(Some(&long_thai), "unused");
        assert!(
            slug.len() <= MAX_SLUG_BYTES,
            "{} bytes exceeds the byte cap",
            slug.len()
        );
        assert!(slug.chars().count() <= MAX_SLUG_CHARS);
        // The cap that actually bit here is the BYTE one — a char-only cap
        // would have produced 60 * 3 = 180 bytes.
        assert!(slug.chars().count() < MAX_SLUG_CHARS, "{}", slug.len());

        // …and the whole filename still fits comfortably inside a 255-byte
        // path component.
        let file_name = format!("2026-08-29-{slug}.md");
        assert!(file_name.len() < 255, "{}", file_name.len());

        // Truncation landed on a char boundary (a `String` proves valid
        // UTF-8; slicing at a bad index would have panicked in `cap_slug`).
        assert!(std::str::from_utf8(slug.as_bytes()).is_ok());
    }

    /// The confinement argument, re-established for the WIDER charset
    /// rather than inherited from the ASCII one.
    ///
    /// Two halves, both required:
    /// 1. The Unicode category claims `sanitize_slug`'s allowlist rests on —
    ///    asserted directly against `char`'s own predicates, so "bidi
    ///    overrides are `Cf`, not alphanumeric" is checked, not assumed.
    /// 2. The resulting invariant over an adversarial corpus: every emitted
    ///    character is either `-` or alphanumeric, and therefore none of
    ///    them is a path separator, `.`, NUL, a newline, a control
    ///    character, or a format character.
    #[test]
    fn the_wider_slug_charset_still_cannot_emit_anything_path_significant() {
        // (1) The category claims themselves.
        for (name, c) in [
            ("solidus", '/'),
            ("reverse solidus", '\\'),
            ("full stop", '.'),
            ("nul", '\0'),
            ("line feed", '\n'),
            ("carriage return", '\r'),
            ("right-to-left override U+202E", '\u{202E}'),
            ("left-to-right override U+202D", '\u{202D}'),
            ("right-to-left isolate U+2067", '\u{2067}'),
            ("pop directional isolate U+2069", '\u{2069}'),
            ("left-to-right mark U+200E", '\u{200E}'),
            ("zero width space U+200B", '\u{200B}'),
            ("soft hyphen U+00AD", '\u{00AD}'),
            ("byte order mark U+FEFF", '\u{FEFF}'),
            ("combining dot above U+0307", '\u{0307}'),
        ] {
            assert!(
                !c.is_alphanumeric(),
                "{name} must not pass the slug allowlist"
            );
        }

        // (2) The invariant, over titles built to attack it.
        let long_thai = "ก".repeat(300);
        let titles: Vec<&str> = vec![
            "../../etc/cron.d/x",
            "/etc/passwd",
            "..",
            ".",
            "C:\\Windows\\system32",
            "a\0b",
            "line\nbreak\r\n",
            "\u{202E}gnp.exe",
            "\u{2066}isolated\u{2069}",
            "soft\u{00AD}hyphen\u{200B}zwsp\u{FEFF}bom",
            "\u{0130}stanbul", // lowercases to `i` + a combining mark
            "บันทึกการประชุม",
            "日本語のみ",
            "Заметка",
            "🎉🎊",
            &long_thai,
        ];
        for title in titles {
            let slug = derive_slug(Some(title), "fallback body");
            assert!(!slug.is_empty(), "empty slug for {title:?}");
            for c in slug.chars() {
                assert!(
                    c == '-' || c.is_alphanumeric(),
                    "slug for {title:?} emitted {c:?} (U+{:04X})",
                    c as u32
                );
                assert!(!c.is_control(), "{title:?} -> {slug:?}");
                assert!(
                    !matches!(c, '/' | '\\' | '.' | '\0'),
                    "{title:?} -> {slug:?}"
                );
            }
            assert_ne!(slug, ".", "{title:?}");
            assert_ne!(slug, "..", "{title:?}");
            assert!(slug.len() <= MAX_SLUG_BYTES, "{title:?} -> {}", slug.len());

            // The derived path is a single, plain component — the same
            // property `resolve_create_under_vault`'s layer 1 enforces
            // independently.
            let rel = PathBuf::from("00-inbox").join(format!("2026-08-29-{slug}.md"));
            assert!(
                rel.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
                "{title:?} produced a non-Normal component: {rel:?}"
            );
            assert_eq!(rel.components().count(), 2, "{title:?} -> {rel:?}");
        }
    }

    // ── Step 1b: resolve_create_under_vault, tested directly (raw,
    // deliberately UNsanitized `rel` paths — proves the guard itself is a
    // real, independent line of defense, not merely inert dead code behind
    // sanitization that already made these cases unreachable) ────────────

    /// Vault dir + a real sentinel file one level ABOVE it — the fixture
    /// these lower-level `resolve_create_under_vault`/`derive_slug` guard
    /// tests need to prove a crafted path can never make it out. Mirrors
    /// [`fixture_with_outside_file`]'s layout, minus the HTTP router that
    /// helper additionally wires (not needed at this level).
    fn vault_with_outside_sentinel() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let workspace = tempfile::tempdir().unwrap();
        let vault_root = workspace.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("onebrain.yml"), "folders: {}\n").unwrap();
        let outside = workspace.path().join("outside.md");
        std::fs::write(&outside, OUTSIDE_SENTINEL).unwrap();
        (workspace, vault_root, outside)
    }

    #[test]
    fn resolve_create_under_vault_rejects_a_raw_parent_traversal_before_touching_the_filesystem() {
        let (_workspace, vault_root, outside) = vault_with_outside_sentinel();
        let before = std::fs::read_to_string(&outside).unwrap();

        let rel = Path::new("00-inbox/../../outside.md");
        let err = resolve_create_under_vault(&vault_root, rel).unwrap_err();
        assert!(
            err.to_string().contains("not a plain vault-relative path"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            before,
            "sentinel must be untouched"
        );
    }

    #[test]
    fn resolve_create_under_vault_rejects_a_raw_absolute_path() {
        let (_workspace, vault_root, outside) = vault_with_outside_sentinel();
        let before = std::fs::read_to_string(&outside).unwrap();

        // `outside` is itself an absolute path pointing OUTSIDE the vault —
        // exactly the `/etc/x` shape from the brief, using a path this test
        // is actually allowed to touch.
        let err = resolve_create_under_vault(&vault_root, &outside).unwrap_err();
        assert!(
            err.to_string().contains("not a plain vault-relative path"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), before);
    }

    #[test]
    fn resolve_create_under_vault_creates_the_missing_parent_and_returns_a_confined_path() {
        let (_workspace, vault_root, _outside) = vault_with_outside_sentinel();
        let rel = Path::new("00-inbox/2026-08-29-hello.md");
        let confined = resolve_create_under_vault(&vault_root, rel).unwrap();
        let root_canon = vault_root.canonicalize().unwrap();
        assert!(
            confined.starts_with(&root_canon),
            "{confined:?} not under {root_canon:?}"
        );
        assert_eq!(confined.file_name().unwrap(), "2026-08-29-hello.md");
        assert!(vault_root.join("00-inbox").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_create_under_vault_rejects_a_symlinked_parent_escaping_the_vault() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let vault_root = workspace.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        let outside_dir = workspace.path().join("outside-dir");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let sentinel = outside_dir.join("secret.md");
        std::fs::write(&sentinel, OUTSIDE_SENTINEL).unwrap();
        // `00-inbox` inside the vault is a symlink pointing OUTSIDE it —
        // the syntactic layer sees only well-formed Normal components
        // (`00-inbox/2026-08-29-hello.md`), so only the canonicalization
        // layer can catch this.
        symlink(&outside_dir, vault_root.join("00-inbox")).unwrap();

        let rel = Path::new("00-inbox/2026-08-29-hello.md");
        let err = resolve_create_under_vault(&vault_root, rel).unwrap_err();
        assert!(err.to_string().contains("escapes the vault"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            OUTSIDE_SENTINEL
        );
        assert!(
            !outside_dir.join("2026-08-29-hello.md").exists(),
            "must never have written into the symlinked-out directory"
        );
    }

    // ── Step 1c: the full derive_slug -> resolve_create_under_vault
    // pipeline, exactly as `capture_note` composes them, against every
    // adversarial title the brief names ────────────────────────────────

    #[test]
    fn capture_pipeline_confines_every_adversarial_title_and_never_touches_outside_content() {
        let (_workspace, vault_root, outside) = vault_with_outside_sentinel();
        let before = std::fs::read_to_string(&outside).unwrap();
        let root_canon = vault_root.canonicalize().unwrap();

        let long_title = "x".repeat(500);
        let long_thai_title = "ก".repeat(500);
        let titles: Vec<&str> = vec![
            "../../etc/cron.d/x",
            "/etc/x",
            "",
            "!!! ??? ...",
            &long_title,
            "日本語のみ",
            "日本語 Mixed Ascii Title",
            // The widened charset's own corpus: real non-Latin titles now
            // reach the filesystem as themselves rather than as a fallback,
            // so the guard must be exercised against them too.
            "บันทึกการประชุม",
            "ประชุมกับทีมเรื่องงบประมาณ",
            "Заметка о встрече",
            &long_thai_title,
            "\u{202E}gnp.exe",
            "\u{0130}stanbul",
            "🎉🎊",
        ];

        for (i, title) in titles.iter().enumerate() {
            let slug = derive_slug(Some(title), "fallback body text for slug derivation");
            assert!(
                !slug.is_empty(),
                "slug must never be empty for title {title:?}"
            );
            assert!(
                slug.chars().count() <= MAX_SLUG_CHARS,
                "slug too long for title {title:?}: {slug}"
            );
            assert!(
                slug.len() <= MAX_SLUG_BYTES,
                "slug too many BYTES for title {title:?}: {} ({slug})",
                slug.len()
            );
            let rel = PathBuf::from("00-inbox").join(format!("2026-08-{:02}-{slug}.md", i + 1));
            let confined = resolve_create_under_vault(&vault_root, &rel).unwrap_or_else(|e| {
                panic!("title {title:?} produced slug {slug:?} which the guard rejected: {e}")
            });
            assert!(
                confined.starts_with(&root_canon),
                "title {title:?} escaped the vault: {confined:?}"
            );
        }

        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            before,
            "outside sentinel must be untouched by any adversarial title"
        );
    }

    // ── Step 2 / Step 3 fixtures ──────────────────────────────────────────

    /// Like [`fixture_router_with_read_only_policy`], but for
    /// `policy.mutating` — `brain_capture` is `RiskClass::Mutating`, so its
    /// Step 2/3 tests drive that axis, plus `approval_wait_seconds` (how
    /// long an unanswered approval blocks) and `grant_ttl_minutes` (how
    /// long an approval's resulting grant then lasts) independently.
    /// Returns the `Arc<GatewayState>` too — these tests need direct
    /// access to `state.approvals`/`state.grants` to resolve a pending
    /// approval out-of-band and assert grant state, mirroring
    /// `approval_routes.rs`'s own tests against a bare `approval_router`.
    fn fixture_router_with_mutating_policy(
        mutating: policy::PolicyMode,
        approval_wait_seconds: u64,
        grant_ttl_minutes: u64,
    ) -> (tempfile::TempDir, axum::Router, Arc<GatewayState>, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("onebrain.yml"), "folders: {}\n").unwrap();

        // A SECOND named vault, so the multi-vault grant-scoping property
        // (a grant for one vault must never authorize writes into another)
        // can be driven end to end. Nested under the same tempdir purely so
        // one `TempDir` still owns everything the fixture creates; vault
        // resolution doesn't care, and `inbox_note_count(dir.path())` only
        // ever looks at the FIRST vault's own `00-inbox`.
        let second = root.join("vault-two");
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("onebrain.yml"), "folders: {}\n").unwrap();

        let mut vaults = BTreeMap::new();
        vaults.insert("t1".to_string(), root.to_path_buf());
        vaults.insert("t2".to_string(), second);
        let config = GatewayConfig {
            default_vault: Some(root.to_path_buf()),
            vaults,
            policy: policy::PolicyConfig {
                mutating,
                approval_wait_seconds,
                grant_ttl_minutes,
                ..policy::PolicyConfig::default()
            },
            ..GatewayConfig::default()
        };
        let audit = AuditLog::open_at(audit_log_path(root)).unwrap();
        let state = Arc::new(GatewayState::new(config, audit));
        let (auth_ctx, token) = test_auth_ctx(root);
        let router = build_gateway_router(state.clone(), auth_ctx);
        (dir, router, state, token)
    }

    /// Count of `.md` files directly under `<vault_root>/00-inbox` — `0`
    /// (not a panic) when the folder doesn't exist yet, since a denied or
    /// timed-out `brain_capture` never even reaches `create_dir_all`.
    fn inbox_note_count(vault_root: &Path) -> usize {
        let inbox = vault_root.join("00-inbox");
        if !inbox.is_dir() {
            return 0;
        }
        std::fs::read_dir(&inbox)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
            .count()
    }

    // ── Step 2: the tool under policy.mutating: auto / deny ──────────────

    #[tokio::test]
    async fn brain_capture_under_auto_policy_creates_a_note_with_frontmatter_and_body() {
        let (dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::Auto, 300, 30);
        let body = call_body(
            1,
            "brain_capture",
            serde_json::json!({"title": "My Quick Idea", "text": "Some captured note body."}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_capture")),
        )
        .await;
        assert!(resp.get("error").is_none(), "{resp}");
        let path = resp["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("no path in response: {resp}"));
        assert!(path.starts_with("00-inbox/"), "{path}");
        assert!(path.contains("my-quick-idea"), "{path}");
        assert!(path.ends_with(".md"), "{path}");

        let content = std::fs::read_to_string(dir.path().join(path)).unwrap();
        assert!(content.starts_with("---\n"), "{content}");
        assert!(content.contains("tags: [capture]"), "{content}");
        assert!(content.contains("created:"), "{content}");
        assert!(content.contains("Some captured note body."), "{content}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["tool"], "brain_capture");
        assert_eq!(entries[0]["decision"], "auto");
        assert_eq!(entries[0]["outcome"], "ok");
        assert!(
            !entries[0]["args_summary"]
                .as_str()
                .unwrap()
                .contains("Some captured note body"),
            "the audit trail must never carry the raw note body: {entries:?}"
        );
    }

    /// The round-2 finding E regression, end to end through the real
    /// router: two Thai captures on the same day, with DIFFERENT titles,
    /// must produce two distinct notes.
    ///
    /// Before the charset widening both derived the identical
    /// `00-inbox/YYYY-MM-DD-capture.md`, so the second failed on a same-day
    /// collision — and under `ask_once` the human had already approved it
    /// (recording a grant), so every later Thai capture that day failed
    /// silently-auto-approved. The feature was unusable in the language this
    /// vault's owner writes in.
    #[tokio::test]
    async fn two_thai_captures_on_the_same_day_write_two_distinct_notes() {
        let (dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::Auto, 300, 30);

        let mut paths = Vec::new();
        for (title, text) in [
            ("บันทึกการประชุม", "ประชุมกับทีมเรื่องงบประมาณ"),
            ("สรุปงบประมาณ", "ตัวเลขงบประมาณไตรมาสสี่"),
        ] {
            let body = call_body(
                1,
                "brain_capture",
                serde_json::json!({ "title": title, "text": text }),
            );
            let resp = post(
                &router,
                body,
                &token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await;
            assert!(resp.get("error").is_none(), "{resp}");
            let path = resp["result"]["structuredContent"]["path"]
                .as_str()
                .unwrap_or_else(|| panic!("no path in response: {resp}"))
                .to_string();
            assert!(path.starts_with("00-inbox/"), "{path}");
            assert!(
                !path.contains(FALLBACK_SLUG),
                "a Thai title must derive its own slug, not the fallback: {path}"
            );
            assert!(
                dir.path().join(&path).is_file(),
                "the note must exist on disk at the reported path: {path}"
            );
            paths.push(path);
        }

        assert_ne!(
            paths[0], paths[1],
            "two different Thai titles must not collapse to one filename"
        );
        assert_eq!(inbox_note_count(dir.path()), 2);
    }

    /// Step 2's no-clobber requirement: two captures on the SAME day with
    /// the SAME title derive the SAME filename — the second must surface
    /// `new_note`'s existing-file refusal as a clean tool error (never an
    /// opaque internal/500-shaped one), and the first note's own content
    /// must be completely untouched.
    #[tokio::test]
    async fn brain_capture_same_day_same_title_collision_is_a_clean_error_not_a_crash() {
        let (dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::Auto, 300, 30);
        let make_body = || {
            call_body(
                1,
                "brain_capture",
                serde_json::json!({"title": "Duplicate Title", "text": "first capture"}),
            )
        };
        let resp1 = post(
            &router,
            make_body(),
            &token,
            &standard_headers("tools/call", Some("brain_capture")),
        )
        .await;
        assert!(resp1.get("error").is_none(), "{resp1}");

        let resp2 = post(
            &router,
            make_body(),
            &token,
            &standard_headers("tools/call", Some("brain_capture")),
        )
        .await;
        let message = resp2["error"]["message"].as_str().unwrap_or_else(|| {
            panic!("expected a clean JSON-RPC tool error, not a crash: {resp2}")
        });
        assert!(message.contains("capture failed"), "{message}");
        assert!(message.contains("already exists"), "{message}");
        // The message must be THIS crate's, not `onebrain_fs::note::new_note`'s
        // own "file exists: <path> (use --force)" — `--force` is a CLI flag no
        // MCP client can pass, so forwarding it tells the caller to do
        // something impossible.
        assert!(
            !message.contains("--force"),
            "the client-facing message must not name a CLI flag: {message}"
        );
        assert!(
            message.contains("00-inbox/"),
            "the vault-relative path is the one useful detail: {message}"
        );

        let path = resp1["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap();
        let content = std::fs::read_to_string(dir.path().join(path)).unwrap();
        assert!(content.contains("first capture"), "{content}");
        assert_eq!(
            inbox_note_count(dir.path()),
            1,
            "the second, colliding attempt must not have created a second file"
        );
    }

    #[tokio::test]
    async fn brain_capture_under_deny_policy_creates_no_file() {
        let (dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::Deny, 300, 30);
        let body = call_body(
            1,
            "brain_capture",
            serde_json::json!({"title": "Never", "text": "never created"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_capture")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("policy denies"), "{message}");
        assert_eq!(
            inbox_note_count(dir.path()),
            0,
            "a policy-denied capture must create no file"
        );
    }

    #[tokio::test]
    async fn brain_capture_is_listed_with_read_only_hint_false() {
        let (_dir, router, token) = fixture_router();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {},
        })
        .to_string();
        let resp = post(&router, body, &token, &standard_headers("tools/list", None)).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        let tool = tools
            .iter()
            .find(|t| t["name"] == "brain_capture")
            .unwrap_or_else(|| panic!("brain_capture missing from tools/list: {resp}"));
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            serde_json::Value::Bool(false),
            "brain_capture must carry readOnlyHint:false — it is the first non-read-only tool: {tool}"
        );
    }

    // ── Step 3: the approval path ─────────────────────────────────────────

    /// Polls `state.approvals.list()` until exactly one entry appears (up
    /// to ~2s), the shared "wait for the call to actually register a
    /// pending approval" step every Step 3 test below needs before it can
    /// resolve one out-of-band or assert the call is genuinely blocked.
    async fn wait_for_one_pending(state: &Arc<GatewayState>) -> PendingApproval {
        for _ in 0..200 {
            let pending = state.approvals.list();
            if pending.len() == 1 {
                return pending.into_iter().next().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no pending approval appeared within the poll window");
    }

    #[tokio::test]
    async fn brain_capture_ask_once_blocks_then_approve_out_of_band_completes_the_write() {
        let (dir, router, state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskOnce, 300, 30);

        let call_router = router.clone();
        let call_token = token.clone();
        let handle = tokio::spawn(async move {
            let body = call_body(
                1,
                "brain_capture",
                serde_json::json!({"title": "Approve Me", "text": "note body one"}),
            );
            post(
                &call_router,
                body,
                &call_token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await
        });

        let pending = wait_for_one_pending(&state).await;
        assert!(
            !handle.is_finished(),
            "the call must still be blocked, not yet returned"
        );
        assert!(state
            .approvals
            .resolve(&pending.id, approval::Decision::Approve));

        let resp = handle.await.unwrap();
        assert!(resp.get("error").is_none(), "{resp}");
        let path = resp["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap();
        assert!(dir.path().join(path).exists());

        // Grant-after-approve: recorded by the WAITER itself
        // (`server::await_approval`), independent of whether an operator
        // ever hits the `/approvals` HTTP surface at all.
        assert!(
            state
                .grants
                .has(&GrantKey::new("test-client", None, RiskClass::Mutating)),
            "an Approve must record a grant for (client, class)"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "approved");
        assert_eq!(entries[0]["outcome"], "ok");
    }

    #[tokio::test]
    async fn brain_capture_ask_once_deny_out_of_band_returns_a_policy_error_with_no_file() {
        let (dir, router, state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskOnce, 300, 30);

        let call_router = router.clone();
        let call_token = token.clone();
        let handle = tokio::spawn(async move {
            let body = call_body(
                1,
                "brain_capture",
                serde_json::json!({"title": "Deny Me", "text": "note body"}),
            );
            post(
                &call_router,
                body,
                &call_token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await
        });

        let pending = wait_for_one_pending(&state).await;
        assert!(state
            .approvals
            .resolve(&pending.id, approval::Decision::Deny));

        let resp = handle.await.unwrap();
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("denied"), "{message}");
        assert_eq!(
            inbox_note_count(dir.path()),
            0,
            "a denied capture must not create a file"
        );
        assert!(
            !state
                .grants
                .has(&GrantKey::new("test-client", None, RiskClass::Mutating)),
            "a Deny must never record a grant"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "denied");
        assert_eq!(entries[0]["outcome"], "error");
    }

    #[tokio::test]
    async fn brain_capture_ask_once_times_out_with_no_file_when_nothing_answers() {
        let (dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskOnce, 0, 30);
        let body = call_body(
            1,
            "brain_capture",
            serde_json::json!({"title": "Timeout", "text": "note body"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_capture")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("timed out"), "{message}");
        assert_eq!(
            inbox_note_count(dir.path()),
            0,
            "a timed-out capture must not create a file"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "timedout");
        assert_eq!(entries[0]["outcome"], "error");
    }

    /// The whole point of a grant: a second `ask_once` call from the SAME
    /// client and risk class, within the grant's TTL, proceeds with NO new
    /// approval at all.
    #[tokio::test]
    async fn brain_capture_second_call_within_grant_ttl_skips_approval() {
        let (dir, router, state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskOnce, 300, 30);

        let call_router = router.clone();
        let call_token = token.clone();
        let handle = tokio::spawn(async move {
            let body = call_body(
                1,
                "brain_capture",
                serde_json::json!({"title": "First Capture", "text": "one"}),
            );
            post(
                &call_router,
                body,
                &call_token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await
        });
        let pending = wait_for_one_pending(&state).await;
        assert!(state
            .approvals
            .resolve(&pending.id, approval::Decision::Approve));
        let resp1 = handle.await.unwrap();
        assert!(resp1.get("error").is_none(), "{resp1}");

        // Different title (avoid the same-day filename collision) —
        // otherwise identical call, same client, same risk class.
        let body2 = call_body(
            2,
            "brain_capture",
            serde_json::json!({"title": "Second Capture", "text": "two"}),
        );
        let resp2 = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &router,
                body2,
                &token,
                &standard_headers("tools/call", Some("brain_capture")),
            ),
        )
        .await
        .expect("second call must not block on a fresh approval — the grant should satisfy it");
        assert!(resp2.get("error").is_none(), "{resp2}");
        assert!(
            state.approvals.list().is_empty(),
            "the second call must never have registered a new pending approval"
        );
        assert_eq!(
            inbox_note_count(dir.path()),
            2,
            "both captures must have created a file"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0]["decision"], "approved");
        assert_eq!(
            entries[1]["decision"], "auto",
            "the second call's audit entry must show `auto` — it was satisfied by the live grant, not a fresh approval"
        );
    }

    // ── Fix wave: the guard's third layer (F1) ───────────────────────────

    /// Layer 3 of [`resolve_create_under_vault`]: the guard's proof only
    /// transfers to the write if the confined path IS the path the write
    /// will open. An inbox that is a symlink to another directory INSIDE
    /// the same vault passes layer 2 (`starts_with(root)` holds — the
    /// target is in the vault) yet resolves to a different path than
    /// `root.join(rel)`, which is what `new_note` will join. Fail closed:
    /// the guard did not vouch for the path the write would actually use.
    #[cfg(unix)]
    #[test]
    fn resolve_create_under_vault_rejects_a_parent_symlinked_elsewhere_inside_the_vault() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let vault_root = workspace.path().join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        let real_dir = vault_root.join("real-inbox");
        std::fs::create_dir_all(&real_dir).unwrap();
        symlink(&real_dir, vault_root.join("00-inbox")).unwrap();

        let rel = Path::new("00-inbox/2026-08-29-hello.md");
        let err = resolve_create_under_vault(&vault_root, rel).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not resolve to its own vault-relative location"),
            "{err}"
        );
        assert!(
            !real_dir.join("2026-08-29-hello.md").exists(),
            "nothing may have been written through the link"
        );
    }

    #[test]
    fn resolve_create_under_vault_returns_the_exact_path_the_write_will_target() {
        let (_workspace, vault_root, _outside) = vault_with_outside_sentinel();
        let rel = Path::new("00-inbox/2026-08-29-hello.md");
        let confined = resolve_create_under_vault(&vault_root, rel).unwrap();
        assert_eq!(
            confined,
            vault_root.canonicalize().unwrap().join(rel),
            "the returned path must be exactly `new_note`'s own join, root resolved"
        );
    }

    // ── Fix wave: the daemon-reindex kill switch (F3) ────────────────────

    /// The in-crate half of the switch: `cfg!(test)` is true inside THIS
    /// test binary, so no unit test can ever reach
    /// `daemon_client::ensure_running` from `capture_note` — which is what
    /// would spawn `onebrain daemon start` against the developer's real
    /// `$HOME`.
    #[test]
    fn reindex_channel_is_disabled_inside_this_test_binary() {
        assert!(
            !reindex_channel_enabled(),
            "a unit test must never be able to spawn a daemon from capture_note"
        );
    }

    /// The out-of-process half: the env var must flip the switch off for a
    /// SEPARATELY COMPILED binary, where `cfg!(test)` is false and therefore
    /// no protection at all. `cfg!(test)` is true here, so the var's effect
    /// is asserted on the same predicate `capture_note` reads —
    /// `super::env_switch_on(DISABLE_DAEMON_REINDEX_ENV)` — rather than on
    /// `reindex_channel_enabled`, which this binary already forces false.
    /// `tests/gateway_approval_e2e.rs` is what proves the composed
    /// behaviour in a real subprocess.
    #[test]
    fn daemon_reindex_env_var_follows_the_crate_env_switch_convention() {
        {
            let _env = crate::test_env::set_var(DISABLE_DAEMON_REINDEX_ENV, "1");
            assert!(super::super::env_switch_on(DISABLE_DAEMON_REINDEX_ENV));
        }
        {
            let _env = crate::test_env::set_var(DISABLE_DAEMON_REINDEX_ENV, "");
            assert!(
                !super::super::env_switch_on(DISABLE_DAEMON_REINDEX_ENV),
                "a set-but-empty value must count as unset, like ONEBRAIN_NO_DAEMON"
            );
        }
    }

    /// The observable witness, not just the predicate: a real `brain_capture`
    /// under `auto` policy, with `$HOME` pointed at an empty tempdir, must
    /// leave NO daemon runtime state behind. `daemon_client::ensure_running`
    /// creates `$HOME/.onebrain/run/` (via `resolve_slot` →
    /// `ensure_private_run_dir`) before it can spawn or even discover
    /// anything, so the absence of that directory proves the reindex block
    /// was never entered — the same property the e2e asserts against the
    /// spawned gateway subprocess, here for this crate's own tests.
    #[tokio::test]
    async fn a_unit_test_capture_leaves_no_daemon_runtime_state_under_home() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", home.path().as_os_str()),
            ("USERPROFILE", home.path().as_os_str()),
        ]);

        let (dir, router, _state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::Auto, 300, 30);
        let body = call_body(
            1,
            "brain_capture",
            serde_json::json!({"title": "No Daemon Please", "text": "body"}),
        );
        let resp = post(
            &router,
            body,
            &token,
            &standard_headers("tools/call", Some("brain_capture")),
        )
        .await;
        // The capture itself must have SUCCEEDED — otherwise the absence of
        // daemon state below would prove nothing (an early failure would
        // also skip the reindex block).
        assert!(resp.get("error").is_none(), "{resp}");
        assert_eq!(inbox_note_count(dir.path()), 1);

        assert!(
            !home.path().join(".onebrain").join("run").exists(),
            "capture_note must not have touched the daemon run directory at all"
        );
    }

    // ── Fix wave: bounded pending approvals (F4) ─────────────────────────

    /// Past the per-client cap, a gated call is refused with a policy error
    /// instead of registering yet another pending entry (and, in
    /// production, firing yet another blocking GUI dialog). Driven by
    /// pre-filling the registry for the SAME `client_id` the fixture's token
    /// carries, so the assertion is deterministic rather than racing N
    /// concurrent in-flight calls.
    #[tokio::test]
    async fn brain_capture_is_refused_once_the_pending_approval_cap_is_reached() {
        let (dir, router, state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskOnce, 300, 30);

        let now = now_epoch_secs();
        let mut held = Vec::new();
        for i in 0..approval::MAX_PENDING_APPROVALS_PER_CLIENT {
            held.push(
                state
                    .approvals
                    .register(PendingApproval {
                        id: format!("filler-{i}"),
                        client_id: "test-client".to_string(),
                        tool: "brain_capture".to_string(),
                        vault: None,
                        summary: "filler".to_string(),
                        created: now,
                        expires: now + 300,
                        class: RiskClass::Mutating,
                    })
                    .unwrap_or_else(|e| panic!("filler {i} must fit under the cap: {e:?}")),
            );
        }

        let body = call_body(
            1,
            "brain_capture",
            serde_json::json!({"title": "Over Cap", "text": "should never be written"}),
        );
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &router,
                body,
                &token,
                &standard_headers("tools/call", Some("brain_capture")),
            ),
        )
        .await
        .expect("an over-cap call must be refused immediately, never left waiting");

        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("too many approval requests"), "{message}");
        assert_eq!(
            state.approvals.list().len(),
            approval::MAX_PENDING_APPROVALS_PER_CLIENT,
            "a refused call must not have registered a further pending approval"
        );
        assert_eq!(
            inbox_note_count(dir.path()),
            0,
            "a refused call must not write anything"
        );

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "denied", "{entries:?}");
        assert_eq!(entries[0]["outcome"], "error", "{entries:?}");
    }

    // ── Fix wave: grants are vault-scoped (F5) ───────────────────────────

    /// Consent scope and grant scope must match: the operator approved a
    /// write into `t1` (the dialog and `args_summary` both say so), so a
    /// later write into `t2` must ask again rather than ride the same grant.
    #[tokio::test]
    async fn a_grant_earned_for_one_vault_does_not_authorize_another_vault() {
        let (dir, router, state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskOnce, 300, 30);

        let call_router = router.clone();
        let call_token = token.clone();
        let handle = tokio::spawn(async move {
            let body = call_body(
                1,
                "brain_capture",
                serde_json::json!({"title": "Into Vault One", "text": "one", "vault": "t1"}),
            );
            post(
                &call_router,
                body,
                &call_token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await
        });
        let pending = wait_for_one_pending(&state).await;
        assert_eq!(
            pending.vault.as_deref(),
            Some("t1"),
            "the pending entry must carry the vault the call named"
        );
        assert!(state
            .approvals
            .resolve(&pending.id, approval::Decision::Approve));
        assert!(handle.await.unwrap().get("error").is_none());

        assert!(
            state.grants.has(&GrantKey::new(
                "test-client",
                Some("t1".to_string()),
                RiskClass::Mutating
            )),
            "the approval must have granted (client, t1, Mutating)"
        );
        assert!(
            !state.grants.has(&GrantKey::new(
                "test-client",
                Some("t2".to_string()),
                RiskClass::Mutating
            )),
            "and NOTHING for the other vault"
        );

        // Same client, same risk class, live grant — but a different vault,
        // so this call must block on a FRESH approval rather than proceed.
        let call_router = router.clone();
        let call_token = token.clone();
        let second = tokio::spawn(async move {
            let body = call_body(
                2,
                "brain_capture",
                serde_json::json!({"title": "Into Vault Two", "text": "two", "vault": "t2"}),
            );
            post(
                &call_router,
                body,
                &call_token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await
        });
        let pending2 = wait_for_one_pending(&state).await;
        assert_eq!(pending2.vault.as_deref(), Some("t2"));
        assert!(
            !second.is_finished(),
            "the cross-vault call must be blocked on a fresh approval, not satisfied by t1's grant"
        );
        assert!(state
            .approvals
            .resolve(&pending2.id, approval::Decision::Deny));
        let resp2 = second.await.unwrap();
        assert!(resp2["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("denied")));

        assert_eq!(
            inbox_note_count(dir.path()),
            1,
            "only the approved vault-one capture may have been written"
        );
        assert_eq!(
            inbox_note_count(&dir.path().join("vault-two")),
            0,
            "the denied vault-two capture must have written nothing"
        );
    }

    // ── Fix wave: ask_always never leaves standing consent (F12) ─────────

    /// "Always ask" means always ask. `decide` already ignores grants under
    /// `ask_always`, so this asserts the stronger property: the waiter must
    /// not RECORD one either, or a later refactor of `decide` would silently
    /// start honoring consent that was never given for repeat use.
    #[tokio::test]
    async fn brain_capture_under_ask_always_records_no_grant_and_asks_every_time() {
        let (dir, router, state, token) =
            fixture_router_with_mutating_policy(policy::PolicyMode::AskAlways, 300, 30);

        for (id, title) in [(1u32, "First Always"), (2, "Second Always")] {
            let call_router = router.clone();
            let call_token = token.clone();
            let handle = tokio::spawn(async move {
                let body = call_body(
                    id,
                    "brain_capture",
                    serde_json::json!({"title": title, "text": "body"}),
                );
                post(
                    &call_router,
                    body,
                    &call_token,
                    &standard_headers("tools/call", Some("brain_capture")),
                )
                .await
            });
            let pending = wait_for_one_pending(&state).await;
            assert!(state
                .approvals
                .resolve(&pending.id, approval::Decision::Approve));
            let resp = handle.await.unwrap();
            assert!(resp.get("error").is_none(), "{resp}");

            assert!(
                !state
                    .grants
                    .has(&GrantKey::new("test-client", None, RiskClass::Mutating)),
                "an ask_always approval must never leave a grant behind (call {id})"
            );
        }

        assert_eq!(
            inbox_note_count(dir.path()),
            2,
            "both approved captures must have been written"
        );
        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(
            entries[1]["decision"], "approved",
            "the SECOND call must also have been approved by a human, not auto-allowed: {entries:?}"
        );
    }

    // ── Fix wave: an empty capture body is invalid_params (F14) ──────────

    #[tokio::test]
    async fn brain_capture_rejects_an_empty_or_whitespace_only_text() {
        for text in ["", "   \n\t "] {
            let (dir, router, _state, token) =
                fixture_router_with_mutating_policy(policy::PolicyMode::Auto, 300, 30);
            let body = call_body(
                1,
                "brain_capture",
                serde_json::json!({"title": "Empty Body", "text": text}),
            );
            let resp = post(
                &router,
                body,
                &token,
                &standard_headers("tools/call", Some("brain_capture")),
            )
            .await;
            let message = resp["error"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("expected a JSON-RPC error for {text:?}: {resp}"));
            assert!(message.contains("`text` is empty"), "{message}");
            assert_eq!(
                inbox_note_count(dir.path()),
                0,
                "a rejected capture must not leave a titled, bodyless stub behind"
            );
        }
    }

    // ── Fix wave: capabilities tells the truth about the pack (F17/F20) ──

    /// The `brain` pack's prose sits in the same `capabilities` payload as a
    /// `tools` array reporting `brain_capture` with `risk_class: mutating`.
    /// It must not contradict it.
    #[test]
    fn brain_pack_note_does_not_claim_the_pack_is_read_only() {
        let packs = capability_packs(&policy::PolicyConfig::default());
        let brain = packs
            .iter()
            .find(|p| p.name == "brain")
            .unwrap_or_else(|| panic!("no brain pack"));
        assert!(
            !brain.note.to_lowercase().contains("read-only"),
            "the pack note must not call a pack containing a write tool read-only: {}",
            brain.note
        );
        assert!(
            brain.note.contains("brain_capture"),
            "the pack note should name the write tool: {}",
            brain.note
        );
        assert!(
            brain
                .tools
                .iter()
                .any(|t| t.name == "brain_capture" && matches!(t.risk_class, RiskClass::Mutating)),
            "precondition: the same payload reports brain_capture as mutating"
        );
    }

    /// [`brain_pack_tools`] is a hand-maintained parallel list (see its doc
    /// comment). This pins the half that CAN be checked cheaply: its name
    /// set must equal the set `#[tool_router]` actually registers, so a tool
    /// added on one side and forgotten on the other fails here instead of
    /// shipping a `capabilities` response that lies about what exists.
    #[tokio::test]
    async fn brain_pack_tools_matches_the_tools_the_router_registers() {
        let (_dir, router, token) = fixture_router();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {},
        })
        .to_string();
        let resp = post(&router, body, &token, &standard_headers("tools/list", None)).await;
        let mut registered: Vec<String> = resp["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tools array: {resp}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        registered.sort();

        let mut reported: Vec<String> = brain_pack_tools(&policy::PolicyConfig::default())
            .into_iter()
            .map(|t| t.name)
            .collect();
        reported.sort();

        assert_eq!(
            reported, registered,
            "capabilities' tool list and the router's registered tools have drifted apart"
        );
    }
}
