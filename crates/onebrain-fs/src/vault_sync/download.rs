//! Step 1 — download the upstream tarball and extract it.
//!
//! Port of Bun's `downloadTarball` + `extractTarball` + `buildTarSpawnOverrides`
//! from vault-sync.ts. The Rust implementation differs in one way: we use the
//! pure-Rust `tar` + `flate2` crates instead of spawning the system `tar` CLI.
//! That removes the Windows MSYS drive-letter footgun (Bun issue #126) entirely,
//! but we keep `build_tar_spawn_overrides` exported so parity tests can pin the
//! historical behavior.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use super::types::FetchFn;

/// GitHub tarball API base. The branch is appended raw — caller is responsible
/// for URL-safety, which is trivial in practice (branch names are git refs).
const TARBALL_BASE: &str = "https://api.github.com/repos/onebrain-ai/onebrain/tarball/";

/// Default fetch implementation — a blocking reqwest GET that returns the raw
/// body bytes on 2xx and an error string with Bun's hint table on 4xx/5xx.
///
/// Test escape hatch: when the `ONEBRAIN_VAULT_SYNC_FIXTURE` environment variable
/// is set, the fetch reads bytes from that local file path instead of hitting
/// the network. This lets `assert_cmd` integration tests exercise the full CLI
/// flow without a real GitHub request. Production users never see this; CI
/// builds keep the var unset.
pub fn default_fetch_fn() -> FetchFn {
    Box::new(|url: &str| -> Result<Vec<u8>, String> {
        if let Ok(fixture) = std::env::var("ONEBRAIN_VAULT_SYNC_FIXTURE") {
            return std::fs::read(&fixture).map_err(|e| format!("fixture read {fixture}: {e}"));
        }
        let client = reqwest::blocking::Client::builder()
            .user_agent("onebrain-cli")
            .build()
            .map_err(|e| format!("HTTP client init failed: {e}"))?;
        let resp = client
            .get(url)
            .send()
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format_http_error(status.as_u16(), url));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| format!("read body: {e}"))
    })
}

/// Build the HTTP error message for a non-2xx status. Matches Bun's
/// "HTTP <code> downloading tarball from <url><hint>" format.
pub(crate) fn format_http_error(status: u16, url: &str) -> String {
    let hint = match status {
        403 => " — check repo permissions or GITHUB_TOKEN",
        404 => " — repo or branch not found",
        429 => " — rate limited, wait and retry",
        _ => "",
    };
    format!("HTTP {status} downloading tarball from {url}{hint}")
}

/// Compose the tarball URL for a branch.
pub(crate) fn tarball_url(branch: &str) -> String {
    // The Bun source appends `branch` raw — we mirror that exactly so the
    // request stays byte-identical for parity diffs.
    format!("{TARBALL_BASE}{branch}")
}

/// Step 1a — invoke the fetch hook to download the tarball bytes.
pub fn download_tarball(branch: &str, fetch: &FetchFn) -> Result<Vec<u8>, String> {
    let url = tarball_url(branch);
    fetch(&url)
}

/// Step 1b — extract a gzipped tarball into `dest_dir` and return the path of
/// the single top-level directory inside the archive.
///
/// Bun's contract: any error here is critical → propagated as the sync's
/// `error` field. The top-level directory is named `onebrain-ai-onebrain-<sha>/`
/// in real GitHub archives; tests use `onebrain-ai-onebrain-abc1234/`.
pub fn extract_tarball(bytes: &[u8], dest_dir: &Path) -> std::io::Result<PathBuf> {
    let cursor = Cursor::new(bytes);
    let decoder = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest_dir)?;

    // Find the top-level directory. Real archives contain exactly one folder
    // at the root; we mirror Bun's `entries.find(e => e !== 'bundle.tar.gz')`
    // but since we never write the tarball to disk first, the search is over
    // the freshly-unpacked entries only.
    for entry in fs::read_dir(dest_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            return Ok(entry.path());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Extracted tarball contains no top-level directory",
    ))
}

