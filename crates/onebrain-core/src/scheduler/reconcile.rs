//! Ownership-based reconciliation of installed scheduler artifacts (#410, #352).
//!
//! Labels (`com.onebrain.<label>`) are a GLOBAL namespace with no vault
//! component, so "not in onebrain.yml" is not evidence that an artifact is
//! ours to delete — the v3.4.21 ledger deleted another vault's live job on
//! exactly that inference and was reverted. Everything here decides from the
//! ARTIFACT: each renderer writes `ONEBRAIN_VAULT=<vault>` into what it
//! installs, the parsers below read it (or the legacy `--vault` argv) back,
//! and [`plan_reconcile`] only ever proposes deleting an artifact whose owner
//! is provably the current vault. A parse that proves nothing yields
//! [`Ownership::Unknown`], and Unknown is never deleted.

use crate::scheduler::xml::unescape as xml_unescape;
use std::path::{Component, Path, PathBuf};

/// Lexical path normalization (no symlink resolution, no disk touch):
/// drops `.` components and pops on `..`. Equivalent to Node's
/// `path.resolve` after the base is applied. Moved here from
/// `register_schedule.rs` so the planner and the CLI agree on one rule.
pub fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The environment variable every renderer writes into its artifact. The CLI
/// already honours it (`--vault` help: "beats ONEBRAIN_VAULT"), so it is a
/// real setting for onebrain commands and an inert one for foreign binaries.
pub const VAULT_ENV_KEY: &str = "ONEBRAIN_VAULT";

/// Task Scheduler XML has no environment element, so the Windows marker is
/// the `<RegistrationInfo><Description>` text with this prefix.
pub const TASK_DESCRIPTION_PREFIX: &str = "OneBrain scheduled entry for vault ";

/// Who installed an artifact, as far as the artifact itself says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// The artifact names the vault that installed it.
    Vault(PathBuf),
    /// No marker and no `--vault` argv — a legacy foreign-command artifact,
    /// a hand-edited file, or garbage. Never deleted.
    Unknown,
}

/// One installed artifact: its label (the `<label>` in `com.onebrain.<label>`)
/// and what it says about its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledArtifact {
    pub label: String,
    pub owner: Ownership,
}

/// What `register` should do about artifacts that are not in the config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// Owned by this vault, not in the config → remove.
    pub prune: Vec<String>,
    /// No ownership evidence → report, keep.
    pub unknown: Vec<String>,
    /// Owned by another vault → report, keep.
    pub foreign: Vec<(String, PathBuf)>,
}

/// Classify every installed artifact whose label is absent from
/// `current_labels`. Labels that ARE current are never classified — the
/// install loop overwrote them, and a cross-vault collision on a current
/// label is the pre-existing global-namespace limitation, not this
/// function's business. Output is sorted by label so reports are stable.
pub fn plan_reconcile(
    installed: &[InstalledArtifact],
    current_labels: &[String],
    vault: &Path,
) -> ReconcilePlan {
    let vault = normalize_path(vault);
    let mut plan = ReconcilePlan::default();
    for artifact in installed {
        if current_labels.iter().any(|l| l == &artifact.label) {
            continue;
        }
        match &artifact.owner {
            Ownership::Vault(p) if normalize_path(p) == vault => {
                plan.prune.push(artifact.label.clone())
            }
            Ownership::Vault(p) => plan.foreign.push((artifact.label.clone(), p.clone())),
            Ownership::Unknown => plan.unknown.push(artifact.label.clone()),
        }
    }
    plan.prune.sort();
    plan.unknown.sort();
    plan.foreign.sort();
    plan
}

// ── binary identity (shared by every legacy `--vault` argv fallback) ───────

/// The last path component of `argv0`, splitting on both `/` and `\` so it
/// works for POSIX paths (launchd/systemd artifacts) and Windows paths (Task
/// Scheduler `<Command>`) regardless of which OS is running this code —
/// `std::path::Path` alone would miss backslash separators on a non-Windows
/// host.
fn argv0_basename(argv0: &str) -> &str {
    argv0.rsplit(['/', '\\']).next().unwrap_or(argv0)
}

