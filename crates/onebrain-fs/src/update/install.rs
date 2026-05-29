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
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS)))
        .user_agent("onebrain-cli-update")
        .build()
        .into();
    // ureq surfaces 4xx/5xx as `Err(StatusCode)`; an `Ok` is already 2xx. The
    // release archive is several MB, so stream it through `into_reader()`
    // (no size cap) rather than the limited `read_to_vec()`.
    match agent.get(url).call() {
        Ok(resp) => {
            let mut reader = resp.into_body().into_reader();
            let mut buf = Vec::with_capacity(8 * 1024 * 1024);
            std::io::Read::read_to_end(&mut reader, &mut buf)
                .map_err(|e| UpdateError::Network(format!("read body: {e}")))?;
            Ok(buf)
        }
        Err(ureq::Error::StatusCode(code)) => Err(UpdateError::GithubStatus(code)),
        Err(e) => Err(UpdateError::Network(format!("GET {url}: {e}"))),
    }
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
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS)))
        .user_agent("onebrain-cli-update")
        .build()
        .into();
    match agent.get(url).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map_err(|e| UpdateError::Network(format!("read checksum body: {e}"))),
        Err(ureq::Error::StatusCode(code)) => Err(UpdateError::Checksum(format!(
            "checksum file unavailable at {url} (HTTP {code})"
        ))),
        Err(e) => Err(UpdateError::Network(format!("GET {url}: {e}"))),
    }
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
    /// npm-managed: the `@onebrain-ai/cli` global package — the binary lives
    /// under `node_modules/@onebrain-ai/cli`. Hand off to `npm install -g` so
    /// npm's metadata stays in sync; swapping the file in place would desync it
    /// (the same divergence the Homebrew path avoids).
    Npm,
    /// Direct download / `cargo-binstall` / manual — a file we own and can
    /// safely swap in place.
    Direct,
}

/// Classify the install by where `current_exe` resolves. We canonicalize
/// first to follow brew's `bin/onebrain` symlink to its Cellar target, then
/// hand off to the pure [`classify_path`].
///
/// If `canonicalize` fails (effectively never for a currently-running binary)
/// we fall back to the raw path, which classifies as `Direct`. That's the safe
/// direction: the worst case is an in-place swap of a still-SHA-256-verified
/// binary (the old pre-3.1.4 behavior) — never a verification bypass, and never
/// a spurious `Homebrew` that would skip the swap.
pub(crate) fn detect_install_channel(current_exe: &Path) -> InstallChannel {
    let resolved = fs::canonicalize(current_exe).unwrap_or_else(|_| current_exe.to_path_buf());
    classify_path(&resolved)
}

/// Pure path classifier. A Homebrew install canonicalizes into
/// `…/Cellar/onebrain/<version>/bin/onebrain` on `/opt/homebrew`,
/// `/usr/local`, and Linuxbrew alike, so the `/Cellar/onebrain/` segment is
/// the reliable signal. Split out so it's testable without a real filesystem.
///
/// brew is Unix-only, so the forward-slash literal is correct: a Windows path
/// never matches and always resolves to `Direct` (the Windows update path is
/// unwired anyway — see `AssetInfo::extract_binary`).
fn classify_path(resolved: &Path) -> InstallChannel {
    let s = resolved.to_string_lossy();
    if s.contains("/Cellar/onebrain/") {
        InstallChannel::Homebrew
    } else if s.contains("/node_modules/@onebrain-ai/") {
        // The npm wrapper's native binary canonicalizes to
        // `<prefix>/lib/node_modules/@onebrain-ai/cli/bin/onebrain`; pnpm/yarn
        // and `bun add -g` nest it under `.../node_modules/@onebrain-ai/cli`
        // too (bun via its bin symlink). The scoped `@onebrain-ai` segment is
        // the precise signal — a bare `node_modules` check would over-match
        // unrelated binaries. Unix path separator: Windows npm globals use
        // backslashes and fall through to Direct (Windows update is unwired —
        // see `AssetInfo::extract_binary`).
        InstallChannel::Npm
    } else {
        InstallChannel::Direct
    }
}

/// The Homebrew tap that ships the `onebrain` formula.
const ONEBRAIN_TAP: &str = "onebrain-ai/onebrain";

/// Best-effort, quiet refresh of the `onebrain` Homebrew tap so a
/// freshly-published formula is visible to `brew upgrade`.
///
/// `brew upgrade` does NOT fetch new formulae — that's `brew update`, which
/// refreshes EVERY tap and can be slow. We scope the refresh to just our tap by
/// git-pulling its checkout (resolved via `brew --repository <tap>`). All
/// failures are swallowed: this is a convenience that removes the need for a
/// manual `brew update`, never a hard requirement — `brew_upgrade` proceeds and
/// the post-install version guard still catches a genuine no-op.
fn refresh_onebrain_tap() {
    use std::process::Command;
    let Ok(out) = Command::new("brew")
        .args(["--repository", ONEBRAIN_TAP])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let tap_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tap_dir.is_empty() {
        return;
    }
    // `--ff-only` keeps it safe (never a merge commit on a read-only tap);
    // `--quiet` keeps the framed update report clean.
    let _ = Command::new("git")
        .args(["-C", &tap_dir, "pull", "--ff-only", "--quiet"])
        .status();
}

