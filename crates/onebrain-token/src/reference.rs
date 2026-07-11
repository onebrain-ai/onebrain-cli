//! The frozen already-sent reference envelope (design §3b) — the ONE shape
//! every surface emits when the ledger says a doc was already delivered this
//! session, so the daemon route (`/api/token/ledger/check`), the MCP/CLI read
//! surfaces (Track 4), and the read-hook (Track 5/8) can never drift.
//!
//! JSON shape (frozen — T5/T8 consume it verbatim):
//! ```json
//! { "doc_path": "...", "hash": "...", "sent_earlier": true,
//!   "bytes_saved": 1234, "rematerialize": "onebrain search get <path> --force" }
//! ```
//! `sent_earlier` is always `true` — the envelope only exists on a repeat,
//! unchanged delivery.

use serde::{Deserialize, Serialize};

/// SHA-256 hex of `content` — the fingerprint the already-sent ledger keys on.
/// A surface hashes the EXACT bytes it is about to deliver (design §3b), so the
/// ledger tracks delivered-vs-current content, not index state: a doc edited on
/// disk but not yet reindexed hashes differently and is correctly re-delivered.
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// The already-sent reference a surface may return instead of a full body.
/// Build it with [`ReferenceEnvelope::new`] so the `rematerialize` instruction
/// is composed the same way everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceEnvelope {
    /// Vault-relative path of the doc a reference stands in for.
    pub doc_path: String,
    /// Content hash on file for that doc (equals the caller's current hash —
    /// the reference is only emitted when they match).
    pub hash: String,
    /// Always `true`: the envelope exists only because the doc was already
    /// delivered, unchanged, earlier this session.
    pub sent_earlier: bool,
    /// Bytes the reference avoids re-sending — credited in full to gain.
    pub bytes_saved: u64,
    /// The exact command that re-materializes the full body (design §3c).
    pub rematerialize: String,
}

impl ReferenceEnvelope {
    /// Compose a reference for `doc_path` at `hash`, crediting `bytes_saved`.
    /// The `rematerialize` instruction is the single source of truth for the
    /// force path — `onebrain search get <path> --force`.
    pub fn new(doc_path: impl Into<String>, hash: impl Into<String>, bytes_saved: u64) -> Self {
        let doc_path = doc_path.into();
        let rematerialize = format!("onebrain search get {doc_path} --force");
        ReferenceEnvelope {
            doc_path,
            hash: hash.into(),
            sent_earlier: true,
            bytes_saved,
            rematerialize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_composes_the_frozen_shape() {
        let r = ReferenceEnvelope::new("notes/a.md", "deadbeef", 4096);
        assert_eq!(r.doc_path, "notes/a.md");
        assert_eq!(r.hash, "deadbeef");
        assert!(r.sent_earlier);
        assert_eq!(r.bytes_saved, 4096);
        assert_eq!(r.rematerialize, "onebrain search get notes/a.md --force");
    }

    #[test]
    fn serializes_to_the_frozen_json_keys() {
        let r = ReferenceEnvelope::new("a.md", "h", 1);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        // Exactly the five frozen keys T5/T8 consume — no more, no less.
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bytes_saved",
                "doc_path",
                "hash",
                "rematerialize",
                "sent_earlier"
            ]
        );
        assert_eq!(v["sent_earlier"], true);
    }

    #[test]
    fn content_hash_is_stable_and_content_sensitive() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
        assert_eq!(content_hash("abc").len(), 64, "sha-256 hex is 64 chars");
    }
}
