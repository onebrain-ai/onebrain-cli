//! Clap subcommand tree — 3 root verbs + 24 resource groups + hidden v3.0
//! aliases. Locked at v3.1 per [[cli/specs/01-architecture §2.4]].
//!
//! Every group's verb list is captured as a `Subcommand` enum even when the
//! body is `unimplemented!()` — the tree shape itself is the v3.1 deliverable
//! (locks the public command surface for v3.2+).

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "onebrain",
    version,
    about = "OneBrain CLI — Personal AI OS for Obsidian"
)]
pub struct Cli {
    /// Override vault root (highest priority · beats ONEBRAIN_VAULT and walk-up).
    /// Global: accepted pre- or post-subcommand.
    #[arg(long, global = true, value_name = "PATH")]
    pub vault: Option<PathBuf>,

    /// Output format. Default `text` is TTY-friendly; pipe-detected calls
    /// drop color/pretty automatically.
    #[arg(
        short = 'o',
        long,
        global = true,
        default_value = "text",
        value_parser = ["text", "json", "yaml", "table", "tsv"],
        value_name = "FMT"
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
    //   20-23 · config & maintenance (plugin, schedule, config, skill)
    //   30   · search/index (qmd)
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

    // ───── Resource groups (24 · alphabetical) ─────────────────────────
    // v3.1.0 UX: groups whose every verb still returns `E_NOT_IMPLEMENTED`
    // are marked `hide = true` so they don't clutter `onebrain --help`. The
    // tree shape stays locked per spec §2.4 — typed commands still parse and
    // dispatch (returning exit 72), they just don't advertise in help.
    #[command(hide = true)]
    Avatar(AvatarCmd),
    #[command(hide = true)]
    Bookmark(BookmarkCmd),
    #[command(hide = true)]
    Bundle(BundleCmd),
    #[command(display_order = 12)]
    Checkpoint(CheckpointCmd),
    #[command(hide = true)]
    Config(ConfigCmd),
    #[command(hide = true)]
    Daemon(DaemonCmd),
    #[command(hide = true)]
    Date(DateCmd),
    #[command(hide = true)]
    Dream(DreamCmd),
    #[command(hide = true)]
    Frontmatter(FrontmatterCmd),
    #[command(hide = true)]
    Gateway(GatewayCmd),
    #[command(display_order = 13)]
    Harness(HarnessCmd),
    #[command(hide = true)]
    Inbox(InboxCmd),
    #[command(hide = true)]
    Log(LogCmd),
    #[command(hide = true)]
    Memory(MemoryCmd),
    #[command(hide = true)]
    Note(NoteCmd),
    #[command(hide = true)]
    Pause(PauseCmd),
    #[command(display_order = 20)]
    Plugin(PluginCmd),
    #[command(display_order = 30)]
    Qmd(QmdCmd),
    #[command(display_order = 21)]
    Schedule(ScheduleCmd),
    #[command(hide = true)]
    Serve(ServeCmd),
    #[command(display_order = 11)]
    Session(SessionCmd),
    #[command(display_order = 23)]
    Skill(SkillCmd),
    #[command(hide = true)]
    Task(TaskCmd),
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
}

// ─────────────────────────────────────────────────────────────────────────
// Root verb args
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Skip prompts · install the Essentials schedule preset (CI-friendly).
    #[arg(long)]
    pub yes: bool,
    /// Vault root directory · defaults to cwd.
    #[arg(long = "vault-dir", value_name = "PATH")]
    pub vault_dir: Option<PathBuf>,
    /// Overwrite an existing vault.yml without prompting.
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
    /// Emit the report as a single JSON document.
    #[arg(long)]
    pub json: bool,
}

// ─────────────────────────────────────────────────────────────────────────
// Resource group: avatar (forward-compat, all verbs unimplemented in v3.1)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct AvatarCmd {
    #[command(subcommand)]
    pub verb: AvatarVerb,
}
#[derive(Subcommand, Debug)]
pub enum AvatarVerb {
    Start,
    Pair,
    Status,
    Revoke,
    Doctor,
}

