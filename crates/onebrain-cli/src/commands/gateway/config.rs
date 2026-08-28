//! Machine-level gateway config: `~/.onebrain/gateway.yml`.
//!
//! Machine-level (not vault-level) because one gateway spans vaults — the
//! `vaults:` name→path map is the first multi-vault registry in the codebase
//! (nothing else tracks more than one vault; verified 2026-08-28).
//! Missing file is NOT an error: zero-config `onebrain gateway run` inside a
//! vault serves that vault via the normal env/walk-up resolution chain.
//!
//! Consumed by `onebrain gateway run` (Task 4, `gateway/mod.rs::run`), which
//! loads this config and hands it to [`crate::commands::gateway::server`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const DEFAULT_GATEWAY_PORT: u16 = 7717;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Loopback port to serve on. 0 = OS-assigned ephemeral port.
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Vault served when a tool call names no vault. Falls back to the
    /// standard env/walk-up resolution chain when unset.
    #[serde(default)]
    pub default_vault: Option<PathBuf>,
    /// Named vaults a tool call may select via its `vault` argument.
    #[serde(default)]
    pub vaults: BTreeMap<String, PathBuf>,
}

fn default_gateway_port() -> u16 {
    DEFAULT_GATEWAY_PORT
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: default_gateway_port(),
            default_vault: None,
            vaults: BTreeMap::new(),
        }
    }
}

/// `~/.onebrain/gateway.yml`. Same home resolution as
/// `daemon_client::run_dir` (honours `$HOME` / `%USERPROFILE%`); the
/// `ONEBRAIN_CACHE_DIR` data-dir override is unrelated and deliberately
/// ignored here.
pub fn gateway_config_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("resolve home directory for gateway config")?;
    Ok(home.join(".onebrain").join("gateway.yml"))
}

pub fn load_gateway_config() -> anyhow::Result<GatewayConfig> {
    load_gateway_config_at(&gateway_config_path()?)
}

pub(crate) fn load_gateway_config_at(path: &Path) -> anyhow::Result<GatewayConfig> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GatewayConfig::default()),
        Err(e) => return Err(e).context(format!("read {}", path.display())),
    };
    let cfg: GatewayConfig = serde_yaml::from_str(&content)
        .map_err(onebrain_core::error::CoreError::InvalidYaml)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_port_7717_no_default_vault_and_no_vaults() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.port, DEFAULT_GATEWAY_PORT);
        assert!(cfg.default_vault.is_none());
        assert!(cfg.vaults.is_empty());
    }

    #[test]
    fn parses_full_yaml_and_fills_missing_keys_with_defaults() {
        let full: GatewayConfig = serde_yaml::from_str(
            "port: 8080\ndefault_vault: /tmp/v1\nvaults:\n  ob-1: /tmp/v1\n  work: /tmp/v2\n",
        )
        .unwrap();
        assert_eq!(full.port, 8080);
        assert_eq!(
            full.default_vault.as_deref(),
            Some(std::path::Path::new("/tmp/v1"))
        );
        assert_eq!(full.vaults.len(), 2);

        let sparse: GatewayConfig = serde_yaml::from_str("vaults:\n  ob-1: /tmp/v1\n").unwrap();
        assert_eq!(
            sparse.port, DEFAULT_GATEWAY_PORT,
            "missing port must default"
        );
        assert!(sparse.default_vault.is_none());
    }

    #[test]
    fn load_from_missing_file_returns_defaults_and_invalid_yaml_errors_e65() {
        let dir = tempfile::tempdir().unwrap();
        let missing = load_gateway_config_at(&dir.path().join("gateway.yml")).unwrap();
        assert_eq!(missing.port, DEFAULT_GATEWAY_PORT);

        let bad = dir.path().join("bad.yml");
        std::fs::write(&bad, "port: [not-a-port\n").unwrap();
        let err = load_gateway_config_at(&bad).unwrap_err();
        let has_invalid_yaml = err.chain().any(|c| {
            matches!(
                c.downcast_ref::<onebrain_core::error::CoreError>(),
                Some(onebrain_core::error::CoreError::InvalidYaml(_))
            )
        });
        assert!(
            has_invalid_yaml,
            "chain must carry CoreError::InvalidYaml: {err:?}"
        );
    }
}
