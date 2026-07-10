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
//! Both entry points build a [`ServeConfig`] and serve the same router. They
//! differ in three ways:
//! - **Shutdown signal** — `onebrain serve` (foreground) → Ctrl-C
//!   (`tokio::signal::ctrl_c`); `daemon __run` (detached) → SIGTERM (or the
//!   daemon's idle-timeout).
//! - **Engine ownership** — the daemon sets `hold_engine` so the search
//!   [`Engine`](onebrain_search::engine::Engine) is opened ONCE and held on
//!   [`AppState::search_engine`] for the process lifetime (the sole redb owner;
//!   `serve` opens per-request). This also lights up the token-gated
//!   `/api/internal/*` reindex + status routes.
//! - **Entry point** — `serve` calls [`run_server`]; the daemon uses
//!   [`build_router_with_state`] + [`run_server_from_router`] so it can read
//!   `last_activity` for idle-shutdown and publish `daemon.json` on bind.
//!
//! Factoring the router into [`build_router`] (which never touches a socket)
//! keeps every handler unit-testable via `tower::ServiceExt::oneshot`.
//!
//! ## Security model (single-tenant, localhost)
//! - Bind `127.0.0.1` — the primary boundary. The `--host` flag was removed
//!   (#205); a non-loopback bind (`$ONEBRAIN_BIND`, containers only) is
//!   single-tenant only and MUST run behind TLS.
//! - EVERY route — `/api/*` AND the static SPA — requires the per-session token
//!   (header, `?token=` query, or the `onebrain_token` cookie seeded by the
//!   first `?token=` load), the **sole** exception being `GET`/`HEAD /robots.txt`
//!   (static public boilerplate, no vault data). Gating the static shell too is
//!   what stops an unauthenticated browser from loading the page (which carries
//!   the token).
//! - `GET /api/vault/file` canonicalises the requested path and rejects
//!   anything that escapes the vault root (`..`, absolute paths, symlinks out).

mod api;
mod auth;
mod chat;
mod headers;
mod internal;
mod search;
mod r#static;
mod token;
mod translate;
mod webview;

#[cfg(test)]
mod tests;

pub use r#static::{
    has_embedded_ui, parse_webui_released, parse_webui_version, webui_released, webui_version,
};
pub use token::resolve_token;

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
    /// Static web dist to serve as an SPA, overriding the embedded web UI.
    /// `None` → serve the binary-embedded UI (or a built-in placeholder page if
    /// this binary was built without one).
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
    /// When `true`, open the search [`Engine`] ONCE at router-build time and
    /// hold it for the process lifetime (the warm-daemon model — see
    /// [`SharedEngine`]). The daemon sets this so it is the sole redb owner and
    /// mcp/CLI clients route search + reindex through `/api/vault/search` /
    /// `/api/internal/*`. `serve` leaves it `false` (opens per-request as
    /// before), since a foreground `serve` is short-lived and not the canonical
    /// engine owner.
    pub hold_engine: bool,
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
            hold_engine: false,
        }
    }

    /// The `host:port` socket address this config binds.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

/// The one search [`Engine`] a warm daemon opens at boot and holds for its
/// whole lifetime. redb (the engine's KV store) is single-process /
/// single-writer, so exactly ONE process may open a given collection's engine
/// at a time — the daemon is that process, and mcp + CLI search become HTTP
/// clients of it (`/api/vault/search`, `/api/internal/*`).
///
/// A blocking [`std::sync::Mutex`] (not tokio's) because every use is inside a
/// `spawn_blocking` closure: the engine's search/reindex calls are synchronous
/// (tantivy + redb + embedding), so they must never run on an async worker.
/// Reads and reindex writes all serialise on this one lock; a reindex holds it
/// only for its (small-batch) duration so interleaved searches make progress
/// between batches.
pub type SharedEngine = Arc<std::sync::Mutex<onebrain_search::engine::Engine>>;

/// Shared, immutable state handed to every handler via axum's `State`
/// extractor. Wrapped in an `Arc` by the router so cloning per-request is a
/// refcount bump, not a deep copy of the (potentially long) vault path.
///
/// `Debug` is hand-written (not derived) because [`SharedEngine`]'s `Engine`
/// isn't `Debug` — and its contents (an index handle, a lazy embedder) aren't
/// meaningfully printable anyway; the field renders as a presence flag.
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
    /// The persistent search engine, opened ONCE at daemon boot and held for
    /// the process lifetime (see [`SharedEngine`]). `None` when no engine is
    /// held — the foreground `serve` command and the unit-test router leave
    /// this `None`, so `/api/vault/search` opens the engine per-request as
    /// before, and `/api/internal/*` report 503. The daemon (`daemon __run`)
    /// supplies `Some` so it is the sole redb owner and mcp/CLI clients route
    /// through it.
    pub search_engine: Option<SharedEngine>,
    /// Monotonic-ish "last request seen" marker: epoch-seconds of the most
    /// recent authenticated request, bumped by the auth middleware on every
    /// request that reaches the surface. The daemon's idle-shutdown loop reads
    /// it to decide when the process has been idle long enough to exit and
    /// release the redb lock. `serve` and the tests ignore it (they never poll
    /// for idle-shutdown), so it's harmless overhead there.
    pub last_activity: Arc<std::sync::atomic::AtomicU64>,
    /// Caps how many `POST /api/chat` agent turns may run at once. Each chat
    /// turn spawns a full `claude` agent (loads MEMORY/plugins, burns API
    /// tokens), so an unbounded fan-out is a denial-of-wallet vector even behind
    /// the auth token. Handlers `try_acquire_owned()` a permit and hold it for
    /// the lifetime of the turn (503 when exhausted).
    pub chat_limit: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("vault_root", &self.vault_root)
            // Token elided — never print the auth credential, even in Debug.
            .field("dist_dir", &self.dist_dir)
            .field("search_engine_held", &self.search_engine.is_some())
            .finish_non_exhaustive()
    }
}

