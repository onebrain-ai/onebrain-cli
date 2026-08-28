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
//! This task ships four tools: `capabilities` (self-description),
//! `brain_tasks` (open task listing, reusing `task_list.rs`'s scan/filter
//! composition verbatim), `brain_get` (traversal-guarded single-file read),
//! and `brain_search` (daemon-routed hybrid search — see the `brain_search`
//! doc comment for why it never falls back to a direct engine).
//!
//! `build_gateway_router`/`GatewayServer::new` are called from `onebrain
//! gateway run` (Task 4, `gateway/mod.rs::run`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

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
use crate::commands::gateway::approval::Approvals;
use crate::commands::gateway::approval_routes::approval_router;
use crate::commands::gateway::audit::{AuditEntry, AuditLog, Decision, Outcome};
use crate::commands::gateway::auth::core::now_epoch_secs;
use crate::commands::gateway::auth::middleware::require_bearer;
use crate::commands::gateway::auth::Principal;
use crate::commands::gateway::oauth_routes::{
    authorize_router, register_router, token_router, well_known_router, AuthCtx,
};
use crate::commands::gateway::policy::{self, Grants, PolicyOutcome, RiskClass};
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
    /// Pending human approvals (Gateway PR 4, Task 3) — the operator-facing
    /// `/approvals` HTTP surface (`approval_routes.rs`) reads/writes this
    /// directly via `Arc<GatewayState>`; no tool handler in THIS task calls
    /// `register`/`wait` yet (see [`policy_gate`]'s doc comment — that
    /// wiring is a later task).
    pub approvals: Approvals,
}

impl GatewayState {
    pub fn new(config: GatewayConfig, audit: AuditLog) -> Self {
        Self {
            config,
            grants: Grants::new(),
            audit,
            approvals: Approvals::new(),
        }
    }
}

#[derive(Clone)]
pub struct GatewayServer {
    state: Arc<GatewayState>,
    tool_router: ToolRouter<Self>,
}

