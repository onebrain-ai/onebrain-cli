//! Tantivy-backed BM25 lexical index with a script-aware tokenizer.
//!
//! tantivy's built-in tokenizers split on whitespace/punctuation, which is
//! useless for scripts that don't use spaces between words — Thai, Lao,
//! Khmer, Myanmar, and the CJK family (Chinese, Japanese, Korean): an entire
//! run of such text collapses into a single (unsearchable) token under the
//! default analyzer. [`ScriptAwareTokenizer`] fixes this by splitting input
//! into alternating no-space-script / other runs (Unicode block checks —
//! see [`is_no_space_char`]) and routing each run through the appropriate
//! sub-tokenizer: no-space-script runs get character-bigrammed, other runs
//! get the default lowercased alphanumeric tokenizer.
//!
//! ## No-space-script segmentation: bigram fallback (not word-segmentation)
//!
//! For Thai specifically, `nlpo3`'s `newmm` (maximal-matching) word
//! segmenter needs a Thai word dictionary (`words_th.txt`) to build its
//! trie. That dictionary is **not bundled** in the published `nlpo3` crate:
//! `Cargo.toml.orig` explicitly lists `words_th.txt` under `exclude`, and
//! `NewmmTokenizer::new(dict_path: &str)` / `DictSource::FilePath` expect an
//! external file on disk that we do not have and are not vendoring in this
//! task. `NewmmTokenizer::from_word_list` / `DictSource::WordList` would let
//! us supply a dictionary in-process, but we'd still need to source the
//! word list content, which is a separate follow-up (vendoring PyThaiNLP's
//! dictionary asset + license review). CJK scripts have their own
//! per-language segmenters too (e.g. `jieba` for Chinese, `lindera` for
//! Japanese), with the same "needs an external dictionary asset" shape.
//!
//! Per this task's fallback rule, no-space-script runs are instead
//! tokenized as overlapping **character bigrams** (2-char sliding windows,
//! byte offsets tracked precisely). This has no dictionary dependency,
//! needs no new crates, and is the standard technique for CJK/Thai
//! substring search: a multi-character query segments into overlapping
//! bigrams too, so a bigram-indexed document and a bigram-tokenized query
//! naturally share terms without any segmentation ambiguity. It trades some
//! precision (bigrams are coarser than real word boundaries) for zero setup
//! cost. Revisit with true per-language word segmenters (nlpo3 for Thai,
//! jieba for Chinese, lindera for Japanese, ...) once the dictionary assets
//! are vendored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, TokenStream, Tokenizer,
};
use tantivy::{doc, Index, IndexWriter, Term};

use crate::chunk::Chunk;

/// Name under which the script-aware analyzer is registered on the index's
/// [`tantivy::tokenizer::TokenizerManager`]. Used for the `body` and
/// `heading_path` fields' schema-declared tokenizer and for query-time
/// segmentation, so index-time and query-time tokenization always match.
const SCRIPT_AWARE_TOKENIZER: &str = "script_aware";

/// Weight applied to `heading_path` term matches relative to `body` matches.
///
/// Below 1.0 on purpose. A chunk's `heading_path` repeats across EVERY chunk
/// under that section, so an over-weighted heading match floods the top-k with
/// every sibling chunk of a section whose title happens to contain the query
/// term — a real hazard in a vault where hundreds of session logs share
/// boilerplate headings ("Key Decisions", "Action Items").
///
/// Calibrated on a real 782-doc vault, not guessed. Two 30-query probe sets:
/// **A** = queries taken from a heading occurring in exactly ONE file (gold =
/// that file); **C** = distinctive body-term queries that never touch headings
/// (regression guard). BM25-only, top-10:
///
/// | boost | A hit@10 | A MRR | C hit@10 | C MRR  |
/// |-------|----------|-------|----------|--------|
/// | 0.00  | 0.300    | 0.185 | 0.800    | 0.5667 |
/// | 0.15  | 0.500    | 0.277 | 0.800    | 0.5486 |
/// | 0.25  | 0.567    | 0.318 | 0.800    | 0.5486 |
/// | 0.35  | 0.600    | 0.428 | 0.800    | 0.5469 |
/// | 0.50  | 0.700    | 0.490 | 0.767    | 0.5122 |
/// | 0.75  | 0.733    | 0.631 | 0.700    | 0.4770 |
///
/// 0.35 is the knee: it doubles heading-query hit@10 and lifts their MRR ~2.3×
/// while leaving the body-query set's hit@10 untouched (its MRR moves −3.5%,
/// inside the noise of a 30-query sample). Past 0.35 the body set starts
/// losing hits outright.
const HEADING_BOOST: f32 = 0.35;

/// The distinguishing fragment of tantivy's schema-mismatch message
/// (`index/index.rs`: "An index exists but the schema does not match.").
/// Only the stable middle of the sentence is matched, so upstream
/// capitalization or punctuation edits don't break recognition.
const SCHEMA_MISMATCH_MARKER: &str = "schema does not match";

/// True when `err` is tantivy's "an index exists but the schema does not
/// match" — i.e. the on-disk index was written by a build whose schema differs
/// from this one.
///
/// Typed-first: the [`tantivy::TantivyError::SchemaError`] variant is the
/// primary gate, so an unrelated error type can never be mistaken for a
/// mismatch. The message is then checked as a SECOND, narrowing condition
/// (C4). Within tantivy 0.26.1 the schema-mismatch site is the only reachable
/// `SchemaError` producer on [`LexIndex::open`]'s call graph, so the variant
/// alone is correct *today* — but this predicate authorizes a destructive
/// `remove_dir_all`, and a future tantivy adding a `SchemaError` anywhere else
/// on that path (a bad field option, an unknown tokenizer) would silently turn
/// into a wipe. Requiring both keeps the guarantee no wider than the intent;
/// a wording change upstream degrades to a loud hard failure, never to a
/// spurious wipe.
pub fn is_schema_mismatch(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<tantivy::TantivyError>(),
        Some(tantivy::TantivyError::SchemaError(msg)) if msg.contains(SCHEMA_MISMATCH_MARKER)
    )
}

