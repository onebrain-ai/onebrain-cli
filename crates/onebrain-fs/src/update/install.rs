//! Self-update install path: fetch the GitHub Release tarball for the
//! running platform and swap the binary in place.
//!
//! Why this exists: the pre-alpha.9 install path called
//! `bun install -g @onebrain-ai/cli@<v>` / `npm install -g …`, but the v3.x
//! Rust binary was never published to npm — every `onebrain update` from
//! alpha.1 through alpha.8 ultimately bailed with "package exists but version
//! not found." Real-world `onebrain update` was broken for the entire alpha
//! cycle. This module replaces that path with a direct GitHub-Release fetch
//! + atomic binary swap.
//!
//! Trust model: the binary is fetched over HTTPS from
//! `github.com/onebrain-ai/onebrain-cli/releases/download/...` and verified
//! against the published `<archive>.sha256` before the swap (v3.1.4). The
//! GitHub TLS chain authenticates the transport; the checksum closes the gap
//! where a corrupted or tampered asset would otherwise be installed. An
//! unverifiable asset (missing/malformed `.sha256`, or a mismatch) is a hard
//! failure — we never swap it in. Signature (cosign) verification remains a
//! follow-up for once the release pipeline signs its artifacts.
//!
//! Install channel: a Homebrew-managed binary lives in the Cellar behind a
//! `brew` symlink. Swapping it in place would desync brew's metadata, so
//! [`detect_install_channel`] routes those installs to `brew upgrade` instead
//! of the direct fetch + swap.

use super::UpdateError;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const DOWNLOAD_TIMEOUT_SECS: u64 = 90;
const RELEASES_DOWNLOAD_BASE: &str =
    "https://github.com/onebrain-ai/onebrain-cli/releases/download";

/// Env var override for the download base URL — lets integration tests redirect
/// the request to a local mockito server without bypassing the rest of the
/// install pipeline. Mirrors `ONEBRAIN_GITHUB_RELEASES_URL` for the API side.
const DOWNLOAD_ENV_OVERRIDE: &str = "ONEBRAIN_GITHUB_RELEASES_DOWNLOAD_URL";

/// Fetch the release asset for the running target triple, extract the
/// `onebrain` binary, and atomically replace `current_exe` with it.
///
/// On success the running process is unaffected (the new binary takes over
/// only on next invocation). On any failure the current binary is left
/// untouched — including the case where the rename of the new binary into
/// place itself fails after the .old rename succeeded (we roll back).
pub(crate) fn fetch_and_swap_binary(version: &str, current_exe: &Path) -> Result<(), UpdateError> {
    let asset = AssetInfo::for_running_target()?;
    let tag = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    let base =
        std::env::var(DOWNLOAD_ENV_OVERRIDE).unwrap_or_else(|_| RELEASES_DOWNLOAD_BASE.to_string());
    let url = format!("{base}/{tag}/onebrain-{}.{}", asset.triple, asset.extension);

    let archive_bytes = download_archive(&url)?;
    // Verify against the published `<archive>.sha256` BEFORE we decode or
    // swap. A failure here aborts the update with the live binary untouched.
    verify_archive_checksum(&url, &archive_bytes)?;
    let new_binary_bytes = asset.extract_binary(&archive_bytes)?;
    swap_binary(current_exe, &new_binary_bytes)
}

/// Encapsulates the per-target-triple bits of asset naming and archive
/// format. One asset per `(target_arch, target_os, target_env)` triple — we
/// resolve the running triple via `cfg!` so a downloaded onebrain binary
/// always matches the host platform.
struct AssetInfo {
    triple: &'static str,
    extension: &'static str,
    /// File name inside the archive (`onebrain` on Unix, `onebrain.exe` on
    /// Windows). The release-side packaging step uses the same convention.
    binary_name: &'static str,
}

