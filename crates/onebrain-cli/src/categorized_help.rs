//! Root `--help` categorized command groups.
//!
//! clap v4 (derive) has no native subcommand-category headings. This module
//! implements the feature for the ROOT command only:
//!
//! 1. [`CATEGORIES`] defines the ordered category list and which subcommand
//!    names belong to each category.
//! 2. [`build_categorized_commands_block`] iterates the root [`clap::Command`]'s
//!    **visible** subcommands, groups them by category, and formats the
//!    multi-section text that replaces the flat `Commands:` block.
//! 3. [`print_root_help`] builds the patched [`clap::Command`] (root only —
//!    subcommand help is untouched), renders long help to stdout, and exits 0.
//!    It is called from `main` when argv signals a root-level help request.
//!
//! # Drift guard
//!
//! A unit test (`drift_guard_category_map_complete`) asserts that every visible
//! root subcommand is assigned to exactly one category, and every category-map
//! entry corresponds to a real visible subcommand. Adding a new visible command
//! without categorising it, or leaving a stale entry in the map, causes CI to
//! fail immediately.

use clap::Command;

/// One category: an emoji, a name, and the ordered subcommand names in it.
///
/// The `emoji` is rendered only when stdout is a terminal (see
/// [`build_categorized_commands_block`]'s `use_emoji` parameter) so piped /
/// redirected `--help` stays plain ASCII.
pub struct Category {
    pub emoji: &'static str,
    pub name: &'static str,
    pub commands: &'static [&'static str],
}

/// Ordered category list.  **Edit here to change category membership.**
///
/// - Order of categories = display order in `--help`.
/// - Order of commands within a category = display order within the section.
/// - Every *visible* root subcommand MUST appear here exactly once (the
///   drift-guard test enforces this).
pub const CATEGORIES: &[Category] = &[
    Category {
        emoji: "⚙️",
        name: "System Management",
        commands: &["init", "update", "doctor", "plugin", "qmd", "schedule"],
    },
    Category {
        emoji: "🧠",
        name: "Vault Management",
        commands: &["vault", "note", "task", "search"],
    },
    Category {
        emoji: "🔄",
        name: "Session Management",
        commands: &["session", "checkpoint"],
    },
    Category {
        emoji: "🚀",
        name: "Launch Management",
        commands: &["harness", "serve", "skill"],
    },
];

/// Build the multi-section `Commands:\n\n<Heading>\n  name   about\n…`
/// string that replaces the flat command list in root `--help`.
///
/// Descriptions are pulled live from the [`clap::Command`] objects so they
/// can never drift from the source-of-truth `about` annotations.
///
/// Only **visible** subcommands (i.e. `!cmd.is_hide_set()`) are included;
/// the category map already omits hidden commands, but the visibility filter
/// here is a belt-and-suspenders guard.
///
/// `use_emoji` controls whether each heading is prefixed with its category
/// emoji. Callers pass `false` when stdout is not a terminal so piped help
/// stays plain ASCII.
pub fn build_categorized_commands_block(root: &Command, use_emoji: bool) -> String {
    // Collect visible subcommands into a name → owned-about map.
    let subcmds: std::collections::HashMap<String, String> = root
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .filter_map(|c| {
            c.get_about()
                .map(|about| (c.get_name().to_owned(), about.to_string()))
        })
        .collect();

    // Determine alignment column: max length of all visible command names + 4.
    let max_name_len = subcmds.keys().map(|k| k.len()).max().unwrap_or(0);
    // Minimum 10 to match clap's default alignment feel.
    let col = max_name_len.max(10) + 4;

    let mut out = String::with_capacity(512);
    out.push_str("Commands:\n");

    for cat in CATEGORIES {
        // Skip categories whose every command is invisible (future-proof).
        let visible_in_cat: Vec<(&str, &str)> = cat
            .commands
            .iter()
            .filter_map(|&name| subcmds.get(name).map(|about| (name, about.as_str())))
            .collect();
        if visible_in_cat.is_empty() {
            continue;
        }

        // Category heading sits flush at the left margin (no indent); commands
        // are indented 2 spaces under it.
        out.push('\n');
        if use_emoji {
            out.push_str(cat.emoji);
            out.push_str("  ");
        }
        out.push_str(cat.name);
        out.push('\n');

        for (name, about) in &visible_in_cat {
            // Indent 2 spaces (inside category), then name left-padded to `col`.
            out.push_str("  ");
            out.push_str(name);
            let pad = col.saturating_sub(name.len());
            for _ in 0..pad {
                out.push(' ');
            }
            out.push_str(about);
            out.push('\n');
        }
    }

    out
}

