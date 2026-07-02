//! Flat mmap-backed vector store: packed `f32` rows on disk, `redb` metadata
//! (chunk id <-> row mapping, tombstones, free-list), and exact cosine
//! (dot-product) top-k search via `simsimd` (with a pure-Rust `dot_scalar`
//! fallback on windows-arm64, where `simsimd`'s C can't build — see `dot`).
//!
//! Vectors are assumed to already be L2-normalized by the embedder (see
//! [`crate::embed`]), so cosine similarity reduces to a plain dot product —
//! this store never re-normalizes.
//!
//! ## On-disk layout (under the store directory)
//! - `vectors.bin` — packed `f32`, row-major, fixed stride = `dims * 4` bytes.
//!   Row `i` lives at byte offset `i * dims * 4`. Rows are read via seek +
//!   read into an aligned `Vec<f32>` buffer (see "mmap alignment" below).
//! - `meta.redb` — a `redb` database with tables:
//!   - `chunk_to_row`: chunk id -> row index
//!   - `row_to_chunk`: row index -> chunk id
//!   - `tombstones`: row index -> `()` (presence = tombstoned)
//!   - `free_rows`: row index -> `()` (presence = reusable slot, i.e. a
//!     tombstoned row whose slot may be recycled by a future `add`)
//!   - `header`: fixed keys -> `u64` (`DIMS_KEY` = vector dimensionality,
//!     `NEXT_ROW_KEY` = number of rows ever allocated, i.e. the append
//!     cursor)
//!
//! ## mmap alignment
//! `memmap2` maps the file read-only as `&[u8]`, but a raw byte mapping has
//! no alignment guarantee for `f32` (mmap only guarantees page alignment,
//! not 4-byte float alignment relative to an arbitrary row offset — though
//! in practice `dims * 4` keeps rows 4-byte aligned, relying on that is
//! fragile and a bytemuck-free `&[u8] as &[f32]` cast is unsound per Rust's
//! aliasing/alignment rules regardless). Instead of casting, each row is
//! copied out of the mmap via `bytemuck`-free manual `f32::from_le_bytes`
//! decoding into an owned `Vec<f32>` — correctness over cleverness, and the
//! copy is cheap (one row = a few KB at most).
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
// simsimd is excluded on windows-arm64 (its C can't build there — see Cargo.toml
// and `dot_scalar` below); import it only where it's actually a dependency.
#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
use simsimd::SpatialSimilarity;

/// Pure-Rust scalar dot product, compiled on **all** targets.
///
/// Accumulates in `f64` to mirror `simsimd`'s `f32::dot` (which returns
/// `Option<f64>`), so the two agree within the `1e-5` tolerance asserted by the
/// parity test below. LLVM auto-vectorizes this loop, so the scalar path is not
/// meaningfully slower than the SIMD one at our vector widths — it exists solely
/// because `simsimd`'s C cannot be compiled for `aarch64-pc-windows-msvc` (MSVC
/// `cl` rejects its GCC-flavored dialect; clang-cl half-detects ARM so NEON
/// paths miss `arm_neon.h`; disabling all SIMD then trips the Windows SDK's own
/// arch detection in `winnt.h`). See ADR 0018 and the windows-arm64 history
/// comment in `.github/workflows/release.yml`.
///
/// Kept compiled on ALL targets so the parity test runs everywhere simsimd
/// exists — but on those targets the production `dot` uses simsimd, not this, so
/// the library build has no non-test caller. `allow(dead_code)` is scoped OFF
/// windows-arm64 (where `dot_scalar` IS the live path and the lint stays on).
#[cfg_attr(
    not(all(target_os = "windows", target_arch = "aarch64")),
    allow(dead_code)
)]
fn dot_scalar(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum()
}

/// Dispatches the dot product: `simsimd` where it builds, the pure-Rust
/// `dot_scalar` on windows-arm64 where it doesn't. Both accumulate in `f64` and
/// the caller casts the result to `f32`, so the two paths are interchangeable.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f64 {
    #[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
    {
        f32::dot(a, b).unwrap_or(f64::NEG_INFINITY)
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        dot_scalar(a, b)
    }
}

