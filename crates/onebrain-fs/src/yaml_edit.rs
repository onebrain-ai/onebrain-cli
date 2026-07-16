//! Comment-preserving line edits for `onebrain.yml`.
//!
//! v3.4.8 ships a self-documenting commented config template (ADR 0026), so
//! every writer that touches the file on a routine path must stop
//! round-tripping it through `serde_yaml` (which drops all comments). This
//! module is the shared home for the surgical alternative: locate the target
//! key by walking block-form section headers and replace ONLY the value
//! portion of its line — indentation, surrounding comment lines, inline
//! `# …` comments, key order, and the file's CRLF/LF style all survive.
//!
//! Consumers: `vault_sync::vault_yml::update_vault_yml` (sync step 7) and
//! `onebrain-cli`'s doctor `--fix` reset recipe (`reset_config_value`
//! delegates here). The remaining serde-based structural writers are tracked
//! in issue #200.

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}
fn is_blank(l: &str) -> bool {
    l.trim().is_empty()
}
fn is_comment(l: &str) -> bool {
    l.trim_start().starts_with('#')
}

/// Extract the bare key token from a mapping line's key portion (everything
/// before the first `:`), or `None` when the line has no `:` (not a
/// `key: value` shape). `raw` must already be trimmed of leading indentation
/// — and, for a commented line, of its leading `#`. The returned token is
/// whitespace-trimmed and stripped of a single pair of surrounding quotes,
/// so callers compare the REAL YAML key rather than a raw text prefix:
/// `check_timeout_ms` matches `check_timeout_ms : 500` (a space before the
/// colon is valid YAML) and `"check_timeout_ms": 500` / `'check_timeout_ms':
/// 500` (quoted), but never the longer key `check_timeout_ms_extra: 1`.
///
/// (A colon *inside* a quoted key — `"a:b": 1` — is not handled; none of
/// onebrain.yml's keys contain one, and this stays a line-oriented editor,
/// not a YAML parser.)
pub fn key_token(raw: &str) -> Option<&str> {
    let before = raw.split_once(':')?.0.trim();
    let unquoted = before
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| before.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(before);
    Some(unquoted)
}

/// The bare key token of a line at the given block level: its own token when
/// active, or the token inside its `# …` comment when `allow_commented` and
/// the line is a comment. `None` when the line isn't a `key: value` shape
/// (or is a comment while `allow_commented` is false).
fn line_key_token(line: &str, allow_commented: bool) -> Option<&str> {
    let trimmed = line.trim_start();
    if is_comment(line) {
        if !allow_commented {
            return None;
        }
        key_token(trimmed.trim_start_matches('#').trim_start())
    } else {
        key_token(trimmed)
    }
}

/// Locate the line index of the key at `segments` by walking block-form
/// section headers. Returns `None` when a parent is an inline mapping
/// (`checkpoint: {messages: 0}`), the key line can't be found, or the shape
/// is otherwise not understood — never guesses.
fn locate(lines: &[String], segments: &[&str]) -> Option<usize> {
    locate_impl(lines, segments, false)
}

