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

use std::path::Path;

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, TokenStream, Tokenizer,
};
use tantivy::{doc, Index, IndexWriter, Term};

use crate::chunk::Chunk;

/// Name under which the script-aware analyzer is registered on the index's
/// [`tantivy::tokenizer::TokenizerManager`]. Used for both the `body`
/// field's schema-declared tokenizer and query-time segmentation, so
/// index-time and query-time tokenization always match.
const SCRIPT_AWARE_TOKENIZER: &str = "script_aware";

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
        let heading_path = schema_builder.add_text_field("heading_path", TEXT | STORED);
        let body_options = TEXT.set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(SCRIPT_AWARE_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let body = schema_builder.add_text_field("body", body_options);
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

    /// Add a chunk as a new document. Does not delete any prior document
    /// with the same `chunk_id` — call [`Self::delete`] first if
    /// re-indexing an updated chunk.
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
        // Guard top_k == 0: tantivy's `TopDocs::with_limit` asserts limit > 0
        // and would panic. Mirror `VectorStore::search`'s guard so a raw
        // `--top-k 0` on the CLI returns empty rather than aborting.
        if top_k == 0 {
            return Ok(Vec::new());
        }
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
        let mut units: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (start, end, is_script) in split_script_runs(query) {
            let run = &query[start..end];
            if is_script {
                let grams = script_bigrams(run, start);
                if grams.is_empty() {
                    continue;
                }
                let n = grams.len();
                let subs: Vec<(Occur, Box<dyn Query>)> = grams
                    .into_iter()
                    .map(|(text, _, _)| {
                        let term = Term::from_field_text(self.body, &text);
                        let tq: Box<dyn Query> =
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                        (Occur::Should, tq)
                    })
                    .collect();
                let min = n;
                units.push((
                    Occur::Should,
                    Box::new(BooleanQuery::with_minimum_required_clauses(subs, min)),
                ));
            } else {
                for (text, _, _) in other_tokens(run, start) {
                    let term = Term::from_field_text(self.body, &text);
                    let tq: Box<dyn Query> =
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                    units.push((Occur::Should, tq));
                }
            }
        }
        if units.is_empty() {
            return Ok(Vec::new());
        }
        let query = BooleanQuery::new(units);

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let top_docs = searcher.search(&query, &TopDocs::with_limit(top_k).order_by_score())?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            if let Some(id) = retrieved.get_first(self.chunk_id).and_then(|v| v.as_str()) {
                results.push((id.to_string(), score));
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
    pub fn search_with_heading(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(String, String, f32)>> {
        let raw = self.search(query, top_k)?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // Re-run the search to recover stored fields alongside the chunk_id.
        // Cheap relative to the query itself (same reader, same top_docs), and
        // keeps `search`'s hot-path signature `(chunk_id, score)` untouched for
        // its many callers (hybrid fuse, mcp, webui).
        //
        // TODO(v3.4.x): this issues a redundant per-hit `TermQuery` +
        // `order_by_score` to re-fetch each doc, even though `search()` already
        // retrieved the doc address whose STORED `heading_path` we want. Fold
        // heading (and eventually a STORED-body snippet) into a single pass by
        // having `search()` return the doc addresses — folded into the
        // deferred-snippet follow-up (body-STORED schema change + reindex
        // migration), so it lands with that work rather than as a separate
        // refactor now.
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let mut by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (chunk_id, _) in &raw {
            // Look up the doc for this chunk_id to fetch its heading_path.
            let term = Term::from_field_text(self.chunk_id, chunk_id);
            let tq = TermQuery::new(term, IndexRecordOption::Basic);
            let hits = searcher.search(&tq, &TopDocs::with_limit(1).order_by_score())?;
            if let Some((_, addr)) = hits.first() {
                let doc: tantivy::TantivyDocument = searcher.doc(*addr)?;
                let heading = doc
                    .get_first(self.heading_path)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                by_id.insert(chunk_id.clone(), heading);
            }
        }
        Ok(raw
            .into_iter()
            .map(|(id, score)| {
                let heading = by_id.get(&id).cloned().unwrap_or_default();
                (id, heading, score)
            })
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

    /// A read-only open + `search` must succeed even when a stale writer lock
    /// file is present on disk (e.g. left behind by a killed `reindex`). Since
    /// `open`/`search` never acquire the writer lock, the stale file is
    /// irrelevant to them. Regression test for the `LockBusy` failure.
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
