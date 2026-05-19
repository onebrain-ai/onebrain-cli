//! `onebrain update` core — fetch the latest release from GitHub and install
//! the corresponding `@onebrain-ai/cli` binary.
//!
//! Byte-for-byte parity with Bun v2.x `src/commands/update.ts`:
//!
//!   1. Read current binary version (`onebrain --version`).
//!   2. Fetch `releases/latest` from GitHub.
//!   3. If currentVersion == latestVersion, skip install and emit the
//!      "nothing to do" non-TTY line.
//!   4. Otherwise, install via `bun install -g @onebrain-ai/cli@<v>` (Unix)
//!      or PowerShell-wrapped `npm install` (Windows).
//!   5. Validate by spawning `onebrain --version` and checking the regex
//!      `v\d+\.\d+` against stdout (ATOMIC GATE — install without validate
//!      counts as failure).
//!
//! All four external IO surfaces are injectable through `UpdateOptions` so
//! the unit + integration tests run offline. Defaults call out to the real
//! environment via `reqwest::blocking` and `std::process::Command`.
//!
//! v3.0 ships non-TTY output only — TTY spinner / colored output is deferred
//! to v3.0.1 (no `picocolors` / `cli-banner` equivalent in tree yet).

use chrono::{DateTime, Datelike, Utc};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Public result type returned by [`run_update`].
///
/// Mirrors `UpdateResult` in `update.ts`. `ok` is `true` only when every
/// required step finished cleanly (fetch then install then validate, or
/// fetch in `--check` mode, or fetch with currentVersion equal to
/// latestVersion).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpdateResult {
    pub ok: bool,
    pub exit_code: i32,
    pub latest_version: Option<String>,
    pub current_version: Option<String>,
    pub error: Option<String>,
}

/// Release info parsed from the GitHub `releases/latest` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: String,
    pub published_at: Option<DateTime<Utc>>,
}

/// Closure aliases mirror the Bun `fetchFn`, `installBinaryFn`,
/// `validateBinaryFn`, and `currentVersionFn` injection points.
pub type FetchFn = Box<dyn Fn() -> Result<ReleaseInfo, UpdateError> + Send + Sync>;
pub type InstallFn = Box<dyn Fn(&str) -> Result<(), UpdateError> + Send + Sync>;
pub type ValidateFn = Box<dyn Fn() -> bool + Send + Sync>;
pub type CurrentVersionFn = Box<dyn Fn() -> CurrentVersion + Send + Sync>;
type LineSink = Box<dyn FnMut(&str) + Send>;

/// Current version of the locally-installed `onebrain` binary, plus its
/// publish date if the `--version` output included one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentVersion {
    pub version: String,
    pub published_at: Option<DateTime<Utc>>,
}

/// Options driving [`run_update`]. All four IO closures default to the real
/// system implementations when `None`. Tests pass canned closures.
#[derive(Default)]
pub struct UpdateOptions {
    /// Dry-run · fetch latest version and report, do not install.
    pub check: bool,
    pub fetch_fn: Option<FetchFn>,
    pub install_fn: Option<InstallFn>,
    pub validate_fn: Option<ValidateFn>,
    pub current_version_fn: Option<CurrentVersionFn>,
    /// Optional output sink (line-oriented, plain text). Defaults to stdout
    /// when `None`. Stderr lines (errors) always go to stderr regardless.
    pub stdout_lines: Option<LineSink>,
    pub stderr_lines: Option<LineSink>,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("GitHub API returned HTTP {0}")]
    GithubStatus(u16),

    #[error("GitHub response missing tag_name")]
    MissingTag,

    #[error("network: {0}")]
    Network(String),

    #[error("decode: {0}")]
    Decode(String),

    #[error("Binary install failed (exit {exit_code}): {stderr}")]
    InstallBinary { exit_code: i32, stderr: String },

    #[error("spawn `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Source of truth for the GitHub releases URL. The env var
/// `ONEBRAIN_GITHUB_RELEASES_URL` overrides at runtime so mockito-backed
/// integration tests can redirect the request without faking the entire
/// HTTP client.
const DEFAULT_GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/onebrain-ai/onebrain/releases/latest";

const GITHUB_ENV_OVERRIDE: &str = "ONEBRAIN_GITHUB_RELEASES_URL";

const HTTP_TIMEOUT_SECS: u64 = 15;

