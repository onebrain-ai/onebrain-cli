//! `onebrain vault current` — new in v3.1.
//!
//! Reports which vault is currently active and HOW it was resolved
//! (`--vault` flag, `ONEBRAIN_VAULT` env, or walk-up from cwd). Designed for
//! both humans (text mode) and skills/scripts (`--json`).
//!
//! Soft-fails (never returns Err) but DOES validate vault.yml — surfaces
//! resolution path even when YAML is broken via the envelope's error field.
//! This matches the design's "show resolution source even if nothing
//! resolved" intent.

use crate::output::{emit, Envelope, ErrorInfo, OutputMode, VaultInfo};
use crate::vault_ctx;
use anyhow::Result;
use onebrain_core::load_vault_config;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize, Default)]
pub struct VaultCurrentData {
    /// True when a vault was located AND `vault.yml` parsed successfully.
    /// A walk-up hit on an unreadable / malformed `vault.yml` sets this to
    /// `false` and populates `error` so callers can distinguish "no vault
    /// at all" from "vault present but broken".
    pub detected: bool,
    /// Basename of the vault directory (e.g. `ob-1`). `None` when not detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absolute path. `None` when not detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Which mechanism resolved the vault — `"--vault flag"`,
    /// `"ONEBRAIN_VAULT env"`, or `"walk-up"`. `None` when not detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Current working directory (always present; useful for walk-up debug).
    pub cwd: PathBuf,
    /// Populated when the resolver found a vault directory but
    /// `load_vault_config` failed (broken symlink, malformed YAML,
    /// permission denied). `detected` is `false` in this case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

pub fn run(flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let resolved = vault_ctx::resolve(flag)?;

    let (data, vault_info) = match resolved.as_ref() {
        Some(r) => {
            // R1 B1: gate `detected` on successful vault.yml parse so a
            // broken symlink or malformed YAML doesn't claim "detected".
            match load_vault_config(&r.root) {
                Ok(_) => {
                    let info = VaultInfo {
                        name: r.root.name(),
                        path: r.root.as_path().to_path_buf(),
                    };
                    let d = VaultCurrentData {
                        detected: true,
                        name: Some(r.root.name()),
                        path: Some(r.root.as_path().to_path_buf()),
                        source: Some(r.source.label().to_string()),
                        cwd: cwd.clone(),
                        error: None,
                    };
                    (d, Some(info))
                }
                Err(e) => {
                    let d = VaultCurrentData {
                        detected: false,
                        // Surface path/source so the user knows WHICH vault.yml
                        // was rejected, but flag detected=false so consumers
                        // don't accidentally trust the path.
                        name: Some(r.root.name()),
                        path: Some(r.root.as_path().to_path_buf()),
                        source: Some(r.source.label().to_string()),
                        cwd: cwd.clone(),
                        error: Some(ErrorInfo::new(e.error_code(), e.to_string())),
                    };
                    (d, None)
                }
            }
        }
        None => {
            let d = VaultCurrentData {
                detected: false,
                cwd: cwd.clone(),
                ..Default::default()
            };
            (d, None)
        }
    };

    let envelope = Envelope::ok("vault.current", vault_info, data);

    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<VaultCurrentData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let mut out = String::new();
    if d.detected {
        // We know `name` / `path` / `source` are all `Some` when detected.
        let name = d.name.as_deref().unwrap_or("?");
        let path = d
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".into());
        let source = d.source.as_deref().unwrap_or("?");
        out.push_str(&format!("vault: {name}\n"));
        out.push_str(&format!("path:  {path}\n"));
        out.push_str(&format!("via:   {source}\n"));
    } else if let Some(err) = d.error.as_ref() {
        // Resolved a directory but failed to validate vault.yml. Surface
        // the path so the user can fix the bad file.
        let path = d
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".into());
        out.push_str("vault: (resolved but invalid)\n");
        out.push_str(&format!("path:  {path}\n"));
        out.push_str(&format!("error: {} · {}\n", err.code, err.message));
    } else {
        out.push_str("vault: (none detected)\n");
        out.push_str(&format!("cwd:   {}\n", d.cwd.display()));
        out.push('\n');
        out.push_str("Quick fixes:\n");
        out.push_str("  cd into a vault                  → cd ~/Documents/ob-1\n");
        out.push_str("  target a specific vault          → onebrain vault current --vault ~/Documents/ob-1\n");
        out.push_str(
            "  set default vault for shell      → export ONEBRAIN_VAULT=~/Documents/ob-1\n",
        );
        out.push_str("  create a new vault here          → onebrain init\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_data(detected: bool, source: Option<&str>) -> VaultCurrentData {
        VaultCurrentData {
            detected,
            name: if detected { Some("ob-1".into()) } else { None },
            path: if detected {
                Some(PathBuf::from("/tmp/ob-1"))
            } else {
                None
            },
            source: source.map(|s| s.to_string()),
            cwd: PathBuf::from("/tmp"),
            error: None,
        }
    }

    #[test]
    fn text_render_detected_shows_three_lines() {
        let env = Envelope::ok("vault.current", None, make_data(true, Some("walk-up")));
        let s = render_text(&env);
        assert!(s.contains("vault: ob-1"));
        assert!(s.contains("path:  /tmp/ob-1"));
        assert!(s.contains("via:   walk-up"));
    }

    #[test]
    fn text_render_not_detected_shows_quickfix() {
        let env = Envelope::ok("vault.current", None, make_data(false, None));
        let s = render_text(&env);
        assert!(s.contains("(none detected)"));
        assert!(s.contains("Quick fixes:"));
        assert!(s.contains("ONEBRAIN_VAULT"));
        assert!(s.contains("onebrain init"));
    }

    #[test]
    fn data_skips_none_fields_in_json() {
        let data = make_data(false, None);
        let v = serde_json::to_value(&data).unwrap();
        // detected and cwd are always present; name/path/source/error are
        // omitted when None thanks to `skip_serializing_if`.
        assert!(v.get("name").is_none());
        assert!(v.get("path").is_none());
        assert!(v.get("source").is_none());
        assert!(v.get("error").is_none());
        assert!(v.get("detected").is_some());
        assert!(v.get("cwd").is_some());
    }

    #[test]
    fn text_render_invalid_vault_yml_shows_error_block() {
        let data = VaultCurrentData {
            detected: false,
            name: Some("ob-1".into()),
            path: Some(PathBuf::from("/tmp/ob-1")),
            source: Some("walk-up".into()),
            cwd: PathBuf::from("/tmp"),
            error: Some(ErrorInfo::new(
                "E_INVALID_YAML",
                "vault.yml has invalid syntax",
            )),
        };
        let env = Envelope::ok("vault.current", None, data);
        let s = render_text(&env);
        assert!(s.contains("(resolved but invalid)"));
        assert!(s.contains("E_INVALID_YAML"));
        assert!(s.contains("/tmp/ob-1"));
    }
}
