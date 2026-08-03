//! `onebrain skill run` — spawn a headless agent harness on a OneBrain skill
//! with the vault as `cwd`, inheriting the parent process env (so PATH/HOME
//! survive for Homebrew lookups) with the `onebrain` binary's own directory
//! prepended to PATH (see [`child_path_with_exe_dir`] — under launchd the
//! parent has a minimal PATH, so this keeps nested `onebrain` calls from the
//! skill's own hooks resolving; #124). Used by the launchd scheduler to
//! dispatch OneBrain skills headlessly, and runnable manually from inside a
//! vault.
//!
//! Harnesses (`--harness`, default `claude`):
//!   - claude → `claude -p "<prompt>" --add-dir <vault> [--model <m>]`
//!   - gemini → `gemini -p "<prompt>" --include-directories <vault>
//!     --approval-mode yolo [-m <m>]` (yolo so a skill that runs `onebrain`
//!     shell commands or writes files isn't blocked on an approval prompt in
//!     an unattended run — same trust model as `claude -p` under the
//!     scheduler's settings allow-list).
//!
//! `ONEBRAIN_HEADLESS=1` is set on the child so `onebrain session init`
//! reports `headless: true`, which lets INSTRUCTIONS.md skip the interactive
//! startup ceremony (greeting + status + memory/inbox/task/orphan scans) and
//! go straight to the requested skill.
//!
//! The child's stdin is redirected from `/dev/null`: `claude -p` appends any
//! piped stdin to the prompt, so an inherited interactive TTY (no EOF) makes
//! it block forever. launchd already gives the child a null stdin, but a
//! manual terminal run does not — so we set it explicitly to keep both paths
//! non-blocking. stdout/stderr stay inherited so the user sees the output.
//!
//! Exit codes mirror Bun's `runSkillCommand`:
//!
//! - `78` (EX_CONFIG) — no OneBrain config (onebrain.yml / vault.yml) present.
//!   NOTE (#263): both CLI entry points (`skill run` and the legacy
//!   `run-skill` alias) now pre-resolve the vault via `vault_ctx::require`
//!   before calling this function, so a missing/invalid vault surfaces as
//!   `E_VAULT_NOT_FOUND` (exit 64) at the dispatch layer and this `78` guard
//!   is effectively unreachable through the CLI. It's kept only as
//!   defense-in-depth for any direct in-process caller of [`run`].
//! - `127` — couldn't run/track the child (spawn failed, e.g. the harness
//!   binary not on disk; or a fault while waiting on it)
//! - `128 + signal` — child terminated by signal (Unix only)
//! - any other code — propagated from child verbatim
//! - `1` — fallback when child exited with no code and no signal

