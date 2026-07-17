//! The Tier-1 raw gain log (design 0030): append-only JSONL, one file per
//! calendar month (`gain/YYYY-MM.jsonl`), the source of truth Track 2's
//! rollups are rebuilt from. No transaction cost — a plain file append —
//! and crash-tolerant: a partial last line (process killed mid-write) is
//! silently skipped on read rather than poisoning the whole file.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};

use super::event::GainEvent;
use super::GainSink;

/// Appends [`GainEvent`]s to `<dir>/YYYY-MM.jsonl`, rotating on the event's
/// own timestamp (not wall-clock at write time — deterministic for tests
/// and safe for events recorded slightly out of real-time order).
#[derive(Debug, Clone)]
pub struct JsonlGainWriter {
    dir: PathBuf,
}

impl JsonlGainWriter {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        JsonlGainWriter { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file_for_ts(&self, ts: i64) -> PathBuf {
        // Out-of-range `ts` falls back to the Unix epoch, mirroring
        // `rollup::utc_or_epoch`: a corrupt timestamp lands in an obviously
        // wrong, self-flagging `1970-01.jsonl` instead of silently polluting
        // the current month's live file.
        let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        self.dir
            .join(format!("{:04}-{:02}.jsonl", dt.year(), dt.month()))
    }

    /// Append one event to its month's file, creating the directory and
    /// file as needed.
    pub fn append(&self, event: &GainEvent) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.file_for_ts(event.ts);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Reads every well-formed event across all monthly files directly in
    /// the directory (non-recursive — a subdirectory such as `archive/` is
    /// NOT descended into), oldest file first. A truncated or corrupt final
    /// line in any file (the crash-tolerance case) is skipped, not an error.
    pub fn read_all(&self) -> io::Result<Vec<GainEvent>> {
        let mut out = Vec::new();
        if !self.dir.exists() {
            return Ok(out);
        }

        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        read_events_from_files(&files, &mut out)?;
        Ok(out)
    }

    /// Like [`read_all`](Self::read_all), but also descends into
    /// subdirectories — in particular the `archive/<ts>-<label>/` epoch dirs
    /// `onebrain token gain --reset` creates. A rollup rebuild uses this so
    /// it always reflects every event ever recorded, archived or not
    /// (design §5: "archived epochs remain queryable").
    pub fn read_all_recursive(&self) -> io::Result<Vec<GainEvent>> {
        let mut out = Vec::new();
        collect_recursive(&self.dir, &mut out)?;
        Ok(out)
    }
}

fn read_events_from_files(files: &[PathBuf], out: &mut Vec<GainEvent>) -> io::Result<()> {
    for path in files {
        let file = match fs::File::open(path) {
            Ok(f) => f,
            // TOCTOU (#283): `token gain --reset` renames monthly files into
            // `archive/` between the directory listing and this open. A file
            // that vanished mid-walk contributes zero events — never fail a
            // concurrent read (e.g. a live dashboard poll) over it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for line in io::BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<GainEvent>(&line) {
                out.push(event);
            }
            // Malformed/partial line (e.g. crash mid-write): skip it, don't
            // fail the whole read.
        }
    }
    Ok(())
}

fn collect_recursive(dir: &Path, out: &mut Vec<GainEvent>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    let files: Vec<PathBuf> = entries
        .iter()
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .cloned()
        .collect();
    read_events_from_files(&files, out)?;

    for dir_entry in entries.iter().filter(|p| p.is_dir()) {
        collect_recursive(dir_entry, out)?;
    }
    Ok(())
}

