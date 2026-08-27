//! Clap subcommand tree — 3 root verbs + 26 resource groups + hidden v3.0
//! aliases. Locked at v3.1 per [[cli/specs/01-architecture §2.4]]; `search`
//! (v3.4.0) is the first post-lock addition — a new native-search command
//! surface, not a v3.1 tree-shape change. `mcp` (v3.4.1) promotes the MCP
//! stdio server from `search mcp` to a top-level command — it hosts search
//! tools today, with more vault tool groups mounting on the same command
//! later. `token` (v3.4.10) is the second post-lock addition.
//!
//! Every group's verb list is captured as a `Subcommand` enum even when the
//! body is `unimplemented!()` — the tree shape itself is the v3.1 deliverable
//! (locks the public command surface for v3.2+).

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "onebrain",
    version,
    disable_help_subcommand = true,
    propagate_version = true,
    term_width = 200
)]
pub struct Cli {
    // `vault` doc is a single line so `--help` (long) renders identically to
    // `-h` (short) — clap's default expand-paragraphs-on-long behaviour would
    // otherwise diverge the two help screens. On `init`, this flag is the
    // target directory for the NEW vault (defaults to cwd) and walk-up
    // discovery is skipped — init creates a vault, doesn't consume one.
    /// Override vault root (highest priority · beats ONEBRAIN_VAULT and walk-up). Global: accepted pre- or post-subcommand.
    #[arg(long, global = true, value_name = "PATH")]
    pub vault: Option<PathBuf>,

    #[arg(
        short = 'o',
        long,
        global = true,
        default_value = "text",
        value_parser = ["text", "json", "yaml"],
        value_name = "FMT",
        hide_default_value = true,
        hide_possible_values = true,
        help = "Output format. Default `text` is TTY-friendly; pipe-detected calls drop color/pretty automatically\n[default: text, possible values: text, json, yaml]"
    )]
    pub output: String,

    /// Shorthand for `--output json`.
    #[arg(long, global = true, conflicts_with_all = ["output", "yaml"])]
    pub json: bool,

    /// Shorthand for `--output yaml`.
    #[arg(long, global = true, conflicts_with_all = ["output", "json"])]
    pub yaml: bool,

    /// Force pretty-print even when stdout is piped (text mode).
    #[arg(long, global = true)]
    pub pretty: bool,

    /// Force monochrome output (also honoured via NO_COLOR env var).
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Suppress info-level logs · errors still emit on stderr.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    // ───── Root verbs (3 · frozen) ─────────────────────────────────────
    // `display_order` clusters commands by domain in `--help`:
    //   1-3  · system / lifecycle (init, update, doctor)
    //   10-13 · vault & session ops (vault, session, checkpoint, harness)
    //   14-18 · content & serving (serve, note, task, search, mcp)
    //   20-23 · config & maintenance (plugin, schedule, config, skill)
    //   99   · clap-managed `help` meta
    //
    /// Initialize a new vault (interactive setup).
    #[command(display_order = 1)]
    Init(InitArgs),
    /// Self-update the CLI binary (auto-detects install channel).
    #[command(display_order = 2)]
    Update(UpdateArgs),
    /// Diagnose system (vault + plugin + CLI · includes harness).
    #[command(display_order = 3)]
    Doctor(DoctorArgs),
    /// Generate a shell completion script (hidden · used by the Homebrew
    /// formula and the post-init hint). Writes to stdout.
    #[command(hide = true)]
    Completions(CompletionsArgs),
    /// Internal cross-harness hook bridge. Kept in the installed CLI so an
    /// active task does not depend on files inside a replaceable plugin cache.
    #[command(hide = true)]
    Hook,

    // ───── Resource groups (13 · alphabetical) ─────────────────────────
    // v3.4.24 (#334): the 12 groups whose every verb returned
    // `E_NOT_IMPLEMENTED` were REMOVED from the parser — avatar, bookmark,
    // bundle, config, date, dream, frontmatter, gateway, inbox, log, memory,
    // pause. `hide = true` kept them out of `--help`, but the parser still
    // accepted them, so anything that discovers verbs by trying them (a
    // script, a doc, a user) got exit 72 for a surface that did not exist.
    // They now fail as unknown commands, indistinguishable from a typo.
    //
    // This deliberately overrides the v3.1 "the tree shape IS the deliverable"
    // position (spec §2.4) and ADR 0006; both are marked superseded.
    //
    // `hide = true` remains for groups that are hidden but REAL (daemon), and
    // for the hidden v3.0 aliases further down.
    #[command(display_order = 12)]
    Checkpoint(CheckpointCmd),
    #[command(hide = true)]
    Daemon(DaemonCmd),
    #[command(display_order = 13)]
    Harness(HarnessCmd),
    /// Serve OneBrain over MCP (stdio) — search tools today, more vault tool
    /// groups to come.
    #[command(display_order = 18)]
    Mcp,
    #[command(display_order = 15)]
    Note(NoteCmd),
    #[command(display_order = 20)]
    Plugin(PluginCmd),
    #[command(display_order = 21)]
    Schedule(ScheduleCmd),
    #[command(display_order = 17)]
    Search(SearchCmd),
    /// Serve the local web UI + vault JSON API (foreground · Ctrl-C to stop).
    #[command(display_order = 14)]
    Serve(ServeArgs),
    #[command(display_order = 11)]
    Session(SessionCmd),
    #[command(display_order = 23)]
    Skill(SkillCmd),
    #[command(display_order = 16)]
    Task(TaskCmd),
    /// Token-optimization telemetry — `gain` (savings), `check` (read-hook
    /// gate), `discover` (field-test measurement).
    #[command(display_order = 22)]
    Token(TokenCmd),
    #[command(display_order = 10)]
    Vault(VaultCmd),

    // ───── Hidden v3.0 aliases (back-compat · removed v3.5) ────────────
    #[command(hide = true, name = "session-init")]
    SessionInitAlias(LegacySessionInitArgs),
    #[command(hide = true, name = "orphan-scan")]
    OrphanScanAlias(LegacyOrphanScanArgs),
    #[command(hide = true, name = "qmd-reindex")]
    QmdReindexAlias,
    #[command(hide = true, name = "register-hooks")]
    RegisterHooksAlias(LegacyRegisterHooksArgs),
    #[command(hide = true, name = "register-schedule")]
    RegisterScheduleAlias(LegacyRegisterScheduleArgs),
    #[command(hide = true, name = "migrate")]
    MigrateAlias(LegacyMigrateArgs),
    #[command(hide = true, name = "vault-sync")]
    VaultSyncAlias(LegacyVaultSyncArgs),
    #[command(hide = true, name = "run-skill")]
    RunSkillAlias(LegacyRunSkillArgs),

    /// Removed in v3.4.5 — use `onebrain search …`. Hidden catch-all so the
    /// removal emits a helpful migration error instead of clap's bare
    /// "unrecognized subcommand".
    #[command(hide = true, disable_help_flag = true)]
    Qmd {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        rest: Vec<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// Root verb args
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Skip prompts · install the Essentials schedule preset (CI-friendly).
    #[arg(long)]
    pub yes: bool,
    /// Overwrite an existing onebrain.yml without prompting.
    #[arg(long)]
    pub force: bool,
    /// Skip the embedded vault-sync step (scaffold only).
    #[arg(long = "no-sync")]
    pub no_sync: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    /// Dry run · report what would change without installing.
    #[arg(long)]
    pub check: bool,
    /// Skip the 1-hour release-info cache.
    #[arg(long)]
    pub fresh: bool,
    /// Emit a single JSON document with version delta info.
    #[arg(long)]
    pub json: bool,
    /// Emit the richer "plan" JSON (implies `--check`).
    #[arg(long, conflicts_with = "check")]
    pub plan: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    /// Attempt auto-repair recipes for any warnings, then re-run the checks.
    #[arg(long)]
    pub fix: bool,
    /// Skip the confirmation prompt before `--fix` applies changes (for scripts).
    #[arg(long)]
    pub yes: bool,
    /// Emit the report as a single JSON document.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CompletionsArgs {
    /// Target shell (bash · zsh · fish · powershell · elvish).
    pub shell: clap_complete::Shell,
}
// ─────────────────────────────────────────────────────────────────────────
// checkpoint (3 verbs · stop/reset wired to legacy, orphans wired to legacy)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Auto-save management (stop · reset · orphans)",
    disable_help_subcommand = true
)]
pub struct CheckpointCmd {
    #[command(subcommand)]
    pub verb: CheckpointVerb,
}
#[derive(Subcommand, Debug)]
pub enum CheckpointVerb {
    /// Auto-save checkpoint metadata · used by Claude Code's Stop hook.
    Stop {
        /// Vault root override.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
    },
    /// Reset the checkpoint cadence counter · used by /wrapup skill.
    Reset {
        /// Vault root override.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Reset the cadence state of an ALREADY-RESOLVED session token instead
        /// of re-deriving one from the environment. `onebrain hook` counts
        /// cadence under the token it derives from the harness session id
        /// (`ONEBRAIN_HOOK_SESSION_ID`), which the agent shell running
        /// `/wrapup` does not have — without this flag the reset lands on a
        /// different token's state file and the counter survives the wrapup.
        /// Takes the resolved token verbatim (never re-hashed).
        #[arg(long, value_name = "TOKEN", hide = true)]
        session_token: Option<String>,
    },
    /// Find orphan checkpoints needing /wrapup synthesis · used by SessionStart hook.
    Orphans {
        /// 07-logs/ folder path inside the vault.
        logs_folder: String,
        /// Current session token.
        session_token: String,
    },
}
// ─────────────────────────────────────────────────────────────────────────
// daemon (forward-compat)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(disable_help_subcommand = true)]
pub struct DaemonCmd {
    #[command(subcommand)]
    pub verb: DaemonVerb,
}
#[derive(Subcommand, Debug)]
pub enum DaemonVerb {
    /// Start the OneBrain daemon as a detached background process.
    Start {
        /// Vault the daemon should bind. Takes precedence over `$ONEBRAIN_VAULT`
        /// (which stays a back-compat fallback). Threaded to the detached
        /// `__run` child so callers convey the vault explicitly instead of
        /// mutating the process environment.
        #[arg(long)]
        vault: Option<PathBuf>,
    },
    /// Stop a per-vault daemon (SIGTERM + slot-file cleanup). Defaults to the
    /// cwd-resolved vault's slot; `--vault` targets a specific vault, `--all`
    /// stops every running daemon.
    Stop {
        /// Vault whose daemon to stop. Default: the cwd-resolved vault's slot
        /// (the vault-less daemon when cwd isn't inside a vault).
        #[arg(long)]
        vault: Option<PathBuf>,
        /// Stop EVERY running daemon on the machine (all per-vault slots).
        #[arg(long)]
        all: bool,
    },
    /// Report every running daemon (one per per-vault slot): PID, port, vault,
    /// and version.
    Status,
    /// Internal: the detached daemon body. Spawned by `daemon start`; not for
    /// direct use. Runs the HTTP surface until SIGTERM.
    #[command(name = "__run", hide = true)]
    Run {
        /// Vault to bind (passed by `daemon start`). Precedence over
        /// `$ONEBRAIN_VAULT`.
        #[arg(long)]
        vault: Option<PathBuf>,
    },
}
// ─────────────────────────────────────────────────────────────────────────
// harness (1-verb · documented exception · wired to legacy)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Detect or run an AI harness (claude / gemini / codex)",
    subcommand_required = true,
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct HarnessCmd {
    #[command(subcommand)]
    pub verb: HarnessVerb,
}
#[derive(Subcommand, Debug)]
pub enum HarnessVerb {
    /// Detect the active harness (Claude Code / Gemini / Codex / direct).
    Detect,
    /// Run a prompt through Claude, Gemini, or Codex headlessly. Omit <PROMPT> to read from stdin (`cat note.md | …`).
    Run {
        /// Vault root override · also accepts global `--vault`, and walks up from cwd when omitted.
        /// Ignored when `--mode ad-hoc`.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// The prompt to send. If omitted, the prompt is read from stdin.
        prompt: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value_t = HarnessMode::WithContext,
            hide_default_value = true,
            hide_possible_values = true,
            help = "Inject the vault's OneBrain context before answering\n[default: with-context, possible values: with-context, ad-hoc]"
        )]
        mode: HarnessMode,
        #[arg(
            long,
            value_enum,
            default_value_t = HarnessArg::Claude,
            hide_default_value = true,
            hide_possible_values = true,
            help = "AI runtime to run the prompt through\n[default: claude, possible values: claude, gemini, codex]"
        )]
        harness: HarnessArg,
        /// Model passed through to the harness (`claude --model <m>`,
        /// `gemini -m <m>`, or `codex exec --model <m>`).
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
    },
}