use crate::cli::HarnessArg;
use anyhow::{anyhow, Context, Result};
use onebrain_core::find_config_file;
use onebrain_core::scheduler::run_record::{safe_tail, RunRecord, RunSource, TAIL_MAX_BYTES};
use onebrain_core::Harness;
use onebrain_fs::{
    build_prompt_for_harness, resolve_claude_bin, resolve_codex_bin, resolve_gemini_bin,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Entry point invoked from `main.rs`. Returns the exit code the binary
/// should call `std::process::exit` with.
pub fn run(
    vault: &str,
    skill: &str,
    args: &[String],
    harness: HarnessArg,
    model: Option<&str>,
    want_json: bool,
) -> Result<i32> {
    let vault_path = PathBuf::from(vault);
    if find_config_file(&vault_path).is_none() {
        eprintln!(
            "✗ Vault not found at {vault} (no onebrain.yml present)\n\
             💡 run this from inside a vault, or pass `--vault <path>` pointing at one"
        );
        return Ok(78); // EX_CONFIG (sysexits.h)
    }

    let pairs = parse_args(args)?;
    let core_harness = match harness {
        HarnessArg::Claude => Harness::Claude,
        HarnessArg::Gemini => Harness::Gemini,
        HarnessArg::Codex => Harness::Codex,
    };
    let prompt = build_prompt_for_harness(skill, &pairs, core_harness).map_err(|e| anyhow!(e))?;

    let env_lookup = |k: &str| std::env::var(k).ok();
    let path_exists = |p: &Path| p.exists();
    let home = std::env::var("HOME").ok();
    let resolution = match harness {
        HarnessArg::Claude => resolve_claude_bin(None, env_lookup, path_exists, home.as_deref()),
        HarnessArg::Gemini => resolve_gemini_bin(None, env_lookup, path_exists, home.as_deref()),
        HarnessArg::Codex => resolve_codex_bin(None, env_lookup, path_exists, home.as_deref()),
    };
    if let Some(warning) = &resolution.warning {
        // `warning`'s own text is kept stable (Bun-parity, see
        // `resolve_bin`'s doc comment) — only the wrapping hint is new.
        eprintln!("⚠ {warning}\n💡 fix or unset the env var above to silence this");
    }

    // Skills always run with-context: --add-dir / --include-directories the vault.
    let mut argv = harness_argv(harness, &prompt, Some(vault), model, want_json);
    add_managed_hook_trust(harness, &vault_path, &mut argv);

    let started = chrono::Local::now();
    let t0 = std::time::Instant::now();
    let outcome = spawn_harness(&resolution.path, &argv, &vault_path, harness, "the skill")?;

    // Finally-equivalent: everything below runs whether the harness succeeded,
    // failed, or was killed. A logging failure must NEVER change the exit code —
    // a mechanism that can kill the job it logs is the bug class this release
    // exists to remove (#372).
    let entry_name = skill.trim_start_matches('/').to_string();
    let log_note = write_job_log(&entry_name, &outcome.captured);
    let record = RunRecord {
        started,
        entry_name: entry_name.clone(),
        harness: Some(core_harness.as_str().to_string()),
        exit_code: outcome.code,
        duration_secs: t0.elapsed().as_secs(),
        machine: machine_name(),
        source: if std::env::var_os("ONEBRAIN_SCHEDULED").is_some() {
            RunSource::Scheduled
        } else {
            RunSource::Manual
        },
        output_tail: build_tail(&outcome.captured, log_note.as_deref()),
    };
    if let Err(e) = append_run_record(&vault_path, &record) {
        // Under the scheduler this eprintln has no reader (v3.4.23 removed the
        // plist redirect), which is exactly why the vault record is the primary
        // channel and this is only a courtesy for interactive runs.
        eprintln!("⚠ could not write the vault run record: {e}");
    }

    Ok(outcome.code)
}

/// Append the run's raw output to the CLI-owned job log.
///
/// Returns `Some(reason)` when the log could not be written, so the reason can
/// be carried into the vault record — under the scheduler nothing reads stderr,
/// so a suppressed log that only complained to stderr would be invisible. That
/// is the "fall back and *report*" half of the redirect change; without this it
/// would have no channel.
fn write_job_log(label: &str, captured: &[u8]) -> Option<String> {
    use onebrain_core::scheduler::run_log::{open_job_log, LogSink};
    use std::io::Write;

    let home = std::env::var("HOME").map(PathBuf::from).ok()?;
    let dir = onebrain_core::scheduler::log_dir::default_log_dir(&home, &|k| std::env::var(k).ok());
    match open_job_log(&dir, label) {
        LogSink::File(mut f, _) => {
            let _ = f.write_all(captured);
            let _ = f.flush();
            None
        }
        LogSink::Suppressed { reason } => Some(reason),
    }
}

/// The record's output tail, with a note appended when the raw log could not be
/// written — so the vault record reports the degradation rather than hiding it.
fn build_tail(captured: &[u8], log_note: Option<&str>) -> String {
    let text = String::from_utf8_lossy(captured);
    let mut tail = safe_tail(&text, TAIL_MAX_BYTES);
    if let Some(reason) = log_note {
        tail.push_str(&format!("\n\n[job log unavailable: {reason}]"));
    }
    tail
}

/// This machine's hostname, for telling records apart once vaults sync.
///
/// `$HOSTNAME` is NOT set under launchd, so reading the environment would make
/// every scheduled record read "unknown" — the one context this field exists
/// for. `gethostname(2)` is the only reliable source.
fn machine_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        // SAFETY: `buf` is a valid writable allocation of `buf.len()` bytes,
        // which is exactly the contract `gethostname` requires.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Ok(n) = std::env::var("COMPUTERNAME") {
            if !n.is_empty() {
                return n;
            }
        }
    }
    "unknown".to_string()
}

