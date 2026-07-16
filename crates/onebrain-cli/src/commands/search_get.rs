//! `onebrain search get` — fetch a doc's full indexed text.
//!
//! Vault-required (exit 64 outside a vault). `Engine::get` never touches
//! the embedder, so opening the engine here never triggers a model
//! download (see engine.rs's lazy embedder).

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::SearchGetArgs;
use crate::commands::daemon_client::DaemonHandle;
use crate::commands::search_common::{
    map_daemon_error, normalize_doc_path, open_engine_from_resolved, route_to_daemon,
    strip_vault_prefix,
};
use crate::commands::token_runner;
use crate::output::{emit, Envelope, OutputMode};
use onebrain_token::gain::Surface;
use onebrain_token::{CacheKind, GainEvent, OptLevel, ReferenceEnvelope};

#[derive(Debug, Serialize)]
struct SearchGetData {
    doc_path: String,
    content: String,
    /// Present when the already-sent ledger (design §3b, level 2↑, structured
    /// output only) recognized a repeat, unchanged read: `content` is then
    /// empty and this reference tells the agent how to re-materialize the full
    /// body (`onebrain search get <path> --force`). `--force` always delivers
    /// the full body and never populates this.
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<ReferenceEnvelope>,
}

/// The hint appended when a doc isn't found in the index — shared by the direct
/// and daemon-routed paths so the message never drifts.
const NOT_INDEXED_HINT: &str = "💡  paths are vault-relative (e.g. `00-inbox/note.md`); \
     if the doc is new, `onebrain search reindex` may not have indexed it yet";

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &SearchGetArgs) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    // An absolute path that isn't under the vault root can never be an index
    // key (keys are vault-relative), so `Engine::get` would just miss with a
    // generic "not indexed" hint. Catch it up front with a precise message.
    if is_outside_vault(&args.doc_path, resolved.root.as_path()) {
        anyhow::bail!(
            "❌  path is outside this vault: {}\n💡  `search get` takes vault-relative \
             paths of indexed `.md` notes (e.g. `00-inbox/note.md`)",
            args.doc_path
        );
    }

    // Index keys are vault-relative (forward-slash) paths; accept an
    // absolute path under the vault root and normalize it so
    // `search get /abs/vault/00-inbox/x.md` just works.
    let doc_path = normalize_doc_path(&args.doc_path, resolved.root.as_path());

    // Warm-daemon path: when a daemon already holds the engine, `Engine::get`
    // (a redb read) would clash with it → route the lookup through the daemon.
    // Only when it serves this exact vault; else fall through to a direct open.
    // Captured once so the ledger check below reuses the same backend decision.
    let daemon = route_to_daemon(&resolved);
    // `doc_hash` is the raw-file identity the already-sent ledger keys on
    // (issue #255) — captured from the SAME engine that serves the body so the
    // read-hook (which keys on `doc_hash`) shares the entry. In daemon mode this
    // process holds no engine, so `None` here means the daemon derives it.
    let (content, doc_hash) = if let Some(handle) = &daemon {
        (get_via_daemon(handle, &doc_path)?, None)
    } else {
        // `resolved` is already resolved above — use the vault we already
        // have instead of re-resolving (which would also re-persist an
        // auto-generated `search.collection` to `onebrain.yml` a second
        // time).
        let (engine, _resolved) = open_engine_from_resolved(&resolved)?;
        let content = engine
            .get(&doc_path)
            .map_err(|e| anyhow::anyhow!("{e}\n{NOT_INDEXED_HINT}"))?;
        let doc_hash = engine.doc_hash(&doc_path);
        (content, doc_hash)
    };

    // Level (per-call `--opt-level` > config > default); a bad `--opt-level` is
    // surfaced as an error, never silently downgraded (design contract).
    let cfg = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.token_optimization)
        .unwrap_or_default();
    let level = token_runner::resolve_level(args.opt_level.as_deref(), &cfg)?;
    let session = token_runner::resolve_session_token_opt();

    // Already-sent ledger on the agent-facing (structured) path (design
    // §3b/§3c/§5d): a repeat, unchanged `search get --output json` returns a
    // reference instead of the body; `--force` bypasses the ledger and always
    // delivers the full body (it's the reference's re-materialize target). The
    // ledger keys on the doc's raw-file identity (`doc_hash`), unified with the
    // read-hook so the two surfaces share one entry (#255). Human TTY always
    // gets the full body (never a reference).
    let reference = if mode.is_structured() && !args.force && level >= OptLevel::Balanced {
        match &daemon {
            Some(handle) => token_runner::daemon_ledger_reference(handle, &doc_path, &content),
            None => token_runner::direct_ledger_reference(
                &resolved,
                &doc_path,
                doc_hash.as_deref(),
                &content,
            ),
        }
    } else {
        None
    };

    // Meter the structured surface (design §5d — no surface escapes metering).
    // The ladder's doc-body transforms are MCP-only (§2a — no CLI doc-body
    // surface this release), and the query transforms must not run on a doc body
    // (snippet's fallback would truncate it), so a delivered body records a
    // plain "none" event; a reference records a `ledger_ref` credit.
    if mode.is_structured() {
        let before = content.len() as u64;
        let (transform, after, cache) = match &reference {
            Some(r) => {
                let ref_bytes = serde_json::to_string(r)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0);
                ("ledger_ref", ref_bytes, CacheKind::LedgerRef)
            }
            None => ("none", before, CacheKind::None),
        };
        token_runner::record_gain(
            &resolved,
            GainEvent {
                ts: now_ts(),
                surface: Surface::CliSearch,
                transform: transform.to_string(),
                level,
                bytes_before: before,
                bytes_after: after,
                cache,
                session_token: session.clone(),
            },
        );
    }

    // On a reference the body is dropped (the reference stands in for it).
    // Explicitly `drop` it here rather than letting the shadowing `let`
    // below merely stop naming it — shadowing alone leaves the (possibly
    // large) original `String` allocation live until the end of `run`, not
    // freed at this point.
    let content = if reference.is_some() {
        drop(content);
        String::new()
    } else {
        content
    };
    let data = SearchGetData {
        doc_path,
        content,
        reference,
    };

    let envelope = Envelope::ok("search.get", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Epoch seconds for the metering gain event.
fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Fetch a doc's indexed text through the warm daemon's `/api/internal/get`. A
/// daemon 404 (doc not indexed) maps to the SAME "not indexed yet" error the
/// direct path emits, so the two surfaces are indistinguishable to the user.
fn get_via_daemon(handle: &DaemonHandle, doc_path: &str) -> Result<String> {
    match handle.get(doc_path) {
        Ok(Some(content)) => Ok(content),
        Ok(None) => anyhow::bail!("doc not found: {doc_path}\n{NOT_INDEXED_HINT}"),
        Err(e) => Err(map_daemon_error(
            e,
            "route `search get` through warm daemon",
        )),
    }
}

/// `true` when `input` is an absolute path that is NOT under `vault_root`.
/// Such a path can't correspond to any index key (keys are vault-relative),
/// so the caller should reject it up front with a clear message rather than
/// let the lookup miss generically. Relative inputs are always `false` here
/// (they're resolved against the vault by `normalize_doc_path`).
///
/// Uses the SAME symlink-aware [`strip_vault_prefix`] the ledger normalizer
/// uses (#268): a symlinked-but-inside absolute path (e.g. `/tmp/vault/x.md`
/// against a canonical `/private/tmp/vault` root) canonicalizes to a match, so
/// it is NOT rejected — this guard ran BEFORE `normalize_doc_path` with a
/// purely-lexical strip, which used to `bail!` on exactly those paths before
/// the fix could apply.
fn is_outside_vault(input: &str, vault_root: &std::path::Path) -> bool {
    let p = std::path::Path::new(input);
    p.is_absolute() && strip_vault_prefix(p, vault_root).is_none()
}

fn render_text(env: &Envelope<SearchGetData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    // Structured output can carry a reference (already-sent) instead of a body;
    // in text mode render a human-readable note pointing at the force re-fetch.
    // In practice text mode never activates the ledger (it's structured-only),
    // but keep the renderer total.
    match &d.reference {
        Some(r) => format!(
            "⤷ already sent this session (unchanged) — re-fetch the full body with:\n    {}",
            r.rematerialize
        ),
        None => d.content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_outside_vault_detects_absolute_paths_not_under_root() {
        // `/…` counts as absolute only on unix; Windows needs a drive
        // prefix, so build platform-appropriate paths.
        #[cfg(unix)]
        let (root, outside_a, outside_b, inside) = (
            std::path::Path::new("/vault/ob-1"),
            "/elsewhere/x.md",
            "/vault/other/x.md",
            "/vault/ob-1/00-inbox/note.md",
        );
        #[cfg(windows)]
        let (root, outside_a, outside_b, inside) = (
            std::path::Path::new("C:\\vault\\ob-1"),
            "C:\\elsewhere\\x.md",
            "C:\\vault\\other\\x.md",
            "C:\\vault\\ob-1\\00-inbox\\note.md",
        );
        // Absolute, outside the vault → true.
        assert!(is_outside_vault(outside_a, root));
        assert!(is_outside_vault(outside_b, root));
        // Absolute, under the vault → false (normalize handles it).
        assert!(!is_outside_vault(inside, root));
        // Relative → always false here.
        assert!(!is_outside_vault("00-inbox/note.md", root));
        assert!(!is_outside_vault("./00-inbox/note.md", root));
    }

    /// #268: `is_outside_vault` runs BEFORE `normalize_doc_path` and used a
    /// purely-lexical `strip_prefix`, so an absolute path spelled through a
    /// symlinked component (e.g. `/tmp/vault/...` while the vault root resolves
    /// to canonical `/private/tmp/vault`) was wrongly rejected with "path is
    /// outside this vault" — bailing before the symlink-aware normalizer could
    /// run. It must now canonicalize and recognize the path as INSIDE. A
    /// genuinely-outside absolute path must still be rejected.
    #[cfg(unix)]
    #[test]
    fn is_outside_vault_recognizes_symlinked_inside_path() {
        let base = tempfile::tempdir().unwrap();
        let real_vault = base.path().join("real-vault");
        std::fs::create_dir_all(real_vault.join("00-inbox")).unwrap();
        let doc = real_vault.join("00-inbox/note.md");
        std::fs::write(&doc, "body").unwrap();

        // A symlink standing in for the vault dir (the `/tmp` → `/private/tmp`
        // split, reproduced portably with a real symlink rather than relying
        // on the macOS-specific mapping).
        let link_vault = base.path().join("linked-vault");
        std::os::unix::fs::symlink(&real_vault, &link_vault).unwrap();

        // The caller resolves vault_root to the canonical, symlink-free form,
        // while the user passes the doc through the symlinked spelling.
        let canonical_root = real_vault.canonicalize().unwrap();
        let input_through_symlink = link_vault.join("00-inbox/note.md");

        assert!(
            !is_outside_vault(input_through_symlink.to_str().unwrap(), &canonical_root),
            "a symlinked-but-inside absolute path must NOT be rejected as outside the vault"
        );

        // A genuinely-outside absolute path is still rejected.
        let outside = base.path().join("elsewhere/x.md");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "body").unwrap();
        assert!(
            is_outside_vault(outside.to_str().unwrap(), &canonical_root),
            "an absolute path genuinely outside the vault must still be rejected"
        );
    }

    #[test]
    fn text_prints_raw_content() {
        let e = Envelope::ok(
            "search.get",
            None,
            SearchGetData {
                doc_path: "a.md".into(),
                content: "line1\nline2".into(),
                reference: None,
            },
        );
        assert_eq!(render_text(&e), "line1\nline2");
    }

    #[test]
    fn text_renders_reference_with_force_instruction() {
        let e = Envelope::ok(
            "search.get",
            None,
            SearchGetData {
                doc_path: "a.md".into(),
                content: String::new(),
                reference: Some(ReferenceEnvelope::new("a.md", "h", 42)),
            },
        );
        let s = render_text(&e);
        assert!(s.contains("already sent"), "{s}");
        assert!(s.contains("onebrain search get a.md --force"), "{s}");
    }
}