/// User-Agent. reqwest requires a non-empty UA for the GitHub API otherwise
/// the API returns 403. Use a stable string the v3 release pipeline can
/// audit.
const USER_AGENT: &str = "onebrain-cli/3.0";

// ---------------------------------------------------------------------------
// Windows shell detection
// ---------------------------------------------------------------------------

/// Resolve which PowerShell variant to invoke on Windows. Prefer `pwsh`
/// (PowerShell 7+) with fallback to legacy `powershell.exe`. Memoized via a
/// stdlib `OnceLock` (Bun uses an in-module `let _windowsShell: string |
/// undefined`).
fn windows_shell() -> &'static str {
    static CELL: OnceLock<&'static str> = OnceLock::new();
    CELL.get_or_init(|| {
        let probe = Command::new("pwsh").arg("--version").output();
        match probe {
            Ok(o) if o.status.success() => "pwsh",
            _ => "powershell.exe",
        }
    })
}

// ---------------------------------------------------------------------------
// Date formatting (en-GB, "24 Apr 2026")
// ---------------------------------------------------------------------------

/// Format a release date in `"D MMM YYYY"` form, matching JS
/// `toLocaleDateString('en-GB', { day: 'numeric', month: 'short', year: 'numeric' })`.
///
/// Uses chrono manually rather than `%-d` since `%-d` is a glibc extension
/// not supported on every platform chrono targets.
pub fn format_release_date(date: DateTime<Utc>) -> String {
    let month = match date.month() {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => unreachable!(),
    };
    format!("{} {} {}", date.day(), month, date.year())
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Real `fetchLatestRelease` — calls GitHub releases/latest with the v3
/// JSON Accept header and a User-Agent (mandatory · 403 otherwise).
pub fn default_fetch_latest_release() -> Result<ReleaseInfo, UpdateError> {
    let url = std::env::var(GITHUB_ENV_OVERRIDE)
        .unwrap_or_else(|_| DEFAULT_GITHUB_RELEASES_URL.to_string());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(UpdateError::GithubStatus(resp.status().as_u16()));
    }
    let json: serde_json::Value = resp
        .json()
        .map_err(|e| UpdateError::Decode(e.to_string()))?;
    parse_release_payload(&json)
}

/// Pure parser broken out of `default_fetch_latest_release` for unit testing
/// without the HTTP roundtrip.
pub fn parse_release_payload(json: &serde_json::Value) -> Result<ReleaseInfo, UpdateError> {
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or(UpdateError::MissingTag)?;
    let published_at = json
        .get("published_at")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    Ok(ReleaseInfo {
        version: tag.to_string(),
        published_at,
    })
}

/// Real `defaultInstallBinary` — `bun install -g @onebrain-ai/cli@<v>` on
/// Unix; PowerShell-wrapped `npm install -g '@onebrain-ai/cli@<v>'` on
/// Windows.
pub fn default_install_binary(version: &str) -> Result<(), UpdateError> {
    let cmd = build_install_command(version, cfg!(windows));
    run_subprocess(&cmd, /*install*/ true)
}

/// Pull command construction out so tests can verify the argv shape on
/// every platform without actually executing it.
pub(crate) fn build_install_command(version: &str, is_windows: bool) -> Vec<String> {
    if is_windows {
        // Escape any single quotes for the PowerShell literal string. Semver
        // tags will never contain `'` but mirror Bun's escape pass for
        // safety.
        let safe = version.replace('\'', "''");
        vec![
            windows_shell().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            format!("npm install -g '@onebrain-ai/cli@{safe}'"),
        ]
    } else {
        vec![
            "bun".to_string(),
            "install".to_string(),
            "-g".to_string(),
            format!("@onebrain-ai/cli@{version}"),
        ]
    }
}

fn run_subprocess(argv: &[String], is_install: bool) -> Result<(), UpdateError> {
    let mut iter = argv.iter();
    let program = iter.next().expect("argv non-empty");
    let output = Command::new(program)
        .args(iter)
        .output()
        .map_err(|e| UpdateError::Spawn {
            cmd: program.clone(),
            source: e,
        })?;
    if !output.status.success() {
        let exit_code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return if is_install {
            Err(UpdateError::InstallBinary { exit_code, stderr })
        } else {
            Err(UpdateError::Spawn {
                cmd: program.clone(),
                source: std::io::Error::other(format!("exit {exit_code}")),
            })
        };
    }
    Ok(())
}