/// Detect whether argv is a root-level help request (as opposed to a
/// subcommand-level help request like `onebrain note --help`).
///
/// Returns `true` when either:
/// - argv contains `--help` or `-h` and no subcommand token was found before
///   it (i.e. the help is for the root `onebrain` command), OR
/// - argv contains no subcommand at all (bare `onebrain` → clap prints root help).
///
/// Known global flags (`--vault`, `-o`, `--output`, `--json`, `--yaml`,
/// `--pretty`, `--no-color`, `-q`, `--quiet`, `--version`, `-V`) are skipped;
/// anything else is treated as a subcommand and returns `false`.
pub fn is_root_help_request(args: &[String]) -> bool {
    // Borrow the same logic from `banner::argv_requests_help` — the definition
    // of "root help request" is identical: exhausting all argv tokens without
    // finding a subcommand token.
    //
    // Skip binary name at index 0.
    let after_bin: &[String] = if args.is_empty() { &[] } else { &args[1..] };

    // Does argv even request help?
    let has_help_flag = after_bin.iter().any(|a| a == "--help" || a == "-h");

    // Walk argv looking for the first non-global-flag token.
    let mut iter = after_bin.iter();
    let mut found_subcmd = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--vault" | "-o" | "--output" => {
                let _ = iter.next(); // consume value
            }
            "--json" | "--yaml" | "--pretty" | "--no-color" | "-q" | "--quiet" | "--version"
            | "-V" | "--help" | "-h" => {} // global boolean flags
            _ => {
                found_subcmd = true;
                break;
            }
        }
    }

    // Root help if: no subcommand was found AND a help flag was present
    // (or argv is completely bare — no flags at all).
    //
    // Note: `--version` / `-V` are NOT help requests — they produce version
    // output via clap's normal parse path. A bare `onebrain` (no tokens)
    // triggers `arg_required_else_help` which clap handles, but we intercept
    // here so the root-level help includes the categorized block.
    !found_subcmd && (has_help_flag || after_bin.is_empty())
}

/// Root-help template: conventional layout — about, `Usage:`, then the
/// categorized command block (injected via `{before-help}`), then the Options
/// section. clap v4 has no `{options}`/`{subcommands}` tokens (only `{all-args}`),
/// so we hide every subcommand on the render clone — that makes `{all-args}`
/// render the Options section ALONE — and supply the categorized commands
/// ourselves through `before_help`. `{usage}` is overridden so it keeps
/// `<COMMAND>` despite the hidden subcommands.
const ROOT_HELP_TEMPLATE: &str = "\
{about-with-newline}{usage-heading} {usage}

{before-help}{all-args}{after-help}";