/// Append `record` to the vault skill log for its day.
///
/// `logs_folder` is resolved through the SAME validator the scheduler uses
/// (`resolve_logs_folder`), which rejects `..` and absolute paths. Every other
/// reader in the codebase takes `folders.logs` raw, which is fine for reading
/// but not for a `create_dir_all` + append running unattended at user uid.
fn append_run_record(vault: &Path, record: &RunRecord) -> Result<()> {
    use std::io::Write;

    let logs_folder = crate::commands::register_schedule::resolve_logs_folder(vault);
    let day = record.started.format("%Y-%m-%d").to_string();
    let dir = vault
        .join(&logs_folder)
        .join("log")
        .join(record.started.format("%Y").to_string())
        .join(record.started.format("%m").to_string());
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{day}-{}.md", record.entry_name));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    f.write_all(record.render().as_bytes())
        .with_context(|| format!("append to {}", path.display()))?;
    Ok(())
}

pub(crate) fn add_managed_hook_trust(harness: HarnessArg, vault: &Path, argv: &mut Vec<String>) {
    if harness == HarnessArg::Codex
        && crate::commands::codex_plugin::has_managed_installation(vault)
        && argv.first().is_some_and(|arg| arg == "exec")
    {
        argv.insert(1, "--dangerously-bypass-hook-trust".to_string());
    }
}

/// Build the argv (after the binary name) for the chosen harness. Pure so the
/// per-harness flag mapping is unit-testable. `pub(crate)` so `harness run`
/// can reuse it with a raw prompt instead of the `/onebrain:<skill>` form.
///
/// `context_dir = Some(<vault>)` injects `--add-dir <vault>` (claude) /
/// `--include-directories <vault>` (gemini) so the harness loads OneBrain's
/// CLAUDE.md / GEMINI.md. `context_dir = None` skips that flag — the
/// `--mode ad-hoc` path for `onebrain harness run`. Gemini always gets
/// `--approval-mode yolo` (we pipe stdin null so the harness can't answer an
/// approval prompt).
pub(crate) fn harness_argv(
    harness: HarnessArg,
    prompt: &str,
    context_dir: Option<&str>,
    model: Option<&str>,
    want_json: bool,
) -> Vec<String> {
    if harness == HarnessArg::Codex {
        let mut argv = vec![
            "exec".to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "--skip-git-repo-check".to_string(),
            "--ephemeral".to_string(),
        ];
        if let Some(dir) = context_dir {
            argv.push("-C".to_string());
            argv.push(dir.to_string());
        }
        if let Some(m) = model {
            argv.push("--model".to_string());
            argv.push(m.to_string());
        }
        if want_json {
            argv.push("--json".to_string());
        }
        argv.push(prompt.to_string());
        return argv;
    }
    let mut argv = vec!["-p".to_string(), prompt.to_string()];
    match harness {
        HarnessArg::Claude => {
            if let Some(dir) = context_dir {
                argv.push("--add-dir".to_string());
                argv.push(dir.to_string());
            }
            if let Some(m) = model {
                argv.push("--model".to_string());
                argv.push(m.to_string());
            }
            if want_json {
                argv.push("--output-format".to_string());
                argv.push("json".to_string());
            }
        }
        HarnessArg::Gemini => {
            if let Some(dir) = context_dir {
                argv.push("--include-directories".to_string());
                argv.push(dir.to_string());
            }
            // Auto-approve tools: the harness child has null stdin and can't
            // answer an approval prompt, and OneBrain skills run `onebrain`
            // shell commands + write vault files.
            argv.push("--approval-mode".to_string());
            argv.push("yolo".to_string());
            if let Some(m) = model {
                argv.push("-m".to_string());
                argv.push(m.to_string());
            }
            if want_json {
                argv.push("--output-format".to_string());
                argv.push("json".to_string());
            }
        }
        HarnessArg::Codex => unreachable!("handled above"),
    }
    argv
}

