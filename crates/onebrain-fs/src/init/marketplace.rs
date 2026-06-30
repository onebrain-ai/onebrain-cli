//! `<vault>/.claude-plugin/marketplace.json` writer.
//!
//! Claude Code requires a marketplace manifest at the vault root to discover
//! the plugin shipped at `.claude/plugins/onebrain/`. Without it, the plugin's
//! skills (including `/onboarding`) never load even when `enabledPlugins` is
//! set in `.claude/settings.json`.
//!
//! Idempotent on re-init: by default, if the file already exists we leave it
//! alone so users who hand-tweaked the description / source path don't lose
//! their changes. With `force = true`, the existing file is validated against
//! the canonical shape (`plugins[0].name == "onebrain"` AND
//! `plugins[0].source == "./.claude/plugins/onebrain"`); malformed JSON or a
//! wrong-shape manifest is rewritten and a stderr warning is emitted via the
//! returned `Repaired` outcome so callers can surface it to the user.

use crate::{FsError, Result};
use serde::Serialize;
use serde_json::{
    json,
    ser::{PrettyFormatter, Serializer},
    Value,
};
use std::path::Path;

/// Outcome of [`write_marketplace_json`] / [`write_marketplace_json_with_force`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarketplaceOutcome {
    /// File was newly created.
    Written,
    /// File already existed and was left untouched (default re-init path).
    Skipped,
    /// File existed but was malformed or wrong-shape; rewrote with canonical
    /// content. Only emitted on `force = true`.
    Repaired,
}

impl MarketplaceOutcome {
    /// True if a write happened (Written or Repaired). Kept for back-compat
    /// with the original `bool` return shape.
    #[allow(dead_code)]
    pub(crate) fn wrote(self) -> bool {
        !matches!(self, MarketplaceOutcome::Skipped)
    }
}

/// Default-mode writer: skips if the file already exists (preserving user
/// customizations). Returns `true` when the file was newly written, `false`
/// when it was left untouched. Thin wrapper around
/// [`write_marketplace_json_with_force`] for back-compat.
#[cfg(test)]
pub(crate) fn write_marketplace_json(vault_dir: &Path) -> Result<bool> {
    Ok(write_marketplace_json_with_force(vault_dir, false)?.wrote())
}

/// Write `<vault>/.claude-plugin/marketplace.json`.
///
/// - `force = false` (default): skip if the file already exists.
/// - `force = true`: validate the existing file; if malformed JSON or wrong
///   shape (missing/incorrect `plugins[0].name` or `plugins[0].source`),
///   rewrite with canonical content and return [`MarketplaceOutcome::Repaired`].
///   A canonical existing file still returns [`MarketplaceOutcome::Skipped`].
pub(crate) fn write_marketplace_json_with_force(
    vault_dir: &Path,
    force: bool,
) -> Result<MarketplaceOutcome> {
    let plugin_dir = vault_dir.join(".claude-plugin");
    let path = plugin_dir.join("marketplace.json");
    if path.exists() {
        if !force {
            return Ok(MarketplaceOutcome::Skipped);
        }
        // --force: validate shape, repair if needed.
        let canonical = is_canonical_existing(&path);
        if canonical {
            return Ok(MarketplaceOutcome::Skipped);
        }
        // Rewrite as canonical.
        std::fs::create_dir_all(&plugin_dir).map_err(|e| FsError::Io {
            path: plugin_dir.clone(),
            source: e,
        })?;
        atomic_write_canonical(&path)?;
        return Ok(MarketplaceOutcome::Repaired);
    }
    std::fs::create_dir_all(&plugin_dir).map_err(|e| FsError::Io {
        path: plugin_dir.clone(),
        source: e,
    })?;
    atomic_write_canonical(&path)?;
    Ok(MarketplaceOutcome::Written)
}

/// True if the file at `path` parses as JSON AND has the canonical shape
/// (`plugins[0].name == "onebrain"` AND `plugins[0].source ==
/// "./.claude/plugins/onebrain"`). Any read / parse / shape failure returns
/// false — the caller will treat it as a repair trigger.
fn is_canonical_existing(path: &Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    v.get("plugins")
        .and_then(|p| p.get(0))
        .map(|p0| {
            p0.get("name").and_then(|n| n.as_str()) == Some("onebrain")
                && p0.get("source").and_then(|s| s.as_str()) == Some("./.claude/plugins/onebrain")
        })
        .unwrap_or(false)
}