impl AssetInfo {
    fn for_running_target() -> Result<Self, UpdateError> {
        // The host's target triple is also the asset triple. Linux musl is
        // detected via `target_env = "musl"` so musl-Alpine users fetch the
        // statically-linked asset; everything else gets gnu.
        let info = if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
            AssetInfo {
                triple: "aarch64-apple-darwin",
                extension: "tar.gz",
                binary_name: "onebrain",
            }
        } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
            AssetInfo {
                triple: "x86_64-apple-darwin",
                extension: "tar.gz",
                binary_name: "onebrain",
            }
        } else if cfg!(all(
            target_arch = "aarch64",
            target_os = "linux",
            target_env = "gnu"
        )) {
            AssetInfo {
                triple: "aarch64-unknown-linux-gnu",
                extension: "tar.gz",
                binary_name: "onebrain",
            }
        } else if cfg!(all(
            target_arch = "x86_64",
            target_os = "linux",
            target_env = "gnu"
        )) {
            AssetInfo {
                triple: "x86_64-unknown-linux-gnu",
                extension: "tar.gz",
                binary_name: "onebrain",
            }
        } else if cfg!(all(
            target_arch = "x86_64",
            target_os = "linux",
            target_env = "musl"
        )) {
            AssetInfo {
                triple: "x86_64-unknown-linux-musl",
                extension: "tar.gz",
                binary_name: "onebrain",
            }
        } else if cfg!(all(
            target_arch = "aarch64",
            target_os = "linux",
            target_env = "musl"
        )) {
            AssetInfo {
                triple: "aarch64-unknown-linux-musl",
                extension: "tar.gz",
                binary_name: "onebrain",
            }
        } else if cfg!(all(target_arch = "aarch64", target_os = "windows")) {
            AssetInfo {
                triple: "aarch64-pc-windows-msvc",
                extension: "zip",
                binary_name: "onebrain.exe",
            }
        } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
            AssetInfo {
                triple: "x86_64-pc-windows-msvc",
                extension: "zip",
                binary_name: "onebrain.exe",
            }
        } else {
            // Include `target_env` (libc family on Linux) so musl-on-novel-
            // arch users know to file the right issue. `FAMILY` returns
            // "unix" / "windows" which obscured musl vs gnu in the alpha.9
            // initial cut (Reviewer A-M3).
            let env_hint = if cfg!(target_env = "musl") {
                "musl"
            } else if cfg!(target_env = "gnu") {
                "gnu"
            } else {
                std::env::consts::FAMILY
            };
            return Err(UpdateError::Install(format!(
                "no published binary for target arch={} os={} env={} — \
                 build from source or open an issue requesting this triple",
                std::env::consts::ARCH,
                std::env::consts::OS,
                env_hint,
            )));
        };
        Ok(info)
    }

    fn extract_binary(&self, archive_bytes: &[u8]) -> Result<Vec<u8>, UpdateError> {
        if self.extension == "tar.gz" {
            extract_tar_gz(archive_bytes, self.binary_name)
        } else {
            // Windows path: zip extraction is intentionally not wired for
            // v3.0.0 GA. The `binary_targets[]` listing in `update --plan`
            // excludes Windows triples to match, so users running --plan
            // never see Windows advertised + then fail at install time
            // (Reviewer A-H1).
            Err(UpdateError::Install(format!(
                "zip extraction not yet wired (v3.0.0 ships unix-only update; \
                 windows users: download {} manually until v3.0.1)",
                self.binary_name
            )))
        }
    }
}

fn download_archive(url: &str) -> Result<Vec<u8>, UpdateError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .user_agent("onebrain-cli-update")
        .build()
        .map_err(|e| UpdateError::Network(format!("build http client: {e}")))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| UpdateError::Network(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(UpdateError::GithubStatus(resp.status().as_u16()));
    }
    let mut buf = Vec::with_capacity(8 * 1024 * 1024);
    resp.copy_to(&mut buf)
        .map_err(|e| UpdateError::Network(format!("read body: {e}")))?;
    Ok(buf)
}

fn extract_tar_gz(archive_bytes: &[u8], target_name: &str) -> Result<Vec<u8>, UpdateError> {
    let gz = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive
        .entries()
        .map_err(|e| UpdateError::Decode(format!("tar entries: {e}")))?
    {
        let mut entry = entry.map_err(|e| UpdateError::Decode(format!("tar entry: {e}")))?;
        // Defense-in-depth: reject anything that isn't a regular file. A
        // malicious tar entry could otherwise be a symlink or directory
        // named `onebrain`; `read_to_end` would return linkname bytes (or
        // zero for dirs) which `swap_binary` would then chmod 0755 and
        // rename over the live binary. Real release pipelines emit regular
        // files, so this guard is purely belt-and-braces (Reviewer A-M2).
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| UpdateError::Decode(format!("tar path: {e}")))?;
        // The release tarball flat-packs the binary at the archive root, but
        // accept a one-level prefix too (some toolchains nest under a folder
        // named after the triple).
        let matches_root = path.file_name().and_then(|n| n.to_str()) == Some(target_name);
        if !matches_root {
            continue;
        }
        let mut buf = Vec::with_capacity(8 * 1024 * 1024);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| UpdateError::Decode(format!("read binary entry: {e}")))?;
        return Ok(buf);
    }
    Err(UpdateError::Decode(format!(
        "binary `{target_name}` not found in archive"
    )))
}