/// Convert clap's `Vec<String>` of `key=value` tokens into ordered pairs.
/// Reject malformed entries early — the scheduler is the only caller and we
/// want loud failure rather than silent passthrough.
fn parse_args(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for token in raw {
        let (k, v) = token
            .split_once('=')
            .with_context(|| format!("--arg expects key=value, got: {token}"))?;
        if k.is_empty() {
            return Err(anyhow!("--arg has empty key: {token}"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// Prepend `exe_dir` to `existing_path` (the child process PATH) so a headless
/// `claude` can find the `onebrain` binary even under launchd's minimal PATH.
/// Idempotent: if `existing_path` already starts with `exe_dir`, returns it
/// unchanged. Uses the platform PATH separator.
fn child_path_with_exe_dir(exe_dir: &str, existing_path: &str) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    if existing_path.is_empty() {
        exe_dir.to_string()
    } else if existing_path == exe_dir || existing_path.starts_with(&format!("{exe_dir}{sep}")) {
        existing_path.to_string()
    } else {
        format!("{exe_dir}{sep}{existing_path}")
    }
}

/// Spawn the chosen harness binary on the prepared argv. Shared by `skill run`
/// and `harness run` — `subject` is the noun in the spinner message
/// ("the skill" vs "the prompt") so the watched-run UI matches whichever
/// command the user invoked.
/// What a harness run produced: the exit code the binary should return, and
/// the child's output as we saw it.
///
/// `captured` feeds the CLI-owned job log and the vault run record. It is empty
/// when the child could not be spawned or observed at all — there is nothing to
/// record in that case, and an empty tail is honest about it.
pub(crate) struct HarnessOutcome {
    pub(crate) code: i32,
    pub(crate) captured: Vec<u8>,
}

impl HarnessOutcome {
    /// An outcome with a code but no output — spawn failed, or we lost the child.
    fn bare(code: i32) -> Self {
        Self {
            code,
            captured: Vec::new(),
        }
    }
}

/// A finished child plus everything it wrote.
struct Drained {
    status: std::process::ExitStatus,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
}

impl Drained {
    /// Pass the child's output through to our own streams, verbatim.
    ///
    /// stderr first (warnings, the YOLO note), then stdout (the result) — the
    /// natural reading order. Raw `write_all` on BYTES, never line-oriented
    /// printing: a `println!` per line would append a newline to a trailing
    /// partial line, lose the interleaving, and turn an I/O error or non-UTF-8
    /// output into a silently empty line. Errors are swallowed because the run
    /// already completed; a downstream pipe closing must not flip the exit code.
    fn write_through(&self) {
        use std::io::Write;
        {
            let stderr = std::io::stderr();
            let mut h = stderr.lock();
            let _ = h.write_all(&self.stderr_bytes);
            let _ = h.flush();
        }
        {
            let stdout = std::io::stdout();
            let mut h = stdout.lock();
            let _ = h.write_all(&self.stdout_bytes);
            let _ = h.flush();
        }
    }

    /// Both streams in the same order they are written through, for the log and
    /// the record tail.
    fn combined(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.stderr_bytes.len() + self.stdout_bytes.len());
        v.extend_from_slice(&self.stderr_bytes);
        v.extend_from_slice(&self.stdout_bytes);
        v
    }
}

/// Wait for `child` while draining BOTH pipes concurrently on threads.
///
/// The concurrency is the point, not a style choice. Reading one pipe to EOF
/// and only then calling `wait()` deadlocks as soon as the child fills the
/// other pipe's buffer: the child blocks writing, we block reading, neither
/// moves. `wait_with_output` avoids that internally but consumes the child
/// handle, so a `wait()` error would leave us unable to `kill()` the harness —
/// which would keep running and burning API tokens. Draining on threads gets
/// both properties.
fn wait_draining(child: &mut std::process::Child) -> std::io::Result<Drained> {
    use std::io::Read;
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let status = child.wait()?;
    Ok(Drained {
        status,
        stdout_bytes: stdout_thread.join().unwrap_or_default(),
        stderr_bytes: stderr_thread.join().unwrap_or_default(),
    })
}

pub(crate) fn spawn_harness(
    bin: &Path,
    argv: &[String],
    cwd: &Path,
    harness: HarnessArg,
    subject: &str,
) -> Result<HarnessOutcome> {
    use std::io::IsTerminal;
    let label = harness.as_str();
    let bin_env_var = match harness {
        HarnessArg::Claude => "CLAUDE_BIN",
        HarnessArg::Gemini => "GEMINI_BIN",
        HarnessArg::Codex => "CODEX_BIN",
    };
    // No `env` override beyond ONEBRAIN_HEADLESS: child inherits parent env so
    // PATH/HOME survive. stdout/stderr stay inherited so the harness's output
    // (and colour) reach the terminal verbatim; stdin is forced to null so
    // `claude -p` / `gemini -p` never block reading an interactive TTY — see
    // module docs. ONEBRAIN_HEADLESS=1 drives the session-init handshake that
    // lets the skill skip the interactive startup ceremony.
    let mut command = Command::new(bin);
    command
        .args(argv)
        .current_dir(cwd)
        .env("ONEBRAIN_HEADLESS", "1")
        .stdin(Stdio::null());

    // Under launchd, the parent `onebrain` process (spawned by launchd itself)
    // has a minimal PATH without e.g. `/opt/homebrew/bin`. The headless
    // `claude`/`gemini` child then can't resolve bare `onebrain` invocations
    // from its own skill hooks (session-init, checkpoint, etc.), which fail
    // with exit 78 (#124). Prepend our own binary's directory to the child's
    // PATH so nested `onebrain` calls resolve. Safe for interactive runs too
    // (the user's PATH already has it, so this is a no-op there).
    if let Ok(current_exe) = std::env::current_exe() {
        command.env("ONEBRAIN_BIN", &current_exe);
        let exe_dir = current_exe.parent().map(Path::to_path_buf);
        if let Some(exe_dir) = exe_dir {
            if let Some(dir) = exe_dir.to_str() {
                let existing = std::env::var("PATH").unwrap_or_default();
                command.env("PATH", child_path_with_exe_dir(dir, &existing));
            }
        }
    }

    // Non-interactive (launchd scheduler, piped, CI). This used to be a bare
    // `command.status()` with INHERITED stdio, which worked only because the
    // plist redirected stdout/stderr to a file. v3.4.23 removed that redirect
    // for skill-mode entries — it was the pre-exec failure path behind #315 and
    // #372 — so inherited output would now go to launchd's discard and vanish.
    // We capture it here instead, where a real process can also write it to its
    // own log and into the vault run record.
    //
    // Piped and drained on THREADS, exactly like the interactive branch below.
    // Reading one pipe to EOF before `wait()` deadlocks the moment the child
    // fills the other pipe's ~64 KB buffer, and `wait_with_output` consumes the
    // child handle, which would make the kill+reap recovery below impossible.
    if !std::io::stderr().is_terminal() {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "✗ Failed to spawn {label} ({}): {e}\n\
                     💡 make sure `{label}` is installed and on PATH (check with `which {label}`), \
                     or set `{bin_env_var}` to its full path",
                    bin.display()
                );
                return Ok(HarnessOutcome::bare(127));
            }
        };
        let drained = match wait_draining(&mut child) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "✗ Lost track of {label} while it was running: {e}\n\
                     💡 this is an OS/process fault, not a skill problem — rerun \
                     `onebrain skill run`"
                );
                let _ = child.kill();
                let _ = child.wait();
                return Ok(HarnessOutcome::bare(127));
            }
        };
        drained.write_through();
        return Ok(HarnessOutcome {
            code: translate_exit(&drained.status),
            captured: drained.combined(),
        });
    }

    // Interactive: the harness runs a full headless agent session (plugin load,
    // startup, the skill's own work, LLM inference) and buffers its result
    // until done — often tens of seconds. We pipe its stdout/stderr, show an
    // in-place `indicatif` spinner during the wait, then dump the captured
    // streams once on exit.
    //
    // ASSUMES: `claude -p` and `gemini -p` are buffered-flush-at-end (no
    // real-time streaming in their headless modes today). Capturing therefore
    // loses nothing visible and removes the spinner-vs-flush race that was the
    // v3.2.4 tradeoff that turned the wait into a wall of "still running"
    // newlines. If a future harness version starts streaming via `-p`, this
    // branch will swallow that streaming until exit — revisit then.
    //
    // The child has a null stdin (see module docs), so a harness can never
    // answer an approval prompt — gemini runs with `--approval-mode yolo`.
    // Note that explicitly on a watched run so it isn't a surprise.
    use indicatif::{ProgressBar, ProgressStyle};

    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let approval_note = match harness {
        HarnessArg::Gemini => " (auto-approving tools)",
        HarnessArg::Claude | HarnessArg::Codex => "",
    };
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg} · {elapsed}")
            .expect("static template is valid")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    spinner.set_message(format!(
        "Running {label} on {subject} headlessly{approval_note}"
    ));
    spinner.enable_steady_tick(std::time::Duration::from_millis(120));

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            spinner.finish_and_clear();
            eprintln!(
                "✗ Failed to spawn {label} ({}): {e}\n\
                 💡 make sure `{label}` is installed and on PATH (check with `which {label}`), \
                 or set `{bin_env_var}` to its full path",
                bin.display()
            );
            return Ok(HarnessOutcome::bare(127));
        }
    };

    // Drain both pipes concurrently on threads while we keep the child handle —
    // see `wait_draining`, which both this branch and the non-interactive one
    // use so the deadlock reasoning lives in exactly one place.
    let drained = match wait_draining(&mut child) {
        Ok(d) => d,
        Err(e) => {
            // Couldn't observe the child — a harness/OS fault, not a skill
            // failure, so 127 (the "couldn't run it properly" class) rather
            // than 1. kill+wait reaps the child so it doesn't keep running
            // (and burning API tokens) after we return.
            spinner.finish_and_clear();
            eprintln!(
                "✗ Lost track of {label} while it was running: {e}\n\
                 💡 this is an OS/process fault, not a skill problem — rerun `onebrain skill run`; \
                 if it persists, check system resource limits (open files/processes)"
            );
            let _ = child.kill();
            let _ = child.wait();
            return Ok(HarnessOutcome::bare(127));
        }
    };
    spinner.finish_and_clear();

    // Flush the captured streams verbatim — see `Drained::write_through`, which
    // both branches share. Explicit `flush` there covers the block-buffered
    // piped-stdout case (e.g. `onebrain skill run … | tee log`).
    drained.write_through();

    Ok(HarnessOutcome {
        code: translate_exit(&drained.status),
        captured: drained.combined(),
    })
}