const CHUNK_TO_ROW: TableDefinition<&str, u64> = TableDefinition::new("chunk_to_row");
const ROW_TO_CHUNK: TableDefinition<u64, &str> = TableDefinition::new("row_to_chunk");
const TOMBSTONES: TableDefinition<u64, ()> = TableDefinition::new("tombstones");
const FREE_ROWS: TableDefinition<u64, ()> = TableDefinition::new("free_rows");
const HEADER: TableDefinition<&str, u64> = TableDefinition::new("header");

const DIMS_KEY: &str = "dims";
const NEXT_ROW_KEY: &str = "next_row";

const VECTORS_FILE: &str = "vectors.bin";
const META_FILE: &str = "meta.redb";

/// Flat mmap-backed vector store with exact cosine (dot) top-k search.
pub struct VectorStore {
    dir: PathBuf,
    dims: usize,
    db: Database,
}

impl VectorStore {
    /// Opens (creating if absent) a vector store rooted at `dir`, holding
    /// `dims`-dimensional vectors. Creates `dir` and its backing files if
    /// they don't exist yet.
    pub fn open(dir: &Path, dims: usize) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating vector store dir {}", dir.display()))?;

        // Discard any leftover `vectors.bin.tmp` from a `compact` that crashed
        // before its final rename. At that point the committed metadata still
        // describes the intact `vectors.bin`, so the tmp is unreferenced
        // scratch and safe to remove. (If it were kept, a later same-named tmp
        // would be created via File::create, so leaving it is harmless either
        // way — removing it just keeps the dir clean and makes intent clear.)
        let tmp_path = dir.join(format!("{VECTORS_FILE}.tmp"));
        match fs::remove_file(&tmp_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("removing stale {}", tmp_path.display()));
            }
        }

        let vectors_path = dir.join(VECTORS_FILE);
        if !vectors_path.exists() {
            File::create(&vectors_path)
                .with_context(|| format!("creating {}", vectors_path.display()))?;
        }

        let meta_path = dir.join(META_FILE);
        let db = Database::create(&meta_path)
            .with_context(|| format!("opening redb database {}", meta_path.display()))?;

        // Ensure all tables + header exist. Also validate/record `dims`.
        let write_txn = db.begin_write()?;
        {
            let mut header = write_txn.open_table(HEADER)?;
            write_txn.open_table(CHUNK_TO_ROW)?;
            write_txn.open_table(ROW_TO_CHUNK)?;
            write_txn.open_table(TOMBSTONES)?;
            write_txn.open_table(FREE_ROWS)?;

            let existing_dims = header.get(DIMS_KEY)?.map(|v| v.value());
            match existing_dims {
                Some(existing) => {
                    if existing as usize != dims {
                        bail!(
                            "vector store at {} was created with dims={existing}, but dims={dims} was requested",
                            dir.display()
                        );
                    }
                }
                None => {
                    header.insert(DIMS_KEY, dims as u64)?;
                    header.insert(NEXT_ROW_KEY, 0u64)?;
                }
            }
        }
        write_txn.commit()?;

        Ok(VectorStore {
            dir: dir.to_path_buf(),
            dims,
            db,
        })
    }

    fn vectors_path(&self) -> PathBuf {
        self.dir.join(VECTORS_FILE)
    }

    fn stride(&self) -> usize {
        self.dims * std::mem::size_of::<f32>()
    }

    /// Reads row `row` out of `vectors.bin` via mmap, copying it into an
    /// owned, correctly-aligned `Vec<f32>` (see module docs on why we copy
    /// instead of casting the mmap bytes directly).
    fn read_row(&self, row: u64) -> Result<Vec<f32>> {
        let path = self.vectors_path();
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: `Mmap::map` is unsound if the mapped file is mutated (in
        // particular truncated) by ANOTHER process while the mapping is live —
        // Rust's borrow rules do NOT prevent that; they only govern access
        // within this process. This store therefore assumes a single writer
        // per collection as a precondition: no other process mutates this
        // collection's `vectors.bin` concurrently. Within this process, all
        // mutation goes through `add`/`compact` (`&mut self`), which the
        // borrow checker keeps from overlapping with `search`/`read_row`
        // (`&self`). Under that single-writer precondition the mapping is
        // stable for its (short) lifetime here.
        let mmap = unsafe { Mmap::map(&file) }.with_context(|| "mmap vectors.bin")?;

        let stride = self.stride();
        let start = row as usize * stride;
        let end = start + stride;
        if end > mmap.len() {
            bail!("row {row} out of bounds ({} bytes mapped)", mmap.len());
        }

        let mut out = Vec::with_capacity(self.dims);
        for chunk in mmap[start..end].chunks_exact(4) {
            out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(out)
    }

    /// Appends `vec` to `vectors.bin` at the given row (growing the file if
    /// needed), without touching any other row.
    fn write_row(&self, row: u64, vec: &[f32]) -> Result<()> {
        use std::io::{Seek, SeekFrom};

        let stride = self.stride();
        let mut file = OpenOptions::new()
            .write(true)
            .open(self.vectors_path())
            .with_context(|| "opening vectors.bin for write")?;
        let offset = row * stride as u64;
        let needed_len = offset + stride as u64;
        if file.metadata()?.len() < needed_len {
            file.set_len(needed_len)?;
        }

        let mut bytes = Vec::with_capacity(stride);
        for x in vec {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&bytes)?;
        Ok(())
    }

    /// Adds a new vector under `chunk_id`, reusing a tombstoned row from the
    /// free-list if one is available, otherwise appending a new row.
    pub fn add(&mut self, chunk_id: &str, vec: &[f32]) -> Result<()> {
        if vec.len() != self.dims {
            bail!("vector has {} dims, store expects {}", vec.len(), self.dims);
        }

        let write_txn = self.db.begin_write()?;
        let row = {
            let mut header = write_txn.open_table(HEADER)?;
            let mut chunk_to_row = write_txn.open_table(CHUNK_TO_ROW)?;
            let mut row_to_chunk = write_txn.open_table(ROW_TO_CHUNK)?;
            let mut tombstones = write_txn.open_table(TOMBSTONES)?;
            let mut free_rows = write_txn.open_table(FREE_ROWS)?;

            // If chunk_id already exists, treat this as a replace: tombstone
            // the old row first so it doesn't linger as a duplicate live row.
            if let Some(old_row) = chunk_to_row.get(chunk_id)?.map(|v| v.value()) {
                tombstones.insert(old_row, ())?;
                free_rows.insert(old_row, ())?;
                row_to_chunk.remove(old_row)?;
            }

            // Reuse a free row if one exists, else append.
            let free_row = free_rows
                .extract_if(|_, _| true)?
                .next()
                .transpose()?
                .map(|(k, _)| k.value());

            let row = match free_row {
                Some(row) => {
                    tombstones.remove(row)?;
                    row
                }
                None => {
                    let next = header.get(NEXT_ROW_KEY)?.map(|v| v.value()).unwrap_or(0);
                    header.insert(NEXT_ROW_KEY, next + 1)?;
                    next
                }
            };

            chunk_to_row.insert(chunk_id, row)?;
            row_to_chunk.insert(row, chunk_id)?;
            row
        };

        // Write the vector row to the file BEFORE committing the metadata
        // transaction. On a reused free-row, committing first and crashing
        // before `write_row` would leave the mapping pointing at the PREVIOUS
        // occupant's vector (wrong results). Writing the row first means a
        // failure leaves the new row unreferenced (the metadata isn't
        // committed) rather than referenced-but-wrong.
        self.write_row(row, vec)?;
        write_txn.commit()?;
        Ok(())
    }

    /// Tombstones the row backing `chunk_id` and pushes it onto the
    /// free-list. No-op if `chunk_id` is not present.
    pub fn remove(&mut self, chunk_id: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut chunk_to_row = write_txn.open_table(CHUNK_TO_ROW)?;
            let mut row_to_chunk = write_txn.open_table(ROW_TO_CHUNK)?;
            let mut tombstones = write_txn.open_table(TOMBSTONES)?;
            let mut free_rows = write_txn.open_table(FREE_ROWS)?;

            let removed_row = chunk_to_row.remove(chunk_id)?.map(|v| v.value());
            if let Some(row) = removed_row {
                row_to_chunk.remove(row)?;
                tombstones.insert(row, ())?;
                free_rows.insert(row, ())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Exact cosine (dot-product) search over all non-tombstoned rows,
    /// returning up to `top_k` `(chunk_id, score)` pairs sorted descending
    /// by score.
    pub fn search(&self, query_vec: &[f32], top_k: usize) -> Vec<(String, f32)> {
        if top_k == 0 || query_vec.len() != self.dims {
            return Vec::new();
        }

        // The return type stays infallible (an empty result on a corrupt
        // store is the least-surprising behavior for a search), but a silent
        // empty/partial result hides corruption. So we log a warning to stderr
        // whenever the transaction/table open fails or any row is skipped
        // during the scan, naming the count, so the corruption is detectable.
        let read_txn = match self.db.begin_read() {
            Ok(txn) => txn,
            Err(e) => {
                eprintln!("onebrain-search: vector search aborted — begin_read failed: {e}");
                return Vec::new();
            }
        };
        let row_to_chunk = match read_txn.open_table(ROW_TO_CHUNK) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("onebrain-search: vector search aborted — open row_to_chunk failed: {e}");
                return Vec::new();
            }
        };
        let tombstones = match read_txn.open_table(TOMBSTONES) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("onebrain-search: vector search aborted — open tombstones failed: {e}");
                return Vec::new();
            }
        };

        let mut scored: Vec<(String, f32)> = Vec::new();
        let mut skipped: usize = 0;
        let iter = match row_to_chunk.iter() {
            Ok(iter) => iter,
            Err(e) => {
                eprintln!("onebrain-search: vector search aborted — row_to_chunk iter failed: {e}");
                return Vec::new();
            }
        };
        for entry in iter {
            let (row_guard, chunk_guard) = match entry {
                Ok(pair) => pair,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let row = row_guard.value();
            if tombstones.get(row).ok().flatten().is_some() {
                continue;
            }
            let chunk_id = chunk_guard.value().to_string();
            let row_vec = match self.read_row(row) {
                Ok(v) => v,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let score = dot(query_vec, &row_vec) as f32;
            scored.push((chunk_id, score));
        }

        if skipped > 0 {
            eprintln!(
                "onebrain-search: vector search skipped {skipped} unreadable row(s) — index may be corrupt (consider a reindex)"
            );
        }

        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(top_k);
        scored
    }

    /// Rewrites `vectors.bin` dropping tombstoned rows and renumbering the
    /// remaining rows contiguously from 0, then clears the free-list.
    ///
    /// Crash-safety: the new (compacted) vectors are written to
    /// `vectors.bin.tmp`, the redb metadata is committed to the NEW row
    /// numbering, and ONLY THEN is the tmp file renamed over `vectors.bin`.
    /// This ordering keeps a crash recoverable — a crash before the metadata
    /// commit leaves the committed metadata still describing the OLD, intact
    /// `vectors.bin` (the tmp is unreferenced scratch), and [`Self::open`]
    /// discards any leftover `vectors.bin.tmp` on the next open. (The prior
    /// order renamed the file first, so a crash between the rename and the
    /// commit left the old row numbers indexing into the new compacted file —
    /// silent, irrecoverable corruption.)
    pub fn compact(&mut self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        let mut remap: HashMap<u64, u64> = HashMap::new();
        let mut live_chunks: Vec<(u64, String)> = Vec::new();

        {
            let row_to_chunk = write_txn.open_table(ROW_TO_CHUNK)?;
            let tombstones = write_txn.open_table(TOMBSTONES)?;
            for entry in row_to_chunk.iter()? {
                let (row_guard, chunk_guard) = entry?;
                let row = row_guard.value();
                if tombstones.get(row)?.is_some() {
                    continue;
                }
                live_chunks.push((row, chunk_guard.value().to_string()));
            }
        }
        live_chunks.sort_by_key(|(row, _)| *row);

        // Read old rows (from the still-live vectors.bin) and write them into
        // a tmp file at their new contiguous row indices. The live file is
        // NOT touched yet.
        let tmp_path = self.dir.join(format!("{VECTORS_FILE}.tmp"));
        {
            let mut tmp_file = File::create(&tmp_path)
                .with_context(|| format!("creating {}", tmp_path.display()))?;
            for (new_row, (old_row, _chunk_id)) in live_chunks.iter().enumerate() {
                let vec = self.read_row(*old_row)?;
                let mut bytes = Vec::with_capacity(self.stride());
                for x in &vec {
                    bytes.extend_from_slice(&x.to_le_bytes());
                }
                tmp_file.write_all(&bytes)?;
                remap.insert(*old_row, new_row as u64);
            }
            tmp_file.flush()?;
        }

        // Rebuild metadata tables against the new row numbering and COMMIT,
        // before swapping the file. A crash before this commit leaves the
        // metadata pointing at the untouched old vectors.bin (recoverable);
        // the orphaned tmp is discarded on the next open.
        {
            let mut header = write_txn.open_table(HEADER)?;
            let mut chunk_to_row = write_txn.open_table(CHUNK_TO_ROW)?;
            let mut row_to_chunk = write_txn.open_table(ROW_TO_CHUNK)?;
            let mut tombstones = write_txn.open_table(TOMBSTONES)?;
            let mut free_rows = write_txn.open_table(FREE_ROWS)?;

            chunk_to_row.retain(|_, _| false)?;
            row_to_chunk.retain(|_, _| false)?;
            tombstones.retain(|_, _| false)?;
            free_rows.retain(|_, _| false)?;

            for (old_row, chunk_id) in &live_chunks {
                let new_row = remap[old_row];
                chunk_to_row.insert(chunk_id.as_str(), new_row)?;
                row_to_chunk.insert(new_row, chunk_id.as_str())?;
            }
            header.insert(NEXT_ROW_KEY, live_chunks.len() as u64)?;
        }
        write_txn.commit()?;

        // Metadata (new numbering) is now durable; swap the compacted file in.
        fs::rename(&tmp_path, self.vectors_path())
            .with_context(|| "renaming vectors.bin.tmp over vectors.bin")?;

        Ok(())
    }

    /// Number of live (non-tombstoned) rows.
    pub fn len(&self) -> usize {
        let Ok(read_txn) = self.db.begin_read() else {
            return 0;
        };
        let Ok(row_to_chunk) = read_txn.open_table(ROW_TO_CHUNK) else {
            return 0;
        };
        let Ok(tombstones) = read_txn.open_table(TOMBSTONES) else {
            return 0;
        };
        let Ok(iter) = row_to_chunk.iter() else {
            return 0;
        };
        iter.filter(|entry| {
            entry
                .as_ref()
                .ok()
                .map(|(row_guard, _)| tombstones.get(row_guard.value()).ok().flatten().is_none())
                .unwrap_or(false)
        })
        .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn norm(v: &[f32]) -> Vec<f32> {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }
    #[test]
    fn add_search_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        s.add("b#0", &norm(&[0.0, 1.0, 0.0])).unwrap();
        let hits = s.search(&norm(&[1.0, 0.1, 0.0]), 2);
        assert_eq!(hits[0].0, "a#0");
        s.remove("a#0").unwrap();
        let hits2 = s.search(&norm(&[1.0, 0.1, 0.0]), 2);
        assert!(hits2.iter().all(|(id, _)| id != "a#0"));
    }
    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = VectorStore::open(dir.path(), 3).unwrap();
            s.add("x#0", &norm(&[0.0, 0.0, 1.0])).unwrap();
        }
        let s = VectorStore::open(dir.path(), 3).unwrap();
        assert_eq!(s.search(&norm(&[0.0, 0.0, 1.0]), 1)[0].0, "x#0");
    }
    #[test]
    fn compact_drops_tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        s.add("b#0", &norm(&[0.0, 1.0, 0.0])).unwrap();
        s.remove("a#0").unwrap();
        s.compact().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.search(&norm(&[0.0, 1.0, 0.0]), 1)[0].0, "b#0");
        // compact's final rename consumes the tmp — nothing left behind.
        assert!(!dir.path().join("vectors.bin.tmp").exists());
    }

    #[test]
    fn open_rejects_dims_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        drop(VectorStore::open(dir.path(), 3).unwrap());
        let err = match VectorStore::open(dir.path(), 8) {
            Ok(_) => panic!("dims mismatch must error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("dims=3"), "{err}");
        assert!(err.contains("dims=8"), "{err}");
    }

    #[test]
    fn add_rejects_wrong_width_vector() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        let err = s.add("a#0", &[1.0, 0.0]).unwrap_err().to_string();
        assert!(err.contains("2 dims"), "{err}");
        assert!(err.contains("expects 3"), "{err}");
    }

    #[test]
    fn add_same_chunk_id_replaces_not_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        s.add("a#0", &norm(&[0.0, 1.0, 0.0])).unwrap();
        assert_eq!(s.len(), 1, "replace, not duplicate");
        let hits = s.search(&norm(&[0.0, 1.0, 0.0]), 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a#0");
        assert!(hits[0].1 > 0.99, "newest vector wins: {}", hits[0].1);
    }

    #[test]
    fn add_reuses_freed_row_after_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        s.add("b#0", &norm(&[0.0, 1.0, 0.0])).unwrap();
        s.remove("a#0").unwrap();
        // The freed row is recycled: the file doesn't grow a third row.
        s.add("c#0", &norm(&[0.0, 0.0, 1.0])).unwrap();
        assert_eq!(s.len(), 2);
        let bytes = std::fs::metadata(dir.path().join("vectors.bin"))
            .unwrap()
            .len();
        assert_eq!(bytes, 2 * 3 * 4, "two rows × three f32s");
        assert_eq!(s.search(&norm(&[0.0, 0.0, 1.0]), 1)[0].0, "c#0");
    }

    #[test]
    fn remove_missing_chunk_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        s.remove("nope#9").unwrap();
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn search_edge_cases_return_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        assert!(
            s.search(&norm(&[1.0, 0.0, 0.0]), 5).is_empty(),
            "empty store"
        );
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        assert!(s.search(&norm(&[1.0, 0.0, 0.0]), 0).is_empty(), "top_k=0");
        assert!(s.search(&[1.0, 0.0], 5).is_empty(), "query dims mismatch");
        assert!(!s.is_empty());
    }

    #[test]
    fn compact_noop_when_nothing_tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        s.add("b#0", &norm(&[0.0, 1.0, 0.0])).unwrap();
        s.compact().unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.search(&norm(&[1.0, 0.0, 0.0]), 1)[0].0, "a#0");
        // Reopen still healthy after a rewrite-in-place compact.
        drop(s);
        let s = VectorStore::open(dir.path(), 3).unwrap();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn compact_empty_store_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = VectorStore::open(dir.path(), 3).unwrap();
        s.compact().unwrap();
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
    }

    /// A leftover `vectors.bin.tmp` (a `compact` that crashed before its final
    /// rename) must be discarded on the next `open`, leaving the committed
    /// metadata + intact `vectors.bin` fully queryable.
    #[test]
    fn open_discards_leftover_tmp() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = VectorStore::open(dir.path(), 3).unwrap();
            s.add("a#0", &norm(&[1.0, 0.0, 0.0])).unwrap();
        }
        // Simulate a crashed compact: metadata + vectors.bin are intact, but a
        // stale tmp remains on disk.
        let tmp = dir.path().join("vectors.bin.tmp");
        std::fs::write(&tmp, b"garbage").unwrap();

        let s = VectorStore::open(dir.path(), 3).unwrap();
        assert!(!tmp.exists(), "stale tmp must be discarded on open");
        assert_eq!(s.search(&norm(&[1.0, 0.0, 0.0]), 1)[0].0, "a#0");
    }

    /// Plain correctness of the pure-Rust scalar dot product, compiled on all
    /// targets (this is the ONLY dot path on windows-arm64, where simsimd is
    /// absent).
    #[test]
    fn dot_scalar_is_correct() {
        assert_eq!(dot_scalar(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]), 32.0);
        assert_eq!(dot_scalar(&[], &[]), 0.0);
        assert_eq!(dot_scalar(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]), 0.0);
        // Unit vectors dotted with themselves = 1.0.
        let u = norm(&[1.0, 2.0, 2.0]);
        assert!((dot_scalar(&u, &u) - 1.0).abs() < 1e-6);
    }

    /// On hosts where simsimd is a dependency (everything except windows-arm64),
    /// the scalar fallback must agree with simsimd within 1e-5 — this is what
    /// justifies swapping in `dot_scalar` on the target that can't build simsimd.
    #[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
    #[test]
    fn dot_scalar_matches_simsimd() {
        let cases: [(&[f32], &[f32]); 4] = [
            (&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]),
            (&[0.5, -0.25, 0.75, 1.5], &[-1.0, 2.0, 0.0, 0.5]),
            (&[0.1, 0.2, 0.3, 0.4, 0.5], &[0.5, 0.4, 0.3, 0.2, 0.1]),
            (&[-3.0, -2.0, -1.0], &[1.0, 2.0, 3.0]),
        ];
        for (a, b) in cases {
            let scalar = dot_scalar(a, b);
            let simd = f32::dot(a, b).unwrap();
            assert!(
                (scalar - simd).abs() < 1e-5,
                "scalar {scalar} vs simsimd {simd} for {a:?}·{b:?}"
            );
        }
    }
}