/// Atomically replace `current_exe` with `new_bytes`. On Unix the new file
/// is renamed over the running binary — POSIX allows this even while the
/// old inode is held open by the running process. On Windows the running
/// .exe is locked; we rename it to `.old` first to free the original name,
/// then move the new binary into place. The `.old` file is left behind for
/// the OS to clean up on next reboot (rustup uses the same pattern).
fn swap_binary(current_exe: &Path, new_bytes: &[u8]) -> Result<(), UpdateError> {
    let _parent = current_exe
        .parent()
        .ok_or_else(|| UpdateError::Install("current binary has no parent dir".to_string()))?;
    let new_path: PathBuf = {
        let mut p = current_exe.to_path_buf();
        let name = current_exe
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("onebrain");
        p.set_file_name(format!("{name}.new"));
        p
    };
    write_binary(&new_path, new_bytes)?;
    set_executable(&new_path)?;

    if cfg!(windows) {
        let old_path = {
            let mut p = current_exe.to_path_buf();
            let name = current_exe
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("onebrain.exe");
            p.set_file_name(format!("{name}.old"));
            p
        };
        // Rename live exe out of the way (allowed on Windows even when locked).
        if let Err(e) = fs::rename(current_exe, &old_path) {
            let _ = fs::remove_file(&new_path);
            return Err(UpdateError::Install(format!(
                "windows: rename live binary to .old: {e}"
            )));
        }
        if let Err(e) = fs::rename(&new_path, current_exe) {
            // Try to roll back the .old → live rename so the user isn't left
            // with a broken install. Surface the rollback outcome to stderr
            // so the user has a paper trail if the rollback itself failed —
            // alpha.9 review C-S5 / A-L6: previously both branches silently
            // suppressed the rollback error.
            match fs::rename(&old_path, current_exe) {
                Ok(_) => {
                    eprintln!(
                        "onebrain update: rolled back to previous binary after install failed"
                    );
                }
                Err(rollback_err) => {
                    eprintln!(
                        "onebrain update: ROLLBACK FAILED — binary may be missing at {} \
                         (rollback error: {rollback_err}); previous binary saved at {}",
                        current_exe.display(),
                        old_path.display(),
                    );
                }
            }
            // Clean up the .new file we couldn't install.
            let _ = fs::remove_file(&new_path);
            return Err(UpdateError::Install(format!(
                "windows: rename new binary into place: {e}"
            )));
        }
    } else if let Err(e) = fs::rename(&new_path, current_exe) {
        let _ = fs::remove_file(&new_path);
        return Err(UpdateError::Install(format!(
            "unix: rename new binary into place: {e}"
        )));
    }
    Ok(())
}

