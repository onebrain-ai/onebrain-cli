//! The CLI's OWN job log — opened after exec, unlike the launchd redirect it
//! replaces for skill-mode entries (see launchd.rs). Running as a real process
//! means a missing directory is recoverable: we create it, and if we cannot, we
//! say so instead of dying with zero output the way #372 did.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum LogSink {
    File(File, PathBuf),
    /// No file could be opened. The run continues regardless — logging must
    /// never be able to kill the job it logs.
    Suppressed {
        reason: String,
    },
}

/// Open (creating as needed) the append-only raw log for `label`.
///
/// Never returns an error: a job must not die because its log could not be
/// opened. That is precisely the failure this release removes — the launchd
/// redirect killed runs pre-exec for exactly this reason (#372).
pub fn open_job_log(log_dir: &Path, label: &str) -> LogSink {
    if let Err(e) = fs::create_dir_all(log_dir) {
        return LogSink::Suppressed {
            reason: format!("cannot create {}: {e}", log_dir.display()),
        };
    }
    let path = log_dir.join(format!("onebrain-{label}.log"));
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => LogSink::File(f, path),
        Err(e) => LogSink::Suppressed {
            reason: format!("cannot open {}: {e}", path.display()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_missing_log_directory_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("gone").join("deeper");
        assert!(!missing.exists(), "fixture must start without the dir");

        match open_job_log(&missing, "daily") {
            LogSink::File(_, p) => {
                assert!(p.exists(), "log file created at {p:?}");
                assert!(missing.is_dir(), "the missing directory was created");
            }
            LogSink::Suppressed { reason } => panic!("should have created it: {reason}"),
        }
    }

    #[test]
    fn suppresses_instead_of_failing_when_the_directory_cannot_be_created() {
        // A FILE where the directory should be — create_dir_all cannot win.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocked");
        fs::write(&blocker, b"i am a file, not a directory").unwrap();

        match open_job_log(&blocker, "daily") {
            LogSink::Suppressed { reason } => assert!(!reason.is_empty(), "must say why"),
            LogSink::File(_, p) => panic!("must not claim a file at {p:?}"),
        }
    }

    #[test]
    fn suppresses_when_the_open_itself_fails_after_the_directory_exists() {
        // The OTHER `Suppressed` arm: `create_dir_all` succeeds (the directory
        // is real), but the exact log-file path is occupied by a directory, so
        // `OpenOptions::open` fails with EISDIR. Distinct from the fixture above,
        // which never reaches `OpenOptions::open` at all.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        fs::create_dir_all(&dir).unwrap();
        let occupied = dir.join("onebrain-daily.log");
        fs::create_dir_all(&occupied).unwrap();

        match open_job_log(&dir, "daily") {
            LogSink::Suppressed { reason } => assert!(!reason.is_empty(), "must say why"),
            LogSink::File(_, p) => panic!("must not claim a file at {p:?}"),
        }
    }

    #[test]
    fn appends_rather_than_truncating_across_runs() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");

        let LogSink::File(mut f1, p1) = open_job_log(&dir, "daily") else {
            panic!("first open must succeed");
        };
        f1.write_all(b"run one\n").unwrap();
        drop(f1);

        let LogSink::File(mut f2, p2) = open_job_log(&dir, "daily") else {
            panic!("second open must succeed");
        };
        assert_eq!(p1, p2, "same label resolves to the same file");
        f2.write_all(b"run two\n").unwrap();
        drop(f2);

        let body = fs::read_to_string(&p2).unwrap();
        assert!(body.contains("run one"), "prior content survived: {body:?}");
        assert!(body.contains("run two"), "new content appended: {body:?}");
    }
}
