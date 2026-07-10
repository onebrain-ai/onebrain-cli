//! Section layout for `onebrain.yml` — Style-A banners + a fixed section
//! order shared by the fresh-init template and `doctor --fix`.
//!
//! v3.4.8 (ADR 0026 addendum) groups the config into labelled sections:
//! General → Vault layout → Agent behavior → Search → Automation → System
//! (stats last, marked system-managed). Two consumers share this ONE
//! definition so they can never drift:
//!
//! - [`render_onebrain_yml`](crate::render_onebrain_yml) builds each
//!   top-level block's text and hands the ordered blocks to [`assemble`],
//!   which emits the banners and blank separators.
//! - `onebrain doctor --fix` calls [`restructure_config`] to reorder an
//!   EXISTING vault's top-level blocks into this same order and insert the
//!   banners — WITHOUT reparsing/re-emitting YAML: each block is moved as an
//!   opaque line-range so every value, user comment, and inline comment
//!   survives byte-for-byte. `doctor` (read-only) uses
//!   [`config_layout_matches`] to report drift.
//!
//! The restructure is idempotent: banners and the system-managed note are
//! recognised as structural and stripped before re-segmentation, so a second
//! `--fix` produces byte-identical output.

/// Total display width (in chars) of a section banner line.
const BANNER_WIDTH: usize = 60;

/// The system-managed note emitted directly under the `System` banner, above
/// the `stats:` block. Also recognised as structural (stripped) during
/// re-segmentation so the restructure stays idempotent.
pub const SYSTEM_MANAGED_NOTE: &str = "# Managed by OneBrain — do not edit.";

/// One layout section: its banner title and the top-level keys that belong to
/// it, in the order they should appear within the section.
struct SectionDef {
    title: &'static str,
    keys: &'static [&'static str],
}

/// The locked section order (ADR 0026 addendum, 2026-07-09). `stats` is the
/// only member of the last section so it always lands at the bottom.
const SECTIONS: &[SectionDef] = &[
    SectionDef {
        title: "General",
        keys: &["update_channel"],
    },
    SectionDef {
        title: "Vault layout",
        keys: &["folders"],
    },
    SectionDef {
        title: "Agent behavior",
        keys: &["checkpoint", "recap"],
    },
    SectionDef {
        title: "Search",
        keys: &["search"],
    },
    SectionDef {
        title: "Automation",
        keys: &["schedule"],
    },
    SectionDef {
        title: "System",
        keys: &["stats"],
    },
];

/// Index of the `System` section (stats) — the always-last bucket.
const SYSTEM_SECTION: usize = 5;

/// Const-context `&str` equality (`==` on `&str` is not const-stable).
const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// Compile-time guard against drift between `SYSTEM_SECTION` and `SECTIONS`:
// inserting a section before "System" becomes a build error, not a silent
// misplacement of `stats` / SYSTEM_MANAGED_NOTE.
const _: () = {
    assert!(SYSTEM_SECTION < SECTIONS.len());
    assert!(const_str_eq(SECTIONS[SYSTEM_SECTION].title, "System"));
};

/// Render a Style-A banner line for `title`, padded with box-drawing dashes to
/// [`BANNER_WIDTH`] columns. Deterministic — the fresh template and the
/// restructure emit byte-identical banners.
pub fn section_banner(title: &str) -> String {
    let prefix = format!("# ── {title} ");
    let used = prefix.chars().count();
    let dashes = BANNER_WIDTH.saturating_sub(used).max(1);
    format!("{prefix}{}", "─".repeat(dashes))
}

/// True when `line` is one of the module's OWN section banners — an exact
/// match against the canonical [`section_banner`] render of a known section
/// title. Structural — stripped before re-segmentation.
///
/// Exact-match on purpose: a looser prefix test (e.g. `# ─`) would classify a
/// user's own dash-art comment (`# ─ my divider`) as structural and silently
/// delete it on restructure, violating the byte-for-byte contract — and make
/// `config_layout_matches` report perpetual drift on such a file. Only lines
/// this module itself emits are ever stripped.
fn is_banner_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    SECTIONS.iter().any(|s| trimmed == section_banner(s.title))
}