/// Whether `argv0` is the onebrain binary: basename `onebrain`, or on
/// Windows `onebrain.exe`. Case-sensitive — every renderer this CLI writes
/// spells its own binary name consistently, so case-folding would only widen
/// what a foreign binary could spoof.
///
/// This gates every legacy `--vault` argv fallback below (#410 review round
/// 1): a foreign scheduled job that happens to accept its own `--vault` flag
/// is NOT evidence that the path named there is a vault this CLI installed
/// for — only onebrain's own skill-mode and command-mode invocations ever
/// meant `--vault` that way, and only those two shapes ever got the fallback
/// written by a renderer in the first place.
fn is_onebrain_binary(argv0: &str) -> bool {
    let base = argv0_basename(argv0);
    base == "onebrain" || base.strip_suffix(".exe") == Some("onebrain")
}

// ── launchd ────────────────────────────────────────────────────────────────

/// Text between the first `<string>` and `</string>` at or after `from`.
fn next_plist_string(text: &str, from: usize) -> Option<&str> {
    let rest = &text[from..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(&rest[start..end])
}

/// The `<string>` that follows an exact `<key>{key}</key>` element — but
/// ONLY when the value is immediately (past whitespace) a `<string>`.
///
/// This is an allowlist, not a denylist: an earlier version rejected a
/// handful of named non-string types it happened to think of (`<key>`,
/// `<integer>`, `<dict>`, `<array>`) and let anything else — `<true/>`,
/// `<false/>`, `<real>`, `<data>`, `<date>` — walk past the key's real value
/// to grab an unrelated LATER `<string>` (#410 review round 1). Requiring
/// the very next element to be `<string>` is exhaustive by construction.
fn plist_string_after_key(text: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let at = text.find(&needle)? + needle.len();
    let after = &text[at..];
    let trimmed = after.trim_start();
    if !trimmed.starts_with("<string>") {
        return None;
    }
    let start = at + (after.len() - trimmed.len()) + "<string>".len();
    let end = text[start..].find("</string>")? + start;
    Some(xml_unescape(&text[start..end]))
}

/// The `<string>` that follows an exact `<string>{value}</string>` element.
fn plist_string_after_string(text: &str, value: &str) -> Option<String> {
    let needle = format!("<string>{value}</string>");
    let at = text.find(&needle)? + needle.len();
    next_plist_string(text, at).map(xml_unescape)
}

/// argv[0] of a plist: the first `<string>` inside the `<array>` that
/// follows `<key>ProgramArguments</key>`.
fn plist_program_arguments_argv0(text: &str) -> Option<String> {
    let needle = "<key>ProgramArguments</key>";
    let at = text.find(needle)? + needle.len();
    next_plist_string(text, at).map(xml_unescape)
}

/// Owner of a launchd plist: the `ONEBRAIN_VAULT` marker, else — ONLY when
/// argv[0] is the onebrain binary itself — the value after a standalone
/// `--vault` argv element (pre-marker skill-mode and onebrain command-mode
/// artifacts), else Unknown. The argv[0] gate (#410 review round 1) matters
/// because a foreign scheduled job that happens to accept its own `--vault`
/// flag is not evidence it was installed for that vault. Tolerant scanning
/// of our own byte-pinned template — no plist parser is a dependency here.
pub fn owner_from_plist(text: &str) -> Ownership {
    if let Some(v) = plist_string_after_key(text, VAULT_ENV_KEY) {
        return Ownership::Vault(PathBuf::from(v));
    }
    let is_onebrain =
        plist_program_arguments_argv0(text).is_some_and(|argv0| is_onebrain_binary(&argv0));
    if is_onebrain {
        if let Some(v) = plist_string_after_string(text, "--vault") {
            return Ownership::Vault(PathBuf::from(v));
        }
    }
    Ownership::Unknown
}

// ── systemd ────────────────────────────────────────────────────────────────

/// `Environment="KEY=value"` for a `[Service]` section. Always quoted.
/// Inside the quotes systemd unescapes `\\` and `\"`, and expands `%`
/// specifiers — but NOT `$` variables (that is `ExecStart`'s behaviour, hence
/// the difference from `systemd::quote_arg`).
pub fn quote_env_assignment(key: &str, value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("Environment=\"{key}={escaped}\"")
}

/// Inverse of the quoting in [`quote_env_assignment`].
fn unquote_env_value(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => out.push(n),
                None => out.push('\\'),
            },
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                out.push('%');
            }
            other => out.push(other),
        }
    }
    out
}