/// Path of the crash-safe "this lex index was wiped and still needs
/// repopulating" marker for the tantivy directory `dir`.
///
/// Deliberately a SIBLING of `dir`, not a file inside it: [`LexIndex::open_or_reset`]
/// wipes `dir` with `remove_dir_all`, so anything stored inside would be
/// destroyed by the very operation it is meant to record. For the normal
/// layout (`<collection>/index/tantivy`) the marker lands at
/// `<collection>/index/.tantivy.rebuild-pending` — inside the collection, so
/// it travels with a copied/moved collection.
///
/// Being a sibling also means nothing that deletes `dir` deletes the marker:
/// `search reindex --force` removes the three NAMED index artifacts rather
/// than the `index/` dir, so it clears this file explicitly (see the CLI's
/// `wipe_index_files`) — a forced wipe leaves no bookkeeping behind either.
///
/// `pub` for the tests rather than for production callers: production code
/// goes through [`rebuild_pending`] / [`clear_rebuild_marker`] and never needs
/// the path itself, but the B1 / B-A1 / D2 regression tests must PLANT and
/// inspect the marker to reconstruct the crash states they pin. Keep it
/// exported.
pub fn rebuild_marker_path(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tantivy".to_string());
    dir.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.rebuild-pending"))
}

/// True when a previous run wiped this lex index but has not yet finished
/// repopulating it. The engine must repopulate on the next open whenever this
/// is true — the in-memory `was_reset` flag from [`LexIndex::open_or_reset`]
/// does NOT survive a crash, and the post-wipe directory has a *matching*
/// schema, so a later open would otherwise see a healthy-looking EMPTY index
/// and never rebuild it.
pub fn rebuild_pending(dir: &Path) -> bool {
    rebuild_marker_path(dir).exists()
}

/// Remove the rebuild marker written by [`LexIndex::open_or_reset`]. Call
/// only AFTER the repopulate has been committed. Absent marker = success
/// (idempotent).
pub fn clear_rebuild_marker(dir: &Path) -> Result<()> {
    let path = rebuild_marker_path(dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("clearing lex rebuild marker {}", path.display()))),
    }
}

/// Unicode blocks for scripts that don't use spaces between words. Each
/// tuple is an inclusive `(start, end)` character range.
const NO_SPACE_SCRIPT_BLOCKS: &[(char, char)] = &[
    // Thai
    ('\u{0E00}', '\u{0E7F}'),
    // Lao
    ('\u{0E80}', '\u{0EFF}'),
    // Myanmar
    ('\u{1000}', '\u{109F}'),
    // Khmer
    ('\u{1780}', '\u{17FF}'),
    // CJK Unified Ideographs
    ('\u{4E00}', '\u{9FFF}'),
    // CJK Unified Ideographs Extension A
    ('\u{3400}', '\u{4DBF}'),
    // Hiragana
    ('\u{3040}', '\u{309F}'),
    // Katakana
    ('\u{30A0}', '\u{30FF}'),
    // Hangul Syllables
    ('\u{AC00}', '\u{D7A3}'),
];

/// True if `c` belongs to a script that is conventionally written without
/// spaces between words (Thai, Lao, Myanmar, Khmer, or CJK). Such scripts
/// need bigram tokenization instead of tantivy's default whitespace split
/// (see module docs).
fn is_no_space_char(c: char) -> bool {
    NO_SPACE_SCRIPT_BLOCKS
        .iter()
        .any(|(start, end)| (*start..=*end).contains(&c))
}

/// Split `text` into byte-offset-tagged runs of consecutive no-space-script
/// / other characters, e.g. `"abc กขค def"` ->
/// `[(0,3,false), (3,4,false)/* space stays non-script */, (4,10,true), ...]`
/// (exact boundaries depend on byte lengths; the point is runs never mix
/// scripts).
fn split_script_runs(text: &str) -> Vec<(usize, usize, bool)> {
    let mut runs = Vec::new();
    let mut start = 0usize;
    let mut current_is_script: Option<bool> = None;

    for (idx, ch) in text.char_indices() {
        let is_script = is_no_space_char(ch);
        match current_is_script {
            None => current_is_script = Some(is_script),
            Some(prev) if prev != is_script => {
                runs.push((start, idx, prev));
                start = idx;
                current_is_script = Some(is_script);
            }
            _ => {}
        }
    }
    if let Some(is_script) = current_is_script {
        runs.push((start, text.len(), is_script));
    }
    runs
}

/// Emit overlapping 2-char (bigram) tokens for a no-space-script run, with
/// correct byte offsets relative to the *original* input string
/// (`run_offset` is where this run starts in that original string).
///
/// A single-character run emits one unigram token so short runs are still
/// searchable (a lone character can't form a bigram).
fn script_bigrams(run: &str, run_offset: usize) -> Vec<(String, usize, usize)> {
    let chars: Vec<(usize, char)> = run.char_indices().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chars.len() == 1 {
        let (off, ch) = chars[0];
        let start = run_offset + off;
        let end = run_offset + off + ch.len_utf8();
        return vec![(ch.to_string(), start, end)];
    }
    let mut tokens = Vec::with_capacity(chars.len() - 1);
    for window in chars.windows(2) {
        let (off_a, ch_a) = window[0];
        let (off_b, ch_b) = window[1];
        let start = run_offset + off_a;
        let end = run_offset + off_b + ch_b.len_utf8();
        let mut text = String::with_capacity(ch_a.len_utf8() + ch_b.len_utf8());
        text.push(ch_a);
        text.push(ch_b);
        tokens.push((text, start, end));
    }
    tokens
}

/// Emit tokens for a non-script run using the same rules as tantivy's
/// built-in `default` analyzer (split on non-alphanumeric, drop
/// tokens > 40 chars, lowercase), with offsets translated back to the
/// original input string.
fn other_tokens(run: &str, run_offset: usize) -> Vec<(String, usize, usize)> {
    let mut analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .build();
    let mut stream = analyzer.token_stream(run);
    let mut tokens = Vec::new();
    while let Some(tok) = stream.next() {
        tokens.push((
            tok.text.clone(),
            run_offset + tok.offset_from,
            run_offset + tok.offset_to,
        ));
    }
    tokens
}