/// [`locate`]'s implementation, parameterized by `allow_commented_key`: when
/// `true`, the FINAL segment may also match a commented-out placeholder line
/// (`# <key>: <value>`) at the same block level — used by
/// [`key_or_commented_placeholder_present`] for keys the fresh template
/// legitimately emits commented-out by default (e.g.
/// `token_optimization.get_max_tokens`, which follows a per-level ladder
/// unless uncommented). Parent segments always require an active,
/// block-form header regardless of this flag — only the leaf key's match
/// relaxes.
fn locate_impl(lines: &[String], segments: &[&str], allow_commented_key: bool) -> Option<usize> {
    let (last, parents) = segments.split_last()?;

    // Window (start..end) of the current block; top level = whole file with
    // parent indent -1 (any indent 0 line qualifies as a child).
    let (mut start, mut end) = (0usize, lines.len());
    let mut parent_indent: isize = -1;

    // The window's direct-child indent level: the SHALLOWEST non-blank,
    // non-comment line — deeper lines are grandchildren and must never
    // match a key lookup for this level (a nested `model:` must not shadow
    // a sibling `model:`, and vice versa). When the block has NO active
    // children (every child commented out — e.g. a user-disabled
    // `token_optimization:` block, issue #270), fall back to the shallowest
    // COMMENT line deeper than the parent, so the `allow_commented_key` path
    // can still locate the commented placeholders instead of returning None
    // for every sub-key (which would make `--fix` append fresh active
    // duplicates under the user's commented lines).
    let child_indent =
        |lines: &[String], start: usize, end: usize, parent_indent: isize| -> Option<usize> {
            (start..end)
                .filter(|&i| !is_blank(&lines[i]) && !is_comment(&lines[i]))
                .map(|i| indent_of(&lines[i]))
                .min()
                .or_else(|| {
                    (start..end)
                        .filter(|&i| {
                            is_comment(&lines[i]) && (indent_of(&lines[i]) as isize) > parent_indent
                        })
                        .map(|i| indent_of(&lines[i]))
                        .min()
                })
        };

    for seg in parents {
        let level = child_indent(lines, start, end, parent_indent)?;
        // The section header: first non-blank, non-comment line in the window
        // sitting exactly at the direct-child level whose KEY TOKEN is `seg`
        // (a raw `starts_with("seg:")` would miss `seg :` / `"seg":`).
        let idx = (start..end).find(|&i| {
            let l = &lines[i];
            !is_blank(l)
                && !is_comment(l)
                && indent_of(l) == level
                && (level as isize) > parent_indent
                && line_key_token(l, false) == Some(*seg)
        })?;
        // Refuse inline mappings (`seg: {…}` / `seg: null`) — only a
        // block-form header can carry the child lines we walk next. A
        // trailing `# …` comment on the header line is fine (`search:  # my
        // search config`); anything else after the colon means an inline
        // value. Split on the FIRST colon (never a fixed `seg:` slice — that
        // breaks on `seg :` / `"seg":`).
        let after_colon = lines[idx]
            .trim_start()
            .split_once(':')
            .map(|(_, a)| a.trim_start())
            .unwrap_or("");
        if !(after_colon.is_empty() || after_colon.starts_with('#')) {
            return None;
        }
        let header_indent = indent_of(&lines[idx]) as isize;
        // Block extent: subsequent blank lines, comment lines, or lines
        // indented deeper than the header.
        let mut block_end = idx + 1;
        while block_end < end {
            let l = &lines[block_end];
            if is_blank(l) || is_comment(l) || (indent_of(l) as isize) > header_indent {
                block_end += 1;
            } else {
                break;
            }
        }
        start = idx + 1;
        end = block_end;
        parent_indent = header_indent;
    }

    // The key line inside the final window, at the direct-child level only.
    let level = child_indent(lines, start, end, parent_indent)?;
    if (level as isize) <= parent_indent {
        return None;
    }
    (start..end).find(|&i| {
        let l = &lines[i];
        indent_of(l) == level && line_key_token(l, allow_commented_key) == Some(*last)
    })
}

/// True when the key at `segments` is already "documented" in `text` —
/// either as an active (uncommented) key ([`locate`] would find it) or as a
/// commented-out placeholder line (`# <key>: <value>`) at the same block
/// level. Doctor's `token_optimization` sub-key backfill (issue #270) uses
/// this instead of a plain presence check for two reasons: (1) two sub-keys
/// (`get_max_tokens` / `snippet_max_chars`) are legitimately emitted
/// commented-out by the fresh template — a commented placeholder there is
/// already covered, never "missing"; (2) a user who deliberately comments
/// out an otherwise-active key (e.g. `# check_timeout_ms: 500` to disable a
/// tunable) is never fought — their own comment wins, mirroring
/// [`insert_comment_above`]'s "a user's own comment always wins" rule.
pub fn key_or_commented_placeholder_present(text: &str, segments: &[&str]) -> bool {
    let (lines, _, _) = split(text);
    locate_impl(&lines, segments, true).is_some()
}