/// Whether `onebrain harness run` loads OneBrain's vault context before
/// invoking the harness, or runs the prompt ad-hoc with no project context.
///
/// `WithContext` (default) passes the harness-specific vault flag and sets
/// `cwd = <vault>` so the harness loads OneBrain's project instructions;
/// requires a vault (exit 78 if missing).
///
/// `AdHoc` skips the context-dir flag and forces `cwd = $TMPDIR` so claude /
/// gemini can't auto-walk-up from a vault subdir and silently re-load
/// OneBrain's `CLAUDE.md`. The harness answers the raw prompt with no
/// project context, regardless of where the user invoked from. User-level
/// config (`~/.claude/CLAUDE.md`) still loads — that's separate.
///
/// Variant doc comments are intentionally one-line so `harness run --help`
/// stays compact (matches `skill run --help`'s density). The longer
/// description lives here for rustdoc + future devs reading the source.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[value(rename_all = "kebab-case")]
pub enum HarnessMode {
    // Variant docs intentionally omitted so clap doesn't render a multi-line
    // `Possible values:` block under `--mode <MODE>` — that block was the
    // last thing forcing `harness run --help` into clap's "long" format. The
    // semantics now live in (a) the help-string wrap on the `mode` arg above
    // and (b) the enum-level rustdoc on `HarnessMode` for source readers.
    #[default]
    WithContext,
    AdHoc,
}
// ─────────────────────────────────────────────────────────────────────────
// note (14 verbs · 11 locked 2026-05-25 ship v3.2.0 · edit/delete/mkdir added 2026-06-25)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Vault note operations (search · read · edit · move · archive · …)",
    disable_help_subcommand = true
)]
pub struct NoteCmd {
    #[command(subcommand)]
    pub verb: NoteVerb,
}
#[derive(Subcommand, Debug)]
pub enum NoteVerb {
    /// Search note contents by substring (default) or regex.
    Search(NoteSearchArgs),
    /// List notes with metadata, sorted by name/mtime/created.
    List(NoteListArgs),
    /// Find files/folders by glob, optionally filtered by mtime.
    Find(NoteFindArgs),
    /// Read a note's contents: whole body, a section, frontmatter, or tasks.
    Read(NoteReadArgs),
    /// Append content to a note (at EOF, or under a `--section` heading).
    Append(NoteAppendArgs),
    /// Create a new note, optionally from a template, with inline frontmatter.
    New(NoteNewArgs),
    /// Move/rename a note and rewrite every incoming wikilink.
    Move(NoteMoveArgs),
    /// Archive a note into the dated archive bucket (`<root>/YYYY/MM/<file>`).
    Archive(NoteArchiveArgs),
    /// Write content verbatim to a note (create or overwrite).
    Edit(NoteEditArgs),
    /// Delete a note (moves it to `.trash/`).
    Delete(NoteDeleteArgs),
    /// Create a folder (and any missing parents).
    Mkdir(NoteMkdirArgs),
    /// List every note that links to the target note.
    Backlinks(NoteBacklinksArgs),
    /// List orphan notes — notes with zero incoming wikilinks.
    Orphans(NoteOrphansArgs),
    /// Print note statistics (line/word/char counts, headings, links, tasks).
    Stat(NoteStatArgs),
}

