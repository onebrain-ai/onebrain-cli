use crate::doctor::Check;
use onebrain_core::{DoctorResult, VaultConfig};
use std::path::Path;

pub struct FoldersCheck;

impl Check for FoldersCheck {
    fn name(&self) -> &'static str {
        "folders"
    }

    fn run(&self, vault_root: &Path, config: &VaultConfig) -> DoctorResult {
        // Order matches Bun's STANDARD_FOLDER_KEYS:
        // inbox, projects, areas, knowledge, resources, agent, archive, logs.
        let folders: [&String; 8] = [
            &config.folders.inbox,
            &config.folders.projects,
            &config.folders.areas,
            &config.folders.knowledge,
            &config.folders.resources,
            &config.folders.agent,
            &config.folders.archive,
            &config.folders.logs,
        ];

        let missing: Vec<String> = folders
            .iter()
            .filter(|name| !vault_root.join(name.as_str()).is_dir())
            .map(|name| (*name).clone())
            .collect();

        if missing.is_empty() {
            DoctorResult::ok("folders", "8/8 present")
        } else {
            let present = 8 - missing.len();
            let message = format!("{}/8 present", present);
            let hint = format!("Missing: {}", missing.join(", "));
            let details: Vec<String> = missing
                .iter()
                .map(|name| format!("missing: {}", name))
                .collect();
            DoctorResult::error("folders", message)
                .with_hint(hint)
                .with_details(details)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_core::DoctorStatus;
    use tempfile::tempdir;

    fn cfg() -> VaultConfig {
        VaultConfig {
            qmd_collection: None,
            checkpoint: Default::default(),
            folders: Default::default(),
            search: Default::default(),
        }
    }

    #[test]
    fn all_eight_folders_present_reports_ok() {
        let d = tempdir().unwrap();
        for name in [
            "00-inbox",
            "01-projects",
            "02-areas",
            "03-knowledge",
            "04-resources",
            "05-agent",
            "06-archive",
            "07-logs",
        ] {
            std::fs::create_dir_all(d.path().join(name)).unwrap();
        }
        let r = FoldersCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Ok);
        assert_eq!(r.message, "8/8 present");
    }

    #[test]
    fn all_missing_reports_error_with_missing_hint() {
        let d = tempdir().unwrap();
        let r = FoldersCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert_eq!(r.message, "0/8 present");
        assert_eq!(
            r.hint.as_deref(),
            Some("Missing: 00-inbox, 01-projects, 02-areas, 03-knowledge, 04-resources, 05-agent, 06-archive, 07-logs")
        );
    }

    #[test]
    fn partial_missing_reports_error_with_n_of_8() {
        let d = tempdir().unwrap();
        for name in [
            "00-inbox",
            "01-projects",
            "02-areas",
            "03-knowledge",
            "04-resources",
        ] {
            std::fs::create_dir_all(d.path().join(name)).unwrap();
        }
        let r = FoldersCheck.run(d.path(), &cfg());
        assert_eq!(r.status, DoctorStatus::Error);
        assert_eq!(r.message, "5/8 present");
        assert_eq!(r.details.len(), 3);
        assert!(r.details.iter().any(|x| x == "missing: 05-agent"));
        assert!(r.details.iter().any(|x| x == "missing: 06-archive"));
        assert!(r.details.iter().any(|x| x == "missing: 07-logs"));
    }
}
