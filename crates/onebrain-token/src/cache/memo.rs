//! Query-result memoization (design §3a) — lossless.
//!
//! Key = SHA-256 of `(normalized query, mode, top_k, min_candidates,
//! min_score, index_generation, index_nonce)`; value = the resolved hit set
//! (an opaque serialized string the caller owns — the crate stays ignorant of
//! the hit shape). Two components make stale service structurally impossible:
//! - **`generation`** (bumped on every reindex, `onebrain-search`) invalidates
//!   entries when the SAME index changes.
//! - **`index_nonce`** (a stable per-index-instance id, reset only when the
//!   index is rebuilt from scratch) invalidates entries across a rebuild —
//!   `generation` alone restarts at 0→1 on a `--force`/wipe and would collide
//!   with pre-rebuild entries stored under the old instance's generation 1.
//!
//! `generation` and `index_nonce` live in DIFFERENT redb files (`meta.redb`
//! under `index/` vs the memo store in `token.redb`), with independent
//! lifecycles — folding BOTH into the key is what covers a rebuild that resets
//! one without touching the other.

use redb::{Database, ReadableDatabase, TableDefinition};
use sha2::{Digest, Sha256};

use super::CacheResult;

/// `memo_key_hex -> serialized_hit_set`.
const MEMO_TABLE: TableDefinition<&str, &str> = TableDefinition::new("memo");

/// The content-addressed key for one memoized query result. Opaque hex digest;
/// build it with [`MemoKey::new`] from the exact components that determine a
/// result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoKey(String);

impl MemoKey {
    /// Hash the seven result-determining components into a stable key. Each
    /// component is length/type-delimited so distinct tuples can never collide
    /// by concatenation (`"a"+"bc"` vs `"ab"+"c"`). Integers are fixed-width
    /// little-endian `u64` (platform-independent — `usize` is widened first),
    /// and `min_score` is hashed by its raw IEEE-754 bits so two byte-identical
    /// floats always hash the same. `index_nonce` (design §3a / MAJOR 2)
    /// disambiguates index INSTANCES so a rebuilt index — whose `generation`
    /// restarts at 0→1 — never reuses a prior instance's key space.
    pub fn new(
        query_norm: &str,
        mode: &str,
        top_k: usize,
        min_candidates: usize,
        min_score: f32,
        generation: u64,
        index_nonce: &str,
    ) -> Self {
        let mut h = Sha256::new();
        // Domain tag + NUL-delimited strings, fixed-width ints.
        h.update(b"onebrain-token/memo/v1\0");
        h.update(query_norm.as_bytes());
        h.update([0u8]);
        h.update(mode.as_bytes());
        h.update([0u8]);
        h.update((top_k as u64).to_le_bytes());
        h.update((min_candidates as u64).to_le_bytes());
        h.update(min_score.to_bits().to_le_bytes());
        h.update(generation.to_le_bytes());
        h.update(index_nonce.as_bytes());
        h.update([0u8]);
        let digest = h.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        MemoKey(hex)
    }

    /// The hex digest string used as the redb key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The memoization layer over a [`Database`]. Obtain via
/// [`super::TokenCache::memo`].
pub struct Memo<'a> {
    db: &'a Database,
}

impl<'a> Memo<'a> {
    pub(super) fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// The stored hit set for `key`, or `None` on a miss. A miss includes the
    /// structural case where the caller's `generation` moved since the entry
    /// was written — the key simply differs, so nothing matches.
    pub fn get(&self, key: &MemoKey) -> CacheResult<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(MEMO_TABLE) {
            Ok(t) => t,
            // A never-written table reads as an empty cache, not an error.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(table.get(key.as_str())?.map(|v| v.value().to_string()))
    }

    /// Store `hits` under `key`. Idempotent — re-putting the same key
    /// overwrites, which is correct: same components ⟹ same result set.
    pub fn put(&self, key: &MemoKey, hits: &str) -> CacheResult<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(MEMO_TABLE)?;
            table.insert(key.as_str(), hits)?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::TokenCache;

    fn cache() -> (tempfile::TempDir, TokenCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache::open(&dir.path().join("token.redb")).unwrap();
        (dir, cache)
    }

    const NONCE: &str = "idx-nonce-A";