/// Output of the `capabilities` tool.
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
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct PackInfo {
    pub name: String,
    pub enabled: bool,
    pub tools: Vec<String>,
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

/// The brain pack's own tool names, kept in one place so `capabilities`
/// can't drift from what `#[tool_router]` actually registers.
fn brain_pack_tools() -> Vec<String> {
    vec![
        "capabilities".to_string(),
        "brain_tasks".to_string(),
        "brain_get".to_string(),
        "brain_search".to_string(),
    ]
}

/// The full pack list `capabilities` reports. Only `brain` is enabled this
/// task; `developer`/`files`/`mac` are the roadmapped packs (later PRs),
/// listed disabled so a caller can see what's coming without probing for
/// tools that don't exist yet.
fn capability_packs() -> Vec<PackInfo> {
    vec![
        PackInfo {
            name: "brain".to_string(),
            enabled: true,
            tools: brain_pack_tools(),
            note: "Read-only vault search, retrieval, and task listing.".to_string(),
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
/// class `class`. `Ok(Decision::Auto)` means the call may proceed; `Err`
/// carries BOTH the [`Decision`] to record in the audit log and the
/// client-facing [`ErrorData`] to return, for the two ways a call may not
/// proceed:
///
/// - `PolicyOutcome::Deny` (policy `deny`, or a scope/pack mismatch) →
///   `Decision::Denied`.
/// - `PolicyOutcome::NeedApproval` → `Decision::Blocked`. This gateway build
///   (Task 2) has no interactive approval channel to route the request to
///   yet (that lands in Task 3+) — rather than hang forever or silently
///   allow, it fails closed with a clear "not yet supported" error. Once an
///   approval channel exists, THIS is the branch a later task replaces with
///   an actual register-and-wait call.
///
/// All four of today's tools are `RiskClass::ReadOnly`, which defaults to
/// `PolicyMode::Auto` — so under the DEFAULT `gateway.yml`, every call here
/// takes the `Allow` path and this function is a no-op pass-through. The
/// `Deny`/`NeedApproval` branches become reachable the moment an operator
/// customizes `policy.read_only`, or a future `Mutating`/`Destructive` tool
/// ships with its own default mode.
fn policy_gate(
    state: &GatewayState,
    principal: &Principal,
    tool: &str,
    class: RiskClass,
) -> Result<Decision, (Decision, ErrorData)> {
    match policy::decide(&state.config.policy, &state.grants, principal, class) {
        PolicyOutcome::Allow => Ok(Decision::Auto),
        PolicyOutcome::Deny => Err((
            Decision::Denied,
            ErrorData::invalid_request(format!("gateway policy denies this call [{tool}]"), None),
        )),
        PolicyOutcome::NeedApproval => Err((
            Decision::Blocked,
            ErrorData::invalid_request(
                format!(
                    "this call requires interactive approval, which this gateway build does not yet support [{tool}]"
                ),
                None,
            ),
        )),
    }
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
        args_summary: meta.args_summary,
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
        let (decision, result) =
            match policy_gate(&self.state, &principal, "capabilities", RiskClass::ReadOnly) {
                Ok(decision) => {
                    let config = &self.state.config;
                    let out = CapabilitiesOut {
                        gateway_version: env!("CARGO_PKG_VERSION").to_string(),
                        protocol_version: ProtocolVersion::V_2026_07_28.as_str().to_string(),
                        packs: capability_packs(),
                        vaults: config.vaults.keys().cloned().collect(),
                        default_vault: default_vault_display(config),
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
                args_summary: "capabilities: (no arguments)".to_string(),
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

        let (decision, result) =
            match policy_gate(&self.state, &principal, "brain_tasks", RiskClass::ReadOnly) {
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

        let (decision, result) =
            match policy_gate(&self.state, &principal, "brain_get", RiskClass::ReadOnly) {
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
                            .map_err(|e| {
                                ErrorData::invalid_params(format!("reading {rel}: {e}"), None)
                            })
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

        let (decision, result) =
            match policy_gate(&self.state, &principal, "brain_search", RiskClass::ReadOnly) {
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
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for GatewayServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "OneBrain Gateway — Brain pack (read-only). Call `capabilities` first to see \
                 packs and vaults. `brain_search` finds notes, `brain_get` reads one, \
                 `brain_tasks` lists open tasks. Vault-relative paths; select a vault by name \
                 via the `vault` argument or omit it for the default vault.",
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

        // Final-review fix (Item 2): every Brain-pack tool is read-only, so
        // every listed tool must carry `annotations.readOnlyHint == true` —
        // parsed as JSON (not substring-matched) so a tool that carries the
        // hint nested somewhere unexpected, or omits it, fails clearly.
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
    /// `NeedApproval` branches of the Task 2 wiring since all four existing
    /// tools are `RiskClass::ReadOnly`.
    fn fixture_router_with_read_only_policy(
        read_only: policy::PolicyMode,
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
        let (dir, router, token) = fixture_router_with_read_only_policy(policy::PolicyMode::Deny);
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

    /// A `policy.read_only: ask_once` config with NO prior grant must fail
    /// closed — Task 2 wires `decide` and the audit trail, but ships no
    /// interactive approval channel yet (that's Task 3+), so `NeedApproval`
    /// must surface as a clear "not yet supported" client error and an
    /// audit `decision: "blocked"`, never a silent allow and never a hang.
    #[tokio::test]
    async fn capabilities_is_blocked_when_policy_needs_approval_with_no_channel_available() {
        let (dir, router, token) =
            fixture_router_with_read_only_policy(policy::PolicyMode::AskOnce);
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
        assert!(
            message.contains("requires interactive approval"),
            "{message}"
        );
        assert!(message.contains("does not yet support"), "{message}");

        let entries = read_audit_entries(dir.path());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["decision"], "blocked");
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
}