// ─────────────────────────────────────────────────────────────────────────
// bookmark
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct BookmarkCmd {
    #[command(subcommand)]
    pub verb: BookmarkVerb,
}
#[derive(Subcommand, Debug)]
pub enum BookmarkVerb {
    List,
    Get { id: String },
    Import { source: PathBuf },
}

// ─────────────────────────────────────────────────────────────────────────
// bundle (forward-compat)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(disable_help_subcommand = true)]
pub struct BundleCmd {
    #[command(subcommand)]
    pub verb: BundleVerb,
}
#[derive(Subcommand, Debug)]
pub enum BundleVerb {
    Install { name: String },
    Help { name: String },
    Info { name: String },
    Init { name: String },
    Lint { name: String },
    Update { name: String },
    Remove { name: String },
    Doctor,
}

// ─────────────────────────────────────────────────────────────────────────
// checkpoint (3 verbs · stop/reset wired to legacy, orphans wired to legacy)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Auto-save management (stop · reset · orphans)")]
pub struct CheckpointCmd {
    #[command(subcommand)]
    pub verb: CheckpointVerb,
}
#[derive(Subcommand, Debug)]
pub enum CheckpointVerb {
    /// Write a checkpoint file for the current session (Stop hook).
    Stop {
        /// Vault root override.
        #[arg(long = "vault-dir", value_name = "PATH")]
        vault_dir: Option<PathBuf>,
    },
    /// Reset checkpoint counter (Stop hook reset).
    Reset {
        /// Vault root override.
        #[arg(long = "vault-dir", value_name = "PATH")]
        vault_dir: Option<PathBuf>,
    },
    /// Scan for orphan checkpoint files (replaces v3.0 `orphan-scan`).
    Orphans {
        /// 07-logs/ folder path inside the vault.
        logs_folder: String,
        /// Current session token.
        session_token: String,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// config (vault.yml shaping · stubs in v3.1)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ConfigCmd {
    #[command(subcommand)]
    pub verb: ConfigVerb,
}
#[derive(Subcommand, Debug)]
pub enum ConfigVerb {
    Get { key: String },
    Set { key: String, value: String },
    List,
    Init,
}

// ─────────────────────────────────────────────────────────────────────────
// daemon (forward-compat)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct DaemonCmd {
    #[command(subcommand)]
    pub verb: DaemonVerb,
}
#[derive(Subcommand, Debug)]
pub enum DaemonVerb {
    Start,
    Stop,
    Status,
}

// ─────────────────────────────────────────────────────────────────────────
// date (vault-free utilities)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct DateCmd {
    #[command(subcommand)]
    pub verb: DateVerb,
}
#[derive(Subcommand, Debug)]
pub enum DateVerb {
    Today,
    Now,
    Format { input: String, fmt: String },
    Parse { input: String },
}

// ─────────────────────────────────────────────────────────────────────────
// dream (forward-compat)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct DreamCmd {
    #[command(subcommand)]
    pub verb: DreamVerb,
}
#[derive(Subcommand, Debug)]
pub enum DreamVerb {
    List,
    Tick { id: String },
    Done { id: String },
    Snooze { id: String, until: String },
}

