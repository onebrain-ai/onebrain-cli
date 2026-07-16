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
    // a sibling `model:`, and vice versa).
    let child_indent = |lines: &[String], start: usize, end: usize| -> Option<usize> {
        (start..end)
            .filter(|&i| !is_blank(&lines[i]) && !is_comment(&lines[i]))
            .map(|i| indent_of(&lines[i]))
            .min()
    };

    for seg in parents {
        let header = format!("{seg}:");
        let level = child_indent(lines, start, end)?;
        // The section header: first non-blank, non-comment line in the window
        // sitting exactly at the direct-child level and starting with `seg:`.
        let idx = (start..end).find(|&i| {
            let l = &lines[i];
            !is_blank(l)
                && !is_comment(l)
                && indent_of(l) == level
                && (level as isize) > parent_indent
                && l.trim_start().starts_with(&header)
        })?;
        // Refuse inline mappings (`seg: {…}` / `seg: null`) — only a
        // block-form header can carry the child lines we walk next. A
        // trailing `# …` comment on the header line is fine (`search:  # my
        // search config`); anything else after the colon means an inline
        // value.
        let after_header = lines[idx].trim_start()[header.len()..].trim_start();
        if !(after_header.is_empty() || after_header.starts_with('#')) {
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
    let key_prefix = format!("{last}:");
    let commented_key_prefix = format!("# {key_prefix}");
    let level = child_indent(lines, start, end)?;
    if (level as isize) <= parent_indent {
        return None;
    }
    (start..end).find(|&i| {
        let l = &lines[i];
        if is_blank(l) || indent_of(l) != level {
            return false;
        }
        let trimmed = l.trim_start();
        if is_comment(l) {
            allow_commented_key && trimmed.starts_with(&commented_key_prefix)
        } else {
            trimmed.starts_with(&key_prefix)
        }
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
    let header = format!("{parent}:");
    let parent_idx = lines
        .iter()
        .position(|l| indent_of(l) == 0 && !is_comment(l) && l.trim_start().starts_with(&header));

    let Some(pidx) = parent_idx else {
        // No `parent:` line at all → append a fresh block.
        return append_block(text, parent, &[(key, value)]);
    };

    // Only `parent:` (optionally with a trailing `# comment`) is a block we
    // can extend; anything else after the colon is an inline/scalar value.
    let after = lines[pidx].trim_start()[header.len()..].trim_start();
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