fn write_binary(path: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let mut f = fs::File::create(path)
        .map_err(|e| UpdateError::Install(format!("create {}: {e}", path.display())))?;
    f.write_all(bytes)
        .map_err(|e| UpdateError::Install(format!("write {}: {e}", path.display())))?;
    f.sync_all()
        .map_err(|e| UpdateError::Install(format!("sync {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| UpdateError::Install(format!("stat {}: {e}", path.display())))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
        .map_err(|e| UpdateError::Install(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), UpdateError> {
    // Windows: executability is driven by the .exe extension, not a mode bit.
    Ok(())
}

// ---------------------------------------------------------------------------
// SHA-256 verification (v3.1.4)
// ---------------------------------------------------------------------------

/// Download the published `<archive>.sha256` sidecar and verify it against the
/// archive bytes we just fetched. Network half — the parse + compare logic
/// lives in [`verify_checksum`] so it can be unit-tested without HTTP.
fn verify_archive_checksum(archive_url: &str, archive_bytes: &[u8]) -> Result<(), UpdateError> {
    let sums_url = format!("{archive_url}.sha256");
    let sums_text = download_text(&sums_url)?;
    verify_checksum(&sums_text, archive_bytes)
}

/// Parse a `shasum -a 256` / `sha256sum` style checksum file and compare its
/// digest against the SHA-256 of `archive_bytes`. A malformed file or a
/// mismatch is a [`UpdateError::Checksum`] — pure, so it's the unit-test seam.
fn verify_checksum(sums_file: &str, archive_bytes: &[u8]) -> Result<(), UpdateError> {
    let expected = parse_sha256_line(sums_file).ok_or_else(|| {
        UpdateError::Checksum(format!(
            "no SHA-256 digest in checksum file (first line: {:?})",
            sums_file.lines().next().unwrap_or("")
        ))
    })?;
    let actual = sha256_hex(archive_bytes);
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        Err(UpdateError::Checksum(format!(
            "mismatch — expected {expected}, computed {actual} (refusing to install)"
        )))
    }
}

/// Extract the hex digest from a `<64-hex>  <filename>` line. Returns it only
/// when the first whitespace-delimited token is exactly 64 hex chars, so a
/// truncated or HTML (404 page) body is rejected rather than mis-parsed.
fn parse_sha256_line(contents: &str) -> Option<String> {
    let token = contents.split_whitespace().next()?;
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// Like [`download_archive`] but returns the (small) body as text — for the
/// `.sha256` sidecar. A missing sidecar is a `Checksum` error (we cannot
/// verify, so we must refuse), not a generic network error.
fn download_text(url: &str) -> Result<String, UpdateError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .user_agent("onebrain-cli-update")
        .build()
        .map_err(|e| UpdateError::Network(format!("build http client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| UpdateError::Network(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Checksum(format!(
            "checksum file unavailable at {url} (HTTP {})",
            resp.status().as_u16()
        )));
    }
    resp.text()
        .map_err(|e| UpdateError::Network(format!("read checksum body: {e}")))
}

// ---------------------------------------------------------------------------
// Install-channel detection + Homebrew delegation (v3.1.4)
// ---------------------------------------------------------------------------

/// How the running binary was installed — decides whether `onebrain update`
/// swaps the binary itself or hands off to the package manager.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InstallChannel {
    /// Homebrew-managed: lives in the Cellar behind a `brew` symlink.
    Homebrew,
    /// Direct download / `cargo-binstall` / manual — a file we own and can
    /// safely swap in place.
    Direct,
}

/// Classify the install by where `current_exe` resolves. We canonicalize
/// first to follow brew's `bin/onebrain` symlink to its Cellar target, then
/// hand off to the pure [`classify_path`].
pub(crate) fn detect_install_channel(current_exe: &Path) -> InstallChannel {
    let resolved = fs::canonicalize(current_exe).unwrap_or_else(|_| current_exe.to_path_buf());
    classify_path(&resolved)
}

/// Pure path classifier. A Homebrew install canonicalizes into
/// `…/Cellar/onebrain/<version>/bin/onebrain` on `/opt/homebrew`,
/// `/usr/local`, and Linuxbrew alike, so the `/Cellar/onebrain/` segment is
/// the reliable signal. Split out so it's testable without a real filesystem.
fn classify_path(resolved: &Path) -> InstallChannel {
    if resolved.to_string_lossy().contains("/Cellar/onebrain/") {
        InstallChannel::Homebrew
    } else {
        InstallChannel::Direct
    }
}

/// Delegate the update to Homebrew. `brew upgrade onebrain` refreshes the tap
/// formula and is idempotent (a no-op when already current); stdio is
/// inherited so the user sees brew's own output. We never swap a Cellar
/// binary in place — that would leave brew's metadata pointing at a version
/// it no longer manages.
pub(crate) fn brew_upgrade() -> Result<(), UpdateError> {
    use std::process::Command;
    let status = Command::new("brew")
        .args(["upgrade", "onebrain"])
        .status()
        .map_err(|e| {
            UpdateError::Install(format!(
                "Homebrew install detected but `brew` is not runnable ({e}). \
                 Run `brew upgrade onebrain` manually."
            ))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(UpdateError::Install(format!(
            "`brew upgrade onebrain` failed (exit {}). \
             Try `brew update && brew upgrade onebrain`.",
            status.code().unwrap_or(-1)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tempfile::tempdir;

    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let buf: Vec<u8> = Vec::new();
        let encoder = GzEncoder::new(buf, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, name, *data).unwrap();
        }
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extract_tar_gz_returns_binary_bytes() {
        let payload = b"#!/bin/sh\necho hello\n";
        let archive = make_tar_gz(&[("onebrain", payload)]);
        let out = extract_tar_gz(&archive, "onebrain").unwrap();
        assert_eq!(out, payload);
    }

    #[test]
    fn extract_tar_gz_skips_unrelated_entries() {
        let archive = make_tar_gz(&[
            ("README.md", b"docs" as &[u8]),
            ("onebrain", b"BINARY"),
            ("LICENSE", b"AGPL"),
        ]);
        let out = extract_tar_gz(&archive, "onebrain").unwrap();
        assert_eq!(out, b"BINARY");
    }

    #[test]
    fn extract_tar_gz_errors_when_binary_missing() {
        let archive = make_tar_gz(&[("README.md", b"docs" as &[u8])]);
        let err = extract_tar_gz(&archive, "onebrain").unwrap_err();
        assert!(format!("{err:?}").contains("not found"));
    }

    #[test]
    fn swap_binary_replaces_unix_file() {
        let d = tempdir().unwrap();
        let bin = d.path().join("onebrain");
        fs::write(&bin, b"OLD").unwrap();
        swap_binary(&bin, b"NEW").unwrap();
        let after = fs::read(&bin).unwrap();
        assert_eq!(after, b"NEW");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        // Tempfile cleaned up
        let new_path = d.path().join("onebrain.new");
        assert!(!new_path.exists());
    }

    #[test]
    fn asset_for_running_target_returns_some_triple() {
        // Smoke-only: whatever triple this test runs on should be one of the
        // published targets. Real cross-platform matrix is enforced by the
        // release workflow + parity tests, not here.
        let info = AssetInfo::for_running_target().unwrap();
        assert!(!info.triple.is_empty());
        assert!(info.extension == "tar.gz" || info.extension == "zip");
    }

    // -----------------------------------------------------------------
    // v3.1.4 — SHA-256 verification
    // -----------------------------------------------------------------

    // SHA-256("abc") — the canonical NIST test vector.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(sha256_hex(b"abc"), ABC_SHA256);
        // Empty input → the well-known empty-string digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn parse_sha256_line_accepts_shasum_format() {
        // `shasum -a 256` / `sha256sum` emit `<hex>␠␠<filename>`.
        let line = format!("{ABC_SHA256}  onebrain-aarch64-apple-darwin.tar.gz\n");
        assert_eq!(parse_sha256_line(&line).as_deref(), Some(ABC_SHA256));
        // Bare digest, no filename.
        assert_eq!(parse_sha256_line(ABC_SHA256).as_deref(), Some(ABC_SHA256));
        // Uppercase is normalized to lowercase.
        assert_eq!(
            parse_sha256_line(&ABC_SHA256.to_uppercase()).as_deref(),
            Some(ABC_SHA256)
        );
    }

    #[test]
    fn parse_sha256_line_rejects_malformed() {
        assert_eq!(parse_sha256_line(""), None);
        assert_eq!(parse_sha256_line("deadbeef"), None); // too short
        assert_eq!(parse_sha256_line(&"z".repeat(64)), None); // non-hex
        assert_eq!(parse_sha256_line("<!DOCTYPE html><html>404"), None); // 404 page body
    }

    #[test]
    fn verify_checksum_accepts_matching_digest() {
        let sums = format!("{ABC_SHA256}  onebrain.tar.gz");
        assert!(verify_checksum(&sums, b"abc").is_ok());
    }

    #[test]
    fn verify_checksum_rejects_mismatch() {
        let sums = format!("{ABC_SHA256}  onebrain.tar.gz");
        let err = verify_checksum(&sums, b"tampered").unwrap_err();
        assert!(matches!(err, UpdateError::Checksum(_)));
        assert!(format!("{err}").contains("mismatch"));
    }

    #[test]
    fn verify_checksum_rejects_unparseable_file() {
        let err = verify_checksum("not a checksum", b"abc").unwrap_err();
        assert!(matches!(err, UpdateError::Checksum(_)));
    }

    // -----------------------------------------------------------------
    // v3.1.4 — install-channel detection
    // -----------------------------------------------------------------

    #[test]
    fn classify_path_detects_homebrew_cellar() {
        use std::path::Path;
        // Apple Silicon brew prefix.
        assert_eq!(
            classify_path(Path::new(
                "/opt/homebrew/Cellar/onebrain/3.1.4/bin/onebrain"
            )),
            InstallChannel::Homebrew
        );
        // Intel brew prefix.
        assert_eq!(
            classify_path(Path::new("/usr/local/Cellar/onebrain/3.1.4/bin/onebrain")),
            InstallChannel::Homebrew
        );
        // Linuxbrew.
        assert_eq!(
            classify_path(Path::new(
                "/home/linuxbrew/.linuxbrew/Cellar/onebrain/3.1.4/bin/onebrain"
            )),
            InstallChannel::Homebrew
        );
    }

    #[test]
    fn classify_path_treats_non_cellar_as_direct() {
        use std::path::Path;
        assert_eq!(
            classify_path(Path::new("/usr/local/bin/onebrain")),
            InstallChannel::Direct
        );
        assert_eq!(
            classify_path(Path::new("/home/user/.local/bin/onebrain")),
            InstallChannel::Direct
        );
        // A different formula's Cellar must not trip the onebrain matcher.
        assert_eq!(
            classify_path(Path::new("/opt/homebrew/Cellar/ripgrep/14.0/bin/onebrain")),
            InstallChannel::Direct
        );
    }
}