/// Split `text` into lines plus its newline style and trailing-newline flag,
/// and re-join edits with both preserved.
fn split(text: &str) -> (Vec<String>, &'static str, bool) {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    (
        text.lines().map(str::to_string).collect(),
        newline,
        text.ends_with('\n'),
    )
}

fn join(lines: Vec<String>, newline: &str, ends_with_newline: bool) -> String {
    let mut out = lines.join(newline);
    if ends_with_newline {
        out.push_str(newline);
    }
    out
}

/// Set `segments`' value to `value` in raw config text via a
/// comment-preserving line edit. `segments` is the key path
/// (`["search", "reranker", "min_score"]`); parents are located as
/// block-form section headers by walking each block's extent, and only the
/// final key line's VALUE portion is replaced.
///
/// Returns `None` (caller decides the fallback) when [`locate`] can't
/// address the key.
pub fn set_value(text: &str, segments: &[&str], value: &str) -> Option<String> {
    let (mut lines, newline, ends_with_newline) = split(text);
    let idx = locate(&lines, segments)?;
    let last = segments.last()?;
    let key_prefix = format!("{last}:");

    let line = &lines[idx];
    let indent = &line[..indent_of(line)];
    let after_key = &line.trim_start()[key_prefix.len()..];
    // Preserve an inline comment on the key line (including its exact
    // leading whitespace) — conservatively, only when the value portion
    // carries no quote characters (a `#` inside a quoted scalar is data,
    // not a comment).
    let comment_suffix = match after_key.find('#') {
        Some(h)
            if h > 0 && after_key[..h].ends_with(' ') && !after_key[..h].contains(['"', '\'']) =>
        {
            &after_key[after_key[..h].trim_end_matches(' ').len()..]
        }
        _ => "",
    };
    lines[idx] = format!("{indent}{last}: {value}{comment_suffix}");
    Some(join(lines, newline, ends_with_newline))
}

/// True when the key at `segments` exists (addressable by [`locate`]) AND
/// the line directly above it is not a comment — i.e. the key lacks
/// self-documentation. Absent/unaddressable keys return `false` (nothing to
/// document).
pub fn key_lacks_comment(text: &str, segments: &[&str]) -> bool {
    let (lines, _, _) = split(text);
    match locate(&lines, segments) {
        Some(idx) => idx == 0 || !is_comment(&lines[idx - 1]),
        None => false,
    }
}

/// Insert `comment` (one or more full `# …` lines separated by `\n`,
/// indentation excluded) directly above the key at `segments`, matching the
/// key line's indentation. Multi-line comments are inserted line by line and
/// re-joined with the FILE's newline style, so a CRLF config never gains bare
/// LFs. Returns `None` — no change — when the key can't be located OR the
/// line directly above it is already a comment (the user's own comments
/// always win; never replaced, never deduped).
pub fn insert_comment_above(text: &str, segments: &[&str], comment: &str) -> Option<String> {
    let (mut lines, newline, ends_with_newline) = split(text);
    let idx = locate(&lines, segments)?;
    if idx > 0 && is_comment(&lines[idx - 1]) {
        return None;
    }
    let indent = lines[idx][..indent_of(&lines[idx])].to_string();
    for (n, comment_line) in comment.split('\n').enumerate() {
        lines.insert(idx + n, format!("{indent}{comment_line}"));
    }
    Some(join(lines, newline, ends_with_newline))
}

/// Append a top-level `key: value` line to `text`, preserving the file's
/// newline style and guaranteeing exactly one trailing newline before the
/// appended line. Callers must have verified the key is genuinely absent at
/// the top level (appending a duplicate would corrupt the YAML).
pub fn append_top_level(text: &str, key: &str, value: &str) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(newline);
    }
    out.push_str(&format!("{key}: {value}{newline}"));
    out
}

