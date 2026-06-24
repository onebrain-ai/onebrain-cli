//! The OneBrain HTTP surface (v3.3 daemon step 2).
//!
//! ONE local listener that does two jobs (per the build-level design
//! `2026-06-04-daemon-serve-design.md` §3):
//!
//! 1. **Static SPA** — serves a pre-built web `dist/` directory, falling back to
//!    `index.html` for unknown non-`/api` routes (client-side routing). The
//!    served `index.html` has the per-session auth token injected so a browser
//!    SPA can read it without it being hard-coded.
//! 2. **Read-only JSON API** under `/api/*` — a thin HTTP veneer over the CLI's
//!    existing vault primitives (`onebrain_core` config + a `walkdir` tree +
//!    `std::fs` file read). No vault logic is re-implemented here.
//!
//! ## Shared by `serve` and `daemon __run`
//! Both entry points build a [`ServeConfig`] and call [`run_server`]. The only
//! difference is the *shutdown signal*:
//! - `onebrain serve` (foreground) → Ctrl-C (`tokio::signal::ctrl_c`).
//! - `daemon __run` (detached) → SIGTERM (`tokio::signal::unix`).
//!
//! Factoring the router into [`build_router`] (which never touches a socket)
//! keeps every handler unit-testable via `tower::ServiceExt::oneshot`.
//!
//! ## Security model (single-tenant, localhost)
//! - Bind `127.0.0.1` by default — the primary boundary. A remote self-host
//!   (`--host 0.0.0.0`) is single-tenant only and MUST run behind TLS.
//! - Every `/api/*` route requires the per-session token (header). Static
//!   routes are open on localhost (the token rides in the served HTML).
//! - `GET /api/vault/file` canonicalises the requested path and rejects
//!   anything that escapes the vault root (`..`, absolute paths, symlinks out).

mod api;
mod auth;
mod chat;
mod r#static;
mod token;

#[cfg(test)]
mod tests;

pub use token::generate_token;

use anyhow::Context;
use axum::Router;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

/// Everything [`run_server`] / [`build_router`] need to stand up the surface.
///
/// Cheap to clone-by-`Arc`: the router shares one [`AppState`] across handlers,
/// so this struct is consumed once at build time.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Static web dist to serve as an SPA. `None` → serve a built-in
    /// placeholder `index.html` so the API is still reachable for testing.
    pub dist_dir: Option<PathBuf>,
    /// The vault the JSON API reads from, or `None` when no real vault is bound.
    /// All `/api/vault/*` paths resolve relative to (and are confined within)
    /// this root. When `None`, the vault handlers return 503 instead of touching
    /// the filesystem (see [`AppState::vault_root`]). `serve` always passes
    /// `Some` (it requires a vault); the daemon may pass `None`.
    pub vault_root: Option<PathBuf>,
    /// Bind address. Defaults to `127.0.0.1` (see [`ServeConfig::localhost`]).
    pub host: IpAddr,
    /// Bind port.
    pub port: u16,
    /// Per-session random auth token, required on every `/api/*` call.
    pub token: String,
}

impl ServeConfig {
    /// Build a localhost (`127.0.0.1`) config — the common case for both
    /// `serve` and the daemon. Callers override `host` afterwards for a remote
    /// self-host.
    pub fn localhost(
        vault_root: Option<PathBuf>,
        port: u16,
        token: String,
        dist_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            dist_dir,
            vault_root,
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            token,
        }
    }

    /// The `host:port` socket address this config binds.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

/// Shared, immutable state handed to every handler via axum's `State`
/// extractor. Wrapped in an `Arc` by the router so cloning per-request is a
/// refcount bump, not a deep copy of the (potentially long) vault path.
#[derive(Debug)]
pub struct AppState {
    /// The vault the JSON API reads from, or `None` when no real vault is bound.
    ///
    /// `None` is the security-critical state: the daemon may start without a
    /// vault (e.g. `$ONEBRAIN_VAULT` unset/invalid), and in that case the three
    /// vault handlers MUST refuse to serve the filesystem (503) rather than fall
    /// back to a root like `/` and expose `/etc/passwd`. Static serving + the
    /// token still work so the daemon runs and reports cleanly. The foreground
    /// `serve` command always supplies `Some` (it requires a vault up front).
    pub vault_root: Option<PathBuf>,
    pub token: String,
    pub dist_dir: Option<PathBuf>,
    /// Caps how many `POST /api/chat` agent turns may run at once. Each chat
    /// turn spawns a full `claude` agent (loads MEMORY/plugins, burns API
    /// tokens), so an unbounded fan-out is a denial-of-wallet vector even behind
    /// the auth token. Handlers `try_acquire_owned()` a permit and hold it for
    /// the lifetime of the turn (503 when exhausted).
    pub chat_limit: Arc<tokio::sync::Semaphore>,
}