#[derive(Args, Debug)]
pub struct NoteSearchArgs {
    /// Pattern to match (literal substring by default · regex with `--mode regex`).
    pub pattern: String,
    /// Scope the search to a subfolder (relative to the vault root).
    #[arg(long)]
    pub folder: Option<PathBuf>,
    /// Maximum matches to return.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Match mode: `lex` (literal substring) or `regex` (Rust regex).
    #[arg(long, default_value = "lex", value_parser = ["lex", "regex"])]
    pub mode: String,
}

#[derive(Args, Debug)]
pub struct NoteListArgs {
    /// Scope the listing to a subfolder (relative to the vault root).
    #[arg(long)]
    pub folder: Option<PathBuf>,
    /// Maximum notes to return.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Sort order.
    #[arg(long, default_value = "mtime", value_parser = ["name", "mtime", "created"])]
    pub sort: String,
}

#[derive(Args, Debug)]
pub struct NoteFindArgs {
    /// Glob pattern. No `/` → matches the basename (like `find -name`);
    /// containing `/` → matches the vault-relative path (e.g. `**/topic-*.md`).
    pub glob: String,
    /// Restrict to files or folders.
    #[arg(long = "type", value_parser = ["file", "folder"])]
    pub r#type: Option<String>,
    /// Day offset: `-N` = modified in the last N days · `0` = today · `+N` = older than N days.
    #[arg(long)]
    pub mtime: Option<i64>,
    /// Maximum results to return.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct NoteReadArgs {
    /// Note path, relative to the vault root.
    pub path: PathBuf,
    /// Extract only the content under this heading (matched by heading text).
    #[arg(long, conflicts_with_all = ["frontmatter_only", "tasks_only"])]
    pub section: Option<String>,
    /// Emit only the parsed YAML frontmatter.
    #[arg(long, conflicts_with_all = ["section", "tasks_only"])]
    pub frontmatter_only: bool,
    /// Emit only task lines (`- [ ]` / `- [x]`).
    #[arg(long, conflicts_with_all = ["section", "frontmatter_only"])]
    pub tasks_only: bool,
    /// Max lines when reading the body (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct NoteAppendArgs {
    /// Note path, relative to the vault root. The note must already exist
    /// (use `note new` to create one).
    pub path: PathBuf,
    /// Text to append, verbatim. No de-duplication is performed.
    /// `allow_hyphen_values` so Markdown list/task lines (`- [ ] …`, `- item`)
    /// parse as the content rather than being mistaken for a flag.
    #[arg(allow_hyphen_values = true)]
    pub content: String,
    /// Append under this heading instead of at EOF. If the heading is absent,
    /// it is created as a level-2 heading (`## H`) at the end of the file.
    #[arg(long)]
    pub section: Option<String>,
}

#[derive(Args, Debug)]
pub struct NoteNewArgs {
    /// New note path, relative to the vault root (e.g. `03-knowledge/ml/New Topic.md`).
    pub path: PathBuf,
    /// Template name. Resolves `.claude/plugins/onebrain/templates/<NAME>.md` and
    /// substitutes `{{date}}`, `{{title}}`, `{{slug}}`.
    #[arg(long)]
    pub template: Option<String>,
    /// Inline frontmatter pairs, `key=value`, comma-separated (e.g. `tags=ai,status=draft`).
    #[arg(long, value_delimiter = ',')]
    pub frontmatter: Vec<String>,
    /// Overwrite the note if it already exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct NoteMoveArgs {
    /// Source note path, relative to the vault root.
    pub from: PathBuf,
    /// Destination note path, relative to the vault root.
    pub to: PathBuf,
    /// Skip the wikilink rewrite — just move the file.
    #[arg(long)]
    pub no_link_update: bool,
    /// Compute and print the plan as structured data; write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct NoteArchiveArgs {
    /// Note to archive, relative to the vault root.
    pub path: PathBuf,
    /// Archive root, relative to the vault root. Destination is
    /// `<archive-root>/YYYY/MM/<filename>` (current UTC date).
    #[arg(long, default_value = "06-archive")]
    pub archive_root: PathBuf,
}

#[derive(Args, Debug)]
pub struct NoteStatArgs {
    /// Note path, relative to the vault root.
    pub path: PathBuf,
}

#[derive(Args, Debug)]
pub struct NoteBacklinksArgs {
    /// Target note path, relative to the vault root.
    pub path: PathBuf,
    /// Also scan notes under `06-archive` (excluded by default).
    #[arg(long)]
    pub include_archive: bool,
}

#[derive(Args, Debug)]
pub struct NoteOrphansArgs {
    /// Scope the scan to a subfolder (relative to the vault root).
    #[arg(long)]
    pub folder: Option<PathBuf>,
    /// Maximum orphans to return.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct NoteEditArgs {
    /// Note path, relative to the vault root.
    pub path: PathBuf,
    /// New content to write verbatim. The note is created if it does not exist.
    /// `allow_hyphen_values` so Markdown list/task lines (`- [ ] …`, `- item`)
    /// parse as content rather than being mistaken for a flag.
    #[arg(allow_hyphen_values = true)]
    pub content: String,
}

#[derive(Args, Debug)]
pub struct NoteDeleteArgs {
    /// Note path, relative to the vault root (moved to `.trash/`, not removed).
    pub path: PathBuf,
}

#[derive(Args, Debug)]
pub struct NoteMkdirArgs {
    /// Folder path, relative to the vault root. Parent directories are created
    /// as needed. Errors if the path already exists.
    pub path: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────────
// search (6 verbs · v3.4.0 · native search engine, replaces external qmd)
// `mcp` (the search engine's stdio server) moved to a top-level `Cmd::Mcp`
// in v3.4.1 — see `commands::mcp`.
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Native vault search over `*.md` notes (hybrid query · lex · vector · reindex · …)",
    help_template = SEARCH_HELP_TEMPLATE,
    disable_help_subcommand = true
)]
pub struct SearchCmd {
    #[command(subcommand)]
    pub verb: SearchVerb,
}

/// `search --help` layout: the shared query-verb flags sit right below the
/// Commands list (before Options) so they're seen next to the verbs they
/// belong to; the `.md`-only indexing scope note closes the help.
const SEARCH_HELP_TEMPLATE: &str = "\
{about-with-newline}
Only Markdown (`*.md`) files are indexed; other file types in the vault are never touched.

{usage-heading} {usage}

Commands:
{subcommands}

Search flags (query · search · vsearch):
  --top-k <N>        Maximum hits to return (default 10)
  --min-score <S>    Drop low-confidence hits (vsearch: cosine, ≈0.85+ is confident; search: BM25; query: RRF)

Options:
{options}";

#[derive(Subcommand, Debug)]
pub enum SearchVerb {
    /// Hybrid search (lex + vector, RRF-fused).
    Query(SearchQueryArgs),
    /// Lexical (BM25) search only — never triggers a model download.
    Search(SearchQueryArgs),
    /// Semantic (vector) search only.
    Vsearch(SearchQueryArgs),
    /// Fetch a doc's full indexed text.
    Get(SearchGetArgs),
    /// Report index status (collection, embed model, cache dir, index size) —
    /// never triggers a model download.
    Status,
    /// Reindex the vault's `*.md` notes (whole vault, or specific doc paths).
    Reindex(SearchReindexArgs),
    /// Manage the embedding model (list supported models · switch model).
    Model(SearchModelCmd),
}