/// Real `defaultValidateBinary` — spawn `onebrain --version`, expect a match
/// against `v\d+\.\d+`. Any failure returns `false` (matches Bun's
/// catch-all-and-return-false pattern).
pub fn default_validate_binary() -> bool {
    let cmd = build_version_command(cfg!(windows));
    spawn_version_command(&cmd)
        .map(|stdout| version_regex_matches(stdout.trim()))
        .unwrap_or(false)
}

/// Real `defaultCurrentVersion` — spawn `onebrain --version`, regex-parse
/// out `v[\d.]+` and an ISO-style `released YYYY-MM-DD` substring. Failure
/// returns `{ version: "unknown", published_at: None }`.
pub fn default_current_version() -> CurrentVersion {
    let cmd = build_version_command(cfg!(windows));
    let Ok(stdout) = spawn_version_command(&cmd) else {
        return CurrentVersion {
            version: "unknown".to_string(),
            published_at: None,
        };
    };
    parse_current_version_output(&stdout)
}

/// Pure parser shared by `default_current_version` and unit tests.
pub fn parse_current_version_output(stdout: &str) -> CurrentVersion {
    let version = extract_version_prefix(stdout).unwrap_or_else(|| "unknown".to_string());
    let published_at = extract_release_date(stdout);
    CurrentVersion {
        version,
        published_at,
    }
}

pub(crate) fn build_version_command(is_windows: bool) -> Vec<String> {
    if is_windows {
        vec![
            windows_shell().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "onebrain --version".to_string(),
        ]
    } else {
        vec!["onebrain".to_string(), "--version".to_string()]
    }
}