/// Max concurrent `POST /api/chat` agent turns. Small: this is a single-tenant
/// local assistant, not a fleet — a couple in flight is plenty and bounds the
/// parallel API-token burn.
pub const MAX_CONCURRENT_CHATS: usize = 2;

/// Build the axum [`Router`] for a given config WITHOUT binding a socket.
///
/// This is the seam the tests drive directly (`Router::oneshot`) — keeping it
/// socket-free means handler behaviour (auth, path-traversal rejection, JSON
/// shapes, SPA fallback) is testable without a live server or a free port.
///
/// Route layout:
/// ```text
///   /api/config            GET   → onebrain.yml as JSON
///   /api/vault/tree        GET   → recursive folder/file listing
///   /api/vault/file?path=  GET   → one note's content + rev
///       └─ all of /api/*   gated by the auth-token middleware (401 without)
///   /* (everything else)   GET   → static dist (SPA fallback to index.html)
/// ```
pub fn build_router(cfg: ServeConfig) -> Router {
    let state = Arc::new(AppState {
        vault_root: cfg.vault_root,
        token: cfg.token,
        dist_dir: cfg.dist_dir,
        chat_limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CHATS)),
    });

    // The `/api` sub-router carries the auth layer. It is built WITHOUT its own
    // `.with_state` (it stays a `Router<Arc<AppState>>`); the single
    // `.with_state(state)` at the bottom supplies the state to the whole tree —
    // both the nested API routes and the static fallback — exactly once. The
    // auth layer still needs the state directly (it's `from_fn_with_state`), so
    // it gets its own clone here.
    let api = api::router().layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::require_token,
    ));

    // DoS hardening note (fix L, deferred): a `tower_http::timeout::TimeoutLayer`
    // here would cap slow/stuck requests cheaply. It needs tower-http's `timeout`
    // feature, which this crate does NOT enable (only `fs` is on), and turning it
    // on is a Cargo.toml change out of scope for this src-only pass. Add the
    // feature + the 30s `TimeoutLayer` in a follow-up.
    Router::new()
        .nest("/api", api)
        // Static + SPA fallback handles every non-`/api` path. `fallback`
        // (not a route) so it only fires when no API route matched. The handler
        // reads the shared state via the `State` extractor, supplied by the
        // single `.with_state` below.
        .fallback(r#static::serve_static)
        .with_state(state)
}

/// Bind `cfg.host:cfg.port` and serve until `shutdown` resolves.
///
/// The `shutdown` future is the caller's graceful-stop signal (Ctrl-C for
/// `serve`, SIGTERM for the daemon). When it resolves, axum stops accepting new
/// connections and drains in-flight ones before this returns `Ok(())`.
///
/// Logs the bound address via `tracing` so the daemon log / foreground console
/// shows where the surface came up. The session token is NEVER logged: the
/// daemon log lives at `~/.onebrain/run/daemon.log` (a long-lived, potentially
/// world-readable file), so writing `?token=…` there would leak the credential
/// to any local process that can read the log. The foreground `serve` command
/// prints the full token-bearing URL to its OWN stdout (a transient console the
/// user is already looking at) — that's fine and stays in `serve.rs`.
pub async fn run_server(
    cfg: ServeConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = cfg.socket_addr();
    let dist = cfg.dist_dir.clone();

    let router = build_router(cfg);

    // Bind first so a port-in-use error surfaces here (with context) rather
    // than deep inside `axum::serve`.
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP listener on {addr}"))?;
    let bound = listener.local_addr().unwrap_or(addr); // `local_addr` only fails on a closed socket.

    // Log the bound address WITHOUT the token (see the doc comment above): the
    // address tells an operator where to point a browser; the token must not
    // land in the persistent log.
    tracing::info!(
        addr = %bound,
        dist = ?dist,
        "OneBrain HTTP surface listening"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server error")?;

    tracing::info!("HTTP surface shut down cleanly");
    Ok(())
}