#[derive(Args, Debug)]
#[command(disable_help_subcommand = true)]
pub struct SearchModelCmd {
    /// Omitted (bare `search model`) → interactive picker on a TTY, or a
    /// non-hanging informational fallback otherwise (see
    /// `commands::search_model::run_bare`).
    #[command(subcommand)]
    pub verb: Option<SearchModelVerb>,
}
#[derive(Subcommand, Debug)]
pub enum SearchModelVerb {
    /// List supported embedding models with download/disk status — never
    /// opens the engine or downloads anything.
    List(SearchModelListArgs),
    /// Switch the vault's embedding model, persist it to `onebrain.yml`,
    /// and re-embed the index (downloads the new model if not cached).
    Set(SearchModelSetArgs),
    /// Remove a downloaded model's cached files from disk. Refuses to touch
    /// the active model without `--force` (or a TTY confirm).
    Remove(SearchModelRemoveArgs),
}

#[derive(Args, Debug)]
pub struct SearchModelSetArgs {
    /// Model name (see `search model list`).
    pub name: String,
}

/// Column to sort `search model list` rows by.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum ModelSortCol {
    /// Model name (alphabetical).
    Name,
    /// Registry approx download size.
    Size,
    /// Embedding dimensionality.
    Dim,
    /// Thai MIRACL-th score (models without a score sort last).
    Thai,
    /// On-disk size of the downloaded model (not-downloaded sorts last).
    Disk,
    /// Download status (downloaded models first; ties break by name).
    Downloaded,
}

#[derive(Args, Debug)]
pub struct SearchModelListArgs {
    /// Sort rows by a column (default: registry order).
    #[arg(long = "sort", value_enum)]
    pub sort: Option<ModelSortCol>,
    /// Sort descending (only meaningful with `--sort`).
    #[arg(long = "desc", default_value_t = false)]
    pub desc: bool,
}

impl SearchModelListArgs {
    /// Default list args (registry order, ascending) — used by the bare
    /// `search model` non-TTY / structured fallback, which renders the same
    /// static table as an explicit `search model list` with no flags.
    pub fn bare() -> Self {
        Self {
            sort: None,
            desc: false,
        }
    }
}

#[derive(Args, Debug)]
pub struct SearchModelRemoveArgs {
    /// Model name (see `search model list`).
    pub name: String,
    /// Remove without confirmation, even for the active model.
    #[arg(long = "force", default_value_t = false)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct SearchQueryArgs {
    /// Query text.
    pub text: String,
    /// Maximum hits to return.
    #[arg(long = "top-k", default_value_t = 10)]
    pub top_k: usize,
    /// Drop hits scoring below this value. When the Tier-2 reranker is active
    /// (`query`/`vsearch` with the model available), this filters the
    /// calibrated 0–1 `rerank_score` — i.e. a confidence threshold (0.30 is the
    /// default gate; higher = stricter). When reranking is off (reranker
    /// disabled, model not downloaded, or the pure-lex `search` verb), it
    /// filters the raw retrieval score instead — cosine for `vsearch`, BM25 for
    /// `search`, RRF-fused rank for `query`.
    #[arg(long = "min-score")]
    pub min_score: Option<f64>,
    /// Override `search.reranker.min_candidates` for this query: the minimum
    /// pool of top fused results fed to the Tier-2 reranker. Acts as a FLOOR
    /// — the reranked pool is actually `max(min_candidates, top_k)`, so every
    /// returned hit is always reranked regardless of this value. Omit to use
    /// the vault's configured value. Applies to `query` and `vsearch`; has no
    /// effect on the pure-lex `search` verb (never reranked).
    #[arg(long = "min-candidates")]
    pub min_candidates: Option<usize>,
    /// Token-optimization level override for this call
    /// (`off|conservative|balanced|aggressive`). Only shapes structured
    /// (`--output json`/`yaml`) output; human TTY output is never altered.
    /// Precedence: this flag > `onebrain.yml token_optimization.level` > default.
    #[arg(long = "opt-level")]
    pub opt_level: Option<String>,
}

#[derive(Args, Debug)]
pub struct SearchGetArgs {
    /// Doc path, relative to the vault root (as indexed).
    pub doc_path: String,
    /// Re-materialize the FULL body, bypassing the already-sent ledger and the
    /// size cap (design §3c) — this is the command a reference envelope's
    /// `rematerialize` field points back to.
    #[arg(long)]
    pub force: bool,
    /// Token-optimization level override for this call
    /// (`off|conservative|balanced|aggressive`). Only shapes structured
    /// (`--output json`/`yaml`) output; human TTY output is never altered.
    #[arg(long = "opt-level")]
    pub opt_level: Option<String>,
}

