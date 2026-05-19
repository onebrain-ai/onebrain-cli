//! `vault.yml` generation.
//!
//! Mirrors `VAULT_YML_DEFAULTS` from
//! `~/projects/onebrain/src/commands/init.ts` plus an optional `schedule:`
//! block populated from the user's chosen [`SchedulePreset`]. Serialization
//! goes through `serde_yaml` so the resulting file parses back cleanly.

use crate::error::FsError;
use crate::init::presets::{ScheduleEntry, SchedulePreset};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct VaultYml {
    update_channel: &'static str,
    folders: Folders,
    checkpoint: Checkpoint,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    schedule: Vec<ScheduleEntry>,
}

#[derive(Serialize)]
struct Folders {
    inbox: &'static str,
    projects: &'static str,
    areas: &'static str,
    knowledge: &'static str,
    resources: &'static str,
    agent: &'static str,
    archive: &'static str,
    logs: &'static str,
}

impl Default for Folders {
    fn default() -> Self {
        Self {
            inbox: "00-inbox",
            projects: "01-projects",
            areas: "02-areas",
            knowledge: "03-knowledge",
            resources: "04-resources",
            agent: "05-agent",
            archive: "06-archive",
            logs: "07-logs",
        }
    }
}

#[derive(Serialize)]
struct Checkpoint {
    messages: u32,
    minutes: u32,
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            messages: 15,
            minutes: 30,
        }
    }
}

/// Build the vault.yml text content for the given preset. Pure function —
/// useful for unit testing without touching the filesystem.
pub fn render_vault_yml(preset: SchedulePreset) -> Result<String, FsError> {
    let doc = VaultYml {
        update_channel: "stable",
        folders: Folders::default(),
        checkpoint: Checkpoint::default(),
        schedule: preset.entries(),
    };
    serde_yaml::to_string(&doc).map_err(|e| FsError::Io {
        path: std::path::PathBuf::from("vault.yml"),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
    })
}

/// Write `vault.yml` into `vault_dir` with the chosen preset's schedule
/// entries. Overwrites unconditionally — the guard belongs in
/// [`super::run_init`].
pub fn write_vault_yml(vault_dir: &Path, preset: SchedulePreset) -> Result<(), FsError> {
    let path = vault_dir.join("vault.yml");
    let content = render_vault_yml(preset)?;
    std::fs::write(&path, content).map_err(|e| FsError::Io {
        path: path.clone(),
        source: e,
    })
}

/// Convenience: parse a vault.yml string back into a dynamic `serde_yaml::Value`
/// (used in tests to assert round-trip integrity).
#[allow(dead_code)]
pub(crate) fn parse_vault_yml(content: &str) -> Result<serde_yaml::Value, serde_yaml::Error> {
    serde_yaml::from_str(content)
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn render_skip_preset_omits_schedule_key() {
        let yaml = render_vault_yml(SchedulePreset::Skip).unwrap();
        assert!(
            !yaml.contains("schedule"),
            "Skip preset should not write a schedule: key, got:\n{yaml}"
        );
        assert!(yaml.contains("update_channel: stable"));
    }

    #[test]
    fn render_essentials_preset_writes_three_entries() {
        let yaml = render_vault_yml(SchedulePreset::Essentials).unwrap();
        assert!(yaml.contains("schedule:"));
        // /daily, /weekly, /recap
        assert!(yaml.contains("/daily"));
        assert!(yaml.contains("/weekly"));
        assert!(yaml.contains("/recap"));
    }

    #[test]
    fn round_trip_preserves_update_channel_and_folders() {
        let yaml = render_vault_yml(SchedulePreset::Essentials).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            parsed.get("update_channel").and_then(|v| v.as_str()),
            Some("stable")
        );
        let folders = parsed.get("folders").unwrap();
        assert_eq!(
            folders.get("inbox").and_then(|v| v.as_str()),
            Some("00-inbox")
        );
        assert_eq!(
            folders.get("logs").and_then(|v| v.as_str()),
            Some("07-logs")
        );
    }

    #[test]
    fn round_trip_preserves_checkpoint_defaults() {
        let yaml = render_vault_yml(SchedulePreset::Minimal).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let cp = parsed.get("checkpoint").unwrap();
        assert_eq!(cp.get("messages").and_then(|v| v.as_u64()), Some(15));
        assert_eq!(cp.get("minutes").and_then(|v| v.as_u64()), Some(30));
    }

    #[test]
    fn write_creates_file_on_disk() {
        let d = tempdir().unwrap();
        write_vault_yml(d.path(), SchedulePreset::Skip).unwrap();
        assert!(d.path().join("vault.yml").is_file());
        let content = std::fs::read_to_string(d.path().join("vault.yml")).unwrap();
        assert!(content.starts_with("update_channel:"));
    }

    #[test]
    fn maintenance_plus_writes_six_entries() {
        let yaml = render_vault_yml(SchedulePreset::MaintenancePlus).unwrap();
        // Count `cron:` occurrences — each entry has one.
        let n = yaml.matches("cron:").count();
        assert_eq!(n, 6, "Maintenance Plus should write 6 schedule entries");
        assert!(yaml.contains("command: onebrain"));
        assert!(yaml.contains("qmd-reindex"));
    }

    #[test]
    fn unused_indexmap_import_used_via_presets() {
        // Defensive: ScheduleEntry::Skill uses IndexMap. Linker check.
        let _ = indexmap::IndexMap::<String, String>::new();
    }
}
