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
//! `github.com/onebrain-ai/onebrain-cli/releases/download/...`. SHA-256
//! verification is a defense-in-depth follow-up for post-GA (the GitHub TLS
//! chain is the current authentication boundary).

use super::UpdateError;
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
}
