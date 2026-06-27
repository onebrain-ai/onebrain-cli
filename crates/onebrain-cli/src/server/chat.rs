//! `POST /api/chat` — run a OneBrain agent turn by spawning `claude -p` against
//! the bound vault and relaying its `stream-json` output as Server-Sent Events.
//!
//! ## Context retention
//! claude assigns a session id (emitted in the `init` event and echoed on
//! `result`). We forward it to the client as a `session` SSE event; the client
//! stores it and sends it back on the next turn as `session_id`, which we pass
//! as `--resume <id>` so the agent continues the SAME conversation with full
//! prior context. The visible transcript (history) is the client's to persist;
//! this endpoint stays stateless beyond forwarding the session id.
//!
//! ## SSE event types emitted (exactly one terminal event per turn)
//! - `session` `{ session_id }`  — once, as soon as claude reports it
//! - `delta`   `{ text }`        — each assistant text block as it arrives
//! - `done`    `{ result, session_id, is_error }` — the turn finished (terminal)
//! - `error`   `{ message }`     — spawn/exit/read/timeout failure (terminal)
//!
//! ## Security & resource safety
//! - The `/api/*` auth token gates this route (auth middleware runs first).
//! - Concurrency is capped by `AppState::chat_limit` (denial-of-wallet guard):
//!   each turn holds a semaphore permit for its whole lifetime; over the cap → 503.
//! - `session_id` / `model` are validated to a strict charset before reaching the
//!   argv, so a hostile client can't smuggle a `--flag` past `--resume`/`--model`.
//! - The child runs in its OWN process group (unix) and the group is SIGKILLed on
//!   drop/disconnect, so the agent AND its tool/MCP grandchildren are reaped — no
//!   orphan process keeps burning API tokens after the client is gone.
//! - A per-turn timeout bounds a stuck agent.

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures_core::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use super::api::ApiError;
use super::AppState;

/// Hard ceiling on a single turn — a stuck/looping agent can't hold a connection
/// (and a permit) forever.
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
/// If no event arrives for this long the turn is considered hung.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// `POST /api/chat` request body.
#[derive(Debug, Deserialize)]
pub(crate) struct ChatReq {
    /// The user's message for this turn.
    message: String,
    /// Prior claude session id to resume (omit/null to start a new conversation).
    #[serde(default)]
    session_id: Option<String>,
    /// Optional model override (e.g. `"claude-opus-4-8"`).
    #[serde(default)]
    model: Option<String>,
}