#[derive(Args, Debug)]
pub struct SearchReindexArgs {
    /// Specific doc paths to reindex (relative to the vault root). Omit to
    /// reindex the whole vault.
    pub paths: Vec<String>,
    /// Wipe the whole index first (lex + vectors + metadata; downloaded
    /// models are kept) and rebuild from scratch — needed after changes
    /// that invalidate stored vectors. Whole-vault only.
    #[arg(long, default_value_t = false)]
    pub force: bool,
    /// Incremental lex/keyword pass only: never loads or downloads the
    /// embedding model. Changed docs' vectors stay pending until the next
    /// embed pass (`--pending-only`). Safe to call from a hook — it never
    /// prompts and never fails the calling turn (errors degrade to a skip
    /// envelope, exit 0).
    #[arg(
        long = "lex-only",
        default_value_t = false,
        conflicts_with_all = ["pending_only", "force", "paths"]
    )]
    pub lex_only: bool,
    /// Embed only docs whose vectors are pending (from a previous
    /// `--lex-only` pass, or external edits). Loads the model only when
    /// there is pending work. Safe to call from a hook — it never prompts
    /// and never fails the calling turn (errors degrade to a skip envelope,
    /// exit 0).
    #[arg(
        long = "pending-only",
        default_value_t = false,
        conflicts_with_all = ["force", "paths"]
    )]
    pub pending_only: bool,
}
// ─────────────────────────────────────────────────────────────────────────
// plugin (install/migrate hidden · update/uninstall/status/verify visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Plugin lifecycle + hook rewriter",
    disable_help_subcommand = true
)]
pub struct PluginCmd {
    #[command(subcommand)]
    pub verb: PluginVerb,
}
#[derive(Subcommand, Debug)]
pub enum PluginVerb {
    /// Install plugin into the current vault · called by `init` and `plugin update`.
    Install {
        /// Optional vault root override.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Override branch (defaults to onebrain.yml `update_channel`).
        #[arg(long)]
        branch: Option<String>,
        /// Harness to install the plugin for.
        #[arg(long, value_enum, default_value_t = HarnessArg::Claude)]
        harness: HarnessArg,
        /// Report actions without changing harness-global state.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove an explicitly managed harness plugin installation.
    Uninstall {
        /// Harness whose managed plugin should be removed.
        #[arg(long, value_enum, default_value_t = HarnessArg::Claude)]
        harness: HarnessArg,
        /// Report actions without changing harness-global state.
        #[arg(long)]
        dry_run: bool,
    },
    /// Pull plugin from GitHub · rewrite hooks · rebind OS scheduler artifacts.
    Update {
        /// Optional vault root override.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Override branch (defaults to onebrain.yml `update_channel`).
        #[arg(long)]
        branch: Option<String>,
        /// Show what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a one-shot vault migration · called by `plugin update`.
    Migrate {
        name: String,
        cutoff_date: Option<String>,
        #[arg(long, conflicts_with = "cutoff_date")]
        cutoff: Option<String>,
        /// Vault root override · also accepts global `--vault`; walks up from cwd when omitted.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// schedule (register hidden · others visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "OS schedule management (launchd · Task Scheduler · systemd user timers)",
    disable_help_subcommand = true
)]
pub struct ScheduleCmd {
    #[command(subcommand)]
    pub verb: ScheduleVerb,
}
#[derive(Subcommand, Debug)]
pub enum ScheduleVerb {
    /// Show scheduled entries from `onebrain.yml` (or legacy `vault.yml`) with cron/at expression and installed status.
    List,
    Register {
        /// Vault root override · also accepts global `--vault`; walks up from cwd when omitted.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Print the scheduler artifacts that would be written, without touching disk or the OS scheduler.
        #[arg(long)]
        dry_run: bool,
        /// Deactivate and remove the scheduler artifacts for entries currently in onebrain.yml.
        #[arg(long)]
        remove: bool,
        /// Re-emit scheduler artifacts with the current vault path (logs a notice).
        #[arg(long)]
        refresh: bool,
        /// Clear the .paused marker for the given skill.
        #[arg(long)]
        resume: Option<String>,
        /// Print a status report.
        #[arg(long)]
        status: bool,
        /// Fire a scheduled skill once for testing.
        #[arg(long)]
        test: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// serve (v3.3 step 2 — foreground ephemeral HTTP surface)
// ─────────────────────────────────────────────────────────────────────────

/// `onebrain serve` is a flag-based FOREGROUND command (not a verb group): it
/// brings up one local HTTP listener that serves a static web dist (SPA) + the
/// read-only vault JSON API, then blocks until Ctrl-C. The pre-v3.3
/// `start/stop/status` verb stub (`ServeVerb`) was a placeholder; persistent
/// lifecycle lives under `onebrain daemon` instead.
#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Static web dist to serve as an SPA — overrides the web UI embedded in the
    /// binary (for web-UI development). Omit to serve the embedded UI, or a
    /// placeholder page if this binary was built without one.
    #[arg(long, value_name = "PATH")]
    pub dir: Option<PathBuf>,
    /// Bind port (default 6789).
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
    /// Open the served URL in the default browser after binding.
    #[arg(long)]
    pub open: bool,
    /// Vault root override · also accepts global `--vault`; walks up from cwd when omitted.
    /// Field is named `vault_dir` (not `vault`) to avoid colliding with the
    /// global `--vault` arg ID — the collision would otherwise make clap reject
    /// `onebrain serve --vault PATH` (same regression fixed for `skill run` in
    /// v3.2.3). With this name the global `--vault` propagates here normally.
    #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
    pub vault_dir: Option<PathBuf>,
}

// ─────────────────────────────────────────────────────────────────────────
// session (init hidden, others visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Session lifecycle (init)", disable_help_subcommand = true)]
pub struct SessionCmd {
    #[command(subcommand)]
    pub verb: SessionVerb,
}
#[derive(Subcommand, Debug)]
pub enum SessionVerb {
    /// Print session metadata as JSON (called by harness SessionStart hooks).
    Init {
        /// Vault root directory · defaults to auto-detect from cwd.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Preserve a hook-derived session token while collecting other startup metadata.
        #[arg(long, value_name = "TOKEN", hide = true)]
        session_token: Option<String>,
    },
    /// Resolve-only session token — no vault resolution, no
    /// `clean_stale_state_file` cleanup, no side effects at all. For
    /// mid-session token recovery: a caller that already has a live Stop-hook
    /// cadence counter running under a token and just needs to re-learn what
    /// that token is, without `session init`'s state-file cleanup silently
    /// wiping the counter it's trying to recover.
    #[command(hide = true)]
    Token {
        /// Preserve a hook-derived session token instead of re-resolving one.
        #[arg(long, value_name = "TOKEN", hide = true)]
        session_token: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// skill (run/show/info visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(disable_help_subcommand = true, about = "Skill invocation")]
pub struct SkillCmd {
    #[command(subcommand)]
    pub verb: SkillVerb,
}
/// Which AI runtime `skill run` dispatches through. Maps to the
/// `onebrain_core::Harness` runtime identifier (`Direct` is not a `skill run`
/// target — a skill needs an agent).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[value(rename_all = "lowercase")]
pub enum HarnessArg {
    #[default]
    Claude,
    Gemini,
    Codex,
}

impl HarnessArg {
    /// Lowercase binary/label name — also the binary invoked.
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessArg::Claude => "claude",
            HarnessArg::Gemini => "gemini",
            HarnessArg::Codex => "codex",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum SkillVerb {
    // v3.2.15: same about/long_about split as `HarnessVerb::Run` — flag
    // breakdown only in the parent-level `skill --help` Commands: listing
    // (where it's the only summary the user sees of `run`); the verb-level
    // `skill run --help` uses the short prose since the Options: section
    // already lists each flag with its `[default]` + `[possible values]`.
    /// Run a OneBrain skill (`/onebrain:<name>`) in headless mode. Replaces v3.0 `run-skill`.
    Run {
        /// Vault root override · also accepts global `--vault`, and walks up from cwd when omitted.
        #[arg(long = "vault-dir", value_name = "PATH", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Skill name (with or without slash prefix). Positional form: `onebrain skill run daily`.
        name: Option<String>,
        /// Skill name as a flag — `--skill /daily` — for parity with the scheduler's `run-skill` form. Equivalent to the positional `<NAME>`.
        #[arg(long = "skill", value_name = "NAME", conflicts_with = "name")]
        skill: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value_t = HarnessArg::Claude,
            hide_default_value = true,
            hide_possible_values = true,
            help = "AI runtime to run the skill through\n[default: claude, possible values: claude, gemini, codex]"
        )]
        harness: HarnessArg,
        /// Model passed through to the harness. Omit to use the harness default. A faster model speeds up headless runs.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Pass-through arguments (`--arg key=value`).
        #[arg(long = "arg")]
        args: Vec<String>,
    },
    /// Print a skill's SKILL.md body (workflow markdown) · convenience for
    /// skill scripting. Use `onebrain skill run --help` for CLI usage.
    Show {
        /// Skill name (with or without slash prefix · e.g. `daily` or `/daily`).
        name: String,
    },
    /// Print skill metadata (frontmatter) · `--json` for structured output.
    Info {
        /// Skill name (with or without slash prefix · e.g. `daily` or `/daily`).
        name: String,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// task
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "List dated vault tasks (fence-aware)",
    disable_help_subcommand = true
)]
pub struct TaskCmd {
    #[command(subcommand)]
    pub verb: TaskVerb,
}
#[derive(Subcommand, Debug)]
pub enum TaskVerb {
    /// List dated tasks across the vault (fence-aware), filterable by due date.
    List(TaskListArgs),
}

#[derive(Args, Debug)]
pub struct TaskListArgs {
    /// Keep only tasks due on or before this date. Accepts `today` or `YYYY-MM-DD`.
    #[arg(long = "due-by", value_name = "DATE")]
    pub due_by: Option<String>,
    /// Folder prefix to scan (repeatable). Default: projects + areas + inbox from config.
    #[arg(long = "folder", value_name = "PATH")]
    pub folder: Vec<String>,
    /// Include done (`- [x]`) tasks. Default returns open tasks only.
    #[arg(long)]
    pub all: bool,
    /// Return at most this many tasks after filtering and deterministic sorting.
    #[arg(
        long,
        value_name = "N",
        value_parser = parse_positive_usize
    )]
    pub limit: Option<usize>,
}

fn parse_positive_usize(raw: &str) -> Result<usize, String> {
    let value = raw
        .parse::<usize>()
        .map_err(|_| format!("expected a positive integer, got `{raw}`"))?;
    if value == 0 {
        return Err("expected a positive integer, got `0`".into());
    }
    Ok(value)
}

// ─────────────────────────────────────────────────────────────────────────
// token (v3.4.10 · gain · check · discover)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Token-optimization telemetry (savings, pivots, epoch resets)",
    disable_help_subcommand = true
)]
pub struct TokenCmd {
    #[command(subcommand)]
    pub verb: TokenVerb,
}