// ─────────────────────────────────────────────────────────────────────────
// frontmatter (note-level)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct FrontmatterCmd {
    #[command(subcommand)]
    pub verb: FrontmatterVerb,
}
#[derive(Subcommand, Debug)]
pub enum FrontmatterVerb {
    Parse {
        path: PathBuf,
    },
    Extract {
        path: PathBuf,
        key: String,
    },
    Update {
        path: PathBuf,
        key: String,
        value: String,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// gateway (forward-compat)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct GatewayCmd {
    #[command(subcommand)]
    pub verb: GatewayVerb,
}
#[derive(Subcommand, Debug)]
pub enum GatewayVerb {
    Telegram,
    Mcp,
}

// ─────────────────────────────────────────────────────────────────────────
// harness (1-verb · documented exception · wired to legacy)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Detect Claude Code runtime")]
pub struct HarnessCmd {
    /// `detect` is the only verb. Made optional so v3.0's flat
    /// `onebrain harness` invocation (no verb) still works — that path is
    /// silently treated as `harness detect`. Future v3.x verbs can be added
    /// without breaking this; if `verb` is missing the dispatcher picks
    /// `Detect` by default.
    #[command(subcommand)]
    pub verb: Option<HarnessVerb>,
}
#[derive(Subcommand, Debug)]
pub enum HarnessVerb {
    /// Detect the active harness (Claude Code / Gemini / direct).
    Detect,
}

// ─────────────────────────────────────────────────────────────────────────
// inbox
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct InboxCmd {
    #[command(subcommand)]
    pub verb: InboxVerb,
}
#[derive(Subcommand, Debug)]
pub enum InboxVerb {
    List,
    Next,
    Process { id: Option<String> },
}

// ─────────────────────────────────────────────────────────────────────────
// log
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct LogCmd {
    #[command(subcommand)]
    pub verb: LogVerb,
}
#[derive(Subcommand, Debug)]
pub enum LogVerb {
    Query {
        pattern: String,
    },
    #[command(hide = true)]
    Append {
        entry: String,
    },
    #[command(hide = true)]
    Rotate,
    Stats,
}

// ─────────────────────────────────────────────────────────────────────────
// memory
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct MemoryCmd {
    #[command(subcommand)]
    pub verb: MemoryVerb,
}
#[derive(Subcommand, Debug)]
pub enum MemoryVerb {
    List,
    Add { topic: String, content: String },
    Update { id: String, content: String },
    Remove { id: String },
    Promote { id: String },
    Index,
}

// ─────────────────────────────────────────────────────────────────────────
// note (11 verbs locked 2026-05-25 · ships v3.2.0, stubs in v3.1)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct NoteCmd {
    #[command(subcommand)]
    pub verb: NoteVerb,
}
#[derive(Subcommand, Debug)]
pub enum NoteVerb {
    Search { pattern: String },
    List,
    Find { pattern: String },
    Read { path: PathBuf },
    Append { path: PathBuf, content: String },
    New { title: String },
    Move { from: PathBuf, to: PathBuf },
    Archive { path: PathBuf },
    Backlinks { path: PathBuf },
    Orphans,
    Stat { path: PathBuf },
}

// ─────────────────────────────────────────────────────────────────────────
// pause
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct PauseCmd {
    #[command(subcommand)]
    pub verb: PauseVerb,
}
#[derive(Subcommand, Debug)]
pub enum PauseVerb {
    List,
    Snapshot { slug: String },
    Resume { slug: Option<String> },
}