/// Build the spawn-option overrides for a tar subprocess.
///
/// Historical context: Bun shells out to the system `tar` CLI; on Windows
/// MSYS / Git Bash, GNU tar parses paths containing a colon as `host:path`
/// and tries to SSH to the drive letter. Setting `TAR_OPTIONS=--force-local`
/// suppresses that behavior. macOS/BSD tar ignores the env var.
///
/// Our Rust implementation does NOT spawn `tar` — we use the pure-Rust crate
/// instead. This helper is kept exported for parity tests and to document the
/// workaround for anyone porting the Bun semantics elsewhere. Always returns
/// an empty map on non-windows platforms; returns `{"TAR_OPTIONS": "--force-local"}`
/// on windows (overriding any pre-existing user value, matching Bun's policy).
pub fn build_tar_spawn_overrides(
    platform: &str,
    parent_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    if platform != "windows" && platform != "win32" {
        return HashMap::new();
    }
    let mut env = parent_env.clone();
    env.insert("TAR_OPTIONS".to_string(), "--force-local".to_string());
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // --- format_http_error / tarball_url ---

    #[test]
    fn http_error_403_includes_token_hint() {
        let msg = format_http_error(403, "https://example/x");
        assert!(msg.contains("403"));
        assert!(msg.contains("check repo permissions or GITHUB_TOKEN"));
    }

    #[test]
    fn http_error_404_includes_branch_hint() {
        let msg = format_http_error(404, "https://example/x");
        assert!(msg.contains("404"));
        assert!(msg.contains("repo or branch not found"));
    }

    #[test]
    fn http_error_429_includes_ratelimit_hint() {
        let msg = format_http_error(429, "https://example/x");
        assert!(msg.contains("429"));
        assert!(msg.contains("rate limited"));
    }

    #[test]
    fn http_error_500_has_no_hint() {
        let msg = format_http_error(500, "https://example/x");
        assert!(msg.contains("500"));
        assert!(!msg.contains("—"));
    }

    #[test]
    fn tarball_url_appends_branch_raw() {
        assert_eq!(
            tarball_url("main"),
            "https://api.github.com/repos/onebrain-ai/onebrain/tarball/main"
        );
        assert_eq!(
            tarball_url("next"),
            "https://api.github.com/repos/onebrain-ai/onebrain/tarball/next"
        );
    }

    // --- download_tarball with injected fetch ---

    #[test]
    fn download_tarball_with_injected_fetch_returns_bytes() {
        let canned: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00];
        let canned_clone = canned.clone();
        let fetch: FetchFn = Box::new(move |_url: &str| Ok(canned_clone.clone()));
        let got = download_tarball("main", &fetch).expect("should succeed");
        assert_eq!(got, canned);
    }

    #[test]
    fn download_tarball_propagates_fetch_error() {
        let fetch: FetchFn =
            Box::new(|_url: &str| Err("HTTP 404 — repo or branch not found".into()));
        let got = download_tarball("ghost-branch", &fetch);
        let err = got.unwrap_err();
        assert!(err.contains("404"));
    }

    // --- extract_tarball ---

    fn build_test_tarball(prefix: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut buf, Compression::default());
            let mut builder = tar::Builder::new(enc);
            for (rel, content) in files {
                let path = format!("{prefix}/{rel}");
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, &path, *content)
                    .expect("append");
            }
            builder.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn extract_valid_tarball_returns_top_level() {
        let bytes = build_test_tarball(
            "onebrain-ai-onebrain-abc1234",
            &[("README.md", b"# Hello\n"), ("dir/inner.txt", b"x")],
        );
        let dir = tempdir().unwrap();
        let top = extract_tarball(&bytes, dir.path()).expect("ok");
        assert!(top.ends_with("onebrain-ai-onebrain-abc1234"));
        assert!(top.join("README.md").exists());
        assert!(top.join("dir/inner.txt").exists());
    }

    #[test]
    fn extract_corrupted_tarball_returns_error() {
        let dir = tempdir().unwrap();
        let res = extract_tarball(&[0u8, 1, 2, 3, 0xff, 0xfe], dir.path());
        assert!(res.is_err());
    }

    #[test]
    fn extract_archive_with_no_directory_returns_error() {
        // A tarball whose only entries are top-level files (no enclosing dir)
        // should error out — Bun's findFirstDir loop hits the same case.
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut buf, Compression::default());
            let mut builder = tar::Builder::new(enc);
            let content = b"hi";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            // Single file at archive root, no directory.
            builder
                .append_data(&mut header, "loose-file.txt", &content[..])
                .expect("append");
            builder.finish().expect("finish");
        }
        let dir = tempdir().unwrap();
        let res = extract_tarball(&buf, dir.path());
        assert!(res.is_err());
    }

    // --- build_tar_spawn_overrides ---

    #[test]
    fn build_overrides_darwin_returns_empty() {
        let parent: HashMap<_, _> = [("PATH".into(), "/usr/bin".into())].into();
        assert!(build_tar_spawn_overrides("darwin", &parent).is_empty());
    }

    #[test]
    fn build_overrides_linux_returns_empty() {
        let parent: HashMap<_, _> = [("PATH".into(), "/usr/bin".into())].into();
        assert!(build_tar_spawn_overrides("linux", &parent).is_empty());
    }

    #[test]
    fn build_overrides_windows_sets_force_local_and_preserves_parent() {
        let parent: HashMap<_, _> = [
            ("PATH".into(), "C:\\Windows".into()),
            ("FOO".into(), "bar".into()),
        ]
        .into();
        let got = build_tar_spawn_overrides("win32", &parent);
        assert_eq!(got.get("TAR_OPTIONS").unwrap(), "--force-local");
        assert_eq!(got.get("PATH").unwrap(), "C:\\Windows");
        assert_eq!(got.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn build_overrides_windows_overrides_user_set_tar_options() {
        let parent: HashMap<_, _> = [("TAR_OPTIONS".into(), "--verbose".into())].into();
        let got = build_tar_spawn_overrides("windows", &parent);
        assert_eq!(got.get("TAR_OPTIONS").unwrap(), "--force-local");
    }
}