/// Split an `ExecStart=` value into argv the way systemd's word splitter
/// does for our own output: whitespace-separated, double quotes group a
/// word, `\\`/`\"` unescape, `$$`→`$`, `%%`→`%`. Best-effort — it only has
/// to recover `--vault <path>` from units this CLI wrote.
fn split_execstart(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_word = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_word = true;
            }
            '\\' if in_quotes => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                cur.push('$');
            }
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                cur.push('%');
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            other => {
                cur.push(other);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(cur);
    }
    words
}

/// Owner of a systemd `.service` unit: the `Environment="ONEBRAIN_VAULT=…"`
/// line, else — ONLY when argv[0] of `ExecStart=` is the onebrain binary
/// itself — `--vault <path>` in `ExecStart=`, else Unknown. The argv[0] gate
/// (#410 review round 1) matters because a foreign command that happens to
/// accept its own `--vault` flag is not evidence it was installed for that
/// vault.
pub fn owner_from_service_unit(text: &str) -> Ownership {
    let marker_prefix = format!("Environment=\"{VAULT_ENV_KEY}=");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&marker_prefix) {
            if let Some(escaped) = rest.strip_suffix('"') {
                return Ownership::Vault(PathBuf::from(unquote_env_value(escaped)));
            }
        }
    }
    for line in text.lines() {
        if let Some(exec) = line.strip_prefix("ExecStart=") {
            let argv = split_execstart(exec);
            if !argv.first().is_some_and(|a| is_onebrain_binary(a)) {
                continue;
            }
            if let Some(i) = argv.iter().position(|a| a == "--vault") {
                if let Some(path) = argv.get(i + 1) {
                    return Ownership::Vault(PathBuf::from(path));
                }
            }
        }
    }
    Ownership::Unknown
}

// ── Task Scheduler ─────────────────────────────────────────────────────────

fn xml_element_text<'a>(text: &'a str, element: &str) -> Option<&'a str> {
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

