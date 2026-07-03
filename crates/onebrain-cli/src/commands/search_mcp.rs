//! `onebrain search mcp` — MCP stdio server over the native search engine.
//!
//! Mirrors the qmd MCP tool surface (`query`/`get`/`multi_get`/`status`) so the
//! plugin's `.mcp.json` can swap `qmd mcp` -> `onebrain search mcp` without any
//! instruction changes (tool namespace rename lands in v3.4.2). tokio lives only
//! at this boundary; the sync engine is called via `spawn_blocking`.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};

use onebrain_core::path::ResolvedVault;
use onebrain_search::engine::Engine;

use super::search_common::open_engine;
use super::search_status::{status_data_for, SearchStatusData};

#[derive(Clone)]
pub struct SearchMcpServer {
    engine: Arc<Mutex<Engine>>,
    resolved: ResolvedVault,
    tool_router: ToolRouter<Self>,
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[tool_router]
impl SearchMcpServer {
    pub fn new(engine: Engine, resolved: ResolvedVault) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
            resolved,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs a closure against the engine on a blocking thread.
    async fn with_engine<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(&mut Engine) -> anyhow::Result<T> + Send + 'static,
    {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            let mut eng = engine
                .lock()
                .map_err(|_| anyhow::anyhow!("engine mutex poisoned"))?;
            f(&mut eng)
        })
        .await
        .map_err(internal)?
        .map_err(internal)
    }

    #[tool(
        name = "status",
        description = "Show the status of the search index: collection, embed model, document counts, pending changes, and health information."
    )]
    async fn status(&self) -> Result<Json<SearchStatusData>, ErrorData> {
        let resolved = self.resolved.clone();
        self.with_engine(move |eng| status_data_for(eng, &resolved))
            .await
            .map(Json)
    }
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for SearchMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "OneBrain native vault search. Use `query` with typed sub-queries \
             (lex = BM25 keywords, vec = semantic question, hyde = hypothetical \
             answer passage); `get`/`multi_get` to read documents; `status` for \
             index health. Paths are vault-relative.",
        )
    }
}

pub fn run(vault_flag: Option<PathBuf>) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for search mcp")?;
    runtime.block_on(async move {
        let service = SearchMcpServer::new(engine, resolved)
            .serve(stdio())
            .await
            .context("initialize MCP stdio server")?;
        service.waiting().await.context("MCP server terminated")?;
        Ok(())
    })
}