#[derive(Subcommand, Debug)]
pub enum TokenVerb {
    /// Report token-optimization savings — summary, `--by` pivot, or `--history`.
    Gain(TokenGainArgs),
    /// Gate a repeat vault-doc read for the PreToolUse read-hook (design §5b).
    ///
    /// Exit 0 = allow (stdout empty). Exit 2 = deny — the already-sent
    /// reference envelope JSON is on stdout. Fails open (exit 0) on any
    /// error, timeout, missing daemon, or unresolvable session — a read is
    /// NEVER blocked by infrastructure trouble.
    Check(TokenCheckArgs),
    /// Scan Claude Code session transcripts for direct `Read`/`Grep` calls on
    /// vault docs that bypassed the already-sent ledger — the read-hook
    /// field-test measurement instrument (design §5c).
    Discover(TokenDiscoverArgs),
}

#[derive(Args, Debug)]
pub struct TokenGainArgs {
    /// Pivot axes as "<time>[,<dim>]" or "<dim>" — time: day|week|month|year;
    /// dim: surface|transform|level|cache. Either axis alone is valid; omit
    /// entirely for a single grand-total summary.
    #[arg(long = "by", value_name = "AXES")]
    pub by: Option<String>,
    /// Report all-time cumulative traffic across every epoch (including
    /// archived pre-`--reset` windows). Without this, and without `--since`,
    /// the default report is scoped to the current epoch — traffic since the
    /// last `--reset` (or all-time when no reset has happened).
    #[arg(long = "all-time", default_value_t = false)]
    pub all_time: bool,
    /// Inclusive lower bound on the window, YYYY-MM-DD. Queries all epochs
    /// (like `--all-time`) filtered to on-or-after this date; invalid or
    /// non-zero-padded dates error (exit 70).
    // Validated post-parse (`commands::token_gain::validate_since`), not via
    // clap's own `value_parser`: a clap-native parse rejection always exits 2
    // (main.rs calls `err.exit()` before the dispatcher runs), while #287
    // requires exit 70 / E_INVALID_DATE — which only the dispatcher's
    // CoreError chain-walk can produce.
    #[arg(long = "since", value_name = "DATE")]
    pub since: Option<String>,
    /// Show the recent per-call raw log (tails the JSONL) instead of a
    /// summary/pivot — the only mode that reads the raw log directly.
    #[arg(long, default_value_t = false)]
    pub history: bool,
    /// Emit the full pivot structure as JSON. Shorthand for `--output json`;
    /// still renders through the canonical envelope dispatcher.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Archive the current window to `token/gain/archive/<ts>-<label>/`
    /// (never deletes) and start counting fresh. Pair with `--label`.
    #[arg(long, default_value_t = false)]
    pub reset: bool,
    /// Label for the archived epoch. Requires `--reset`.
    #[arg(long, requires = "reset")]
    pub label: Option<String>,
    /// Rebuild the rollup tables from the raw JSONL log (recovery / drift fix).
    #[arg(long, default_value_t = false)]
    pub rebuild: bool,
}

#[derive(Args, Debug)]
pub struct TokenCheckArgs {
    /// Doc path the hook is about to let `Read` touch (vault-relative or
    /// absolute, under the vault).
    pub path: String,
}