/// Segment arbitrary text (no-space-script + other mixed) into `(token,
/// offset_from, offset_to)` triples, byte offsets relative to the whole
/// input. Shared by the tantivy [`Tokenizer`] impl (index-time) and
/// query-time segmentation, so both sides always agree on tokenization.
fn segment(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    for (start, end, is_script) in split_script_runs(text) {
        let run = &text[start..end];
        if is_script {
            out.extend(script_bigrams(run, start));
        } else {
            out.extend(other_tokens(run, start));
        }
    }
    out
}

/// A [`tantivy::tokenizer::Tokenizer`] that splits text into no-space-script
/// vs other runs and tokenizes each appropriately (see module docs).
#[derive(Clone, Default)]
struct ScriptAwareTokenizer;

struct ScriptAwareTokenStream {
    tokens: std::vec::IntoIter<(String, usize, usize)>,
    token: tantivy::tokenizer::Token,
}

impl Tokenizer for ScriptAwareTokenizer {
    type TokenStream<'a> = ScriptAwareTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        ScriptAwareTokenStream {
            tokens: segment(text).into_iter(),
            token: tantivy::tokenizer::Token::default(),
        }
    }
}

impl TokenStream for ScriptAwareTokenStream {
    fn advance(&mut self) -> bool {
        match self.tokens.next() {
            Some((text, offset_from, offset_to)) => {
                self.token.text.clear();
                self.token.text.push_str(&text);
                self.token.offset_from = offset_from;
                self.token.offset_to = offset_to;
                self.token.position = self.token.position.wrapping_add(1);
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &tantivy::tokenizer::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut tantivy::tokenizer::Token {
        &mut self.token
    }
}

/// Tantivy-backed BM25 lexical index over [`Chunk`]s, with script-aware
/// tokenization on the `body` field.
///
/// The [`IndexWriter`] holds tantivy's **exclusive** directory lock
/// (`Failed to acquire Lockfile: LockBusy` if a second writer opens the same
/// dir). Read verbs (`search`) never need it, so the writer is created
/// **lazily** — [`Self::open`] acquires no writer lock, and only the write
/// paths ([`Self::add`], [`Self::delete`], [`Self::commit`]) materialize it
/// on first use via [`Self::writer_mut`]. This lets read-only opens run
/// concurrently with a writer (or after a killed writer left a stale lock).
pub struct LexIndex {
    index: Index,
    writer: Option<IndexWriter>,
    chunk_id: Field,
    doc_path: Field,
    heading_path: Field,
    body: Field,
}

impl LexIndex {
    /// Open the lexical index rooted at `dir`, creating it (and `dir`) if
    /// it doesn't already exist. Registers the script-aware tokenizer on
    /// the index so index-time tokenization is always consistent with what
    /// [`Self::search`] uses at query time. **No writer is created here**, so
    /// no writer lock is acquired — the writer is materialized lazily on the
    /// first write (see the struct docs).
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;

        let mut schema_builder = Schema::builder();
        let chunk_id = schema_builder.add_text_field("chunk_id", STRING | STORED);
        let doc_path = schema_builder.add_text_field("doc_path", STRING | STORED);
        // `heading_path` and `body` MUST share the script-aware tokenizer: a
        // plain `TEXT` field would use tantivy's default English analyzer, so
        // Thai headings would segment differently from Thai bodies and only
        // English heading terms would ever match.
        let script_aware_options = TEXT.set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(SCRIPT_AWARE_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let heading_path =
            schema_builder.add_text_field("heading_path", script_aware_options.clone() | STORED);
        let body = schema_builder.add_text_field("body", script_aware_options);
        let schema = schema_builder.build();

        let mmap_directory = MmapDirectory::open(dir)?;
        let index = Index::builder()
            .schema(schema)
            .open_or_create(mmap_directory)?;
        index
            .tokenizers()
            .register(SCRIPT_AWARE_TOKENIZER, ScriptAwareTokenizer);

        Ok(Self {
            index,
            writer: None,
            chunk_id,
            doc_path,
            heading_path,
            body,
        })
    }

    /// Like [`Self::open`], but self-heals a schema mismatch instead of
    /// failing: when `dir` holds an index built by an older OneBrain whose
    /// tantivy schema differs from the current one, the directory is wiped and
    /// recreated empty. Returns `(index, was_reset)` — a `true` flag tells the
    /// caller the lex index is now EMPTY and must be repopulated (the engine
    /// does this from its redb `chunk_meta`, so no files are re-read and
    /// nothing is re-embedded).
    ///
    /// Only a genuine schema mismatch triggers the reset. Any other error (I/O,
    /// permissions, corruption) propagates untouched — a wipe must never be the
    /// response to a problem we haven't identified.
    ///
    /// **Crash safety.** `was_reset` lives only in memory, so on its own it is
    /// lost if the process dies between the wipe and the repopulate's commit —
    /// and the post-wipe directory has a *current* schema, so the next open
    /// succeeds with `was_reset == false` over a permanently EMPTY index (0
    /// hits, while `status` still reports every doc indexed and `reindex`
    /// skips everything because the hashes say "current"). To close that
    /// window a marker file is written at [`rebuild_marker_path`] — a sibling
    /// of `dir`, so the `remove_dir_all` cannot destroy it — BEFORE the wipe.
    /// The engine repopulates whenever [`rebuild_pending`] is true, regardless
    /// of `was_reset`, and calls [`clear_rebuild_marker`] only after the
    /// repopulate has committed. Marker creation failing aborts before any
    /// destruction: better to refuse the migration than to perform an
    /// unrecorded wipe.
    pub fn open_or_reset(dir: &Path) -> Result<(Self, bool)> {
        match Self::open(dir) {
            Ok(index) => Ok((index, false)),
            Err(err) if is_schema_mismatch(&err) => {
                let marker = rebuild_marker_path(dir);
                if let Some(parent) = marker.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("creating dir for lex rebuild marker {}", marker.display())
                    })?;
                }
                std::fs::write(
                    &marker,
                    "onebrain: the keyword (tantivy) index was wiped for a schema migration and \
                     must be repopulated from redb chunk_meta. Removed automatically once the \
                     rebuild commits; if it lingers, run `onebrain search reindex --force`.\n",
                )
                .with_context(|| format!("writing lex rebuild marker {}", marker.display()))?;
                std::fs::remove_dir_all(dir)?;
                Ok((Self::open(dir)?, true))
            }
            Err(err) => Err(err),
        }
    }