/// Atomic write of the canonical marketplace.json via tmp + rename. Matches
/// the pattern used by `register_hooks::settings::write_settings`.
fn atomic_write_canonical(path: &Path) -> Result<()> {
    let value = json!({
        "name": "onebrain",
        "owner": {
            "name": "OneBrain Contributors"
        },
        "plugins": [
            {
                "name": "onebrain",
                "source": "./.claude/plugins/onebrain",
                "description": "OneBrain — Your AI Thinking Partner (vault-bundled plugin)"
            }
        ]
    });
    let text = pretty_4_space(&value)?;
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, &text).map_err(|e| FsError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    std::fs::rename(&tmp, path).map_err(|e| FsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// 4-space pretty-print matching settings.rs / Bun's `JSON.stringify(v, null, 4)`.
fn pretty_4_space(value: &Value) -> Result<String> {
    let indent = b"    ";
    let formatter = PrettyFormatter::with_indent(indent);
    let mut buf = Vec::with_capacity(256);
    let mut ser = Serializer::with_formatter(&mut buf, formatter);
    value.serialize(&mut ser).map_err(|e| FsError::Io {
        path: std::path::PathBuf::new(),
        source: std::io::Error::other(e),
    })?;
    String::from_utf8(buf).map_err(|e| FsError::Io {
        path: std::path::PathBuf::new(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_marketplace_json_to_correct_path() {
        let d = tempdir().unwrap();
        let wrote = write_marketplace_json(d.path()).unwrap();
        assert!(wrote);
        let path = d.path().join(".claude-plugin").join("marketplace.json");
        assert!(path.is_file());
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["name"], "onebrain");
        assert_eq!(v["plugins"][0]["name"], "onebrain");
        assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
        assert!(v["plugins"][0]["description"]
            .as_str()
            .unwrap()
            .contains("Your AI Thinking Partner"));
    }

    #[test]
    fn does_not_overwrite_existing_marketplace_json() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        let custom = r#"{"name":"customized"}"#;
        std::fs::write(&path, custom).unwrap();
        let wrote = write_marketplace_json(d.path()).unwrap();
        assert!(!wrote, "expected no overwrite on second call");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, custom);
    }

    #[test]
    fn uses_4_space_indent() {
        let d = tempdir().unwrap();
        write_marketplace_json(d.path()).unwrap();
        let text =
            std::fs::read_to_string(d.path().join(".claude-plugin").join("marketplace.json"))
                .unwrap();
        // Inner field "name" should be indented by 4 spaces (1 level × 4).
        assert!(
            text.contains("\n    \"name\""),
            "expected 4-space indent: {text:?}"
        );
    }

    #[test]
    fn second_call_returns_false() {
        let d = tempdir().unwrap();
        assert!(write_marketplace_json(d.path()).unwrap());
        assert!(!write_marketplace_json(d.path()).unwrap());
    }

    /// A1 (SF-B1): a write failure (e.g., the manifest path resolves to a
    /// regular file instead of a directory) must propagate as `Err`, not be
    /// swallowed as a "warning". Pre-fix, `init/mod.rs` matched `Err(e) =>
    /// stderr(...)` and silently continued — yielding `enabledPlugins=true`
    /// without a manifest, which is the exact bug v3.1 set out to fix.
    #[test]
    fn marketplace_write_failure_propagates_as_error() {
        let d = tempdir().unwrap();
        // Pre-create `.claude-plugin` as a regular file (not a directory).
        // create_dir_all() returns AlreadyExists/NotADirectory depending on
        // platform; either way the write path bubbles an io::Error.
        std::fs::write(d.path().join(".claude-plugin"), b"not-a-dir").unwrap();
        let err = write_marketplace_json_with_force(d.path(), false).unwrap_err();
        match err {
            FsError::Io { .. } => {}
            other => panic!("expected FsError::Io, got: {other:?}"),
        }
    }

    /// A2 (SF-B2): `force = true` re-init repairs a pre-existing malformed
    /// (or wrong-shape) marketplace.json. The default (non-force) path still
    /// preserves existing files — only `--force` opts into the repair.
    #[test]
    fn marketplace_force_repairs_malformed_existing() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(&path, "{not valid json").unwrap();

        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Repaired);
        let after = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v["plugins"][0]["name"], "onebrain");
        assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
    }

    /// A2 follow-up: wrong-shape (valid JSON but the canonical fields point
    /// elsewhere) is also a repair trigger under `--force`.
    #[test]
    fn marketplace_force_repairs_wrong_shape_existing() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(
            &path,
            r#"{"plugins":[{"name":"other","source":"./elsewhere"}]}"#,
        )
        .unwrap();

        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Repaired);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["plugins"][0]["name"], "onebrain");
    }

    /// A2 negative: a canonical existing file under `--force` is left alone
    /// (no needless rewrite, mtime preserved).
    #[test]
    fn marketplace_force_leaves_canonical_existing_alone() {
        let d = tempdir().unwrap();
        write_marketplace_json_with_force(d.path(), false).unwrap();
        let path = d.path().join(".claude-plugin").join("marketplace.json");
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Skipped);
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);
    }

    /// A2 default-path negative: WITHOUT `--force`, a corrupt existing file
    /// is NOT repaired (preserves user customizations).
    #[test]
    fn marketplace_default_path_does_not_repair_corrupt() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let out = write_marketplace_json_with_force(d.path(), false).unwrap();
        assert_eq!(out, MarketplaceOutcome::Skipped);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "{not valid json");
    }

    /// C1 (CR-M2): the write goes through tmp + rename so a crash mid-write
    /// can't leave a truncated marketplace.json. Mirrors the existing
    /// `write_settings_atomic_via_tmp_rename` test in register_hooks/settings.rs.
    #[test]
    fn marketplace_write_is_atomic_via_tmp_rename() {
        let d = tempdir().unwrap();
        write_marketplace_json_with_force(d.path(), false).unwrap();
        // Tmp file must not be left behind after the rename.
        let tmp = d.path().join(".claude-plugin").join("marketplace.json.tmp");
        assert!(!tmp.exists(), "tmp leftover at {tmp:?}");
        // Target exists with canonical content.
        let path = d.path().join(".claude-plugin").join("marketplace.json");
        assert!(path.is_file());
    }

    /// `is_canonical_existing`: `.unwrap_or(false)` branch — empty plugins array
    /// means `.get(0)` returns None → the map closure is never called → false.
    #[test]
    fn force_repairs_empty_plugins_array() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(&path, r#"{"plugins":[]}"#).unwrap();

        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Repaired);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["plugins"][0]["name"], "onebrain");
        assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
    }

    /// `is_canonical_existing`: `.unwrap_or(false)` branch — missing `plugins` key
    /// means `.get("plugins")` returns None → `.and_then` yields None → false.
    #[test]
    fn force_repairs_missing_plugins_key() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(&path, r#"{}"#).unwrap();

        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Repaired);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
    }

    /// `is_canonical_existing`: second half of `&&` — correct name but wrong source
    /// makes the first condition true, so the second condition IS evaluated (no
    /// short-circuit) and returns false. The existing wrong-shape test only uses
    /// a wrong name, which short-circuits before evaluating source.
    #[test]
    fn force_repairs_correct_name_wrong_source() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(
            &path,
            r#"{"plugins":[{"name":"onebrain","source":"./somewhere-else"}]}"#,
        )
        .unwrap();

        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Repaired);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["plugins"][0]["source"], "./.claude/plugins/onebrain");
    }

    /// `is_canonical_existing`: correct source but wrong name — first condition
    /// false, short-circuits (source not evaluated). Repair triggered.
    #[test]
    fn force_repairs_wrong_name_correct_source() {
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        std::fs::write(
            &path,
            r#"{"plugins":[{"name":"other-plugin","source":"./.claude/plugins/onebrain"}]}"#,
        )
        .unwrap();

        let out = write_marketplace_json_with_force(d.path(), true).unwrap();
        assert_eq!(out, MarketplaceOutcome::Repaired);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["plugins"][0]["name"], "onebrain");
    }

    /// `MarketplaceOutcome::wrote()` for all three variants. The existing tests
    /// exercise `write_marketplace_json` (which calls `.wrote()`) for Written and
    /// Skipped but never assert `.wrote()` on the Repaired variant directly.
    #[test]
    fn outcome_wrote_returns_correct_values() {
        assert!(
            MarketplaceOutcome::Written.wrote(),
            "Written must report wrote=true"
        );
        assert!(
            MarketplaceOutcome::Repaired.wrote(),
            "Repaired must report wrote=true"
        );
        assert!(
            !MarketplaceOutcome::Skipped.wrote(),
            "Skipped must report wrote=false"
        );
    }

    /// `is_canonical_existing`: `read_to_string` error branch — an unreadable file
    /// returns false, triggering the repair path under `force=true`.
    #[cfg(unix)]
    #[test]
    fn force_treats_unreadable_file_as_non_canonical() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap();
        let dir = d.path().join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marketplace.json");
        // Write canonical content, then strip read permission.
        std::fs::write(
            &path,
            r#"{"plugins":[{"name":"onebrain","source":"./.claude/plugins/onebrain"}]}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let out = write_marketplace_json_with_force(d.path(), true);
        // Restore permissions before any assertions so tempdir cleanup works.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));

        // read_to_string fails → is_canonical_existing returns false → repair branch.
        // The rewrite may succeed (Repaired) or fail if the dir itself restricts
        // writes, but it must NOT return Skipped (which would mean we bypassed repair).
        match out {
            Ok(MarketplaceOutcome::Repaired) => {
                let text = std::fs::read_to_string(&path).unwrap();
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["plugins"][0]["name"], "onebrain");
            }
            Err(_) => {
                // Couldn't rewrite (e.g. EACCES on the tmp write or rename).
                // Still valid: repair branch was entered, which is the contract.
            }
            Ok(MarketplaceOutcome::Skipped) => {
                panic!("unreadable file must not produce Skipped — repair branch not entered")
            }
            Ok(MarketplaceOutcome::Written) => {
                panic!("unexpected Written outcome for pre-existing path")
            }
        }
    }
}