impl GainSink for JsonlGainWriter {
    fn record(&mut self, event: GainEvent) {
        // `GainSink::record` has no way to propagate a `Result` — I/O
        // failure here is swallowed rather than panicking a hot path.
        // Track 3's daemon-side sink can surface failures through its own
        // channel; this crate stays pure-adjacent (its one I/O boundary
        // fails soft).
        let _ = self.append(&event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gain::event::{CacheKind, Surface};
    use crate::level::OptLevel;
    use tempfile::tempdir;

    fn sample_event(ts: i64) -> GainEvent {
        GainEvent {
            ts,
            surface: Surface::CliSearch,
            transform: "whitespace".to_string(),
            level: OptLevel::Conservative,
            bytes_before: 500,
            bytes_after: 300,
            cache: CacheKind::None,
            session_token: Some("sess-1".to_string()),
        }
    }

    #[test]
    fn appends_and_reads_back_events() {
        let dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(dir.path());

        writer.append(&sample_event(1_752_000_000)).unwrap();
        writer.append(&sample_event(1_752_000_100)).unwrap();

        let events = writer.read_all().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn rotates_files_monthly_by_event_timestamp() {
        let dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(dir.path());

        // 2026-06-15 and 2026-07-15 (UTC) — different months.
        writer.append(&sample_event(1_781_000_000)).unwrap();
        writer.append(&sample_event(1_783_800_000)).unwrap();

        let mut files: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert_eq!(
            files.len(),
            2,
            "expected two distinct monthly files: {files:?}"
        );
    }

    #[test]
    fn tolerates_a_truncated_last_line() {
        let dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(dir.path());
        writer.append(&sample_event(1_752_000_000)).unwrap();

        // Simulate a crash mid-write: append a partial JSON line with no
        // trailing newline.
        let path = writer.file_for_ts(1_752_000_000);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{{\"ts\":1752000200,\"surface\":\"cli_se").unwrap();

        let events = writer.read_all().unwrap();
        assert_eq!(
            events.len(),
            1,
            "the well-formed line must still be readable; the partial line must be skipped, not fatal"
        );
    }

    #[test]
    fn read_all_on_missing_dir_returns_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let writer = JsonlGainWriter::new(missing);
        assert_eq!(writer.read_all().unwrap(), Vec::new());
    }

    #[test]
    fn read_all_ignores_a_subdirectory() {
        // `read_all` (non-recursive) must not descend into `archive/` — that
        // is `read_all_recursive`'s job.
        let dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(dir.path());
        writer.append(&sample_event(1_752_000_000)).unwrap();

        let archived = dir.path().join("archive").join("epoch-1");
        fs::create_dir_all(&archived).unwrap();
        let archived_writer = JsonlGainWriter::new(&archived);
        archived_writer
            .append(&sample_event(1_700_000_000))
            .unwrap();

        let events = writer.read_all().unwrap();
        assert_eq!(events.len(), 1, "archived subdirectory must be skipped");
    }

    #[test]
    fn read_all_recursive_includes_archived_epochs() {
        let dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(dir.path());
        writer.append(&sample_event(1_752_000_000)).unwrap();

        let archived = dir.path().join("archive").join("1700000000-baseline");
        fs::create_dir_all(&archived).unwrap();
        let archived_writer = JsonlGainWriter::new(&archived);
        archived_writer
            .append(&sample_event(1_700_000_000))
            .unwrap();
        archived_writer
            .append(&sample_event(1_700_000_100))
            .unwrap();

        let events = writer.read_all_recursive().unwrap();
        assert_eq!(
            events.len(),
            3,
            "recursive read must include both current and archived events"
        );
    }

    #[test]
    fn read_all_recursive_on_missing_dir_returns_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let writer = JsonlGainWriter::new(missing);
        assert_eq!(writer.read_all_recursive().unwrap(), Vec::new());
    }

    /// TOCTOU (#283): `token gain --reset` renames monthly files into
    /// `archive/` between a reader's directory listing and its `File::open`.
    /// A file that vanished mid-walk must contribute zero events, not fail the
    /// whole read (a live dashboard polling during a reset would 500).
    #[test]
    fn read_tolerates_a_file_renamed_away_mid_walk() {
        let dir = tempdir().unwrap();
        let writer = JsonlGainWriter::new(dir.path());
        // Two months → two files.
        writer.append(&sample_event(1_781_000_000)).unwrap(); // 2026-06
        writer.append(&sample_event(1_783_800_000)).unwrap(); // 2026-07

        // Snapshot the listing, THEN delete one file — exactly the race window.
        let mut files: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 2);
        fs::remove_file(&files[0]).unwrap();

        let mut out = Vec::new();
        read_events_from_files(&files, &mut out)
            .expect("a file renamed away mid-walk must not fail the read");
        assert_eq!(out.len(), 1, "the surviving file's events are still read");
    }
}