    #[test]
    fn same_components_produce_the_same_key() {
        let a = MemoKey::new("rust errors", "hybrid", 10, 50, 0.30, 7, NONCE);
        let b = MemoKey::new("rust errors", "hybrid", 10, 50, 0.30, 7, NONCE);
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64, "sha-256 hex is 64 chars");
    }

    #[test]
    fn any_component_change_changes_the_key() {
        let base = MemoKey::new("q", "hybrid", 10, 50, 0.30, 1, NONCE);
        assert_ne!(base, MemoKey::new("q2", "hybrid", 10, 50, 0.30, 1, NONCE));
        assert_ne!(base, MemoKey::new("q", "lex", 10, 50, 0.30, 1, NONCE));
        assert_ne!(base, MemoKey::new("q", "hybrid", 11, 50, 0.30, 1, NONCE));
        assert_ne!(base, MemoKey::new("q", "hybrid", 10, 51, 0.30, 1, NONCE));
        assert_ne!(base, MemoKey::new("q", "hybrid", 10, 50, 0.31, 1, NONCE));
        assert_ne!(base, MemoKey::new("q", "hybrid", 10, 50, 0.30, 2, NONCE));
        assert_ne!(
            base,
            MemoKey::new("q", "hybrid", 10, 50, 0.30, 1, "idx-nonce-B"),
            "a different index nonce must change the key"
        );
    }

    #[test]
    fn no_concatenation_collision_between_adjacent_string_fields() {
        // "ab"+"c" must not hash the same as "a"+"bc" — the NUL delimiter
        // between query and mode is what prevents it.
        let x = MemoKey::new("ab", "c", 0, 0, 0.0, 0, NONCE);
        let y = MemoKey::new("a", "bc", 0, 0, 0.0, 0, NONCE);
        assert_ne!(x, y);
    }

    #[test]
    fn put_then_get_hits() {
        let (_dir, cache) = cache();
        let memo = cache.memo();
        let key = MemoKey::new("q", "hybrid", 10, 50, 0.30, 1, NONCE);
        assert_eq!(memo.get(&key).unwrap(), None, "cold cache misses");
        memo.put(&key, r#"[{"path":"a.md"}]"#).unwrap();
        assert_eq!(
            memo.get(&key).unwrap().as_deref(),
            Some(r#"[{"path":"a.md"}]"#)
        );
    }

    /// The crux of design §3a: a generation bump invalidates ALL prior entries
    /// structurally. We store at generation 1, then look up with a key built at
    /// generation 2 — the stored value is unreachable, so we get a clean miss.
    #[test]
    fn generation_bump_invalidates_prior_entries() {
        let (_dir, cache) = cache();
        let memo = cache.memo();
        let gen1 = MemoKey::new("q", "hybrid", 10, 50, 0.30, 1, NONCE);
        memo.put(&gen1, "old-results").unwrap();

        let gen2 = MemoKey::new("q", "hybrid", 10, 50, 0.30, 2, NONCE);
        assert_eq!(
            memo.get(&gen2).unwrap(),
            None,
            "a bumped generation must miss every entry written under the old one"
        );
        // The old entry is still literally addressable by its own key — proof
        // the miss is structural (key divergence), not a delete.
        assert_eq!(memo.get(&gen1).unwrap().as_deref(), Some("old-results"));
    }

    /// MAJOR 2: an index REBUILD resets generation to 0→1, colliding with an
    /// entry stored under the OLD instance's generation 1. Folding the
    /// per-instance nonce into the key makes the rebuilt index (new nonce) miss
    /// the stale entry — even though the generation number is identical.
    #[test]
    fn fresh_index_nonce_invalidates_prior_entries_at_same_generation() {
        let (_dir, cache) = cache();
        let memo = cache.memo();
        // Old index instance, generation 1.
        let old = MemoKey::new("q", "hybrid", 10, 50, 0.30, 1, "instance-old");
        memo.put(&old, "pre-rebuild-results").unwrap();

        // Rebuilt index: generation restarts at 1, but the nonce is new.
        let rebuilt = MemoKey::new("q", "hybrid", 10, 50, 0.30, 1, "instance-new");
        assert_eq!(
            memo.get(&rebuilt).unwrap(),
            None,
            "a rebuilt index (new nonce) must miss entries from the old instance \
             even at the same generation number"
        );
    }
}
