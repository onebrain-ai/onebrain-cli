//! Typed errors emitted by the scheduler library. Error strings are matched
//! verbatim against Bun for parity — when in doubt, copy the exact substring
//! Bun's tests assert on.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum SchedulerError {
    #[error("invalid cron \"{cron}\": {reason}")]
    InvalidCron { cron: String, reason: String },

    #[error("invalid at \"{at}\": {reason}")]
    InvalidAt { at: String, reason: String },

    #[error("Invalid schedule entry: {reason}")]
    InvalidEntry { reason: String },

    #[error("Skill {0} not found at {1}")]
    SkillNotFound(String, String),

    #[error("Skill {0} has no YAML frontmatter")]
    SkillNoFrontmatter(String),

    #[error("Skill {0} requires user input — cannot schedule")]
    SkillNotSchedulable(String),

    #[error("Skill {skill} requires args: [{missing}]")]
    SkillMissingArgs { skill: String, missing: String },

    #[error("Skill {0} does not declare schedulable: true in frontmatter")]
    SkillSchedulableMissing(String),

    #[error("Arg \"{key}\"=\"{value}\" must not contain shell-special chars (\", $, `, \\) in its key or value")]
    ShellSpecialInArg { key: String, value: String },

    #[error("Arg key or value must not contain shell-special chars (\", $, `, \\): {0}")]
    ShellSpecialInOneShotArg(String),

    #[error("Command not found at absolute path: {0}. Check the path in onebrain.yml — the scheduler will silently fail at run time if the binary is missing.")]
    CommandNotFoundAbsolute(String),

    #[error("Command not found at relative path: {orig} (resolved: {resolved})")]
    CommandNotFoundRelative { orig: String, resolved: String },

    #[error("Command \"{0}\" not found in PATH. Use an absolute path in onebrain.yml — a scheduled job runs with a minimal environment that will not find {0}.")]
    CommandNotFoundInPath(String),

    #[error("Conflict: {new} and {existing} normalize to the same schedule artifact {path}")]
    Conflict {
        new: String,
        existing: String,
        path: String,
    },

    #[error("cron \"{cron}\" needs {needed} Task Scheduler triggers; the limit is 48 (measured — 49 is rejected with 'too many nodes of the same type'). Split it into separate `schedule:` entries.")]
    TooManyTriggers { cron: String, needed: usize },

    #[error("scheduler backend command failed: {command} ({status})\n{stderr}")]
    BackendCommand {
        /// The command line that was run, for the user to reproduce.
        command: String,
        /// The exit status, pre-rendered (`ExitStatus` has no stable Display
        /// across platforms and thiserror needs Display fields).
        status: String,
        /// Captured stderr, verbatim. Never swallowed — a silent fallback is
        /// the failure mode this release exists to remove.
        stderr: String,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