fn spawn_version_command(argv: &[String]) -> Result<String, UpdateError> {
    let mut iter = argv.iter();
    let program = iter.next().expect("version argv non-empty");
    let output = Command::new(program)
        .args(iter)
        .output()
        .map_err(|e| UpdateError::Spawn {
            cmd: program.clone(),
            source: e,
        })?;
    if !output.status.success() {
        return Err(UpdateError::Spawn {
            cmd: program.clone(),
            source: std::io::Error::other(format!("exit {}", output.status.code().unwrap_or(-1))),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Bun regex `/v\d+\.\d+/` — anchor-free, matches mid-string.
pub fn version_regex_matches(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'v' {
            // require at least one digit, then '.', then at least one digit
            let mut j = i + 1;
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j < bytes.len() && bytes[j] == b'.' {
                let after_dot = j + 1;
                let mut k = after_dot;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > after_dot {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Bun `/v[\d.]+/.exec(stdout)?.[0]` — first match. Match `v` followed by
/// one or more digits-or-dots. Returns the matched substring.
pub fn extract_version_prefix(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'v' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            if j > i + 1 {
                return Some(String::from_utf8_lossy(&bytes[i..j]).into_owned());
            }
        }
        i += 1;
    }
    None
}

/// Bun `/released (\d{4}-\d{2}-\d{2})/`. Returns the parsed date at UTC
/// midnight.
pub fn extract_release_date(s: &str) -> Option<DateTime<Utc>> {
    let needle = "released ";
    let idx = s.find(needle)?;
    let after = &s[idx + needle.len()..];
    let bytes = after.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let date_part = &bytes[..10];
    if !date_part[0..4].iter().all(|b| b.is_ascii_digit())
        || date_part[4] != b'-'
        || !date_part[5..7].iter().all(|b| b.is_ascii_digit())
        || date_part[7] != b'-'
        || !date_part[8..10].iter().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let s10 = std::str::from_utf8(date_part).ok()?;
    let full = format!("{s10}T00:00:00Z");
    DateTime::parse_from_rfc3339(&full)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Drive the full update flow with overridable IO. Mirrors Bun `runUpdate`
/// step-for-step.
pub fn run_update(mut opts: UpdateOptions) -> UpdateResult {
    let mut result = UpdateResult {
        ok: false,
        exit_code: 0,
        ..Default::default()
    };

    write_stdout(&mut opts, "OneBrain Update");

    // Step 1 — local version
    let current = opts
        .current_version_fn
        .as_ref()
        .map(|f| f())
        .unwrap_or_else(default_current_version);
    result.current_version = Some(current.version.clone());
    write_stdout(&mut opts, &format!("current: {}", current.version));

    // Step 2 — fetch latest release
    let release = match opts.fetch_fn.as_ref().map(|f| f()) {
        Some(r) => r,
        None => default_fetch_latest_release(),
    };
    let release = match release {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Fetch failed: {e}");
            result.error = Some(msg.clone());
            result.exit_code = 1;
            write_stderr(&mut opts, &format!("update: {msg}"));
            return result;
        }
    };
    write_stdout(&mut opts, &format!("latest: {}", release.version));
    result.latest_version = Some(release.version.clone());

    // --check dry-run
    if opts.check {
        write_stdout(&mut opts, "done: dry run complete — no changes made");
        result.ok = true;
        result.exit_code = 0;
        return result;
    }

    // Already up to date?
    if current.version == release.version {
        write_stdout(
            &mut opts,
            &format!("already up to date: @onebrain-ai/cli {}", release.version),
        );
        write_stdout(&mut opts, "done: nothing to do");
        result.ok = true;
        result.exit_code = 0;
        return result;
    }

    // Step 3 — install
    let install_res = match opts.install_fn.as_ref() {
        Some(f) => f(&release.version),
        None => default_install_binary(&release.version),
    };
    if let Err(e) = install_res {
        let msg = format!("Binary install failed: {e}");
        result.error = Some(msg.clone());
        result.exit_code = 1;
        write_stderr(&mut opts, &format!("update: {msg}"));
        return result;
    }
    write_stdout(
        &mut opts,
        &format!("upgrading: @onebrain-ai/cli {} installed", release.version),
    );

    // Step 4 — atomic gate: validate
    let valid = match opts.validate_fn.as_ref() {
        Some(f) => f(),
        None => default_validate_binary(),
    };
    if !valid {
        let msg = "Binary validation failed. Check PATH.".to_string();
        result.error = Some(msg.clone());
        result.exit_code = 1;
        write_stderr(&mut opts, &format!("update: {msg}"));
        return result;
    }

    write_stdout(&mut opts, "done: run /update in Claude to sync vault files");
    result.ok = true;
    result.exit_code = 0;
    result
}

fn write_stdout(opts: &mut UpdateOptions, line: &str) {
    if let Some(sink) = opts.stdout_lines.as_mut() {
        sink(line);
    } else {
        println!("{line}");
    }
}

fn write_stderr(opts: &mut UpdateOptions, line: &str) {
    if let Some(sink) = opts.stderr_lines.as_mut() {
        sink(line);
    } else {
        eprintln!("{line}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_fetch(version: &'static str) -> FetchFn {
        Box::new(move || {
            Ok(ReleaseInfo {
                version: version.to_string(),
                published_at: None,
            })
        })
    }

    fn current(v: &'static str) -> CurrentVersionFn {
        Box::new(move || CurrentVersion {
            version: v.to_string(),
            published_at: None,
        })
    }

    type SinkLines = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn capturing_sink() -> (super::LineSink, SinkLines) {
        let lines: SinkLines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let lines_clone = lines.clone();
        let sink: super::LineSink = Box::new(move |s: &str| {
            lines_clone.lock().unwrap().push(s.to_string());
        });
        (sink, lines)
    }

    #[test]
    fn full_upgrade_path_fetch_install_validate_ok() {
        let install_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let install_calls_c = install_calls.clone();
        let validate_calls = std::sync::Arc::new(std::sync::Mutex::new(0_u32));
        let validate_calls_c = validate_calls.clone();

        let (stdout_sink, stdout_lines) = capturing_sink();
        let opts = UpdateOptions {
            fetch_fn: Some(mock_fetch("v2.0.0")),
            install_fn: Some(Box::new(move |v: &str| {
                install_calls_c.lock().unwrap().push(v.to_string());
                Ok(())
            })),
            validate_fn: Some(Box::new(move || {
                *validate_calls_c.lock().unwrap() += 1;
                true
            })),
            current_version_fn: Some(current("v1.10.18")),
            stdout_lines: Some(stdout_sink),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(result.ok);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.latest_version.as_deref(), Some("v2.0.0"));
        assert_eq!(result.current_version.as_deref(), Some("v1.10.18"));
        assert_eq!(install_calls.lock().unwrap().as_slice(), &["v2.0.0"]);
        assert_eq!(*validate_calls.lock().unwrap(), 1);
        let out = stdout_lines.lock().unwrap().join("\n");
        assert!(out.contains("OneBrain Update"));
        assert!(out.contains("current: v1.10.18"));
        assert!(out.contains("latest: v2.0.0"));
        assert!(out.contains("upgrading: @onebrain-ai/cli v2.0.0 installed"));
        assert!(out.contains("done: run /update in Claude to sync vault files"));
    }

    #[test]
    fn check_flag_only_fetches() {
        let install_called = std::sync::Arc::new(std::sync::Mutex::new(false));
        let install_called_c = install_called.clone();
        let validate_called = std::sync::Arc::new(std::sync::Mutex::new(false));
        let validate_called_c = validate_called.clone();
        let opts = UpdateOptions {
            check: true,
            fetch_fn: Some(mock_fetch("v2.0.0")),
            install_fn: Some(Box::new(move |_| {
                *install_called_c.lock().unwrap() = true;
                Ok(())
            })),
            validate_fn: Some(Box::new(move || {
                *validate_called_c.lock().unwrap() = true;
                true
            })),
            current_version_fn: Some(current("v1.10.18")),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(result.ok);
        assert_eq!(result.exit_code, 0);
        assert!(!*install_called.lock().unwrap());
        assert!(!*validate_called.lock().unwrap());
    }

    #[test]
    fn already_up_to_date_skips_install() {
        let install_called = std::sync::Arc::new(std::sync::Mutex::new(false));
        let install_called_c = install_called.clone();
        let (stdout_sink, stdout_lines) = capturing_sink();
        let opts = UpdateOptions {
            fetch_fn: Some(mock_fetch("v2.0.0")),
            install_fn: Some(Box::new(move |_| {
                *install_called_c.lock().unwrap() = true;
                Ok(())
            })),
            validate_fn: Some(Box::new(|| true)),
            current_version_fn: Some(current("v2.0.0")),
            stdout_lines: Some(stdout_sink),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(result.ok);
        assert_eq!(result.exit_code, 0);
        assert!(!*install_called.lock().unwrap());
        let out = stdout_lines.lock().unwrap().join("\n");
        assert!(out.contains("already up to date: @onebrain-ai/cli v2.0.0"));
        assert!(out.contains("done: nothing to do"));
    }

    #[test]
    fn fetch_error_exits_1_with_message() {
        let opts = UpdateOptions {
            fetch_fn: Some(Box::new(|| Err(UpdateError::GithubStatus(503)))),
            current_version_fn: Some(current("v1.10.18")),
            stderr_lines: Some(Box::new(|_| {})),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(!result.ok);
        assert_eq!(result.exit_code, 1);
        assert!(result.error.unwrap().starts_with("Fetch failed: "));
    }

    #[test]
    fn install_failure_exits_1() {
        let opts = UpdateOptions {
            fetch_fn: Some(mock_fetch("v2.0.0")),
            install_fn: Some(Box::new(|_| {
                Err(UpdateError::InstallBinary {
                    exit_code: 1,
                    stderr: "EACCES".to_string(),
                })
            })),
            current_version_fn: Some(current("v1.10.18")),
            stderr_lines: Some(Box::new(|_| {})),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(!result.ok);
        assert_eq!(result.exit_code, 1);
        let err = result.error.unwrap();
        assert!(err.contains("Binary install failed"));
    }

    #[test]
    fn validate_failure_exits_1_with_path_hint() {
        let opts = UpdateOptions {
            fetch_fn: Some(mock_fetch("v2.0.0")),
            install_fn: Some(Box::new(|_| Ok(()))),
            validate_fn: Some(Box::new(|| false)),
            current_version_fn: Some(current("v1.10.18")),
            stderr_lines: Some(Box::new(|_| {})),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(!result.ok);
        assert_eq!(result.exit_code, 1);
        assert_eq!(
            result.error.as_deref(),
            Some("Binary validation failed. Check PATH.")
        );
    }

    #[test]
    fn unknown_current_version_still_proceeds() {
        let install_called = std::sync::Arc::new(std::sync::Mutex::new(false));
        let install_called_c = install_called.clone();
        let opts = UpdateOptions {
            fetch_fn: Some(mock_fetch("v2.0.0")),
            install_fn: Some(Box::new(move |_| {
                *install_called_c.lock().unwrap() = true;
                Ok(())
            })),
            validate_fn: Some(Box::new(|| true)),
            current_version_fn: Some(Box::new(|| CurrentVersion {
                version: "unknown".to_string(),
                published_at: None,
            })),
            ..Default::default()
        };
        let result = run_update(opts);
        assert!(result.ok);
        assert_eq!(result.current_version.as_deref(), Some("unknown"));
        assert_eq!(result.latest_version.as_deref(), Some("v2.0.0"));
        assert!(*install_called.lock().unwrap());
    }

    // -----------------------------------------------------------------
    // Pure helpers
    // -----------------------------------------------------------------

    #[test]
    fn version_regex_matches_real_output() {
        assert!(version_regex_matches(
            "OneBrain v2.0.7 — released 2026-04-26"
        ));
        assert!(version_regex_matches("v3.0"));
        assert!(!version_regex_matches("v2"));
        assert!(!version_regex_matches("2.0.7"));
        assert!(!version_regex_matches(""));
    }

    #[test]
    fn extract_version_prefix_finds_first_match() {
        assert_eq!(
            extract_version_prefix("OneBrain v2.0.7 — released 2026-04-26").as_deref(),
            Some("v2.0.7")
        );
        assert_eq!(
            extract_version_prefix("v3.0.0-alpha.1").as_deref(),
            Some("v3.0.0")
        );
        assert_eq!(extract_version_prefix("no version here"), None);
    }

    #[test]
    fn extract_release_date_parses_iso() {
        let d = extract_release_date("OneBrain v2.0.7 — released 2026-04-26").unwrap();
        assert_eq!(d.year(), 2026);
        assert_eq!(d.month(), 4);
        assert_eq!(d.day(), 26);
        assert!(extract_release_date("no date here").is_none());
        assert!(extract_release_date("released 99-99-99").is_none());
    }

    #[test]
    fn parse_release_payload_extracts_tag_and_date() {
        let json = serde_json::json!({
            "tag_name": "v2.0.0",
            "published_at": "2026-04-24T00:00:00Z"
        });
        let info = parse_release_payload(&json).unwrap();
        assert_eq!(info.version, "v2.0.0");
        assert_eq!(info.published_at.unwrap().year(), 2026);
    }

    #[test]
    fn parse_release_payload_rejects_missing_tag() {
        let json = serde_json::json!({});
        assert!(matches!(
            parse_release_payload(&json),
            Err(UpdateError::MissingTag)
        ));
        let json2 = serde_json::json!({ "tag_name": "" });
        assert!(matches!(
            parse_release_payload(&json2),
            Err(UpdateError::MissingTag)
        ));
    }

    #[test]
    fn install_command_unix_uses_bun() {
        let argv = build_install_command("v2.0.0", false);
        assert_eq!(
            argv,
            vec!["bun", "install", "-g", "@onebrain-ai/cli@v2.0.0"]
        );
    }

    #[test]
    fn install_command_windows_uses_powershell_with_npm() {
        let argv = build_install_command("v2.0.0", true);
        assert_eq!(argv.len(), 4);
        assert!(argv[0] == "pwsh" || argv[0] == "powershell.exe");
        assert_eq!(argv[1], "-NoProfile");
        assert_eq!(argv[2], "-Command");
        assert_eq!(argv[3], "npm install -g '@onebrain-ai/cli@v2.0.0'");
    }

    #[test]
    fn install_command_windows_escapes_single_quotes() {
        let argv = build_install_command("v'2.0.0", true);
        assert_eq!(argv[3], "npm install -g '@onebrain-ai/cli@v''2.0.0'");
    }

    #[test]
    fn version_command_unix() {
        assert_eq!(build_version_command(false), vec!["onebrain", "--version"]);
    }

    #[test]
    fn version_command_windows() {
        let argv = build_version_command(true);
        assert_eq!(argv.len(), 4);
        assert!(argv[0] == "pwsh" || argv[0] == "powershell.exe");
        assert_eq!(argv[3], "onebrain --version");
    }

    #[test]
    fn format_release_date_matches_en_gb() {
        let d = DateTime::parse_from_rfc3339("2026-04-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_release_date(d), "24 Apr 2026");
        let d2 = DateTime::parse_from_rfc3339("2026-01-04T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // No leading zero on day per JS en-GB output.
        assert_eq!(format_release_date(d2), "4 Jan 2026");
    }

    #[test]
    fn parse_current_version_output_round_trip() {
        let cv = parse_current_version_output("OneBrain v2.0.7 — released 2026-04-26");
        assert_eq!(cv.version, "v2.0.7");
        assert_eq!(cv.published_at.unwrap().day(), 26);
    }

    #[test]
    fn parse_current_version_falls_back_to_unknown() {
        let cv = parse_current_version_output("garbage");
        assert_eq!(cv.version, "unknown");
        assert!(cv.published_at.is_none());
    }
}
