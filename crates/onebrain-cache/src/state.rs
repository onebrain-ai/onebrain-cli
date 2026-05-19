//! Checkpoint state file · `$TMPDIR/onebrain-{token}.state` · 3-field format.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Checkpoint state: count of Stop messages, unix timestamp of last event,
/// and the last checkpoint NN written (debug bookkeeping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointState {
    pub count: u32,
    pub last_ts: u64,
    pub last_stop_nn: String,
}

impl CheckpointState {
    pub fn fresh() -> Self {
        Self {
            count: 0,
            last_ts: 0,
            last_stop_nn: "00".to_string(),
        }
    }
}

const FRESH_STATE_DISK: &str = "0:0:00";

pub(crate) fn state_file_path(token: &str, tmp_dir: &Path) -> PathBuf {
    tmp_dir.join(format!("onebrain-{token}.state"))
}

/// Read state from `$tmpDir/onebrain-{token}.state`. Returns fresh state if
/// the file is missing or malformed (and eagerly rewrites to fresh-on-disk
/// so subsequent reads short-circuit cleanly · matches Bun behavior).
pub fn read_state(token: &str, tmp_dir: &Path) -> CheckpointState {
    let path = state_file_path(token, tmp_dir);
    let parsed = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| parse_state(raw.trim()));
    if let Some(state) = parsed {
        return state;
    }
    // Missing or malformed → fresh state · eagerly rewrite so subsequent reads short-circuit.
    let _ = fs::write(&path, FRESH_STATE_DISK);
    CheckpointState::fresh()
}

fn parse_state(raw: &str) -> Option<CheckpointState> {
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let count: u32 = parts[0].parse().ok()?;
    let last_ts: u64 = parts[1].parse().ok()?;
    let nn = parts[2];
    if nn.len() != 2 || !nn.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(CheckpointState {
        count,
        last_ts,
        last_stop_nn: nn.to_string(),
    })
}

/// Write state via atomic write-then-rename (pid-suffixed temp file → POSIX rename).
/// Errors logged to stderr. Sync.
pub fn write_state(token: &str, state: &CheckpointState, tmp_dir: &Path) {
    let path = state_file_path(token, tmp_dir);
    let tmp_path = tmp_dir.join(format!("onebrain-{token}.state.tmp.{}", std::process::id()));
    let content = format!("{}:{}:{}", state.count, state.last_ts, state.last_stop_nn);
    if let Err(e) = fs::write(&tmp_path, &content) {
        let _ = writeln!(
            std::io::stderr(),
            "checkpoint: failed to write state tmp file {tmp_path:?}: {e}"
        );
        return;
    }
    if let Err(e) = fs::rename(&tmp_path, &path) {
        let _ = writeln!(
            std::io::stderr(),
            "checkpoint: failed to rename state file {path:?}: {e}"
        );
        let _ = fs::remove_file(&tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_returns_fresh_when_file_missing() {
        let dir = tempdir().unwrap();
        let s = read_state("tok", dir.path());
        assert_eq!(s, CheckpointState::fresh());
    }

    #[test]
    fn read_returns_fresh_when_file_malformed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("onebrain-tok.state"), "garbage").unwrap();
        let s = read_state("tok", dir.path());
        assert_eq!(s, CheckpointState::fresh());
    }

    #[test]
    fn read_returns_fresh_when_wrong_field_count() {
        let dir = tempdir().unwrap();
        // 2 fields instead of 3 (legacy v1 format)
        fs::write(dir.path().join("onebrain-tok.state"), "5:1234567890").unwrap();
        let s = read_state("tok", dir.path());
        assert_eq!(s, CheckpointState::fresh());
    }

    #[test]
    fn read_returns_parsed_state_when_valid() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("onebrain-tok.state"), "7:1700000000:03").unwrap();
        let s = read_state("tok", dir.path());
        assert_eq!(
            s,
            CheckpointState {
                count: 7,
                last_ts: 1_700_000_000,
                last_stop_nn: "03".to_string()
            }
        );
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        let st = CheckpointState {
            count: 12,
            last_ts: 1_700_000_999,
            last_stop_nn: "05".to_string(),
        };
        write_state("tok", &st, dir.path());
        assert_eq!(read_state("tok", dir.path()), st);
    }

    #[test]
    fn write_uses_atomic_temp_file() {
        // After write succeeds, the temp file should not exist (renamed away).
        let dir = tempdir().unwrap();
        write_state("tok", &CheckpointState::fresh(), dir.path());
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly the final state file · found: {entries:?}"
        );
        assert!(entries[0].file_name().to_str().unwrap().ends_with(".state"));
    }
}