/// True when `line` is the system-managed note. Structural — stripped before
/// re-segmentation.
fn is_managed_note(line: &str) -> bool {
    line.trim() == SYSTEM_MANAGED_NOTE
}

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}
fn is_blank(l: &str) -> bool {
    l.trim().is_empty()
}
fn is_comment(l: &str) -> bool {
    l.trim_start().starts_with('#')
}

/// A top-level mapping key line: indent 0, not blank/comment, not a block
/// sequence item (`- …`), and carrying a `key:` mapping key. The key name is
/// everything before the first `:`.
fn top_level_key(l: &str) -> Option<&str> {
    if is_blank(l) || is_comment(l) || indent_of(l) != 0 || l.starts_with('-') {
        return None;
    }
    let key = l.split(':').next()?.trim_end();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// The section index a top-level `key` belongs to, plus its order within that
/// section. `None` for keys absent from [`SECTIONS`] (unknown top-level keys).
fn section_of(key: &str) -> Option<(usize, usize)> {
    for (si, sec) in SECTIONS.iter().enumerate() {
        if let Some(ki) = sec.keys.iter().position(|k| *k == key) {
            return Some((si, ki));
        }
    }
    None
}

/// A top-level block moved as an opaque unit: its key name and its exact text
/// (attached lead comments + header + indented body), with no surrounding
/// blank lines and no banners.
pub(crate) struct Block {
    pub key: String,
    /// Block lines, in order, verbatim. Never contains banner/managed-note
    /// lines and never leading/trailing blanks.
    pub lines: Vec<String>,
}

fn split(text: &str) -> (Vec<String>, &'static str, bool) {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    (
        text.lines().map(str::to_string).collect(),
        newline,
        text.ends_with('\n'),
    )
}

/// Assemble `preamble` (verbatim file header, may be empty) and the already
/// ordered `blocks` into the canonical sectioned layout: a banner (with a
/// leading blank separator, except at the very top) before the first block of
/// each section, the system-managed note under the `System` banner, and no
/// blank between blocks that share a section. Unknown-key blocks get a blank
/// separator but no banner.
///
/// Shared by [`render_onebrain_yml`](crate::render_onebrain_yml) and
/// [`restructure_config`] so the two cannot drift.
pub(crate) fn assemble(
    preamble: &[String],
    blocks: &[Block],
    newline: &str,
    ends_with_newline: bool,
) -> String {
    let mut out: Vec<String> = Vec::new();

    // Preamble verbatim, trailing blanks trimmed.
    let mut pre_end = preamble.len();
    while pre_end > 0 && is_blank(&preamble[pre_end - 1]) {
        pre_end -= 1;
    }
    out.extend(preamble[..pre_end].iter().cloned());
    let mut emitted_any = !out.is_empty();

    // `None` = no block seen yet; `Some(sec)` = section of the previous block
    // (`sec` is itself `Option<usize>`, `None` for unknown keys).
    let mut prev_section: Option<Option<usize>> = None;
    for block in blocks {
        let sec = section_of(&block.key).map(|(si, _)| si);
        if prev_section != Some(sec) {
            if emitted_any {
                out.push(String::new()); // blank separator
            }
            if let Some(si) = sec {
                out.push(section_banner(SECTIONS[si].title));
                if si == SYSTEM_SECTION {
                    out.push(SYSTEM_MANAGED_NOTE.to_string());
                }
            }
            emitted_any = true;
        }
        out.extend(block.lines.iter().cloned());
        prev_section = Some(sec);
    }

    let mut joined = out.join(newline);
    if ends_with_newline {
        joined.push_str(newline);
    }
    joined
}