/// Append a top-level `key:` block with `children` (each rendered `  k: v`) to
/// the end of `text`, preserving newline style and ensuring exactly one
/// trailing newline before the block. Callers must have verified `key` is
/// absent at the top level.
fn append_block(text: &str, key: &str, children: &[(&str, &str)]) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(newline);
    }
    out.push_str(&format!("{key}:{newline}"));
    for (k, v) in children {
        out.push_str(&format!("  {k}: {v}{newline}"));
    }
    out
}

/// Delete the key at `segments` from `text` via a comment-preserving line
/// edit. Returns `Some(new_text)` on success, or `None` when [`locate`] can't
/// address the key — a missing key, or a parent that is an inline mapping
/// (`checkpoint: {…}`) — mirroring [`set_value`]'s locate semantics exactly
/// (exact indent-level match, block-form parents only).
///
/// The deleted region is the key's own line plus its continuation lines: any
/// following lines indented deeper than the key (the block value's children)
/// and blank lines interleaved among them. **Design decision:** it also
/// removes a contiguous run of comment lines sitting directly above the key —
/// its lead doc-comment — because a doc comment left dangling above nothing
/// reads worse than dropping it with its key. Blank separators above the key
/// and sibling keys/comments below are preserved.
pub fn delete_key(text: &str, segments: &[&str]) -> Option<String> {
    let (mut lines, newline, ends_with_newline) = split(text);
    let idx = locate(&lines, segments)?;
    let key_indent = indent_of(&lines[idx]);

    // End of the deleted region: consume deeper-indented continuation lines
    // plus interior blanks that still sit inside the deeper block.
    let mut end = idx + 1;
    while end < lines.len() {
        let l = &lines[end];
        let deeper = !is_blank(l) && indent_of(l) > key_indent;
        // An interior blank belongs to the block only when the next non-blank
        // line is still a deeper-indented child.
        let interior_blank = is_blank(l)
            && lines[end + 1..]
                .iter()
                .find(|n| !is_blank(n))
                .is_some_and(|n| indent_of(n) > key_indent);
        if deeper || interior_blank {
            end += 1;
        } else {
            break;
        }
    }

    // Start of the deleted region: back up over the key's contiguous lead
    // comment lines (at any indent).
    let mut start = idx;
    while start > 0 && is_comment(&lines[start - 1]) {
        start -= 1;
    }

    lines.drain(start..end);
    Some(join(lines, newline, ends_with_newline))
}