/// A claude session id is a UUID-ish token; allow only `[A-Za-z0-9-]` AND forbid
/// a leading `-`. The leading-dash guard is the security-critical part: `--resume`
/// takes an OPTIONAL value, so a value like `--dangerously-skip-permissions` would
/// otherwise be re-parsed by clap as a separate FLAG (flag-smuggling). A real UUID
/// never starts with `-`, so this is safe.
fn valid_session_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A model name is `[A-Za-z0-9._-]` with no leading `-` — same anti-flag-smuggling
/// guard after `--model`.
fn valid_model(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Run one chat turn and stream the agent's reply as SSE.
pub(crate) async fn post_chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatReq>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Concurrency cap — held for the whole turn (moved into the stream below).
    let permit = state.chat_limit.clone().try_acquire_owned().map_err(|_| {
        ApiError::ServiceUnavailable("agent busy — too many concurrent chats".into())
    })?;

    // Validate everything up front: once the SSE 200 begins, an HTTP status can
    // no longer be returned.
    let vault = state
        .vault_root
        .as_deref()
        .ok_or_else(|| ApiError::ServiceUnavailable("no vault bound".to_string()))?
        .to_path_buf();
    let message = req.message.trim().to_string();
    if message.is_empty() {
        return Err(ApiError::BadRequest("empty message".to_string()));
    }
    // Cap the prompt: each turn spawns a `claude` agent, so an oversized message
    // multiplies API-token burn (and would bump the OS argv limit). 100 KB is
    // generous for a chat turn (~25k words) while bounding abuse.
    const MAX_MESSAGE_BYTES: usize = 100 * 1024;
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ApiError::BadRequest(format!(
            "message too large ({} bytes; max {MAX_MESSAGE_BYTES})",
            message.len()
        )));
    }
    if let Some(sid) = req.session_id.as_deref().filter(|s| !s.is_empty()) {
        if !valid_session_id(sid) {
            return Err(ApiError::BadRequest("invalid session id".to_string()));
        }
    }
    if let Some(m) = req.model.as_deref().filter(|s| !s.is_empty()) {
        if !valid_model(m) {
            return Err(ApiError::BadRequest("invalid model".to_string()));
        }
    }

    let bin = onebrain_fs::resolve_claude_bin(
        None,
        |k| std::env::var(k).ok(),
        |p| p.exists(),
        std::env::var("HOME").ok().as_deref(),
    )
    .path;

    let vault_str = vault.to_string_lossy().to_string();
    let mut argv: Vec<String> = vec![
        "-p".into(),
        message,
        "--add-dir".into(),
        vault_str,
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if let Some(sid) = req.session_id.as_ref().filter(|s| !s.is_empty()) {
        argv.push("--resume".into());
        argv.push(sid.clone());
    }
    if let Some(m) = req.model.as_ref().filter(|s| !s.is_empty()) {
        argv.push("--model".into());
        argv.push(m.clone());
    }

    let mut cmd = Command::new(&bin);
    cmd.args(&argv)
        .current_dir(&vault)
        // Strip the daemon's OWN auth token from the agent's environment — `claude`
        // doesn't need it, and neither the agent nor its MCP tools / bash should be
        // able to read the credential that gates the HTTP API. (Other inherited env
        // — PATH, HOME, the model API key — the agent genuinely needs and is the
        // user's own environment anyway, so we leave it.)
        .env_remove("ONEBRAIN_TOKEN")
        // ONEBRAIN_HEADLESS=1 makes `onebrain session init` report headless, so
        // the agent skips the interactive greeting/status ceremony and answers
        // directly — but still loads MEMORY/identity (same as the scheduler).
        .env("ONEBRAIN_HEADLESS", "1")
        // `claude -p` appends piped stdin to the prompt; null it so an inherited
        // TTY can never block the child waiting for EOF.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Reap the direct child on drop (covers the common disconnect case).
        .kill_on_drop(true);
    // Put the agent in its own process group so we can SIGKILL the WHOLE tree
    // (claude + its MCP servers / bash tools / hooks) on disconnect — not just
    // the parent pid, which would orphan the grandchildren.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| {
        tracing::warn!(error = %e, "spawn claude failed");
        ApiError::Internal("could not start the agent".to_string())
    })?;

    #[cfg(unix)]
    let pg_guard = child
        .id()
        .map(|id| ProcessGroupGuard(nix::unistd::Pid::from_raw(id as i32)));

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Drain stderr concurrently so a chatty agent can't fill the stderr pipe and
    // deadlock before we read stdout. We keep only the tail for logging.
    let stderr_tail = Arc::new(tokio::sync::Mutex::new(String::new()));
    let stderr_task = {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut buf = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut buf).await;
            let t: String = buf
                .chars()
                .rev()
                .take(800)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            *tail.lock().await = t;
        })
    };

    let stream = async_stream::stream! {
        // Hold the permit + process-group guard for the whole stream; dropping the
        // stream (client disconnect) releases the permit and SIGKILLs the group.
        let _permit = permit;
        #[cfg(unix)]
        let _pg = pg_guard;

        let mut lines = BufReader::new(stdout).lines();
        let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
        let mut sent_session = false;
        let mut done_sent = false;

        loop {
            let idle = tokio::time::Instant::now() + IDLE_TIMEOUT;
            let until = deadline.min(idle);
            match tokio::time::timeout_at(until, lines.next_line()).await {
                Err(_elapsed) => {
                    if !done_sent {
                        yield Ok(sse_json("error", serde_json::json!({ "message": "the agent timed out" })));
                    }
                    break;
                }
                Ok(Ok(Some(line))) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let v: serde_json::Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(_) => continue, // non-JSON noise — skip
                    };
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("system") => {
                            if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                                    sent_session = true;
                                    yield Ok(sse_json("session", serde_json::json!({ "session_id": sid })));
                                }
                            }
                        }
                        Some("assistant") => {
                            if let Some(content) = v
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_array())
                            {
                                for block in content {
                                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                            if !text.is_empty() {
                                                yield Ok(sse_json("delta", serde_json::json!({ "text": text })));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some("result") => {
                            let sid = v.get("session_id").and_then(|s| s.as_str());
                            if !sent_session {
                                if let Some(sid) = sid {
                                    yield Ok(sse_json("session", serde_json::json!({ "session_id": sid })));
                                }
                            }
                            let is_error =
                                v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                            let result = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
                            done_sent = true;
                            yield Ok(sse_json(
                                "done",
                                serde_json::json!({
                                    "result": result,
                                    "session_id": sid,
                                    "is_error": is_error,
                                }),
                            ));
                        }
                        _ => {}
                    }
                }
                Ok(Ok(None)) => break, // stdout EOF — the agent finished
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "read claude stdout failed");
                    if !done_sent {
                        yield Ok(sse_json("error", serde_json::json!({ "message": "stream read error" })));
                    }
                    break;
                }
            }
        }

        // Reap the child. Surface a non-zero exit (when we haven't already sent a
        // terminal `done`) as a generic error — detail is logged, never sent.
        match child.wait().await {
            Ok(status) if !status.success() => {
                let _ = stderr_task.await;
                let tail = stderr_tail.lock().await.clone();
                tracing::warn!(code = ?status.code(), stderr = %tail, "claude exited non-zero");
                if !done_sent {
                    yield Ok(sse_json("error", serde_json::json!({ "message": "the agent exited with an error" })));
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "wait claude failed");
                if !done_sent {
                    yield Ok(sse_json("error", serde_json::json!({ "message": "the agent could not be run" })));
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Build a named SSE event carrying a JSON data payload. The payloads here are
/// always serializable, so this is infallible.
fn sse_json(event: &str, data: serde_json::Value) -> Event {
    Event::default().event(event).data(data.to_string())
}

/// On drop, SIGKILLs the agent's whole process group so tool/MCP grandchildren
/// are reaped with the parent (not orphaned) when the SSE connection drops.
#[cfg(unix)]
struct ProcessGroupGuard(nix::unistd::Pid);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // ESRCH (already exited) is fine — best-effort cleanup.
        let _ = nix::sys::signal::killpg(self.0, nix::sys::signal::Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_rejects_flag_shaped_and_bad_input() {
        // The security-critical case: a flag-shaped value must never pass.
        assert!(!valid_session_id("--dangerously-skip-permissions"));
        assert!(!valid_session_id("-x"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("has space"));
        assert!(!valid_session_id("semi;colon"));
        assert!(!valid_session_id(&"a".repeat(81)));
    }

    #[test]
    fn session_id_accepts_a_real_uuid() {
        assert!(valid_session_id("312cc1ad-1583-4d3c-b648-c696a540cd5c"));
    }

    #[test]
    fn model_rejects_leading_dash_and_bad_chars() {
        assert!(!valid_model("--foo"));
        assert!(!valid_model("a/b"));
        assert!(!valid_model(""));
        assert!(valid_model("claude-opus-4-8"));
        assert!(valid_model("claude-3.5-sonnet"));
    }
}
