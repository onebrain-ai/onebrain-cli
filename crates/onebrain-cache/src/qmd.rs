//! qmd MCP server query helpers.

use serde_json::Value;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Query `qmd status --json` and extract the `unembedded` count.
///
/// Returns `0` on any error, timeout, missing binary, or unexpected output —
/// matches Bun v2.3.3 `queryQmdUnembedded` (silent fallback so a missing or
/// broken qmd never blocks session-init).
///
/// Timeout: 2000ms.
pub fn query_unembedded_count() -> usize {
    let mut command = build_command();
    let mut child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return 0;
        }
    };

    let (tx, rx) = mpsc::channel::<Option<String>>();
    thread::spawn(move || {
        let mut buf = String::new();
        let mut stdout = stdout;
        let res = stdout.read_to_string(&mut buf).ok().map(|_| buf);
        let _ = tx.send(res);
    });

    let stdout_str = match rx.recv_timeout(Duration::from_millis(2000)) {
        Ok(Some(s)) => {
            let _ = child.wait();
            s
        }
        Ok(None) => {
            let _ = child.wait();
            return 0;
        }
        Err(_) => {
            // Timeout · kill child · return 0.
            let _ = child.kill();
            let _ = child.wait();
            return 0;
        }
    };

    let parsed: Value = match serde_json::from_str(&stdout_str) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    match parsed.get("unembedded") {
        Some(v) => v.as_u64().map(|n| n as usize).unwrap_or(0),
        None => 0,
    }
}

#[cfg(unix)]
fn build_command() -> Command {
    let mut c = Command::new("qmd");
    c.args(["status", "--json"]);
    c
}

#[cfg(windows)]
fn build_command() -> Command {
    let mut c = Command::new("powershell.exe");
    c.args(["-NoProfile", "-Command", "qmd status --json"]);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_zero_when_qmd_not_installed() {
        // In test environments qmd is rarely installed → spawn fails or returns
        // garbage · result must be 0 either way.
        let n = query_unembedded_count();
        // Can't assert exactly 0 in environments that *do* have qmd available,
        // but for CI/sandbox we expect 0. Just check it doesn't panic and
        // returns a usize. (No-op assertion.)
        let _ = n;
    }
}