/// Owner of a Task Scheduler task from its `/Query /XML` document: the
/// `<Description>` marker, else — ONLY when `<Command>` is the onebrain
/// binary itself — `--vault "path"` / `--vault path` in `<Arguments>`, else
/// Unknown. The `<Command>` gate (#410 review round 1) matters because a
/// foreign command that happens to accept its own `--vault` flag is not
/// evidence it was installed for that vault.
pub fn owner_from_task_xml(text: &str) -> Ownership {
    if let Some(desc) = xml_element_text(text, "Description") {
        if let Some(path) = xml_unescape(desc).strip_prefix(TASK_DESCRIPTION_PREFIX) {
            if !path.is_empty() {
                return Ownership::Vault(PathBuf::from(path));
            }
        }
    }
    let is_onebrain =
        xml_element_text(text, "Command").is_some_and(|c| is_onebrain_binary(&xml_unescape(c)));
    if is_onebrain {
        if let Some(args) = xml_element_text(text, "Arguments") {
            let args = xml_unescape(args);
            // `--vault "C:\My Vault\ob"` (quote_win_arg output for a spaced
            // path) or `--vault C:\ob` (unquoted). Backslash-doubling before
            // an inner quote is not reversed — a path containing `"` is not
            // a real case.
            let re = regex::Regex::new(r#"--vault (?:"([^"]+)"|(\S+))"#).expect("static regex");
            if let Some(caps) = re.captures(&args) {
                let path = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
                if let Some(path) = path {
                    return Ownership::Vault(PathBuf::from(path));
                }
            }
        }
    }
    Ownership::Unknown
}

/// Task names under the `\OneBrain\` folder from `schtasks /Query /FO CSV
/// /NH` output. Each row is `"TaskName","Next Run Time","Status"`; only the
/// first field matters. Anything that is not such a row (the "no scheduled
/// tasks" INFO line, blank lines) is skipped.
pub fn task_names_from_csv(csv: &str) -> Vec<String> {
    csv.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix('"')?;
            let end = rest.find('"')?;
            let name = &rest[..end];
            name.starts_with("\\OneBrain\\").then(|| name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_strips_curdir_and_pops_parentdir() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn normalize_path_parent_dir_pops_prefix() {
        assert_eq!(normalize_path(Path::new("/a/b/..")), PathBuf::from("/a"));
    }

    #[test]
    fn normalize_path_plain_absolute_unchanged() {
        assert_eq!(normalize_path(Path::new("/x/y")), PathBuf::from("/x/y"));
    }

    fn art(label: &str, owner: Ownership) -> InstalledArtifact {
        InstalledArtifact {
            label: label.to_string(),
            owner,
        }
    }
    fn owned(p: &str) -> Ownership {
        Ownership::Vault(PathBuf::from(p))
    }

    #[test]
    fn plan_prunes_only_owned_artifacts_absent_from_config() {
        let installed = [
            art("daily", owned("/v/ob-1")),    // current → untouched
            art("digest", owned("/v/ob-1")),   // orphan, ours → prune
            art("weekly", owned("/v/other")),  // orphan, theirs → foreign
            art("legacy", Ownership::Unknown), // orphan, no evidence → unknown
        ];
        let plan = plan_reconcile(&installed, &["daily".to_string()], Path::new("/v/ob-1"));
        assert_eq!(plan.prune, vec!["digest".to_string()]);
        assert_eq!(plan.unknown, vec!["legacy".to_string()]);
        assert_eq!(
            plan.foreign,
            vec![("weekly".to_string(), PathBuf::from("/v/other"))]
        );
    }

    #[test]
    fn a_current_label_is_never_planned_even_when_another_vault_owns_it() {
        // The global-namespace collision: our config has `daily`, and the
        // artifact on disk says another vault installed it. Register will
        // overwrite it (pre-existing behaviour); the PLANNER must not
        // classify it as foreign, or a later step could report or act on it.
        let installed = [art("daily", owned("/v/other"))];
        let plan = plan_reconcile(&installed, &["daily".to_string()], Path::new("/v/ob-1"));
        assert_eq!(plan, ReconcilePlan::default());
    }

    #[test]
    fn ownership_compares_normalized_paths_not_strings() {
        let installed = [art("digest", owned("/v/./x/../ob-1/"))];
        let plan = plan_reconcile(&installed, &[], Path::new("/v/ob-1"));
        assert_eq!(plan.prune, vec!["digest".to_string()]);
    }

    #[test]
    fn empty_config_prunes_every_owned_artifact_and_nothing_else() {
        let installed = [
            art("b", owned("/v/ob-1")),
            art("a", owned("/v/ob-1")),
            art("z", owned("/v/other")),
        ];
        let plan = plan_reconcile(&installed, &[], Path::new("/v/ob-1"));
        assert_eq!(plan.prune, vec!["a".to_string(), "b".to_string()], "sorted");
        assert_eq!(plan.foreign.len(), 1);
    }

    const PLIST_WITH_MARKER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n    <key>Label</key>\n    <string>com.onebrain.backup</string>\n    <key>ProgramArguments</key>\n    <array>\n        <string>/usr/bin/rsync</string>\n        <string>-av</string>\n    </array>\n    <key>EnvironmentVariables</key>\n    <dict>\n        <key>ONEBRAIN_VAULT</key>\n        <string>/Users/u/My &amp; Vault</string>\n    </dict>\n    <key>RunAtLoad</key>\n    <false/>\n</dict>\n</plist>";

    const PLIST_LEGACY_VAULT_ARGV: &str = "<plist version=\"1.0\">\n<dict>\n    <key>ProgramArguments</key>\n    <array>\n        <string>/opt/homebrew/bin/onebrain</string>\n        <string>skill</string>\n        <string>run</string>\n        <string>--vault</string>\n        <string>/Users/u/ob-1</string>\n        <string>--skill</string>\n        <string>/daily</string>\n    </array>\n</dict>\n</plist>";

    #[test]
    fn plist_marker_wins_and_is_xml_unescaped() {
        assert_eq!(
            owner_from_plist(PLIST_WITH_MARKER),
            Ownership::Vault(PathBuf::from("/Users/u/My & Vault"))
        );
    }

    #[test]
    fn plist_without_marker_falls_back_to_vault_argv() {
        assert_eq!(
            owner_from_plist(PLIST_LEGACY_VAULT_ARGV),
            Ownership::Vault(PathBuf::from("/Users/u/ob-1"))
        );
    }

    #[test]
    fn plist_with_neither_is_unknown() {
        assert_eq!(
            owner_from_plist("<!-- stale pre-#116 plist -->"),
            Ownership::Unknown
        );
        assert_eq!(owner_from_plist(""), Ownership::Unknown);
        // `--vault` inside a longer string (a one-shot `/bin/sh -c` wrapper)
        // is not its own element and proves nothing.
        let wrapper = "<array>\n        <string>/bin/sh</string>\n        <string>-c</string>\n        <string>onebrain skill run --vault /x --skill /y</string>\n    </array>";
        assert_eq!(owner_from_plist(wrapper), Ownership::Unknown);
    }

    #[test]
    fn plist_marker_key_without_a_following_string_is_unknown() {
        let broken = "<key>ONEBRAIN_VAULT</key>\n    <integer>1</integer>";
        assert_eq!(owner_from_plist(broken), Ownership::Unknown);
    }

    #[test]
    fn service_unit_marker_line_is_unquoted() {
        let unit = "[Unit]\nDescription=x\n\n[Service]\nType=oneshot\nExecStart=/usr/bin/rsync -av /a /b\nEnvironment=\"ONEBRAIN_VAULT=/home/u/my \\\"vault\\\" 100%%\"\n";
        assert_eq!(
            owner_from_service_unit(unit),
            Ownership::Vault(PathBuf::from("/home/u/my \"vault\" 100%"))
        );
    }

    #[test]
    fn service_unit_without_marker_falls_back_to_execstart_vault_token() {
        let unit = "[Service]\nType=oneshot\nExecStart=/usr/local/bin/onebrain skill run --vault \"/home/u/my vault\" --skill /daily\n";
        assert_eq!(
            owner_from_service_unit(unit),
            Ownership::Vault(PathBuf::from("/home/u/my vault"))
        );
        let bare =
            "[Service]\nExecStart=/usr/local/bin/onebrain search reindex --vault /home/u/ob-1\n";
        assert_eq!(
            owner_from_service_unit(bare),
            Ownership::Vault(PathBuf::from("/home/u/ob-1"))
        );
    }

    #[test]
    fn service_unit_with_neither_is_unknown() {
        assert_eq!(
            owner_from_service_unit("[Service]\nExecStart=/usr/bin/rsync -av /a /b\n"),
            Ownership::Unknown
        );
        assert_eq!(owner_from_service_unit(""), Ownership::Unknown);
    }

    #[test]
    fn task_xml_description_marker_is_unescaped() {
        let xml = "<Task>\n  <RegistrationInfo><Description>OneBrain scheduled entry for vault C:\\My &amp; Vault\\ob</Description></RegistrationInfo>\n</Task>";
        assert_eq!(
            owner_from_task_xml(xml),
            Ownership::Vault(PathBuf::from("C:\\My & Vault\\ob"))
        );
    }

    #[test]
    fn task_xml_without_marker_falls_back_to_vault_in_arguments() {
        let quoted = "<Actions><Exec><Command>C:\\bin\\onebrain.exe</Command><Arguments>skill run --vault &quot;C:\\My Vault\\ob&quot; --skill /daily</Arguments></Exec></Actions>";
        assert_eq!(
            owner_from_task_xml(quoted),
            Ownership::Vault(PathBuf::from("C:\\My Vault\\ob"))
        );
        // Unquoted `--vault` value, still gated on `<Command>` being onebrain.
        let bare = "<Actions><Exec><Command>C:\\bin\\onebrain.exe</Command><Arguments>search reindex --vault C:\\ob</Arguments></Exec></Actions>";
        assert_eq!(
            owner_from_task_xml(bare),
            Ownership::Vault(PathBuf::from("C:\\ob"))
        );
    }

    #[test]
    fn plist_vault_argv_from_non_onebrain_binary_is_unknown() {
        let plist = "<plist version=\"1.0\">\n<dict>\n    <key>ProgramArguments</key>\n    <array>\n        <string>/usr/local/bin/backup-tool</string>\n        <string>--vault</string>\n        <string>/x</string>\n    </array>\n</dict>\n</plist>";
        assert_eq!(owner_from_plist(plist), Ownership::Unknown);
    }

    #[test]
    fn service_unit_vault_token_from_non_onebrain_binary_is_unknown() {
        let unit = "[Service]\nExecStart=/usr/local/bin/backup-tool --vault /x\n";
        assert_eq!(owner_from_service_unit(unit), Ownership::Unknown);
    }

    #[test]
    fn task_xml_vault_arg_from_non_onebrain_command_is_unknown() {
        let xml = "<Actions><Exec><Command>C:\\tools\\backup.exe</Command><Arguments>--vault C:\\x</Arguments></Exec></Actions>";
        assert_eq!(owner_from_task_xml(xml), Ownership::Unknown);
    }

    #[test]
    fn plist_marker_key_followed_by_boolean_then_unrelated_string_is_unknown() {
        // An allowlist (only `<string>` counts) rather than a denylist of
        // named non-string types is what makes this exhaustive — see the
        // doc comment on `plist_string_after_key`.
        let text = "<key>ONEBRAIN_VAULT</key>\n    <true/>\n    <key>Other</key>\n    <string>/not/this</string>";
        assert_eq!(owner_from_plist(text), Ownership::Unknown);
    }

    #[test]
    fn task_xml_old_generic_description_is_unknown() {
        let xml = "<RegistrationInfo><Description>OneBrain scheduled entry</Description></RegistrationInfo><Arguments>-av /a /b</Arguments>";
        assert_eq!(owner_from_task_xml(xml), Ownership::Unknown);
    }

    #[test]
    fn task_names_from_csv_keeps_only_the_onebrain_folder() {
        let csv = "\"\\OneBrain\\daily\",\"9/4/2026 9:00:00 AM\",\"Ready\"\r\n\"\\Microsoft\\Windows\\Defrag\\ScheduledDefrag\",\"N/A\",\"Ready\"\r\n\"\\OneBrain\\onebrain-search-reindex\",\"N/A\",\"Disabled\"\r\n\r\n";
        assert_eq!(
            task_names_from_csv(csv),
            vec![
                "\\OneBrain\\daily".to_string(),
                "\\OneBrain\\onebrain-search-reindex".to_string()
            ]
        );
        assert!(task_names_from_csv("").is_empty());
        assert!(task_names_from_csv(
            "INFO: There are no scheduled tasks presently available at your access level."
        )
        .is_empty());
    }

    #[test]
    fn quote_env_assignment_escapes_what_systemd_expands_inside_quotes() {
        assert_eq!(
            quote_env_assignment("ONEBRAIN_VAULT", "/home/u/plain"),
            "Environment=\"ONEBRAIN_VAULT=/home/u/plain\""
        );
        // `%` is a specifier in Environment=; `$` is NOT expanded there, so it
        // must stay single (unlike quote_arg for ExecStart).
        assert_eq!(
            quote_env_assignment("K", "a \"b\" \\ 100% $HOME"),
            "Environment=\"K=a \\\"b\\\" \\\\ 100%% $HOME\""
        );
    }

    #[test]
    fn env_assignment_round_trips_through_the_service_parser() {
        for value in [
            "/home/u/ob-1",
            "/home/u/my \"vault\"",
            "/tmp/100%",
            "/a\\b",
            "$HOME/v",
        ] {
            let unit = format!(
                "[Service]\n{}\n",
                quote_env_assignment("ONEBRAIN_VAULT", value)
            );
            assert_eq!(
                owner_from_service_unit(&unit),
                Ownership::Vault(PathBuf::from(value)),
                "{value}"
            );
        }
    }
}