#[cfg(unix)]
fn translate_exit(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = status.signal() {
        // POSIX convention: 128 + signal number. SIGTERM (15) -> 143, SIGKILL (9) -> 137.
        return 128 + sig;
    }
    status.code().unwrap_or(1)
}

#[cfg(not(unix))]
fn translate_exit(status: &std::process::ExitStatus) -> i32 {
    // Windows has no POSIX signals · just propagate the code or fall back to 1.
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_empty_returns_empty() {
        assert_eq!(parse_args(&[]).unwrap(), Vec::<(String, String)>::new());
    }

    #[test]
    fn parse_args_preserves_insertion_order() {
        let raw = vec![
            "first=1".to_string(),
            "second=2".to_string(),
            "third=3".to_string(),
        ];
        let parsed = parse_args(&raw).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("first".to_string(), "1".to_string()),
                ("second".to_string(), "2".to_string()),
                ("third".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_args_allows_empty_value() {
        // `topic=` is valid · the skill may interpret an empty value.
        let raw = vec!["topic=".to_string()];
        assert_eq!(
            parse_args(&raw).unwrap(),
            vec![("topic".to_string(), "".to_string())]
        );
    }

    #[test]
    fn parse_args_allows_equals_in_value() {
        let raw = vec!["filter=a=b".to_string()];
        assert_eq!(
            parse_args(&raw).unwrap(),
            vec![("filter".to_string(), "a=b".to_string())]
        );
    }

    #[test]
    fn parse_args_rejects_missing_equals() {
        let err = parse_args(&["badtoken".to_string()]).unwrap_err();
        assert!(err.to_string().contains("key=value"));
    }

    #[test]
    fn parse_args_rejects_empty_key() {
        let err = parse_args(&["=value".to_string()]).unwrap_err();
        assert!(err.to_string().contains("empty key"));
    }

    // ---- child_path_with_exe_dir (#124) ----

    #[test]
    fn child_path_empty_existing_returns_exe_dir() {
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", ""),
            "/opt/homebrew/bin"
        );
    }

    #[test]
    fn child_path_prepends_with_platform_separator() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", "/usr/bin:/bin"),
            format!("/opt/homebrew/bin{sep}/usr/bin:/bin")
        );
    }

    #[test]
    fn child_path_idempotent_when_existing_equals_exe_dir() {
        // Already exactly the exe dir · no change.
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", "/opt/homebrew/bin"),
            "/opt/homebrew/bin"
        );
    }

    #[test]
    fn child_path_idempotent_when_existing_already_prefixed() {
        // exe_dir already leads the PATH · no double-prepend. Build the input
        // with the platform separator so this holds on Windows (`;`) too — the
        // function's dedup guard checks the platform-specific `{exe_dir}{sep}`.
        let sep = if cfg!(windows) { ";" } else { ":" };
        let existing = format!("/opt/homebrew/bin{sep}/usr/bin");
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", &existing),
            existing
        );
    }

    #[test]
    fn child_path_does_not_false_dedup_on_shared_prefix() {
        // `/opt/homebrew/bin2` merely *starts with* the string
        // "/opt/homebrew/bin" but is a different directory · must still
        // prepend. This is why the guard checks `== exe_dir` or
        // `starts_with("{exe_dir}{sep}")`, not a bare `starts_with(exe_dir)`.
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            child_path_with_exe_dir("/opt/homebrew/bin", "/opt/homebrew/bin2:/usr/bin"),
            format!("/opt/homebrew/bin{sep}/opt/homebrew/bin2:/usr/bin")
        );
    }

    // ---- harness_argv ----

    #[test]
    fn harness_argv_claude_uses_add_dir_and_no_model_by_default() {
        let argv = harness_argv(
            HarnessArg::Claude,
            "/onebrain:daily",
            Some("/vault"),
            None,
            false,
        );
        assert_eq!(argv, vec!["-p", "/onebrain:daily", "--add-dir", "/vault"]);
    }

    #[test]
    fn harness_argv_claude_appends_model_flag() {
        let argv = harness_argv(
            HarnessArg::Claude,
            "/onebrain:daily",
            Some("/vault"),
            Some("claude-haiku-4-5"),
            false,
        );
        assert_eq!(
            argv,
            vec![
                "-p",
                "/onebrain:daily",
                "--add-dir",
                "/vault",
                "--model",
                "claude-haiku-4-5"
            ]
        );
    }

    #[test]
    fn harness_argv_gemini_uses_include_directories_and_yolo() {
        let argv = harness_argv(
            HarnessArg::Gemini,
            "/onebrain:daily",
            Some("/vault"),
            None,
            false,
        );
        assert_eq!(
            argv,
            vec![
                "-p",
                "/onebrain:daily",
                "--include-directories",
                "/vault",
                "--approval-mode",
                "yolo"
            ]
        );
    }

    #[test]
    fn harness_argv_claude_omits_add_dir_when_context_dir_none() {
        // `--mode ad-hoc`: no `--add-dir` flag, no vault context loaded.
        let argv = harness_argv(HarnessArg::Claude, "what is 2+2?", None, None, false);
        assert_eq!(argv, vec!["-p", "what is 2+2?"]);
    }

    #[test]
    fn harness_argv_gemini_omits_include_dirs_but_keeps_yolo_for_ad_hoc() {
        // `--mode ad-hoc`: no `--include-directories`, but yolo stays because
        // the child still has null stdin and can't answer approval prompts.
        let argv = harness_argv(HarnessArg::Gemini, "what is 2+2?", None, None, false);
        assert_eq!(argv, vec!["-p", "what is 2+2?", "--approval-mode", "yolo"]);
    }

    #[test]
    fn harness_argv_claude_appends_output_format_json_when_want_json() {
        // `--json` at the OneBrain CLI maps to `--output-format json` on the
        // claude harness — passthrough so the captured stdout is native JSON.
        let argv = harness_argv(HarnessArg::Claude, "hi", None, None, true);
        assert_eq!(argv, vec!["-p", "hi", "--output-format", "json"]);
    }

    #[test]
    fn harness_argv_gemini_appends_output_format_json_when_want_json() {
        // Gemini supports `--output-format json` too. Sits after `--approval-mode
        // yolo` and after `-m <model>` if present.
        let argv = harness_argv(HarnessArg::Gemini, "hi", None, None, true);
        assert_eq!(
            argv,
            vec![
                "-p",
                "hi",
                "--approval-mode",
                "yolo",
                "--output-format",
                "json"
            ]
        );
    }

    #[test]
    fn harness_argv_gemini_appends_short_model_flag() {
        let argv = harness_argv(
            HarnessArg::Gemini,
            "/onebrain:daily",
            Some("/vault"),
            Some("gemini-2.5-flash"),
            false,
        );
        assert_eq!(
            argv,
            vec![
                "-p",
                "/onebrain:daily",
                "--include-directories",
                "/vault",
                "--approval-mode",
                "yolo",
                "-m",
                "gemini-2.5-flash"
            ]
        );
    }

    #[test]
    fn harness_argv_codex_uses_exec_workspace_and_ephemeral() {
        assert_eq!(
            harness_argv(
                HarnessArg::Codex,
                "$onebrain:daily",
                Some("/vault"),
                Some("gpt-5"),
                true
            ),
            vec![
                "exec",
                "--sandbox",
                "workspace-write",
                "--skip-git-repo-check",
                "--ephemeral",
                "-C",
                "/vault",
                "--model",
                "gpt-5",
                "--json",
                "$onebrain:daily"
            ]
        );
    }

    #[test]
    fn vault_local_codex_marker_alone_does_not_add_hook_trust_bypass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".codex")).unwrap();
        std::fs::write(
            dir.path().join(".codex/onebrain-plugin.json"),
            r#"{"managed":true,"plugin":"onebrain@onebrain"}"#,
        )
        .unwrap();
        let mut argv = vec!["exec".to_string(), "prompt".to_string()];
        add_managed_hook_trust(HarnessArg::Codex, dir.path(), &mut argv);
        assert_eq!(argv, ["exec", "prompt"]);
    }

    /// Exercises the spawn path + exit-code translation with the harness stubbed
    /// by a script that exits 43 on EOF / 42 after reading a line.
    ///
    /// NOTE: under `cargo test` the harness's own stdin is already at EOF, so
    /// this cannot *distinguish* a null stdin from an inherited one (both yield
    /// 43) — it pins the EOF→exit-43 contract, not the `.stdin(null)` line
    /// itself. The real fix (an interactive TTY no longer hangs `-p`) is
    /// verified manually; a hermetic guard would need to inject a live parent
    /// stdin, which `spawn_harness` (it reads the process's fd 0) can't take
    /// without an API change made solely for the test.
    #[cfg(unix)]
    #[test]
    fn spawn_harness_gives_child_a_non_blocking_stdin() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("harness_stub.sh");
        {
            let mut f = std::fs::File::create(&stub).unwrap();
            writeln!(
                f,
                "#!/bin/sh\nif IFS= read -r _; then exit 42; else exit 43; fi"
            )
            .unwrap();
        }
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argv = vec!["-p".to_string(), "/daily".to_string()];
        let code = spawn_harness(&stub, &argv, dir.path(), HarnessArg::Claude, "the test")
            .unwrap()
            .code;
        assert_eq!(code, 43, "child should see EOF on a null stdin, not block");
    }
}
