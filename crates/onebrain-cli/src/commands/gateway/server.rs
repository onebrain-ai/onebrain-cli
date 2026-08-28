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
//! ## Dead-code allow
//! This task (Gateway PR 2, task 2) lands the handler and its tools, but no
//! CLI verb calls `build_gateway_router`/`GatewayServer::new` yet — the `run`
//! loop that does arrives in Task 4. Everything below is exercised by this
//! module's own tests but otherwise unreachable from `main` until then. Same
//! rationale as `config.rs`'s file-level allow (and `daemon_client.rs`'s
//! before it): gate the whole module rather than sprinkle per-item
//! `#[allow]`s across every pub item a future task consumes.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use rmcp::handler::server::router::tool::ToolRouter;
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
use crate::commands::gateway::GatewayConfig;
use crate::commands::task_list::{resolve_due_by, resolve_prefixes, TaskCollector};

/// Machine-level gateway state shared across every request. Sessionless
/// (§ above), so this is the only state a tool call can read — no per-session
/// data exists.
pub struct GatewayState {
    pub config: GatewayConfig,
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
    /// `config.vaults` entry; otherwise the raw configured path. `None` when
    /// no `default_vault` is configured (a call would then fall through to
    /// the env/walk-up chain — see [`resolve_vault_arg`]).
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

/// Maps a vault-resolution [`CoreError`] to an MCP `invalid_params` error —
/// the human message plus the stable `E_*` code, e.g. "no OneBrain vault
/// found by walking up from /tmp/x [E_VAULT_NOT_FOUND]".
fn core_error(err: CoreError) -> ErrorData {
    let code = err.error_code();
    ErrorData::invalid_params(format!("{err} [{code}]"), None)
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
/// raw path. Purely a display convenience — `resolve_vault_arg` re-derives
/// resolution from `config.default_vault` itself, not from this string.
fn default_vault_display(config: &GatewayConfig) -> Option<String> {
    let path = config.default_vault.as_ref()?;
    let name = config
        .vaults
        .iter()
        .find(|(_, v)| v.as_path() == path.as_path())
        .map(|(name, _)| name.clone());
    Some(name.unwrap_or_else(|| path.display().to_string()))
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
        description = "Report which capability packs and vaults this OneBrain gateway serves. Call this first to plan which brain_* tool fits the job."
    )]
    async fn capabilities(&self) -> Result<Json<CapabilitiesOut>, ErrorData> {
        let config = &self.state.config;
        Ok(Json(CapabilitiesOut {
            gateway_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: ProtocolVersion::V_2026_07_28.as_str().to_string(),
            packs: capability_packs(),
            vaults: config.vaults.keys().cloned().collect(),
            default_vault: default_vault_display(config),
        }))
    }

