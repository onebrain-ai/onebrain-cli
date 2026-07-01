//! Heading-aware + size-split markdown chunker.
//!
//! Splits a markdown document into [`Chunk`]s along ATX heading boundaries
//! (`#`, `##`, `###`, ...), tracking the active heading stack so each chunk
//! knows its full `heading_path` (e.g. "A > B > C"). Any section whose body
//! exceeds `max_tokens` (approximated via whitespace-word count) is further
//! sliced into overlapping windows so downstream embedding/indexing never
//! sees an oversized chunk.

pub struct Chunk {
    pub chunk_id: String,
    pub doc_path: String,
    pub heading_path: String,
    pub chunk_index: usize,
    pub text: String,
}

/// A line that starts a new ATX heading: `(level, heading_text)`.
fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // ATX requires a space (or end of line) after the '#' run.
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((hashes, rest.trim()))
}

/// Split `body` (the accumulated section text, headings excluded) into one
/// or more windows of at most `max_tokens` words, reusing the last
/// `overlap_tokens` words of window k as the first words of window k+1.
fn split_body_into_windows(body: &str, max_tokens: usize, overlap_tokens: usize) -> Vec<String> {
    let words: Vec<&str> = body.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    if words.len() <= max_tokens || max_tokens == 0 {
        return vec![words.join(" ")];
    }

    let overlap = overlap_tokens.min(max_tokens.saturating_sub(1));
    let stride = max_tokens - overlap;
    let mut windows = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + max_tokens).min(words.len());
        windows.push(words[start..end].join(" "));
        if end == words.len() {
            break;
        }
        start += stride;
    }
    windows
}

/// Chunk a markdown document into heading-scoped, size-bounded pieces.
///
/// Token counting is a whitespace-word approximation
/// (`split_whitespace().count()`), not a real tokenizer.
pub fn chunk_markdown(
    doc_path: &str,
    content: &str,
    max_tokens: usize,
    overlap_tokens: usize,
) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut chunk_index = 0usize;
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut section_buf = String::new();

    let heading_path = |stack: &[(usize, String)]| -> String {
        stack
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    };

    let mut flush =
        |buf: &mut String, stack: &[(usize, String)], chunks: &mut Vec<Chunk>, idx: &mut usize| {
            if buf.trim().is_empty() {
                buf.clear();
                return;
            }
            let path = heading_path(stack);
            for window in split_body_into_windows(buf, max_tokens, overlap_tokens) {
                chunks.push(Chunk {
                    chunk_id: format!("{doc_path}#{idx}"),
                    doc_path: doc_path.to_string(),
                    heading_path: path.clone(),
                    chunk_index: *idx,
                    text: window,
                });
                *idx += 1;
            }
            buf.clear();
        };

    for line in content.lines() {
        if let Some((level, text)) = parse_heading(line) {
            // New heading: flush whatever section text has accumulated
            // under the *current* heading stack before changing it.
            flush(
                &mut section_buf,
                &heading_stack,
                &mut chunks,
                &mut chunk_index,
            );

            while let Some(&(top_level, _)) = heading_stack.last() {
                if top_level >= level {
                    heading_stack.pop();
                } else {
                    break;
                }
            }
            heading_stack.push((level, text.to_string()));
        } else {
            section_buf.push_str(line);
            section_buf.push('\n');
        }
    }
    flush(
        &mut section_buf,
        &heading_stack,
        &mut chunks,
        &mut chunk_index,
    );

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_headings_with_path() {
        let md = "# A\nintro\n## B\nbody b\n### C\nbody c\n";
        let cs = chunk_markdown("n.md", md, 512, 64);
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].heading_path, "A");
        assert_eq!(cs[1].heading_path, "A > B");
        assert_eq!(cs[2].heading_path, "A > B > C");
        assert_eq!(cs[0].chunk_id, "n.md#0");
    }
    #[test]
    fn large_section_is_size_split_with_overlap() {
        let big = "# H\n".to_string() + &"word ".repeat(1200);
        let cs = chunk_markdown("n.md", &big, 512, 64);
        assert!(cs.len() >= 3, "1200 words / 512 -> >=3 chunks");
        assert!(cs.iter().all(|c| c.heading_path == "H"));
        let a: Vec<&str> = cs[0].text.split_whitespace().collect();
        let b: Vec<&str> = cs[1].text.split_whitespace().collect();
        assert_eq!(&a[a.len() - 64..], &b[..64]); // last 64 words of chunk k reappear at start of k+1
    }
    #[test]
    fn empty_and_no_heading() {
        assert!(chunk_markdown("n.md", "", 512, 64).is_empty());
        let cs = chunk_markdown("n.md", "just prose no heading", 512, 64);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].heading_path, "");
    }
}