// ─────────────────────────────────────────────────────────────────────────
// plugin (install/migrate hidden · update/uninstall/status/verify visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Plugin lifecycle + hook rewriter")]
pub struct PluginCmd {
    #[command(subcommand)]
    pub verb: PluginVerb,
}
#[derive(Subcommand, Debug)]
pub enum PluginVerb {
    /// Install plugin into vault (hidden · called by `init`/`plugin update`).
    #[command(hide = true)]
    Install {
        /// Optional vault root override.
        #[arg(long = "vault-dir", value_name = "PATH")]
        vault_dir: Option<PathBuf>,
        /// Override branch (defaults to vault.yml `update_channel`).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Uninstall plugin (placeholder · not yet implemented).
    #[command(hide = true)]
    Uninstall,
    /// Pull plugin from GitHub · rewrite hooks · rebind launchd plists.
    Update {
        /// Optional vault root override.
        #[arg(long = "vault-dir", value_name = "PATH")]
        vault_dir: Option<PathBuf>,
        /// Override branch (defaults to vault.yml `update_channel`).
        #[arg(long)]
        branch: Option<String>,
        /// Show what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a one-shot vault migration (hidden · called by `plugin update`).
    #[command(hide = true)]
    Migrate {
        name: String,
        cutoff_date: Option<String>,
        #[arg(long, conflicts_with = "cutoff_date")]
        cutoff: Option<String>,
        #[arg(long = "vault-dir")]
        vault: Option<PathBuf>,
    },
    /// Plugin status (placeholder).
    #[command(hide = true)]
    Status,
    /// Verify plugin install (placeholder).
    #[command(hide = true)]
    Verify,
}

// ─────────────────────────────────────────────────────────────────────────
// qmd (search/setup/status visible · embed/reindex hidden)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Vault search index")]
pub struct QmdCmd {
    #[command(subcommand)]
    pub verb: QmdVerb,
}
#[derive(Subcommand, Debug)]
pub enum QmdVerb {
    #[command(hide = true)]
    Setup,
    /// Re-embed documents (hidden · auto-fired by indexer hook).
    #[command(hide = true)]
    Embed,
    #[command(hide = true)]
    Status,
    /// Rebuild the qmd search index (replaces v3.0 `qmd-reindex`).
    #[command(hide = true)]
    Reindex,
    #[command(hide = true)]
    Search { query: String },
}

// ─────────────────────────────────────────────────────────────────────────
// schedule (register hidden · others visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "launchd schedule management")]
pub struct ScheduleCmd {
    #[command(subcommand)]
    pub verb: ScheduleVerb,
}
#[derive(Subcommand, Debug)]
pub enum ScheduleVerb {
    #[command(hide = true)]
    List,
    #[command(hide = true)]
    Add { skill: String },
    #[command(hide = true)]
    Remove { skill: String },
    /// Re-write launchd plists (hidden · called by `plugin update`).
    #[command(hide = true)]
    Register {
        /// Vault root override.
        #[arg(long = "vault-dir")]
        vault: Option<PathBuf>,
        /// Print the plists that would be written without touching disk.
        #[arg(long)]
        dry_run: bool,
        /// Remove all plists for entries currently in vault.yml.
        #[arg(long)]
        remove: bool,
        /// Re-emit plists with the current vault path (logs a notice).
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
    #[command(hide = true)]
    Status,
}

// ─────────────────────────────────────────────────────────────────────────
// serve (forward-compat)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ServeCmd {
    #[command(subcommand)]
    pub verb: ServeVerb,
}
#[derive(Subcommand, Debug)]
pub enum ServeVerb {
    Start,
    Stop,
    Status,
}

// ─────────────────────────────────────────────────────────────────────────
// session (init hidden, others visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Session lifecycle (init)")]
pub struct SessionCmd {
    #[command(subcommand)]
    pub verb: SessionVerb,
}
#[derive(Subcommand, Debug)]
pub enum SessionVerb {
    /// Print session metadata as JSON (hidden · SessionStart hook).
    #[command(hide = true)]
    Init {
        /// Vault root directory · defaults to auto-detect from cwd.
        #[arg(long = "vault-dir", value_name = "PATH")]
        vault_dir: Option<PathBuf>,
    },
    #[command(hide = true)]
    Current,
    #[command(hide = true)]
    List,
    #[command(hide = true)]
    Get { id: String },
}

// ─────────────────────────────────────────────────────────────────────────
// skill (bootstrap hidden, list/run/help/info visible)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(disable_help_subcommand = true, about = "Skill invocation")]
pub struct SkillCmd {
    #[command(subcommand)]
    pub verb: SkillVerb,
}
#[derive(Subcommand, Debug)]
pub enum SkillVerb {
    #[command(hide = true)]
    List,
    /// Run a skill in headless mode (replaces v3.0 `run-skill`).
    Run {
        /// Vault root directory (also accepts global `--vault`).
        #[arg(long = "vault-dir")]
        vault: Option<PathBuf>,
        /// Skill name (with or without slash prefix).
        name: String,
        /// Pass-through arguments (`--arg key=value`).
        #[arg(long = "arg")]
        args: Vec<String>,
    },
    /// Bootstrap a skill (hidden · called by skills internally).
    #[command(hide = true)]
    Bootstrap { name: String },
    #[command(hide = true)]
    Help { name: String },
    #[command(hide = true)]
    Info { name: String },
}

// ─────────────────────────────────────────────────────────────────────────
// task
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct TaskCmd {
    #[command(subcommand)]
    pub verb: TaskVerb,
}
#[derive(Subcommand, Debug)]
pub enum TaskVerb {
    List,
    Add { content: String },
    Done { id: String },
}

// ─────────────────────────────────────────────────────────────────────────
// vault (scan/stats/verify/current visible · sync hidden)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
#[command(about = "Vault operations (sync · current)")]
pub struct VaultCmd {
    #[command(subcommand)]
    pub verb: VaultVerb,
}
#[derive(Subcommand, Debug)]
pub enum VaultVerb {
    /// Pull plugin tarball into the current vault (hidden · called by `plugin update`).
    #[command(hide = true)]
    Sync {
        /// Optional positional vault root · defaults to walk-up from cwd.
        vault_root: Option<PathBuf>,
        /// Vault root override · flag-form.
        #[arg(long = "vault-dir", conflicts_with = "vault_root")]
        vault_dir: Option<PathBuf>,
        /// Override branch resolved from vault.yml::update_channel.
        #[arg(long)]
        branch: Option<String>,
    },
    #[command(hide = true)]
    Scan,
    #[command(hide = true)]
    Stats,
    #[command(hide = true)]
    Verify,
    /// Print active vault + resolution source (new in v3.1).
    Current,
}

// ─────────────────────────────────────────────────────────────────────────
// Legacy v3.0 alias args — kept verbatim for back-compat
// ─────────────────────────────────────────────────────────────────────────

#[derive(Args, Debug, Clone)]
pub struct LegacySessionInitArgs {
    #[arg(long = "vault-dir")]
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
    #[arg(long, visible_alias = "vault-dir")]
    pub vault: Option<PathBuf>,
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub remove: bool,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyRegisterScheduleArgs {
    #[arg(long, visible_alias = "vault-dir")]
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
    #[arg(long, visible_alias = "vault-dir")]
    pub vault: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyVaultSyncArgs {
    pub vault_root: Option<PathBuf>,
    #[arg(long = "vault-dir", conflicts_with = "vault_root")]
    pub vault_dir: Option<PathBuf>,
    #[arg(long)]
    pub branch: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LegacyRunSkillArgs {
    /// `--vault` is the v3.0 canonical long; `--vault-dir` is a sibling
    /// alias for parity. Optional so the global pre-subcommand `--vault`
    /// can also be the source; dispatcher requires at least one.
    #[arg(long, visible_alias = "vault-dir")]
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
            "plugin",
            "qmd",
            "schedule",
            "session",
            "skill",
            "vault",
        ] {
            assert!(help.contains(g), "group `{g}` missing from root --help");
        }
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
                verb: SessionVerb::Init { vault_dir },
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
                verb: Some(HarnessVerb::Detect)
            })
        ));

        // v3.0 flat form: `onebrain harness` (no verb). Resolved by the
        // dispatcher to `Detect` because the verb is Option<>.
        let cli = Cli::try_parse_from(["onebrain", "harness"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::Harness(HarnessCmd { verb: None })
        ));
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
    fn unimplemented_groups_still_parse() {
        // A representative sample — Bundle, Daemon, Dream all parse even
        // though their bodies will be unimplemented in v3.1.
        let _ = Cli::try_parse_from(["onebrain", "bundle", "install", "designer"]).unwrap();
        let _ = Cli::try_parse_from(["onebrain", "daemon", "start"]).unwrap();
        let _ = Cli::try_parse_from(["onebrain", "dream", "list"]).unwrap();
        let _ = Cli::try_parse_from(["onebrain", "memory", "list"]).unwrap();
        let _ = Cli::try_parse_from(["onebrain", "note", "search", "TODO"]).unwrap();
    }
}