/// Segment `lines` (already stripped of banners and the managed note) into the
/// file preamble (everything above the first top-level key's attached
/// comments) and the top-level blocks, each block trimmed of surrounding blank
/// lines. Returns `None` when there are no top-level keys.
fn segment(lines: &[String]) -> Option<(Vec<String>, Vec<Block>)> {
    let key_idxs: Vec<usize> = (0..lines.len())
        .filter(|&i| top_level_key(&lines[i]).is_some())
        .collect();
    if key_idxs.is_empty() {
        return None;
    }

    // For a key at `ki`, its block starts at the top of the contiguous run of
    // comment lines directly above it (no blank gap) — the key's attached doc
    // comments travel with it.
    let lead_start = |ki: usize| -> usize {
        let mut s = ki;
        while s > 0 && is_comment(&lines[s - 1]) && !is_blank(&lines[s - 1]) {
            s -= 1;
        }
        s
    };

    let preamble_end = lead_start(key_idxs[0]);
    let preamble: Vec<String> = lines[..preamble_end].to_vec();

    let mut blocks: Vec<Block> = Vec::with_capacity(key_idxs.len());
    for (n, &ki) in key_idxs.iter().enumerate() {
        let start = lead_start(ki);
        let end = key_idxs
            .get(n + 1)
            .map(|&next| lead_start(next))
            .unwrap_or(lines.len());
        // Trim trailing blank lines (inter-block separators live outside the
        // block); internal blanks (followed by more block lines) are kept.
        let mut block_end = end;
        while block_end > start + 1 && is_blank(&lines[block_end - 1]) {
            block_end -= 1;
        }
        let key = top_level_key(&lines[ki]).unwrap().to_string();
        blocks.push(Block {
            key,
            lines: lines[start..block_end].to_vec(),
        });
    }
    Some((preamble, blocks))
}

/// Reorder `blocks` into the canonical section order: known non-`stats` blocks
/// by section order, then unknown-key blocks in their original relative order,
/// then the `stats` block(s) last.
fn reorder(blocks: Vec<Block>) -> Vec<Block> {
    let mut known: Vec<(usize, usize, Block)> = Vec::new(); // (section, within, block)
    let mut unknown: Vec<Block> = Vec::new();
    let mut system: Vec<Block> = Vec::new();
    for block in blocks {
        match section_of(&block.key) {
            Some((SYSTEM_SECTION, _)) => system.push(block),
            Some((si, ki)) => known.push((si, ki, block)),
            None => unknown.push(block),
        }
    }
    // Stable sort by (section, within-section) — encounter order breaks ties.
    known.sort_by_key(|(si, ki, _)| (*si, *ki));

    let mut out: Vec<Block> = Vec::with_capacity(known.len() + unknown.len() + system.len());
    out.extend(known.into_iter().map(|(_, _, b)| b));
    out.extend(unknown);
    out.extend(system);
    out
}

/// Restructure an existing `onebrain.yml`'s top-level blocks into the canonical
/// sectioned layout, preserving each block's internal bytes and inserting the
/// section banners. Idempotent.
///
/// Returns `None` — the caller leaves the file untouched — for every shape
/// the line-oriented block mover can't safely address: invalid YAML, a
/// non-mapping root (sequence/scalar/empty), a FLOW-STYLE root mapping
/// (`{a: 1, b: 2}` — a valid mapping, but it has no block-form top-level key
/// lines to move), or a block mapping with no recognisable top-level keys.
pub fn restructure_config(text: &str) -> Option<String> {
    // Only operate on a genuine mapping — a non-mapping / empty (Null) file is
    // some other check's territory; never guess at a shape we don't own.
    let parsed: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    parsed.as_mapping()?;
    // Decline a flow-style root explicitly. It parses as a mapping (so the
    // gate above does not fire) and would only survive segmentation by the
    // accident of `top_level_key` lumping the whole `{…}` into one opaque
    // block — declining is the honest contract, not accidental safety.
    if text.trim_start().starts_with('{') {
        return None;
    }

    let (lines, newline, ends_with_newline) = split(text);
    // Strip structural lines (banners + managed note) so re-segmentation sees
    // only preamble, key doc-comments, keys, and bodies — the source of
    // idempotency.
    let cleaned: Vec<String> = lines
        .into_iter()
        .filter(|l| !is_banner_line(l) && !is_managed_note(l))
        .collect();
    let (preamble, blocks) = segment(&cleaned)?;
    let ordered = reorder(blocks);
    Some(assemble(&preamble, &ordered, newline, ends_with_newline))
}

