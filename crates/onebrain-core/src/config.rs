use crate::{CoreError, Result, VaultRoot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Optional qmd MCP collection name · None disables semantic search wiring.
    #[serde(default)]
    pub qmd_collection: Option<String>,
}

pub fn load_vault_config(root: &VaultRoot) -> Result<VaultConfig> {
    let path = root.join("vault.yml");
    let content = std::fs::read_to_string(&path).map_err(|source| CoreError::VaultYamlMissing {
        path: path.clone(),
        source,
    })?;
    let config: VaultConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_vault(content: &str) -> (tempfile::TempDir, VaultRoot) {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), content).unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn loads_minimal_vault_yml() {
        let (_dir, root) = write_vault("qmd_collection: ob-1\n");
        let cfg = load_vault_config(&root).unwrap();
        assert_eq!(cfg.qmd_collection.as_deref(), Some("ob-1"));
    }

    #[test]
    fn missing_qmd_collection_defaults_to_none() {
        let (_dir, root) = write_vault("# empty config\n");
        let cfg = load_vault_config(&root).unwrap();
        assert!(cfg.qmd_collection.is_none());
    }

    #[test]
    fn malformed_yaml_returns_invalid_yaml_variant() {
        let (_dir, root) = write_vault("not: : valid");
        let err = load_vault_config(&root).unwrap_err();
        assert!(matches!(err, CoreError::InvalidYaml(_)));
    }

    #[test]
    fn vault_yml_missing_returns_vault_yaml_missing_variant() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "").unwrap();
        let root = crate::find_vault_root(dir.path()).unwrap();
        std::fs::remove_file(root.join("vault.yml")).unwrap();
        let err = load_vault_config(&root).unwrap_err();
        assert!(matches!(err, CoreError::VaultYamlMissing { .. }));
    }
}