/// Max concurrent `POST /api/chat` agent turns. Small: this is a single-tenant
/// local assistant, not a fleet — a couple in flight is plenty and bounds the
/// parallel API-token burn.
pub const MAX_CONCURRENT_CHATS: usize = 2;

/// Current wall-clock time as epoch seconds (0 before the epoch — never in
/// practice). Used to seed + bump [`AppState::last_activity`] for idle-shutdown.
pub(crate) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
///   /robots.txt            GET   → static "Disallow: /" (the ONE public route)
///   /* (everything else)   GET   → static dist (SPA fallback to index.html)
///       └─ every route EXCEPT /robots.txt gated by the auth-token middleware
///          (401 without a header / ?token= / onebrain_token cookie)
/// ```
pub fn build_router(cfg: ServeConfig) -> Router {
    build_router_with_state(cfg).0
}

/// [`build_router`] but also returns the shared [`AppState`], so the daemon can
/// read `last_activity` for its idle-shutdown loop. The router already holds an
/// `Arc` clone of the same state, so the two stay in sync.
pub fn build_router_with_state(cfg: ServeConfig) -> (Router, Arc<AppState>) {
    // Warm-daemon path: open the engine ONCE now and hold it for the process
    // lifetime. Only when `hold_engine` is set AND a vault is bound — a failure
    // to open (never-indexed vault, model resolution error) leaves the held
    // engine `None`, so search transparently falls back to the per-request open
    // and `/api/internal/*` report 503 until the index exists. `serve` and the
    // unit-test router leave `hold_engine` false, so this is skipped entirely.
    let search_engine = if cfg.hold_engine {
        cfg.vault_root
            .as_deref()
            .and_then(internal::open_held_engine)
    } else {
        None
    };

    let state = Arc::new(AppState {
        vault_root: cfg.vault_root,
        token: cfg.token,
        dist_dir: cfg.dist_dir,
        search_engine,
        last_activity: Arc::new(std::sync::atomic::AtomicU64::new(now_epoch_secs())),
        chat_limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CHATS)),
    });

    // The API sub-router stays a `Router<Arc<AppState>>` (no own `.with_state`);
    // the single `.with_state(state)` at the bottom supplies state to the whole
    // tree — nested API routes AND the static fallback — exactly once.
    let api = api::router().merge(internal::router());

    // DoS hardening note (fix L, deferred): a `tower_http::timeout::TimeoutLayer`
    // here would cap slow/stuck requests cheaply. It needs tower-http's `timeout`
    // feature, which this crate does NOT enable (only `fs` is on), and turning it
    // on is a Cargo.toml change out of scope for this src-only pass. Add the
    // feature + the 30s `TimeoutLayer` in a follow-up.
    let router = Router::new()
        .nest("/api", api)
        // Static + SPA fallback handles every non-`/api` path. `fallback`
        // (not a route) so it only fires when no API route matched.
        .fallback(r#static::serve_static)
        // Auth gate on the ENTIRE surface — API *and* the static SPA. Gating the
        // static shell too is what stops an unauthenticated browser from loading
        // the page (which carries the token). `from_fn_with_state` needs the
        // state directly, so it gets its own clone. Applied here (outermost) so
        // every request — nested or fallback — passes through it.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ))
        // Outermost: stamp security headers on EVERY response, including the auth
        // layer's 401s. (Applied after auth so it wraps it.)
        .layer(axum::middleware::from_fn(headers::security_headers))
        .with_state(state.clone());
    (router, state)
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
    run_server_with(cfg, shutdown, |_| {}).await
}

/// [`run_server`] plus an `on_bind` callback fired with the ACTUAL bound
/// [`SocketAddr`] once the listener is up (before serving). The daemon uses it
/// to publish its discovery file (`~/.onebrain/run/daemon.json`) with the real
/// port — which matters when it binds port `0` (OS-assigned) so a client can
/// still find it. `serve` and the tests use the no-op [`run_server`].
pub async fn run_server_with(
    cfg: ServeConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
    on_bind: impl FnOnce(SocketAddr),
) -> anyhow::Result<()> {
    let addr = cfg.socket_addr();
    let router = build_router(cfg);
    run_server_from_router(router, addr, on_bind, shutdown).await
}

/// Bind `addr`, serve `router` until `shutdown` resolves, and fire `on_bind`
/// with the real bound address in between. The lowest-level entry point: the
/// daemon uses it directly so it can pre-build the router (via
/// [`build_router_with_state`]) and keep a handle on the shared state for its
/// idle-shutdown loop. [`run_server`] / [`run_server_with`] are thin wrappers
/// that build the router from a [`ServeConfig`] first.
pub async fn run_server_from_router(
    router: Router,
    addr: SocketAddr,
    on_bind: impl FnOnce(SocketAddr),
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    // Bind first so a port-in-use error surfaces here (with context) rather
    // than deep inside `axum::serve`.
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP listener on {addr}"))?;
    let bound = listener.local_addr().unwrap_or(addr); // `local_addr` only fails on a closed socket.

    // Log the bound address WITHOUT the token (see the doc comment above): the
    // address tells an operator where to point a browser; the token must not
    // land in the persistent log.
    tracing::info!(addr = %bound, "OneBrain HTTP surface listening");

    // Publish discovery (real bound port) now that we know the actual address.
    on_bind(bound);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server error")?;

    tracing::info!("HTTP surface shut down cleanly");
    Ok(())
}