    #[tool(
        name = "brain_tasks",
        description = "List open vault tasks (Obsidian checkbox lines, fence-aware). Use due_by=\"today\" for the daily view. Read-only."
    )]
    async fn brain_tasks(
        &self,
        Parameters(params): Parameters<BrainTasksParams>,
    ) -> Result<Json<BrainTasksOut>, ErrorData> {
        let resolved = resolve_vault_arg(&self.state, params.vault.as_deref())?;
        let vault_name = resolved.root.name();

        let vault_config = load_vault_config(&resolved.root).map_err(core_error)?;

        let cutoff = match params.due_by.as_deref() {
            Some(raw) => Some(
                resolve_due_by(raw).map_err(|e| ErrorData::invalid_params(e.to_string(), None))?,
            ),
            None => None,
        };
        let limit = Some(params.limit.unwrap_or(20));
        let include_prefixes = resolve_prefixes(&vault_config.folders, &[]);
        let root = resolved.root.as_path().to_path_buf();

        // `visit_tasks` walks the filesystem synchronously — off the async
        // runtime, mirroring `mcp.rs`'s own filesystem-walk tools
        // (`expand_multi_get_pattern` via `spawn_blocking`).
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
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        Ok(Json(BrainTasksOut {
            tasks: tasks.into_iter().map(GatewayTaskHit::from).collect(),
            total,
            vault: vault_name,
        }))
    }

    #[tool(
        name = "brain_get",
        description = "Read one vault note by vault-relative path. Read-only."
    )]
    async fn brain_get(
        &self,
        Parameters(params): Parameters<BrainGetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let resolved = resolve_vault_arg(&self.state, params.vault.as_deref())?;
        let vault_root = resolved.root.as_path().to_path_buf();
        let rel = params.file.clone();

        let resolved_path = {
            let vault_root = vault_root.clone();
            let rel_for_resolve = rel.clone();
            tokio::task::spawn_blocking(move || resolve_under_vault(&vault_root, &rel_for_resolve))
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
                // Both of `resolve_under_vault`'s failure modes — canonicalize
                // fails because nothing exists at the joined path, or it
                // succeeds but the result escapes `vault_root` — collapse to
                // the SAME generic message here, deliberately: a distinct
                // "traversal blocked" vs. "genuinely missing" message would
                // hand a caller an oracle for probing what exists outside the
                // vault. Neither branch reaches the file's content either way.
                .map_err(|_| {
                    ErrorData::invalid_params(format!("file not found in vault: {rel}"), None)
                })?
        };

        tokio::fs::read_to_string(&resolved_path)
            .await
            .map(|text| CallToolResult::success(vec![ContentBlock::text(text)]))
            .map_err(|e| ErrorData::invalid_params(format!("reading {rel}: {e}"), None))
    }

    #[tool(
        name = "brain_search",
        description = "Search vault notes (hybrid lexical + semantic via the warm daemon). Returns scored hits with paths and snippets."
    )]
    async fn brain_search(
        &self,
        Parameters(params): Parameters<BrainSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let resolved = resolve_vault_arg(&self.state, params.vault.as_deref())?;
        let vault_path = resolved.root.as_path().to_path_buf();
        let query = params.query.clone();
        let top_k = params.top_k;

        // The gateway is a long-lived, multi-vault process: it must never
        // open a direct `redb` engine itself, because that takes an
        // exclusive per-vault file lock inside a process meant to serve MANY
        // vaults concurrently — one vault's direct-engine open would starve
        // every other vault's request against the same gateway. Search
        // always routes through the warm per-vault daemon; a daemon-start or
        // daemon-search failure is a hard `internal_error`, never a silent
        // fallback to a direct engine (unlike `commands/mcp.rs`'s
        // single-vault stdio server, which legitimately owns that fallback
        // because it only ever holds one vault's lock for its whole process
        // lifetime).
        let hits = tokio::task::spawn_blocking(move || {
            let handle = daemon_client::ensure_running(Some(&vault_path))?;
            handle.search(&query, "hybrid", top_k, None)
        })
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let body = serde_json::to_string_pretty(&hits)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
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

/// Assembles the gateway's MCP HTTP surface: a sessionless (SEP-2567)
/// Streamable HTTP service mounted at `/mcp`. The factory closure builds a
/// fresh [`GatewayServer`] per request (cloning the shared `state` handle) —
/// sessionless mode never reuses a server instance across requests, so no
/// mutable per-connection state can leak between callers.
pub fn build_gateway_router(state: Arc<GatewayState>) -> axum::Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    let service: StreamableHttpService<GatewayServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(GatewayServer::new(state.clone())),
            Default::default(),
            config,
        );
    axum::Router::new().nest_service("/mcp", service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;
    use tower::ServiceExt;

    const PROTOCOL: &str = "2026-07-28";

    /// Builds a fixture vault (`onebrain.yml` + one dated task in
    /// `01-projects/x.md`, plus the same line fenced — which must NOT count)
    /// and a router whose gateway config names it `t1` and sets it default.
    fn fixture_router() -> (tempfile::TempDir, axum::Router) {
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
        let state = Arc::new(GatewayState { config });
        (dir, build_gateway_router(state))
    }

    /// POST `body` to `/mcp` with the given extra headers (beyond the
    /// baseline content-type/accept/host every request needs — the
    /// Streamable HTTP service's DNS-rebinding guard 400s any request with
    /// no `Host` header and no URI authority, and `oneshot` supplies
    /// neither by default).
    async fn post(
        router: &axum::Router,
        body: String,
        extra: &[(&str, &str)],
    ) -> serde_json::Value {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
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
        let (_dir, router) = fixture_router();
        let resp = post(
            &router,
            init_body(1, PROTOCOL),
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
        let (_dir, router) = fixture_router();
        let resp = post(
            &router,
            init_body(1, "2025-11-25"),
            &[("MCP-Protocol-Version", "2025-11-25")],
        )
        .await;
        assert_eq!(resp["result"]["protocolVersion"], "2025-11-25", "{resp}");
    }

    #[tokio::test]
    async fn tools_list_contains_capabilities_and_brain_tasks() {
        let (_dir, router) = fixture_router();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {},
        })
        .to_string();
        let resp = post(&router, body, &standard_headers("tools/list", None)).await;
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tools array: {resp}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"capabilities"), "{names:?}");
        assert!(names.contains(&"brain_tasks"), "{names:?}");
    }

    #[tokio::test]
    async fn capabilities_reports_brain_enabled_developer_disabled() {
        let (_dir, router) = fixture_router();
        let body = call_body(1, "capabilities", serde_json::json!({}));
        let resp = post(
            &router,
            body,
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

    #[tokio::test]
    async fn brain_tasks_counts_the_unfenced_dated_task_only() {
        let (_dir, router) = fixture_router();
        let body = call_body(
            1,
            "brain_tasks",
            serde_json::json!({"due_by": "2026-12-31"}),
        );
        let resp = post(
            &router,
            body,
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

    #[tokio::test]
    async fn brain_tasks_unknown_vault_names_known_vaults_in_the_error() {
        let (_dir, router) = fixture_router();
        let body = call_body(1, "brain_tasks", serde_json::json!({"vault": "nope"}));
        let resp = post(
            &router,
            body,
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
    fn fixture_with_outside_file() -> (tempfile::TempDir, axum::Router) {
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
        let state = Arc::new(GatewayState { config });
        (workspace, build_gateway_router(state))
    }

    #[tokio::test]
    async fn brain_get_round_trips_fixture_note() {
        let (_dir, router) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "hello.md"}));
        let resp = post(
            &router,
            body,
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
        let (_dir, router) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "nope.md"}));
        let resp = post(
            &router,
            body,
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
        let (_dir, router) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "../outside.md"}));
        let resp = post(
            &router,
            body,
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
    #[tokio::test]
    async fn brain_get_rejects_absolute_path_without_leaking() {
        let (_dir, router) = fixture_with_outside_file();
        let body = call_body(1, "brain_get", serde_json::json!({"file": "/etc/hosts"}));
        let resp = post(
            &router,
            body,
            &standard_headers("tools/call", Some("brain_get")),
        )
        .await;
        assert!(
            resp.get("error").is_some(),
            "expected a JSON-RPC error: {resp}"
        );
        // /etc/hosts commonly contains this line on macOS/Linux runners; its
        // presence in the response would mean the file's content leaked.
        assert!(
            !resp.to_string().contains("127.0.0.1"),
            "content leaked: {resp}"
        );
    }

    /// Adversarial: a nested `..` traversal through a real intermediate
    /// directory (`a/../../outside.md`) landing on the same outside file as
    /// the plain `../outside.md` case. Must error AND must not leak.
    #[tokio::test]
    async fn brain_get_rejects_nested_traversal_without_leaking() {
        let (_dir, router) = fixture_with_outside_file();
        let body = call_body(
            1,
            "brain_get",
            serde_json::json!({"file": "a/../../outside.md"}),
        );
        let resp = post(
            &router,
            body,
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
        let (_dir, router) = fixture_router();
        let body = call_body(
            1,
            "brain_search",
            serde_json::json!({"query": "anything", "vault": "nope"}),
        );
        let resp = post(
            &router,
            body,
            &standard_headers("tools/call", Some("brain_search")),
        )
        .await;
        let message = resp["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error: {resp}"));
        assert!(message.contains("t1"), "{message}");
    }
}