/// Print the categorized root `--help` and exit 0.
///
/// Called from `main` when [`is_root_help_request`] is true. Subcommand help
/// (`onebrain note --help`) never reaches this function — clap handles those
/// paths through its normal derive-driven flow, so their help is unchanged.
pub fn print_root_help() {
    use crate::cli::Cli;
    use clap::CommandFactory;
    use std::io::IsTerminal;

    let original = Cli::command();
    // Emoji headings only when stdout is a real terminal — piped / redirected
    // `--help` (e.g. `onebrain --help | cat`) stays plain ASCII.
    let use_emoji = std::io::stdout().is_terminal();
    let categorized = build_categorized_commands_block(&original, use_emoji);

    let mut render_cmd = original
        .clone()
        .help_template(ROOT_HELP_TEMPLATE)
        // Keep `<COMMAND>` in the usage line even though we hide subcommands
        // below (hiding them would otherwise collapse usage to `[OPTIONS]`).
        .override_usage("onebrain [OPTIONS] <COMMAND>")
        .before_help(categorized);

    // Hide all subcommands so `{all-args}` renders the Options section only
    // (clap has no standalone `{options}` token). Our categorized block above
    // is the sole Commands display.
    let subcmd_names: Vec<String> = render_cmd
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect();
    for name in &subcmd_names {
        render_cmd = render_cmd.mut_subcommand(name, |sub| sub.hide(true));
    }

    // `print_help` writes to stdout and flushes.  We call the SHORT-help
    // variant intentionally: clap's internal `--help` handler also resolves to
    // compact format because `long_help_exists()` is false on this command
    // (no arg has a `long_help` annotation), so `use_long` is coerced to
    // false even when the handler uses `print_long_help`.  Calling
    // `print_help()` here makes that coercion explicit and ensures the
    // Options: section renders in the standard compact row format (flag +
    // description on the same line with `[default: …]` wrapped) rather than
    // the one-flag-per-block long format that `print_long_help()` forces when
    // `use_long = true` regardless of `long_help_exists`.
    // Ignore a write failure (e.g. a closed pipe from `onebrain --help | head`):
    // the help request itself succeeded, so we still exit 0 rather than panic.
    let _ = render_cmd.print_help();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::CommandFactory;
    use std::collections::HashMap;
    use std::collections::HashSet;

    /// **Drift guard** — CI fails if any visible root subcommand is missing from
    /// the category map, or if the category map references a non-visible subcommand.
    ///
    /// Add a new visible root subcommand → add it to [`CATEGORIES`].
    /// Hide an existing command → remove it from [`CATEGORIES`] (or it stays as a
    /// no-op because the visibility filter would drop it, but the reverse check
    /// catches the stale entry).
    #[test]
    fn drift_guard_category_map_complete() {
        let root = Cli::command();

        // Collect all visible root subcommand names.
        let visible: HashSet<&str> = root
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name())
            .collect();

        // Collect all names in the category map.
        let categorized: HashSet<&str> = CATEGORIES
            .iter()
            .flat_map(|cat| cat.commands.iter().copied())
            .collect();

        // 1. Every visible subcommand must be in exactly one category.
        let mut missing_from_map: Vec<&str> = visible
            .iter()
            .copied()
            .filter(|name| !categorized.contains(name))
            .collect();
        missing_from_map.sort();
        assert!(
            missing_from_map.is_empty(),
            "Visible root subcommand(s) not in any category — add them to CATEGORIES in \
             categorized_help.rs: {missing_from_map:?}"
        );

        // 2. Every category entry must correspond to a real visible subcommand.
        let mut stale_in_map: Vec<&str> = categorized
            .iter()
            .copied()
            .filter(|name| !visible.contains(name))
            .collect();
        stale_in_map.sort();
        assert!(
            stale_in_map.is_empty(),
            "Category map entry/entries don't match any visible root subcommand — remove them \
             from CATEGORIES in categorized_help.rs: {stale_in_map:?}"
        );

        // 3. Each command appears in at most one category (no duplicates).
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for cat in CATEGORIES {
            for &name in cat.commands {
                if let Some(prev_cat) = seen.insert(name, cat.name) {
                    panic!(
                        "Command `{name}` appears in both `{prev_cat}` and `{}` — \
                         each command must be in exactly one category",
                        cat.name
                    );
                }
            }
        }
    }

    /// Assert the rendered root help block contains all four category names.
    #[test]
    fn rendered_block_contains_all_category_headings() {
        let root = Cli::command();
        let block = build_categorized_commands_block(&root, false);
        for cat in CATEGORIES {
            assert!(
                block.contains(cat.name),
                "category name `{}` missing from rendered block:\n{block}",
                cat.name
            );
        }
    }

    /// Emoji is rendered when `use_emoji` is true and omitted otherwise.
    #[test]
    fn emoji_is_tty_gated() {
        let root = Cli::command();
        let with = build_categorized_commands_block(&root, true);
        let without = build_categorized_commands_block(&root, false);
        for cat in CATEGORIES {
            assert!(
                with.contains(cat.emoji),
                "emoji `{}` missing when use_emoji=true",
                cat.emoji
            );
            assert!(
                !without.contains(cat.emoji),
                "emoji `{}` leaked into plain (use_emoji=false) output",
                cat.emoji
            );
        }
    }

    /// Spot-check category membership.
    #[test]
    fn spot_check_command_placement() {
        let root = Cli::command();
        let block = build_categorized_commands_block(&root, false);

        // qmd must appear under "System Management".
        let system_pos = block
            .find("System Management")
            .expect("System Management heading present");
        let vault_pos = block
            .find("Vault Management")
            .expect("Vault Management heading present");
        let qmd_pos = block.find("\n  qmd").expect("`qmd` entry present");
        assert!(
            qmd_pos > system_pos && qmd_pos < vault_pos,
            "`qmd` must appear between `System Management` and `Vault Management`; \
             positions: system={system_pos} qmd={qmd_pos} vault={vault_pos}"
        );

        // task must appear under "Vault Management".
        let session_pos = block
            .find("Session Management")
            .expect("Session Management heading present");
        let task_pos = block.find("\n  task").expect("`task` entry present");
        assert!(
            task_pos > vault_pos && task_pos < session_pos,
            "`task` must appear between `Vault Management` and `Session Management`; \
             positions: vault={vault_pos} task={task_pos} session={session_pos}"
        );

        // skill must appear under "Launch Management" (last category).
        let launch_pos = block
            .find("Launch Management")
            .expect("Launch Management heading present");
        let skill_pos = block.find("\n  skill").expect("`skill` entry present");
        assert!(
            skill_pos > launch_pos,
            "`skill` must appear under `Launch Management`; \
             positions: launch={launch_pos} skill={skill_pos}"
        );
    }

    /// The `about` text in the block must be pulled from clap, not hardcoded.
    /// Verify one entry's description matches the actual `about` on the Command.
    #[test]
    fn about_text_is_pulled_from_clap_not_hardcoded() {
        let root = Cli::command();
        let block = build_categorized_commands_block(&root, false);

        // Pull the real `qmd` about from clap.
        let qmd_about = root
            .get_subcommands()
            .find(|c| c.get_name() == "qmd")
            .and_then(|c| c.get_about())
            .map(|s| s.to_string())
            .expect("qmd subcommand with about exists");

        assert!(
            block.contains(&qmd_about),
            "qmd about text `{qmd_about}` not found in block; \
             about must be pulled from clap, not hardcoded"
        );
    }

    // ── is_root_help_request ────────────────────────────────────────────────

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn root_help_with_explicit_flag() {
        assert!(is_root_help_request(&args(&["onebrain", "--help"])));
        assert!(is_root_help_request(&args(&["onebrain", "-h"])));
    }

    #[test]
    fn bare_invocation_is_root_help() {
        assert!(is_root_help_request(&args(&["onebrain"])));
    }

    #[test]
    fn subcommand_help_is_not_root_help() {
        assert!(!is_root_help_request(&args(&[
            "onebrain", "note", "--help"
        ])));
        assert!(!is_root_help_request(&args(&["onebrain", "task", "-h"])));
        assert!(!is_root_help_request(&args(&[
            "onebrain", "plugin", "--help"
        ])));
    }

    #[test]
    fn version_flag_is_not_root_help() {
        // --version / -V produce version output via clap's normal parse path;
        // they must NOT be treated as help requests.
        assert!(!is_root_help_request(&args(&["onebrain", "--version"])));
        assert!(!is_root_help_request(&args(&["onebrain", "-V"])));
    }

    #[test]
    fn global_flags_before_help_dont_count_as_subcommand() {
        assert!(is_root_help_request(&args(&[
            "onebrain", "--json", "--help"
        ])));
        assert!(is_root_help_request(&args(&[
            "onebrain", "--vault", "/tmp/v", "--help"
        ])));
    }
}