    /// Number of committed, non-deleted documents in the index — one per
    /// indexed chunk. Cheap: reads the segment metas via a reader, no query.
    /// Used by [`crate::engine::Engine::lex_health`] to detect an index that
    /// is empty while `chunk_meta` still holds chunks.
    pub fn num_docs(&self) -> Result<u64> {
        Ok(self.index.reader()?.searcher().num_docs())
    }

    /// Return the [`IndexWriter`], creating it on first call. This is where
    /// tantivy's exclusive writer lock is acquired — deferred out of
    /// [`Self::open`] so read-only opens stay lock-free (see struct docs).
    fn writer_mut(&mut self) -> Result<&mut IndexWriter> {
        if self.writer.is_none() {
            self.writer = Some(self.index.writer(50_000_000)?);
        }
        // Safe: just ensured `Some` above.
        Ok(self.writer.as_mut().expect("writer just initialized"))
    }

    /// Empty the index: drop every committed document, and discard anything
    /// added on this writer but not yet committed.
    ///
    /// Exists because [`Self::add`] never replaces — a caller that re-adds
    /// every chunk (the engine's rebuild-from-`chunk_meta`) over an index that
    /// is NOT already empty silently doubles it, and duplicate documents
    /// corrupt BM25's document frequencies and average field length, so
    /// relevance degrades with every repeat.
    ///
    /// **Crash safety.** Takes effect only once [`Self::commit`] runs:
    /// tantivy's `delete_all_documents` clears the in-memory segment registers
    /// (committed *and* uncommitted) and reverts the opstamp — it writes
    /// nothing, deletes no segment file, and leaves `meta.json` on disk
    /// untouched. So a `clear()` → `add()`× → `commit()` sequence swaps the
    /// whole content in ONE durable step: a crash anywhere before the commit
    /// leaves the previous index exactly as it was, and there is no observable
    /// half-cleared state.
    ///
    /// Because it also discards uncommitted adds, call it FIRST — before the
    /// adds it is meant to precede — never in the middle of a batch.
    pub fn clear(&mut self) -> Result<()> {
        self.writer_mut()?.delete_all_documents()?;
        Ok(())
    }

    /// Add a chunk as a new document. Does not delete any prior document
    /// with the same `chunk_id` — call [`Self::delete`] first if
    /// re-indexing an updated chunk, or [`Self::clear`] first when re-adding
    /// everything.
    pub fn add(&mut self, chunk: &Chunk) -> Result<()> {
        let chunk_id = self.chunk_id;
        let doc_path = self.doc_path;
        let heading_path = self.heading_path;
        let body = self.body;
        self.writer_mut()?.add_document(doc!(
            chunk_id => chunk.chunk_id.clone(),
            doc_path => chunk.doc_path.clone(),
            heading_path => chunk.heading_path.clone(),
            body => chunk.text.clone(),
        ))?;
        Ok(())
    }

    /// Delete all documents with the given `chunk_id` (exact match on the
    /// `STRING`-indexed id field).
    pub fn delete(&mut self, chunk_id: &str) -> Result<()> {
        let term = Term::from_field_text(self.chunk_id, chunk_id);
        self.writer_mut()?.delete_term(term);
        Ok(())
    }