/// Delegate the update to Homebrew. We refresh the onebrain tap FIRST (see
/// [`refresh_onebrain_tap`]) so `brew upgrade` sees a just-published formula —
/// without it, running `onebrain update` right after a release found a stale
/// local formula, no-op'd ("already installed"), and the post-install version
/// guard then flagged the mismatch. `brew upgrade` itself is idempotent (a
/// no-op when already current); stdio is inherited so the user sees brew's own
/// output. We never swap a Cellar binary in place — that would leave brew's
/// metadata pointing at a version it no longer manages.
pub(crate) fn brew_upgrade() -> Result<(), UpdateError> {
    use std::process::Command;
    refresh_onebrain_tap();
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

/// Build the npm install spec for a target version. Pins to the exact version
/// the update flow resolved (not `@latest`) so the install matches what was
/// already decided, and strips any leading `v` — npm specs are bare semver, so
/// an unstripped tag would yield `@onebrain-ai/cli@v3.2.17`, which npm rejects.
/// Extracted as a pure fn because it's the one silently-breakable bit of
/// [`npm_update`] (the rest just shells out).
fn npm_spec(version: &str) -> String {
    format!("@onebrain-ai/cli@{}", version.trim_start_matches('v'))
}

/// Delegate the update to npm for the `@onebrain-ai/cli` global package.
/// `npm install -g @onebrain-ai/cli@<version>` re-runs the wrapper's binary
/// download and keeps npm's metadata in sync. We never swap the file in place —
/// that would desync npm (the same divergence the Homebrew path avoids). stdio
/// is inherited so the user sees npm's own progress.
///
/// `npm` is resolved from the ambient PATH, so the install targets whatever
/// node prefix is active in this process (relevant under nvm / Volta / fnm — a
/// different active node could install into a different prefix than the running
/// binary, surfacing as a no-op caught by the post-install version guard, not a
/// corruption). Bun users (`bun add -g`) canonicalize into this same Npm arm
/// via their `node_modules` symlink, but would want `bun add -g` instead — Bun
/// is not a v3 canonical install channel, so the npm path + guard is the
/// safe-enough fallback.
pub(crate) fn npm_update(version: &str) -> Result<(), UpdateError> {
    use std::process::Command;
    let spec = npm_spec(version);
    let status = Command::new("npm")
        .args(["install", "-g", &spec])
        .status()
        .map_err(|e| {
            UpdateError::Install(format!(
                "npm install detected but `npm` is not runnable ({e}). \
                 Run `npm install -g {spec}` manually."
            ))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(UpdateError::Install(format!(
            "`npm install -g {spec}` failed (exit {}).",
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

    #[test]
    fn classify_path_detects_npm_global() {
        use std::path::Path;
        // npm global: the native binary canonicalizes to the package's `bin/`
        // (per the wrapper's postinstall) — the REAL production path.
        assert_eq!(
            classify_path(Path::new(
                "/opt/homebrew/lib/node_modules/@onebrain-ai/cli/bin/onebrain"
            )),
            InstallChannel::Npm
        );
        // system-node npm prefix layout, same scope.
        assert_eq!(
            classify_path(Path::new(
                "/usr/local/lib/node_modules/@onebrain-ai/cli/bin/onebrain"
            )),
            InstallChannel::Npm
        );
        // pnpm nests it under the @onebrain-ai scope too.
        assert_eq!(
            classify_path(Path::new(
                "/Users/x/Library/pnpm/global/5/.pnpm/@onebrain-ai+cli@3.2.17/node_modules/@onebrain-ai/cli/onebrain"
            )),
            InstallChannel::Npm
        );
        // An UNSCOPED node_modules binary must NOT match — the `@onebrain-ai`
        // scope is required, so a bare node_modules path stays Direct.
        assert_eq!(
            classify_path(Path::new("/proj/node_modules/.bin/onebrain")),
            InstallChannel::Direct
        );
    }

    #[test]
    fn npm_spec_strips_v_prefix() {
        // The one silently-breakable bit of npm_update: a stray `v` would yield
        // `@onebrain-ai/cli@v3.2.17`, which npm rejects.
        assert_eq!(npm_spec("3.2.17"), "@onebrain-ai/cli@3.2.17");
        assert_eq!(npm_spec("v3.2.17"), "@onebrain-ai/cli@3.2.17");
    }
}