/// True when `text` is already in the canonical section layout (a restructure
/// would be a no-op) OR is a shape [`restructure_config`] declines to touch.
/// `false` signals layout drift for `doctor` to report.
pub fn config_layout_matches(text: &str) -> bool {
    match restructure_config(text) {
        Some(restructured) => restructured == text,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_in_order(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(top_level_key)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn banner_is_fixed_width_and_english() {
        let b = section_banner("General");
        assert!(b.starts_with("# ── General "));
        assert_eq!(b.chars().count(), BANNER_WIDTH);
        assert!(is_banner_line(&b));
    }

    #[test]
    fn banner_with_a_very_long_title_keeps_at_least_one_dash() {
        // Guards the `.max(1)` floor when the title overflows BANNER_WIDTH.
        let long = "A".repeat(80);
        let b = section_banner(&long);
        assert!(b.starts_with(&format!("# ── {long} ")));
        assert!(b.ends_with('─'));
    }

    #[test]
    fn unknown_only_config_gets_no_banner_and_is_idempotent() {
        // A config of only unknown keys: no banner emitted, order preserved,
        // idempotent — exercises the assemble `None`-section path first.
        let text = "beta: 1\nalpha:\n  x: 2\n";
        let out = restructure_config(text).unwrap();
        assert!(!out.contains("# ──"), "no banner for unknown-only: {out}");
        assert_eq!(keys_in_order(&out), vec!["beta", "alpha"]);
        assert_eq!(restructure_config(&out).unwrap(), out);
    }

    #[test]
    fn scrambled_config_is_reordered_to_template_order() {
        // recap/stats mid-file, schedule before search — the real-vault shape.
        let text = "\
# Release channel for plugin updates: stable | next · default: stable
update_channel: stable
folders:
  # Raw braindumps and quick captures · default: 00-inbox
  inbox: 00-inbox
checkpoint:
  messages: 15
recap:
  min_sessions: 6
  min_frequency: 2
stats:
  last_doctor_run: 2026-07-09
schedule:
- cron: 0 9 * * *
  skill: /daily
search:
  collection: ob-1-441565
  embed_model: multilingual-e5-base
";
        let out = restructure_config(text).unwrap();
        assert_eq!(
            keys_in_order(&out),
            vec![
                "update_channel",
                "folders",
                "checkpoint",
                "recap",
                "search",
                "schedule",
                "stats",
            ]
        );
        // Values + user comment survived.
        assert!(out.contains("collection: ob-1-441565"));
        assert!(out.contains("# Raw braindumps and quick captures · default: 00-inbox"));
        assert!(out.contains("  min_sessions: 6"));
        // Banners inserted, stats marked system-managed and last.
        assert!(out.contains(&section_banner("General")));
        assert!(out.contains(&section_banner("System")));
        assert!(out.contains(SYSTEM_MANAGED_NOTE));
        let stats_pos = out.find("stats:").unwrap();
        assert!(out[stats_pos..].contains("last_doctor_run"));
        assert!(
            out.trim_end().ends_with("2026-07-09") || out.contains("last_doctor_run: 2026-07-09")
        );
    }

    #[test]
    fn unknown_top_level_keys_kept_in_relative_order_before_stats() {
        let text = "\
update_channel: stable
zeta_custom: 1
stats:
  last_doctor_run: 2026-07-09
alpha_custom:
  nested: true
checkpoint:
  messages: 15
";
        let out = restructure_config(text).unwrap();
        // Known first (update_channel, checkpoint), then unknowns in their
        // original relative order (zeta before alpha), then stats last.
        assert_eq!(
            keys_in_order(&out),
            vec![
                "update_channel",
                "checkpoint",
                "zeta_custom",
                "alpha_custom",
                "stats",
            ]
        );
        assert!(out.contains("  nested: true"));
    }

    #[test]
    fn schedule_list_and_args_body_survive_verbatim() {
        let text = "\
search:
  collection: c
schedule:
- cron: 0 9 * * *
  skill: /daily
- cron: 45 8 * * *
  command: onebrain
  args:
  - search
  - reindex
update_channel: stable
";
        let out = restructure_config(text).unwrap();
        assert_eq!(
            keys_in_order(&out),
            vec!["update_channel", "search", "schedule"]
        );
        assert!(out.contains("- cron: 0 9 * * *\n  skill: /daily"));
        assert!(out.contains("  args:\n  - search\n  - reindex"));
        assert!(out.contains(&section_banner("Automation")));
    }

    #[test]
    fn restructure_is_idempotent() {
        let text = "\
stats:
  last_doctor_run: 2026-07-09
update_channel: stable
recap:
  min_sessions: 6
checkpoint:
  messages: 15
";
        let once = restructure_config(text).unwrap();
        let twice = restructure_config(&once).unwrap();
        assert_eq!(once, twice, "second restructure must be byte-identical");
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        let text =
            "update_channel: stable\r\nstats:\r\n  last_doctor_run: 2026-07-09\r\ncheckpoint:\r\n  messages: 15\r\n";
        let out = restructure_config(text).unwrap();
        assert!(out.contains("\r\n"), "CRLF must survive: {out:?}");
        assert!(!out.contains("\n\n\r\n"), "no stray LF-only lines");
        // Every line ends with CRLF (no bare LF introduced).
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert_eq!(
            keys_in_order(&out),
            vec!["update_channel", "checkpoint", "stats"]
        );
        // Idempotent under CRLF too.
        assert_eq!(restructure_config(&out).unwrap(), out);
    }

    #[test]
    fn non_mapping_and_empty_return_none() {
        assert!(restructure_config("- a\n- b\n").is_none());
        assert!(restructure_config("# only comments\n").is_none());
        assert!(restructure_config("").is_none());
    }

    #[test]
    fn flow_style_root_mapping_is_declined() {
        // `{a: 1}` IS a valid YAML mapping, so the as_mapping gate alone
        // would let it through — the explicit flow-style guard must decline
        // it (there are no block-form top-level key lines to move) and it
        // must never report drift.
        let flow = "{update_channel: stable, checkpoint: {messages: 15}}\n";
        assert!(restructure_config(flow).is_none());
        assert!(config_layout_matches(flow));
        // Leading whitespace before `{` still declines.
        assert!(restructure_config("  {a: 1}\n").is_none());
    }

    #[test]
    fn user_dash_art_comment_is_not_structural_and_survives() {
        // A user's own dash-art comment (single U+2500 — NOT the canonical
        // banner) must survive the restructure byte-for-byte; only banners
        // this module itself emits are structural.
        let text = "\
checkpoint:
  # ─ my custom divider ─
  messages: 15
update_channel: stable
";
        let out = restructure_config(text).unwrap();
        assert!(
            out.contains("  # ─ my custom divider ─\n  messages: 15"),
            "user dash-art comment must survive in place:\n{out}"
        );
        // A canonical file containing that comment reports NO drift.
        assert!(config_layout_matches(&out));
        assert_eq!(restructure_config(&out).unwrap(), out);
        // A banner-lookalike with a non-section title is a user comment too.
        let lookalike = format!("update_channel: stable # {}\n", "─".repeat(10));
        assert!(config_layout_matches(
            &restructure_config(&lookalike).unwrap()
        ));
    }

    #[test]
    fn eof_without_trailing_newline_is_preserved() {
        let text = "stats:\n  x: 1\nupdate_channel: stable"; // no final \n
        let out = restructure_config(text).unwrap();
        assert!(
            !out.ends_with('\n'),
            "missing final newline must be preserved: {out:?}"
        );
        assert_eq!(keys_in_order(&out), vec!["update_channel", "stats"]);
        // Idempotent in that shape too.
        assert_eq!(restructure_config(&out).unwrap(), out);
    }

    #[test]
    fn config_layout_matches_reports_drift() {
        let ordered = restructure_config("stats:\n  x: 1\nupdate_channel: stable\n").unwrap();
        assert!(config_layout_matches(&ordered));
        assert!(!config_layout_matches(
            "stats:\n  x: 1\nupdate_channel: stable\n"
        ));
        // A shape we decline to touch never reports drift.
        assert!(config_layout_matches("- a\n"));
    }

    #[test]
    fn preamble_header_is_preserved_above_first_banner() {
        let text = "\
# onebrain.yml — vault configuration.
# second header line

# Release channel · default: stable
update_channel: stable
checkpoint:
  messages: 15
";
        let out = restructure_config(text).unwrap();
        assert!(out.starts_with(
            "# onebrain.yml — vault configuration.\n# second header line\n\n# ── General "
        ));
        // The key doc-comment stays attached to its key.
        assert!(out.contains("# Release channel · default: stable\nupdate_channel: stable"));
    }
}