    /// Commit pending adds/deletes so they become visible to [`Self::search`].
    ///
    /// If no write has happened yet the writer was never created, so there is
    /// nothing to commit and no lock is acquired.
    pub fn commit(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.commit()?;
        }
        Ok(())
    }

    /// BM25 search over the `body` field. `query` is segmented with the
    /// same script-aware routine used at index time and turned into a
    /// `Should`-combined [`BooleanQuery`] of per-term [`TermQuery`]s
    /// (deliberately bypassing tantivy's `QueryParser`, which pre-tokenizes
    /// as English and would mis-tokenize no-space scripts). Returns
    /// `(chunk_id, score)`
    /// pairs, highest score first.
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<(String, f32)>> {
        self.search_docs(query, top_k, |searcher, doc_address| {
            let retrieved: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            Ok(retrieved
                .get_first(self.chunk_id)
                .and_then(|v| v.as_str())
                .map(str::to_string))
        })
    }

    /// Build the `Should`-combined [`BooleanQuery`] for `query` using the same
    /// script-aware segmentation as index time. Returns `None` when the query
    /// yields no searchable units (empty / punctuation-only), so callers return
    /// an empty result set without touching the reader.
    fn build_query(&self, query: &str) -> Option<BooleanQuery> {
        // Build one subquery per QUERY UNIT. Each no-space-script run (a
        // pseudo-word like `สุขภาพ`) becomes a nested Boolean requiring ALL
        // of its bigrams — lex semantics for no-space scripts are exact
        // substring-style: a hit means the document really contains the
        // queried word. (70% still let long common-syllable words like
        // `การออกกำลังกาย` match docs that only contain การ+ออก+กำลัง from
        // unrelated text.) Substring queries (`ภาพ` in `สุขภาพ`) still work
        // — only the QUERY's own bigrams are required. Fuzzy/partial recall
        // is the vector side's job. Multi-word Thai should be spaced
        // (`สุขภาพ การออกกำลังกาย` = OR of two runs). Real fix: nlpo3
        // dictionary segmentation (tracked follow-up).
        //
        // Every unit is built TWICE — once against `body`, once against
        // `heading_path` at [`HEADING_BOOST`] — and both are `Should`, so a
        // chunk matching in either field is retrievable and one matching in
        // both scores higher. The two fields share a tokenizer, so the same
        // segmentation applies to each.
        let mut units: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (start, end, is_script) in split_script_runs(query) {
            let run = &query[start..end];
            if is_script {
                let grams = script_bigrams(run, start);
                if grams.is_empty() {
                    continue;
                }
                let n = grams.len();
                let run_query = |field: Field| -> Box<dyn Query> {
                    let subs: Vec<(Occur, Box<dyn Query>)> = grams
                        .iter()
                        .map(|(text, _, _)| {
                            let term = Term::from_field_text(field, text);
                            let tq: Box<dyn Query> =
                                Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                            (Occur::Should, tq)
                        })
                        .collect();
                    Box::new(BooleanQuery::with_minimum_required_clauses(subs, n))
                };
                units.push((Occur::Should, run_query(self.body)));
                units.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(run_query(self.heading_path), HEADING_BOOST)),
                ));
            } else {
                for (text, _, _) in other_tokens(run, start) {
                    let term_query = |field: Field| -> Box<dyn Query> {
                        let term = Term::from_field_text(field, &text);
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                    };
                    units.push((Occur::Should, term_query(self.body)));
                    units.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(
                            term_query(self.heading_path),
                            HEADING_BOOST,
                        )),
                    ));
                }
            }
        }
        if units.is_empty() {
            return None;
        }
        Some(BooleanQuery::new(units))
    }

    /// Shared search core: build the query, run one top-`top_k` pass, and map
    /// each retrieved doc via `extract`, keeping only `Some` results. Both
    /// [`Self::search`] and [`Self::search_with_heading`] go through this so a
    /// hit's STORED fields (chunk_id, heading_path) are read from the SAME
    /// retrieved doc — no per-hit re-query.
    fn search_docs<T>(
        &self,
        query: &str,
        top_k: usize,
        extract: impl Fn(&tantivy::Searcher, tantivy::DocAddress) -> Result<Option<T>>,
    ) -> Result<Vec<(T, f32)>> {
        // Guard top_k == 0: tantivy's `TopDocs::with_limit` asserts limit > 0
        // and would panic. Mirror `VectorStore::search`'s guard so a raw
        // `--top-k 0` on the CLI returns empty rather than aborting.
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let Some(query) = self.build_query(query) else {
            return Ok(Vec::new());
        };

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let top_docs = searcher.search(&query, &TopDocs::with_limit(top_k).order_by_score())?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            if let Some(value) = extract(&searcher, doc_address)? {
                results.push((value, score));
            }
        }
        Ok(results)
    }

    /// Like [`Self::search`], but also returns each hit's `heading_path`, which
    /// is a STORED tantivy field — so the lex-only verb can show the heading
    /// WITHOUT opening the engine's redb metadata (v3.4.6, bug E). The chunk
    /// text/snippet is deliberately NOT returned: the `body` field is indexed
    /// but NOT `STORED`, so retrieving a snippet would require a schema change
    /// plus a full reindex migration (deferred). Returns triples of
    /// `chunk_id`, `heading_path`, and `score`, highest score first.
    ///
    /// Both `chunk_id` and `heading_path` are read from the SAME retrieved doc
    /// in the single search pass — no redundant per-hit `TermQuery`.
    pub fn search_with_heading(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(String, String, f32)>> {
        let hits = self.search_docs(query, top_k, |searcher, doc_address| {
            let retrieved: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            Ok(retrieved
                .get_first(self.chunk_id)
                .and_then(|v| v.as_str())
                .map(|id| {
                    let heading = retrieved
                        .get_first(self.heading_path)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    (id.to_string(), heading)
                }))
        })?;
        Ok(hits
            .into_iter()
            .map(|((id, heading), score)| (id, heading, score))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Chunk;
    fn chunk(id: &str, text: &str) -> Chunk {
        Chunk {
            chunk_id: id.into(),
            doc_path: id.split('#').next().unwrap().into(),
            heading_path: String::new(),
            chunk_index: 0,
            text: text.into(),
        }
    }
    #[test]
    fn bm25_english() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("d1#0", "error handling in rust")).unwrap();
        ix.add(&chunk("d2#0", "cooking pasta recipe")).unwrap();
        ix.commit().unwrap();
        assert_eq!(ix.search("error", 1).unwrap()[0].0, "d1#0");
    }

    /// Build a tantivy index with the PRE-change schema (`heading_path` as
    /// plain `TEXT`, i.e. the default English tokenizer) so the migration path
    /// can be exercised against what shipped vaults actually have on disk.
    fn create_legacy_index(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let mut sb = Schema::builder();
        sb.add_text_field("chunk_id", STRING | STORED);
        sb.add_text_field("doc_path", STRING | STORED);
        sb.add_text_field("heading_path", TEXT | STORED);
        let body_options = TEXT.set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(SCRIPT_AWARE_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        sb.add_text_field("body", body_options);
        let index = Index::builder()
            .schema(sb.build())
            .open_or_create(MmapDirectory::open(dir).unwrap())
            .unwrap();
        index
            .tokenizers()
            .register(SCRIPT_AWARE_TOKENIZER, ScriptAwareTokenizer);
        // Force segment files onto disk so the reopen sees a real index.
        let mut w: IndexWriter = index.writer(50_000_000).unwrap();
        w.commit().unwrap();
    }

    #[test]
    fn opening_a_legacy_schema_index_is_handled_not_silently_wrong() {
        // A shipped vault's tantivy dir was built with `heading_path` under the
        // default English tokenizer. Reopening it with the new schema must NOT
        // silently succeed and serve mis-tokenized heading hits — either it
        // errors loudly (caller migrates) or it rebuilds. This test pins
        // whichever behavior we actually get so a migration can be designed
        // against it rather than guessed.
        let dir = tempfile::tempdir().unwrap();
        create_legacy_index(dir.path());

        let opened = LexIndex::open(dir.path());
        let err = opened
            .err()
            .expect("legacy-schema index must not open silently under the new schema");
        assert!(
            is_schema_mismatch(&err),
            "expected a typed schema mismatch, got: {err}"
        );
    }

    #[test]
    fn open_or_reset_self_heals_a_legacy_schema_index() {
        // The migration path: a vault upgraded from a build with the old schema
        // must not hard-fail. `open_or_reset` wipes and recreates, reporting
        // `was_reset` so the engine knows to repopulate from redb.
        // The tantivy dir is a SUBDIR of the tempdir so the sibling rebuild
        // marker lands inside the tempdir too (and is cleaned up with it).
        let tmp = tempfile::tempdir().unwrap();
        // Name is arbitrary — the marker is derived from whatever the lex dir
        // is called (the real one comes from `CollectionLayout`).
        let dir = tmp.path().join("lex-index");
        std::fs::create_dir_all(&dir).unwrap();
        create_legacy_index(&dir);

        let (mut ix, was_reset) = LexIndex::open_or_reset(&dir).unwrap();
        assert!(was_reset, "legacy schema must be reported as reset");

        // The recreated index is empty but fully usable under the new schema.
        assert!(ix.search("anything", 5).unwrap().is_empty());
        let mut c = chunk("d1#0", "body text");
        c.heading_path = "ข้อเสีย".into();
        ix.add(&c).unwrap();
        ix.commit().unwrap();
        assert_eq!(ix.search("ข้อเสีย", 5).unwrap()[0].0, "d1#0");
    }

    #[test]
    fn open_or_reset_leaves_a_current_schema_index_untouched() {
        // Self-heal must be surgical: an index that opens cleanly keeps its
        // documents and reports `was_reset == false`.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut ix = LexIndex::open(dir.path()).unwrap();
            ix.add(&chunk("d1#0", "error handling in rust")).unwrap();
            ix.commit().unwrap();
        }
        let (ix, was_reset) = LexIndex::open_or_reset(dir.path()).unwrap();
        assert!(!was_reset);
        assert_eq!(ix.search("error", 5).unwrap()[0].0, "d1#0");
        assert!(
            !rebuild_pending(dir.path()),
            "an untouched index must not be marked for rebuild"
        );
    }

    #[test]
    fn open_or_reset_records_the_rebuild_before_wiping() {
        // B1: `was_reset` is in-memory only. If the process dies between the
        // wipe and the repopulate's commit, the directory left behind has a
        // MATCHING schema, so the next open reports `was_reset == false` over
        // a permanently empty index. The marker is what survives that crash —
        // so it must exist on disk the moment `open_or_reset` returns, before
        // anyone has repopulated anything.
        let tmp = tempfile::tempdir().unwrap();
        // Name is arbitrary — the marker is derived from whatever the lex dir
        // is called (the real one comes from `CollectionLayout`).
        let dir = tmp.path().join("lex-index");
        std::fs::create_dir_all(&dir).unwrap();
        create_legacy_index(&dir);

        let (_ix, was_reset) = LexIndex::open_or_reset(&dir).unwrap();
        assert!(was_reset);
        assert!(
            rebuild_pending(&dir),
            "a wiped index must be marked rebuild-pending at {}",
            rebuild_marker_path(&dir).display()
        );

        // It must live OUTSIDE the wiped directory, or the wipe would have
        // taken it with it — and inside the parent, so it travels with the
        // collection.
        let marker = rebuild_marker_path(&dir);
        assert!(
            !marker.starts_with(&dir),
            "marker must not be inside {dir:?}"
        );
        assert_eq!(marker.parent().unwrap(), tmp.path());

        // A second open sees a schema that now MATCHES: `was_reset` is false,
        // which is exactly why the engine must consult the marker instead.
        let (_ix2, was_reset2) = LexIndex::open_or_reset(&dir).unwrap();
        assert!(!was_reset2, "post-wipe schema matches, so no second reset");
        assert!(
            rebuild_pending(&dir),
            "the pending rebuild must survive a reopen that reports was_reset == false"
        );

        clear_rebuild_marker(&dir).unwrap();
        assert!(!rebuild_pending(&dir));
        // Idempotent: clearing an already-clear marker is not an error.
        clear_rebuild_marker(&dir).unwrap();
    }

    #[test]
    fn schema_mismatch_predicate_ignores_unrelated_schema_errors() {
        // C4: the predicate authorizes `remove_dir_all`, so it must be no
        // wider than its guarantee. Today tantivy's only reachable
        // `SchemaError` on `LexIndex::open`'s path is the mismatch one — but a
        // future release adding another (bad field option, unknown tokenizer)
        // must NOT be answered with a wipe.
        let unrelated: anyhow::Error =
            tantivy::TantivyError::SchemaError("field 'body' is not indexed".to_string()).into();
        assert!(
            !is_schema_mismatch(&unrelated),
            "an unrelated SchemaError must never authorize a wipe"
        );

        let mismatch: anyhow::Error = tantivy::TantivyError::SchemaError(
            "An index exists but the schema does not match.".to_string(),
        )
        .into();
        assert!(is_schema_mismatch(&mismatch));

        // Still typed-first: a non-tantivy error carrying the same words is
        // not a mismatch.
        let impostor = anyhow::anyhow!("An index exists but the schema does not match.");
        assert!(!is_schema_mismatch(&impostor));
    }

    #[test]
    fn heading_path_is_searchable() {
        // heading_path carries the section context ("A > B > C"). A query whose
        // terms appear ONLY in the heading must still retrieve the chunk —
        // before this, every TermQuery targeted `body` alone, so the field was
        // indexed but never queried.
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        let mut c = chunk("d1#0", "costs 41x more to build");
        c.heading_path = "GraphRAG > Drawbacks".into();
        ix.add(&c).unwrap();
        ix.add(&chunk("d2#0", "unrelated pasta recipe")).unwrap();
        ix.commit().unwrap();

        let hits = ix.search("drawbacks", 5).unwrap();
        assert_eq!(hits.len(), 1, "heading-only term must retrieve the chunk");
        assert_eq!(hits[0].0, "d1#0");
    }

    #[test]
    fn heading_path_is_script_aware_tokenized() {
        // The field was declared plain `TEXT`, i.e. the default ENGLISH
        // tokenizer, while `body` uses the script-aware one. Thai headings were
        // therefore mis-tokenized. Both fields must segment identically or a
        // bilingual vault gets heading matches only in English.
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        let mut c = chunk("d1#0", "some body text");
        c.heading_path = "ข้อเสีย > ต้นทุน".into();
        ix.add(&c).unwrap();
        ix.commit().unwrap();

        let hits = ix.search("ข้อเสีย", 5).unwrap();
        assert_eq!(hits.len(), 1, "Thai heading term must retrieve the chunk");
        assert_eq!(hits[0].0, "d1#0");
    }

    #[test]
    fn body_match_outranks_heading_only_match() {
        // Heading hits are boosted BELOW body hits: a heading match repeats
        // across every chunk under that section, so it must not outrank a chunk
        // that genuinely discusses the term in its text.
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        let mut heading_only = chunk("d1#0", "completely unrelated filler text");
        heading_only.heading_path = "Reranker".into();
        ix.add(&heading_only).unwrap();
        ix.add(&chunk("d2#0", "the reranker scores candidates"))
            .unwrap();
        ix.commit().unwrap();

        let hits = ix.search("reranker", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "d2#0", "body match must rank first");
    }

    #[test]
    fn a_heading_match_adds_score_on_top_of_an_identical_body_match() {
        // The guard on the DOWNWARD direction, and the one this whole release
        // rests on. Every other heading test asserts RETRIEVAL, and a
        // zero-weight `BoostQuery` still MATCHES — so `HEADING_BOOST = 0.0`
        // left all of them green while delivering exactly none of the recall
        // win. Only 10.0 was guarded (by `body_match_outranks_heading_only_
        // match`), i.e. the direction nobody would drift into.
        //
        // Two chunks with BYTE-IDENTICAL bodies, so their body subquery scores
        // are equal by construction and the heading is the ONLY difference.
        // At boost 0.0 the two scores are exactly equal and the strict `>`
        // below fails.
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        let body = "the reranker scores candidates";
        let mut both = chunk("d1#0", body);
        both.heading_path = "Reranker".into();
        ix.add(&both).unwrap();
        ix.add(&chunk("d2#0", body)).unwrap();
        ix.commit().unwrap();

        let hits = ix.search("reranker", 5).unwrap();
        assert_eq!(hits.len(), 2);
        let score = |id: &str| hits.iter().find(|h| h.0 == id).unwrap().1;
        assert!(
            score("d1#0") > score("d2#0"),
            "a heading match must ADD score over the same body match — heading boost is inert \
             (got d1={}, d2={})",
            score("d1#0"),
            score("d2#0")
        );
        assert_eq!(hits[0].0, "d1#0", "and must therefore rank first");
    }

    #[test]
    fn search_with_heading_returns_stored_heading_path() {
        // Bug E (v3.4.6): heading_path is a STORED tantivy field, so the
        // lex-only verb can recover it without opening redb.
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        let mut c = chunk("d1#0", "error handling in rust");
        c.heading_path = "Errors › Handling".into();
        ix.add(&c).unwrap();
        ix.commit().unwrap();

        let hits = ix.search_with_heading("error", 5).unwrap();
        assert_eq!(hits.len(), 1);
        let (id, heading, _score) = &hits[0];
        assert_eq!(id, "d1#0");
        assert_eq!(heading, "Errors › Handling");
    }

    #[test]
    fn search_with_heading_maps_each_hit_to_its_own_heading() {
        // Multi-hit: every hit's heading_path must come from ITS OWN doc, read
        // from the same retrieved doc in the single search pass (no per-hit
        // re-query, no shared by_id map that could mis-associate headings).
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        for (id, heading) in [
            ("d1#0", "Alpha › One"),
            ("d2#0", "Bravo › Two"),
            ("d3#0", "Charlie › Three"),
        ] {
            let mut c = chunk(id, "shared token error");
            c.heading_path = heading.into();
            ix.add(&c).unwrap();
        }
        ix.commit().unwrap();

        let hits = ix.search_with_heading("error", 10).unwrap();
        assert_eq!(hits.len(), 3, "all three docs match `error`");
        let expected: std::collections::HashMap<&str, &str> = [
            ("d1#0", "Alpha › One"),
            ("d2#0", "Bravo › Two"),
            ("d3#0", "Charlie › Three"),
        ]
        .into_iter()
        .collect();
        for (id, heading, _score) in &hits {
            assert_eq!(
                Some(heading.as_str()),
                expected.get(id.as_str()).copied(),
                "hit {id} must carry its own heading, got {heading}"
            );
        }
    }

    #[test]
    fn search_with_heading_empty_when_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("d1#0", "error handling")).unwrap();
        ix.commit().unwrap();
        assert!(ix.search_with_heading("zorp", 5).unwrap().is_empty());
    }
    #[test]
    fn thai_segmented_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("t1#0", "การจัดการหน่วยความจำในภาษารัสต์"))
            .unwrap();
        ix.commit().unwrap();
        assert_eq!(ix.search("หน่วยความจำ", 1).unwrap()[0].0, "t1#0");
    }
    #[test]
    fn delete_removes_from_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("d1#0", "unique token zorp")).unwrap();
        ix.commit().unwrap();
        ix.delete("d1#0").unwrap();
        ix.commit().unwrap();
        assert!(ix.search("zorp", 1).unwrap().is_empty());
    }
    #[test]
    fn top_k_zero_returns_empty_no_panic() {
        // `TopDocs::with_limit(0)` panics in tantivy; the guard must intercept.
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("d1#0", "error handling in rust")).unwrap();
        ix.commit().unwrap();
        assert!(ix.search("error", 0).unwrap().is_empty());
    }
    #[test]
    fn chinese_bigram_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("z1#0", "机器学习与人工智能")).unwrap(); // "machine learning and AI"
        ix.commit().unwrap();
        assert_eq!(ix.search("机器学习", 1).unwrap()[0].0, "z1#0"); // "machine learning"
    }
    #[test]
    fn japanese_bigram_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("j1#0", "日本語の全文検索")).unwrap(); // "Japanese full-text search"
        ix.commit().unwrap();
        assert_eq!(ix.search("全文検索", 1).unwrap()[0].0, "j1#0"); // "full-text search"
    }
    #[test]
    fn korean_bigram_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("k1#0", "기계학습론")).unwrap(); // "theory of machine learning"
        ix.commit().unwrap();
        assert_eq!(ix.search("학습", 1).unwrap()[0].0, "k1#0"); // "learning"
    }
    #[test]
    fn lao_bigram_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("l1#0", "ການຮຽນຮູ້ຂອງເຄື່ອງຈັກ")).unwrap(); // "machine learning"
        ix.commit().unwrap();
        assert_eq!(ix.search("ຮຽນຮູ້", 1).unwrap()[0].0, "l1#0"); // "learning"
    }
    #[test]
    fn russian_default_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = LexIndex::open(dir.path()).unwrap();
        ix.add(&chunk("r1#0", "машинное обучение и поиск")).unwrap(); // "machine learning and search"
        ix.commit().unwrap();
        assert_eq!(ix.search("обучение", 1).unwrap()[0].0, "r1#0"); // "learning"
    }

    /// A read-only `open` must not acquire tantivy's writer lock, so a second
    /// reader can open the same directory (while the first is alive) and
    /// `search` — no `LockBusy`. Regression test for read verbs failing
    /// concurrently with (or after) a writer.
    #[test]
    fn read_only_open_does_not_lock_writer() {
        let dir = tempfile::tempdir().unwrap();

        // First: write and commit via one index, then keep it alive.
        let mut writer_ix = LexIndex::open(dir.path()).unwrap();
        writer_ix.add(&chunk("d1#0", "shared token quux")).unwrap();
        writer_ix.commit().unwrap();

        // Second: a fresh read-only open of the same dir must succeed and
        // search without acquiring the writer lock. `writer_ix` (which now
        // holds the writer lock) is still alive at this point.
        let reader_ix = LexIndex::open(dir.path()).unwrap();
        assert_eq!(reader_ix.search("quux", 1).unwrap()[0].0, "d1#0");

        // A third read-only open alongside both also works — multiple readers
        // coexist because none of them touch the writer.
        let reader_ix2 = LexIndex::open(dir.path()).unwrap();
        assert_eq!(reader_ix2.search("quux", 1).unwrap()[0].0, "d1#0");
    }

    /// An empty query, a punctuation-only query (no searchable units), and
    /// `top_k == 0` must each return an empty result set without panicking —
    /// the query-build and `TopDocs::with_limit` guards short-circuit first.
    #[test]
    fn search_empty_and_punctuation_queries_return_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = LexIndex::open(dir.path()).unwrap();
        s.add(&chunk("d1#0", "some indexed text สุขภาพ")).unwrap();
        s.commit().unwrap();
        assert!(s.search("", 5).unwrap().is_empty(), "empty query");
        assert!(
            s.search("!!! ??? ---", 5).unwrap().is_empty(),
            "punctuation-only query yields no units"
        );
        assert!(s.search("text", 0).unwrap().is_empty(), "top_k 0");
    }

    #[test]
    fn search_single_thai_word_requires_the_whole_word() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = LexIndex::open(dir.path()).unwrap();
        s.add(&chunk("health#0", "บันทึกเรื่องสุขภาพประจำวัน")).unwrap();
        s.add(&chunk("pic#0", "รูปภาพจากทริปเชียงใหม่")).unwrap();
        s.commit().unwrap();
        // Full word only in health#0 — the doc sharing only the ภา/าพ pairs
        // (ภาพ in pic#0) must NOT match.
        let hits = s.search("สุขภาพ", 5).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].0, "health#0");
        // Substring query still hits both docs containing ภาพ.
        let hits = s.search("ภาพ", 5).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
    }

    /// A read-only open + `search` must succeed even when a stale writer lock
    /// file is present on disk (e.g. left behind by a killed `reindex`). Since
    /// `open`/`search` never acquire the writer lock, the stale file is
    /// irrelevant to them. Regression test for the `LockBusy` failure.
    #[test]
    fn search_succeeds_with_stale_writer_lock_present() {
        let dir = tempfile::tempdir().unwrap();

        // Seed the index so it exists on disk.
        {
            let mut seed = LexIndex::open(dir.path()).unwrap();
            seed.add(&chunk("d1#0", "lonely token wibble")).unwrap();
            seed.commit().unwrap();
        }

        // Simulate a stale writer lock left by a crashed/killed writer.
        let stale_lock = dir.path().join(".tantivy-writer.lock");
        std::fs::write(&stale_lock, b"").unwrap();
        assert!(stale_lock.exists());

        // A read-only open + search must still work: it never touches the
        // writer, so the stale lock does not cause `LockBusy`.
        let reader = LexIndex::open(dir.path()).unwrap();
        assert_eq!(reader.search("wibble", 1).unwrap()[0].0, "d1#0");
    }

    /// A freshly-opened index that is only ever read must not create the
    /// writer at all — `search` goes through the writer-free reader path.
    #[test]
    fn read_only_open_leaves_writer_uninitialized() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut seed = LexIndex::open(dir.path()).unwrap();
            seed.add(&chunk("d1#0", "token flurb")).unwrap();
            seed.commit().unwrap();
        }
        let reader = LexIndex::open(dir.path()).unwrap();
        // Nothing has written, so the lazy writer was never materialized.
        assert!(
            reader.writer.is_none(),
            "read-only open must not create a writer"
        );
        assert_eq!(reader.search("flurb", 1).unwrap()[0].0, "d1#0");
        assert!(reader.writer.is_none(), "search must not create a writer");
    }
}