#[derive(Args, Debug)]
pub struct TokenDiscoverArgs {
    /// Emit the full result as JSON. Still renders through the canonical
    /// envelope dispatcher.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Only scan transcript files modified within the last N days.
    #[arg(long = "since-days", value_name = "N")]
    pub since_days: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────
// vault (scan/stats/verify/current visible · sync hidden)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(
    about = "Vault operations (sync · current)",
    disable_help_subcommand = true
)]
pub struct VaultCmd {
    #[command(subcommand)]
    pub verb: VaultVerb,
}
#[derive(Subcommand, Debug)]
pub enum VaultVerb {
    /// Pull plugin tarball into the current vault · called by `plugin update`.
    Sync {
        /// Optional positional vault root · defaults to walk-up from cwd.
        vault_root: Option<PathBuf>,
        /// Vault root override · flag-form.
        #[arg(long = "vault-dir", conflicts_with = "vault_root", hide = true)]
        vault_dir: Option<PathBuf>,
        /// Override branch resolved from onebrain.yml::update_channel.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Print active vault + resolution source (new in v3.1).
    Current,
}

// ─────────────────────────────────────────────────────────────────────────
// Legacy v3.0 alias args — kept verbatim for back-compat
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone)]
pub struct LegacySessionInitArgs {
    #[arg(long = "vault-dir", hide = true)]
    pub vault_dir: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyOrphanScanArgs {
    pub logs_folder: String,
    pub session_token: String,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyRegisterHooksArgs {
    /// Same surface as v3.0: `--vault` is the long; `--vault-dir` is a
    /// visible alias for parity with sibling commands.
    #[arg(long, alias = "vault-dir")]
    pub vault: Option<PathBuf>,
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub remove: bool,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyRegisterScheduleArgs {
    #[arg(long, alias = "vault-dir")]
    pub vault: Option<PathBuf>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub remove: bool,
    #[arg(long)]
    pub refresh: bool,
    #[arg(long)]
    pub resume: Option<String>,
    #[arg(long)]
    pub status: bool,
    #[arg(long)]
    pub test: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyMigrateArgs {
    pub name: String,
    pub cutoff_date: Option<String>,
    #[arg(long, conflicts_with = "cutoff_date")]
    pub cutoff: Option<String>,
    #[arg(long, alias = "vault-dir")]
    pub vault: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyVaultSyncArgs {
    pub vault_root: Option<PathBuf>,
    #[arg(long = "vault-dir", conflicts_with = "vault_root", hide = true)]
    pub vault_dir: Option<PathBuf>,
    #[arg(long)]
    pub branch: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyRunSkillArgs {
    /// `--vault` is the v3.0 canonical long; `--vault-dir` is a sibling
    /// alias for parity. Optional so the global pre-subcommand `--vault`
    /// can also be the source; when neither is given the dispatcher walks
    /// up from cwd via `vault_ctx::require` (same chain as the modern
    /// `skill run`), per #263 Part 2.
    #[arg(long, alias = "vault-dir")]
    pub vault: Option<PathBuf>,
    #[arg(long)]
    pub skill: String,
    #[arg(long = "arg")]
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_help_renders_without_panic() {
        Cli::command().debug_assert();
    }

    #[test]
    fn root_help_advertises_three_verbs_and_visible_groups() {
        // v3.1.0 UX polish: stub-only groups are hidden from root `--help`.
        // Only groups with at least one user-facing real verb are shown.
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        // 3 root verbs.
        assert!(help.contains("init"), "init missing from help");
        assert!(help.contains("update"), "update missing from help");
        assert!(help.contains("doctor"), "doctor missing from help");
        // Visible groups (have at least one implemented verb).
        for g in [
            "checkpoint",
            "harness",
            "note",
            "plugin",
            "schedule",
            "session",
            "skill",
            "task",
            "vault",
        ] {
            assert!(help.contains(g), "group `{g}` missing from root --help");
        }
    }

    #[test]
    fn note_and_task_surface_but_stub_verbs_stay_hidden() {
        let mut cmd = Cli::command();
        let root_help = cmd.render_long_help().to_string();
        assert!(root_help.contains("note"), "note missing from root --help");
        assert!(root_help.contains("task"), "task missing from root --help");
        // A hidden-but-REAL group must NOT appear. `render_long_help` omits
        // hidden commands entirely, and no visible group's text contains the
        // word, so a bare-name check is both strong and spacing-independent.
        //
        // This was `avatar` until v3.4.24 (#334) removed that group outright —
        // at which point "avatar is absent from help" became true for the
        // wrong reason and could no longer fail. `daemon` is hidden and real,
        // so it still tests what this assertion is for.
        assert!(
            !root_help.contains("daemon"),
            "hidden group `daemon` leaked into root --help"
        );

        // `task --help`: `list` is the only verb since #334 removed add/done.
        let mut cmd = Cli::command();
        let task_help = cmd
            .find_subcommand_mut("task")
            .expect("task subcommand must exist")
            .render_long_help()
            .to_string();
        assert!(task_help.contains("list"), "list missing from task --help");
    }

    #[test]
    fn hidden_aliases_do_not_appear_in_help() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        for alias in [
            "session-init",
            "orphan-scan",
            "qmd-reindex",
            "register-hooks",
            "register-schedule",
            "vault-sync",
            "run-skill",
        ] {
            // The migration text might still reference these — assert they
            // are not present as command entries (followed by a usage line).
            assert!(
                !help.contains(&format!("  {alias} ")),
                "hidden alias `{alias}` leaked into help"
            );
        }
    }

    #[test]
    fn hidden_aliases_still_parse_when_invoked_directly() {
        // session-init alias parses (back-compat).
        let cli = Cli::try_parse_from(["onebrain", "session-init"]).unwrap();
        assert!(matches!(cli.command, Cmd::SessionInitAlias(_)));

        let cli = Cli::try_parse_from(["onebrain", "orphan-scan", "07-logs", "abc123"]).unwrap();
        assert!(matches!(cli.command, Cmd::OrphanScanAlias(_)));

        let cli = Cli::try_parse_from(["onebrain", "qmd-reindex"]).unwrap();
        assert!(matches!(cli.command, Cmd::QmdReindexAlias));
    }

    #[test]
    fn new_paths_parse() {
        let cli = Cli::try_parse_from(["onebrain", "session", "init"]).unwrap();
        match cli.command {
            Cmd::Session(SessionCmd {
                verb: SessionVerb::Init { vault_dir, .. },
            }) => assert!(vault_dir.is_none()),
            _ => panic!("expected Session/Init"),
        }

        let cli =
            Cli::try_parse_from(["onebrain", "checkpoint", "orphans", "07-logs", "abc"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Checkpoint(CheckpointCmd {
                verb: CheckpointVerb::Orphans { .. }
            })
        ));

        let cli = Cli::try_parse_from(["onebrain", "vault", "current"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Vault(VaultCmd {
                verb: VaultVerb::Current
            })
        ));

        let cli = Cli::try_parse_from(["onebrain", "harness", "detect"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Harness(HarnessCmd {
                verb: HarnessVerb::Detect
            })
        ));

        // v3.2.10: a verb is now required (drops the v3.0 flat-form
        // back-compat — `onebrain harness` alone now prints help and exits
        // rather than silently running `detect`). Parsing must fail — clap
        // emits either MissingSubcommand or its help-on-missing variant
        // depending on the `arg_required_else_help` interaction.
        let err = Cli::try_parse_from(["onebrain", "harness"]).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                clap::error::ErrorKind::MissingSubcommand
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ),
            "expected missing-subcommand or display-help kind, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn global_flags_attach_to_any_subcommand() {
        let cli = Cli::try_parse_from(["onebrain", "--json", "task", "list"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.output, "text"); // default; --json takes precedence at resolve time
    }

    #[test]
    fn json_and_yaml_shortcuts_conflict() {
        // Clap enforces conflicts_with.
        let res = Cli::try_parse_from(["onebrain", "--json", "--yaml", "task", "list"]);
        assert!(res.is_err());
    }

    #[test]
    fn vault_flag_is_global() {
        let cli = Cli::try_parse_from(["onebrain", "--vault", "/tmp/v", "task", "list"]).unwrap();
        assert_eq!(cli.vault.as_deref(), Some(std::path::Path::new("/tmp/v")));
    }

    // Regression (v3.2.3): `skill run`, `schedule register`, and `plugin
    // migrate` named their local `--vault-dir` field `vault`, which collides
    // with the global `--vault` arg ID and made clap reject `--vault` on those
    // leaves (`onebrain skill run NAME --vault PATH` → "unexpected argument").
    // Renaming the field to `vault_dir` lets the global `--vault` propagate to
    // every command, matching `session init` / `checkpoint` / `plugin update`.
    #[test]
    fn skill_run_accepts_global_vault_post_subcommand() {
        let cli = Cli::try_parse_from(["onebrain", "skill", "run", "daily", "--vault", "/tmp/v"])
            .unwrap();
        assert_eq!(cli.vault.as_deref(), Some(std::path::Path::new("/tmp/v")));
        assert!(matches!(
            cli.command,
            Cmd::Skill(SkillCmd {
                verb: SkillVerb::Run { .. }
            })
        ));
    }

    #[test]
    fn skill_run_still_accepts_vault_dir_flag() {
        // Back-compat for the launchd scheduler, which passes `--vault-dir`.
        let cli =
            Cli::try_parse_from(["onebrain", "skill", "run", "daily", "--vault-dir", "/tmp/v"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Skill(SkillCmd {
                verb: SkillVerb::Run { .. }
            })
        ));
    }

    #[test]
    fn skill_run_accepts_skill_flag() {
        // v3.2.4: `--skill /daily` (parity with the scheduler's `run-skill`)
        // populates `skill`; the positional `name` stays None.
        let cli = Cli::try_parse_from(["onebrain", "skill", "run", "--skill", "/daily"]).unwrap();
        match cli.command {
            Cmd::Skill(SkillCmd {
                verb: SkillVerb::Run { name, skill, .. },
            }) => {
                assert!(name.is_none(), "positional name should be empty");
                assert_eq!(skill.as_deref(), Some("/daily"));
            }
            _ => panic!("expected skill run"),
        }
    }

    #[test]
    fn skill_run_help_advertises_codex_harness() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("skill")
            .unwrap()
            .find_subcommand_mut("run")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("claude, gemini, codex"));
    }

    #[test]
    fn skill_run_rejects_positional_and_skill_flag_together() {
        // `conflicts_with = "name"` — passing both is ambiguous and must error,
        // so `name.or(skill)` downstream never silently discards one.
        let res = Cli::try_parse_from(["onebrain", "skill", "run", "daily", "--skill", "/daily"]);
        assert!(res.is_err(), "positional + --skill must conflict");
    }

    #[test]
    fn schedule_register_accepts_global_vault_post_subcommand() {
        let cli =
            Cli::try_parse_from(["onebrain", "schedule", "register", "--vault", "/tmp/v"]).unwrap();
        assert_eq!(cli.vault.as_deref(), Some(std::path::Path::new("/tmp/v")));
    }

    #[test]
    fn plugin_migrate_accepts_global_vault_post_subcommand() {
        let cli = Cli::try_parse_from([
            "onebrain", "plugin", "migrate", "logs-v2", "--vault", "/tmp/v",
        ])
        .unwrap();
        assert_eq!(cli.vault.as_deref(), Some(std::path::Path::new("/tmp/v")));
    }

    #[test]
    fn plugin_update_parses_with_dry_run() {
        let cli = Cli::try_parse_from(["onebrain", "plugin", "update", "--dry-run"]).unwrap();
        match cli.command {
            Cmd::Plugin(PluginCmd {
                verb: PluginVerb::Update { dry_run, .. },
            }) => assert!(dry_run),
            _ => panic!("expected plugin update"),
        }
    }

    #[test]
    fn note_archive_parses_with_default_and_custom_root() {
        // Default archive root.
        let cli =
            Cli::try_parse_from(["onebrain", "note", "archive", "01-projects/Old.md"]).unwrap();
        match cli.command {
            Cmd::Note(NoteCmd {
                verb: NoteVerb::Archive(args),
            }) => {
                assert_eq!(args.path, PathBuf::from("01-projects/Old.md"));
                assert_eq!(args.archive_root, PathBuf::from("06-archive"));
            }
            _ => panic!("expected Note/Archive"),
        }

        // Custom --archive-root.
        let cli = Cli::try_parse_from([
            "onebrain",
            "note",
            "archive",
            "note.md",
            "--archive-root",
            "attic",
        ])
        .unwrap();
        match cli.command {
            Cmd::Note(NoteCmd {
                verb: NoteVerb::Archive(args),
            }) => assert_eq!(args.archive_root, PathBuf::from("attic")),
            _ => panic!("expected Note/Archive"),
        }
    }

    #[test]
    fn hidden_but_real_groups_still_parse() {
        // Was `unimplemented_groups_still_parse`, whose premise v3.4.24 (#334)
        // deleted: it asserted that bundle/dream/memory parse, and they no
        // longer do. `daemon` is the surviving hidden-but-REAL group — hidden
        // from `--help`, fully implemented — so the property worth pinning is
        // that hiding a group does not stop it parsing.
        let _ = Cli::try_parse_from(["onebrain", "daemon", "start"]).unwrap();
        let _ = Cli::try_parse_from(["onebrain", "note", "search", "TODO"]).unwrap();
    }

    #[test]
    fn removed_groups_no_longer_parse() {
        // The other half of #334: the 12 removed groups must fail to parse.
        // Paired with the one above so a future re-add cannot pass silently.
        for argv in [
            ["onebrain", "bundle", "install"],
            ["onebrain", "dream", "list"],
            ["onebrain", "memory", "list"],
        ] {
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{argv:?} must no longer parse"
            );
        }
    }

    // v3.3 step 2 — `serve` is a flag-based foreground command (not a verb
    // group). These pin its flag surface + the global-`--vault` propagation.
    #[test]
    fn serve_parses_all_flags() {
        let cli = Cli::try_parse_from([
            "onebrain",
            "serve",
            "--dir",
            "/tmp/dist",
            "--port",
            "8080",
            "--open",
        ])
        .unwrap();
        match cli.command {
            Cmd::Serve(args) => {
                assert_eq!(args.dir.as_deref(), Some(std::path::Path::new("/tmp/dist")));
                assert_eq!(args.port, Some(8080));
                assert!(args.open);
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn serve_host_flag_is_gone() {
        // #205: `--host` was REMOVED (localhost-only bind; `$ONEBRAIN_BIND` is
        // the container escape hatch). The flag must now be a parse error.
        assert!(Cli::try_parse_from(["onebrain", "serve", "--host", "0.0.0.0"]).is_err());
    }

    #[test]
    fn serve_accepts_global_vault_post_subcommand() {
        // Regression guard (mirrors `skill_run_accepts_global_vault…`): the
        // serve-local vault override field is named `vault_dir`, so the global
        // `--vault` propagates instead of colliding with the arg ID.
        let cli = Cli::try_parse_from(["onebrain", "serve", "--port", "8080", "--vault", "/tmp/v"])
            .unwrap();
        assert_eq!(cli.vault.as_deref(), Some(std::path::Path::new("/tmp/v")));
        assert!(matches!(cli.command, Cmd::Serve(_)));
    }

    #[test]
    fn serve_still_accepts_vault_dir_flag() {
        let cli = Cli::try_parse_from(["onebrain", "serve", "--vault-dir", "/tmp/v"]).unwrap();
        match cli.command {
            Cmd::Serve(args) => {
                assert_eq!(
                    args.vault_dir.as_deref(),
                    Some(std::path::Path::new("/tmp/v"))
                )
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn harness_arg_as_str_both_variants() {
        // HarnessArg::as_str() has two match arms; both must be reachable.
        assert_eq!(HarnessArg::Claude.as_str(), "claude");
        assert_eq!(HarnessArg::Gemini.as_str(), "gemini");
    }

    #[test]
    fn harness_run_parses_harness_and_model_flags() {
        let cli = Cli::try_parse_from([
            "onebrain",
            "harness",
            "run",
            "--harness",
            "gemini",
            "--model",
            "gemini-2.0-flash",
            "summarize this",
        ])
        .unwrap();
        match cli.command {
            Cmd::Harness(HarnessCmd {
                verb:
                    HarnessVerb::Run {
                        harness,
                        model,
                        prompt,
                        ..
                    },
            }) => {
                assert_eq!(harness, HarnessArg::Gemini);
                assert_eq!(model.as_deref(), Some("gemini-2.0-flash"));
                assert_eq!(prompt.as_deref(), Some("summarize this"));
            }
            _ => panic!("expected harness run"),
        }
    }

    #[test]
    fn remaining_legacy_aliases_all_parse() {
        // register-hooks alias (v3.0 back-compat).
        let cli = Cli::try_parse_from(["onebrain", "register-hooks"]).unwrap();
        assert!(matches!(cli.command, Cmd::RegisterHooksAlias(_)));

        // register-schedule alias.
        let cli = Cli::try_parse_from(["onebrain", "register-schedule"]).unwrap();
        assert!(matches!(cli.command, Cmd::RegisterScheduleAlias(_)));

        // migrate alias (positional name required).
        let cli = Cli::try_parse_from(["onebrain", "migrate", "logs-v2"]).unwrap();
        assert!(matches!(cli.command, Cmd::MigrateAlias(_)));

        // vault-sync alias.
        let cli = Cli::try_parse_from(["onebrain", "vault-sync"]).unwrap();
        assert!(matches!(cli.command, Cmd::VaultSyncAlias(_)));

        // run-skill alias (--skill is required).
        let cli = Cli::try_parse_from(["onebrain", "run-skill", "--skill", "/daily"]).unwrap();
        assert!(matches!(cli.command, Cmd::RunSkillAlias(_)));
    }

    #[test]
    fn generic_hook_parses_without_modes_and_stays_hidden() {
        assert!(Cli::try_parse_from(["onebrain", "hook"]).is_ok());
        assert!(
            Cli::try_parse_from(["onebrain", "codex-hook", "session-start"]).is_err(),
            "the removed harness-specific hook command must not parse"
        );

        let command = Cli::command();
        assert!(
            command
                .find_subcommand("hook")
                .is_some_and(clap::Command::is_hide_set),
            "internal hook command must stay hidden"
        );
    }

    #[test]
    fn task_list_parses_limit() {
        let cli = Cli::try_parse_from(["onebrain", "task", "list", "--limit", "5"]).unwrap();
        match cli.command {
            Cmd::Task(TaskCmd {
                verb: TaskVerb::List(args),
            }) => assert_eq!(args.limit, Some(5)),
            _ => panic!("expected task list"),
        }
        assert!(
            Cli::try_parse_from(["onebrain", "task", "list", "--limit", "0"]).is_err(),
            "task list should reject a zero limit"
        );
    }

    #[test]
    fn task_list_parses_filters() {
        let cli = Cli::try_parse_from([
            "onebrain",
            "task",
            "list",
            "--due-by",
            "today",
            "--folder",
            "01-projects",
            "--all",
        ])
        .unwrap();
        match cli.command {
            Cmd::Task(TaskCmd {
                verb: TaskVerb::List(args),
            }) => {
                assert_eq!(args.due_by.as_deref(), Some("today"));
                assert_eq!(args.folder, vec!["01-projects".to_string()]);
                assert!(args.all);
                assert_eq!(args.limit, None);
            }
            _ => panic!("expected task list"),
        }
    }
}
