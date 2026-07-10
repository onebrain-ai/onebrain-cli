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
    let level = child_indent(lines, start, end)?;
    if (level as isize) <= parent_indent {
        return None;
    }
    (start..end).find(|&i| {
        let l = &lines[i];
        !is_blank(l)
            && !is_comment(l)
            && indent_of(l) == level
            && l.trim_start().starts_with(&key_prefix)
    })
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