/// Upsert `parent.key = value` where `parent` is a TOP-LEVEL block key,
/// preserving comments and every other line. Handles four shapes:
///
/// - `parent.key` already present → replace its value ([`set_value`], keeping
///   any inline comment).
/// - `parent:` block present, `key` absent → insert `  key: value` as the
///   block's last child (child indentation matched from an existing child,
///   else two spaces).
/// - `parent:` present but inline / scalar / null (not a block header) →
///   replace that single line with a fresh `parent:` block carrying
///   `key: value` (the pre-v3.4.8 "replace the non-mapping value" behaviour,
///   done as a line edit).
/// - `parent:` absent entirely → append a fresh block at EOF.
///
/// Always succeeds (returns `String`): the parent is a top-level key of a
/// mapping-rooted config, the invariant each writer upholds after its own
/// read.
pub fn upsert_child(text: &str, parent: &str, key: &str, value: &str) -> String {
    // Existing child → value replace (preserves an inline comment).
    if let Some(out) = set_value(text, &[parent, key], value) {
        return out;
    }

    let (mut lines, newline, ends_with_newline) = split(text);
    // Match the parent's KEY TOKEN, not a raw `starts_with("{parent}:")`
    // prefix — the same bug already fixed for `locate`/`key_token` elsewhere
    // in this module: a parent key with a space before the colon (`search
    // :`) or a quoted key (`"search":`) would otherwise be missed, falling
    // through to `append_block` and creating a DUPLICATE top-level block.
    let parent_idx = lines.iter().position(|l| {
        indent_of(l) == 0 && !is_comment(l) && line_key_token(l, false) == Some(parent)
    });

    let Some(pidx) = parent_idx else {
        // No `parent:` line at all → append a fresh block.
        return append_block(text, parent, &[(key, value)]);
    };

    // Only `parent:` (optionally with a trailing `# comment`) is a block we
    // can extend; anything else after the colon is an inline/scalar value.
    // Split on the FIRST colon (never a fixed `"{parent}:"` slice — that
    // breaks on `parent :` / `"parent":`, same as `locate_impl`).
    let after = lines[pidx]
        .trim_start()
        .split_once(':')
        .map(|(_, a)| a.trim_start())
        .unwrap_or("");
    if !(after.is_empty() || after.starts_with('#')) {
        lines.splice(
            pidx..pidx + 1,
            [format!("{parent}:"), format!("  {key}: {value}")],
        );
        return join(lines, newline, ends_with_newline);
    }

    // Block header present but `key` absent → insert after the last direct
    // child (interior blanks skipped; a top-level line or comment ends the
    // block).
    let mut last_child = pidx;
    let mut child_indent: Option<usize> = None;
    let mut j = pidx + 1;
    while j < lines.len() {
        let l = &lines[j];
        if is_blank(l) {
            j += 1;
            continue;
        }
        if indent_of(l) == 0 {
            break;
        }
        if !is_comment(l) && child_indent.is_none() {
            child_indent = Some(indent_of(l));
        }
        last_child = j;
        j += 1;
    }
    let indent = " ".repeat(child_indent.unwrap_or(2));
    lines.insert(last_child + 1, format!("{indent}{key}: {value}"));
    join(lines, newline, ends_with_newline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_value_replaces_top_level_key() {
        let text = "# header\nupdate_channel: next\nfolders:\n  inbox: 00-inbox\n";
        let out = set_value(text, &["update_channel"], "stable").unwrap();
        assert_eq!(
            out,
            "# header\nupdate_channel: stable\nfolders:\n  inbox: 00-inbox\n"
        );
    }

    #[test]
    fn set_value_nested_key_preserves_comments_and_siblings() {
        let text = "search:\n  # gate\n  reranker:\n    min_score: 7.5\n    model: m\n";
        let out = set_value(text, &["search", "reranker", "min_score"], "0.30").unwrap();
        assert!(out.contains("    min_score: 0.30\n"), "{out}");
        assert!(out.contains("# gate"), "{out}");
        assert!(out.contains("model: m"), "{out}");
    }

    #[test]
    fn set_value_missing_key_or_inline_parent_is_none() {
        assert!(set_value("folders:\n  inbox: x\n", &["update_channel"], "stable").is_none());
        assert!(set_value(
            "checkpoint: {messages: 0}\n",
            &["checkpoint", "messages"],
            "15"
        )
        .is_none());
        assert!(set_value("a: 1\n", &[], "x").is_none());
    }

    #[test]
    fn key_lacks_comment_true_when_key_is_first_line() {
        // idx == 0: no line sits above the key at all, so it trivially
        // "lacks a comment" — doctor's `--fix` must insert one, not skip it.
        let text = "a: 1\nb: 2\n";
        assert!(key_lacks_comment(text, &["a"]));
    }

    #[test]
    fn key_lacks_comment_true_when_previous_line_is_not_a_comment() {
        let text = "a: 1\nb: 2\n";
        assert!(key_lacks_comment(text, &["b"]));
    }

    #[test]
    fn key_lacks_comment_false_when_comment_directly_above() {
        let text = "# doc for a\na: 1\nb: 2\n";
        assert!(!key_lacks_comment(text, &["a"]));
    }

    #[test]
    fn key_lacks_comment_false_when_key_not_found() {
        let text = "a: 1\nb: 2\n";
        assert!(!key_lacks_comment(text, &["nope"]));
    }

    #[test]
    fn insert_comment_above_multi_line_and_crlf() {
        // A multi-line comment (the schedule header) lands as one line per
        // Vec entry, re-joined with the FILE's newline style.
        let text = "a: 1\r\nschedule:\r\n- cron: 0 9 * * *\r\n  skill: /daily\r\n";
        let out = insert_comment_above(text, &["schedule"], "# line one\n# line two").unwrap();
        assert_eq!(
            out,
            "a: 1\r\n# line one\r\n# line two\r\nschedule:\r\n- cron: 0 9 * * *\r\n  skill: /daily\r\n"
        );
        // Refused when the key already sits under a comment.
        assert!(insert_comment_above(&out, &["schedule"], "# again").is_none());
    }

    #[test]
    fn delete_key_removes_top_level_key_and_its_lead_comment() {
        let text = "# doc for a\na: 1\nb: 2\n";
        let out = delete_key(text, &["a"]).unwrap();
        assert_eq!(out, "b: 2\n");
    }

    #[test]
    fn delete_key_removes_nested_key_preserving_siblings_and_comments() {
        let text = "search:\n  # keep me\n  collection: c\n  embed_model: m\n";
        let out = delete_key(text, &["search", "embed_model"]).unwrap();
        assert_eq!(out, "search:\n  # keep me\n  collection: c\n");
    }

    #[test]
    fn delete_key_removes_block_value_children() {
        let text = "runtime:\n  harness: claude\nfolders:\n  inbox: x\n";
        let out = delete_key(text, &["runtime"]).unwrap();
        assert_eq!(out, "folders:\n  inbox: x\n");
    }

    #[test]
    fn delete_key_crlf_preserved() {
        let text = "a: 1\r\nqmd_collection: ob-1\r\nb: 2\r\n";
        let out = delete_key(text, &["qmd_collection"]).unwrap();
        assert_eq!(out, "a: 1\r\nb: 2\r\n");
    }

    #[test]
    fn delete_key_at_eof_without_trailing_newline() {
        let text = "a: 1\nb: 2";
        let out = delete_key(text, &["b"]).unwrap();
        assert_eq!(out, "a: 1");
    }

    #[test]
    fn delete_key_missing_or_inline_parent_is_none() {
        assert!(delete_key("a: 1\n", &["nope"]).is_none());
        assert!(delete_key("checkpoint: {messages: 0}\n", &["checkpoint", "messages"]).is_none());
        assert!(delete_key("a: 1\n", &[]).is_none());
    }

    #[test]
    fn delete_key_is_idempotent_second_call_is_none() {
        let text = "a: 1\nb: 2\n";
        let out = delete_key(text, &["b"]).unwrap();
        assert_eq!(out, "a: 1\n");
        assert!(delete_key(&out, &["b"]).is_none());
    }

    #[test]
    fn upsert_child_replaces_existing_value_and_keeps_inline_comment() {
        let text = "search:\n  collection: old  # my index\n";
        let out = upsert_child(text, "search", "collection", "new");
        assert_eq!(out, "search:\n  collection: new  # my index\n");
    }

    #[test]
    fn upsert_child_inserts_into_existing_block_before_sibling() {
        let text = "search:\n  collection: c\nfolders:\n  inbox: x\n";
        let out = upsert_child(text, "search", "embed_model", "m");
        assert_eq!(
            out,
            "search:\n  collection: c\n  embed_model: m\nfolders:\n  inbox: x\n"
        );
    }

    #[test]
    fn upsert_child_creates_block_when_parent_absent() {
        let text = "folders:\n  inbox: x\n";
        let out = upsert_child(text, "search", "collection", "c");
        assert_eq!(out, "folders:\n  inbox: x\nsearch:\n  collection: c\n");
    }

    #[test]
    fn upsert_child_replaces_inline_parent_with_block() {
        let text = "search: not-a-mapping\n";
        let out = upsert_child(text, "search", "collection", "c");
        assert_eq!(out, "search:\n  collection: c\n");
    }

    #[test]
    fn upsert_child_into_empty_block_header() {
        let text = "search:\nfolders:\n  inbox: x\n";
        let out = upsert_child(text, "search", "collection", "c");
        assert_eq!(out, "search:\n  collection: c\nfolders:\n  inbox: x\n");
    }

    #[test]
    fn upsert_child_on_empty_text_creates_block() {
        assert_eq!(
            upsert_child("", "search", "collection", "c"),
            "search:\n  collection: c\n"
        );
    }

    #[test]
    fn upsert_child_finds_spaced_and_quoted_parent_key_no_duplicate() {
        // BLOCKING regression (Copilot round-2 finding #1): a raw
        // `starts_with(&header)` prefix check (header = "{parent}:") misses a
        // parent key with a space before the colon (`search :`) or a quoted
        // key (`"search":`) — the SAME bug already fixed elsewhere in this
        // module via `key_token` (see the `key_or_commented_placeholder_present`
        // spaced/quoted regression tests above). Missing the parent falls
        // through to `append_block`, which appends a SECOND top-level
        // `search:` block — a duplicate key that corrupts the config. This
        // must FAIL on the old `starts_with` code and PASS once `upsert_child`
        // matches on `line_key_token` like the rest of the module.
        let spaced = "search :\n  collection: c\nfolders:\n  inbox: x\n";
        let out = upsert_child(spaced, "search", "embed_model", "m");
        assert_eq!(
            out.matches("search").count(),
            1,
            "must not create a duplicate `search` block: {out}"
        );
        assert!(out.contains("embed_model: m"), "{out}");
        assert!(out.contains("collection: c"), "{out}");
        serde_yaml::from_str::<serde_yaml::Value>(&out).expect("result must parse as valid YAML");

        let quoted = "\"search\":\n  collection: c\nfolders:\n  inbox: x\n";
        let out = upsert_child(quoted, "search", "embed_model", "m");
        assert_eq!(
            out.matches("search").count(),
            1,
            "must not create a duplicate `search` block: {out}"
        );
        assert!(out.contains("embed_model: m"), "{out}");
        assert!(out.contains("collection: c"), "{out}");
        serde_yaml::from_str::<serde_yaml::Value>(&out).expect("result must parse as valid YAML");
    }

    #[test]
    fn upsert_child_crlf_preserved_on_insert() {
        let text = "search:\r\n  collection: c\r\n";
        let out = upsert_child(text, "search", "embed_model", "m");
        assert_eq!(out, "search:\r\n  collection: c\r\n  embed_model: m\r\n");
    }

    #[test]
    fn key_or_commented_placeholder_present_finds_active_and_commented_forms() {
        let text = "token_optimization:\n  level: balanced\n  # get_max_tokens: 6000\n";
        assert!(key_or_commented_placeholder_present(
            text,
            &["token_optimization", "level"]
        ));
        assert!(key_or_commented_placeholder_present(
            text,
            &["token_optimization", "get_max_tokens"]
        ));
        assert!(!key_or_commented_placeholder_present(
            text,
            &["token_optimization", "check_timeout_ms"]
        ));
    }

    #[test]
    fn key_or_commented_placeholder_present_respects_user_comment_disabling_active_key() {
        // A user who comments out an otherwise-active key is never fought —
        // the commented line counts as "already documented", not missing.
        let text = "token_optimization:\n  level: balanced\n  # check_timeout_ms: 500\n";
        assert!(key_or_commented_placeholder_present(
            text,
            &["token_optimization", "check_timeout_ms"]
        ));
    }

    #[test]
    fn key_or_commented_placeholder_present_absent_key_is_false() {
        assert!(!key_or_commented_placeholder_present(
            "folders:\n  inbox: x\n",
            &["token_optimization", "level"]
        ));
    }

    #[test]
    fn key_token_extracts_real_key_across_spacing_and_quotes() {
        assert_eq!(key_token("check_timeout_ms: 200"), Some("check_timeout_ms"));
        // Space before the colon is valid YAML.
        assert_eq!(
            key_token("check_timeout_ms : 200"),
            Some("check_timeout_ms")
        );
        assert_eq!(
            key_token("check_timeout_ms   :200"),
            Some("check_timeout_ms")
        );
        // Quoted keys.
        assert_eq!(
            key_token("\"check_timeout_ms\": 200"),
            Some("check_timeout_ms")
        );
        assert_eq!(
            key_token("'check_timeout_ms' : 200"),
            Some("check_timeout_ms")
        );
        // A longer key is NOT the target (raw prefix would false-match).
        assert_eq!(
            key_token("check_timeout_ms_extra: 1"),
            Some("check_timeout_ms_extra")
        );
        // No colon → not a key line.
        assert_eq!(key_token("- attachments"), None);
        assert_eq!(key_token("just_text"), None);
    }

    #[test]
    fn key_or_commented_placeholder_present_matches_spaced_and_quoted_active_keys() {
        // BLOCKING regression (issue #270 R-review): a valid `key : value`
        // (space before colon) or `"key": value` (quoted) must be detected as
        // PRESENT — a raw prefix check missed these, which false-reported the
        // key missing and drove a duplicate-key insert that corrupted the file.
        assert!(key_or_commented_placeholder_present(
            "token_optimization:\n  check_timeout_ms : 500\n",
            &["token_optimization", "check_timeout_ms"]
        ));
        assert!(key_or_commented_placeholder_present(
            "token_optimization:\n  \"check_timeout_ms\": 500\n",
            &["token_optimization", "check_timeout_ms"]
        ));
        // The check must NOT match a different, longer key.
        assert!(!key_or_commented_placeholder_present(
            "token_optimization:\n  check_timeout_ms_extra: 500\n",
            &["token_optimization", "check_timeout_ms"]
        ));
        // Commented placeholder with a space before the colon still counts.
        assert!(key_or_commented_placeholder_present(
            "token_optimization:\n  # check_timeout_ms : 500\n",
            &["token_optimization", "check_timeout_ms"]
        ));
    }

    #[test]
    fn key_or_commented_placeholder_present_finds_keys_in_fully_commented_block() {
        // Finding 2: a block whose children are ALL commented (no active
        // sibling) must still resolve its commented placeholders — otherwise
        // `--fix` re-adds fresh active defaults under the user's commented
        // lines. child_indent's comment fallback covers this.
        let text = "token_optimization:\n  # level: balanced\n  # read_hook: ledger\n";
        assert!(key_or_commented_placeholder_present(
            text,
            &["token_optimization", "level"]
        ));
        assert!(key_or_commented_placeholder_present(
            text,
            &["token_optimization", "read_hook"]
        ));
        // A key that's genuinely absent (not even commented) stays absent.
        assert!(!key_or_commented_placeholder_present(
            text,
            &["token_optimization", "check_timeout_ms"]
        ));
    }

    #[test]
    fn set_value_matches_spaced_key() {
        // The key-token fix also benefits the active-key editors: a spaced
        // key is now addressable (previously missed by the raw prefix).
        let text = "search:\n  reranker:\n    min_score : 7.5\n";
        let out = set_value(text, &["search", "reranker", "min_score"], "0.30").unwrap();
        assert!(out.contains("    min_score: 0.30\n"), "{out}");
    }

    #[test]
    fn append_top_level_handles_trailing_newline_and_crlf() {
        assert_eq!(
            append_top_level("a: 1\n", "update_channel", "stable"),
            "a: 1\nupdate_channel: stable\n"
        );
        assert_eq!(
            append_top_level("a: 1", "update_channel", "stable"),
            "a: 1\nupdate_channel: stable\n"
        );
        assert_eq!(
            append_top_level("a: 1\r\n", "update_channel", "stable"),
            "a: 1\r\nupdate_channel: stable\r\n"
        );
        assert_eq!(
            append_top_level("", "update_channel", "stable"),
            "update_channel: stable\n"
        );
    }
}
