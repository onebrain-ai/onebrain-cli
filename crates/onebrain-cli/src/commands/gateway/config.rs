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

use super::policy::PolicyConfig;

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
    /// Public base URL for the gateway's OAuth issuer (e.g. behind a future
    /// `cloudflared` tunnel — PR 3's later phase). When set, `gateway run`
    /// uses `public_url` (trailing slash trimmed) as the OAuth issuer in
    /// every discovery document and the `/mcp` 401 challenge, instead of
    /// `http://127.0.0.1:<bound-port>` (see `gateway::resolve_issuer`).
    /// `None` (the default) keeps loopback-only issuer resolution — this
    /// build still binds `127.0.0.1` regardless of this setting; nothing is
    /// exposed remotely by setting it alone.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Per-risk-class approval policy (Gateway PR 4, Task 2) — see
    /// [`PolicyConfig`]'s own doc comment for the field meanings and
    /// defaults. `#[serde(default)]` so an existing `gateway.yml` written
    /// before this field existed keeps parsing (and gets the safe
    /// defaults), and a `policy:` block may specify only the sub-fields it
    /// wants to override (each of `PolicyConfig`'s own fields defaults
    /// independently).
    #[serde(default)]
    pub policy: PolicyConfig,
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
            public_url: None,
            policy: PolicyConfig::default(),
        }
    }
}

/// `~/.onebrain/gateway.yml`. Same home resolution as
/// `daemon_client::run_dir` — [`crate::home::home_dir`], which honours
/// `$HOME` / `%USERPROFILE%` on BOTH platforms (plain `dirs::home_dir()` does
/// not: on Windows it is a Known Folder API call that ignores
/// `%USERPROFILE%`, so a sandboxed child would read the real profile's
/// config instead of the one it was pointed at). The `ONEBRAIN_CACHE_DIR`
/// data-dir override is unrelated and deliberately ignored here.
pub fn gateway_config_path() -> anyhow::Result<PathBuf> {
    let home = crate::home::home_dir().context("resolve home directory for gateway config")?;
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
    use super::super::policy::PolicyMode;
    use super::*;

    #[test]
    fn default_config_has_port_7717_no_default_vault_and_no_vaults() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.port, DEFAULT_GATEWAY_PORT);
        assert!(cfg.default_vault.is_none());
        assert!(cfg.vaults.is_empty());
        assert!(cfg.public_url.is_none());
        assert_eq!(cfg.policy.read_only, PolicyMode::Auto);
        assert_eq!(cfg.policy.mutating, PolicyMode::AskOnce);
        assert_eq!(cfg.policy.destructive, PolicyMode::AskAlways);
        assert_eq!(cfg.policy.grant_ttl_minutes, 30);
    }

    /// `GatewayConfig`'s `Default` impl is hand-written (it does not
    /// `derive(Default)`) — this is the regression guard the Task 2 brief
    /// calls out by name: a `policy` field added to the struct but forgotten
    /// in this impl would silently build a `PolicyConfig` with all-zero /
    /// first-variant values instead of the safe `PolicyConfig::default()`.
    /// Comparing field-by-field against `PolicyConfig::default()` directly
    /// (rather than re-asserting each concrete value, which the test above
    /// already does) is what actually catches "hand-written Default drifted
    /// from PolicyConfig::default()", not just "hand-written Default forgot
    /// the field entirely" (which wouldn't compile).
    #[test]
    fn hand_written_default_policy_matches_policy_config_default() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.policy.read_only, PolicyConfig::default().read_only);
        assert_eq!(cfg.policy.mutating, PolicyConfig::default().mutating);
        assert_eq!(cfg.policy.destructive, PolicyConfig::default().destructive);
        assert_eq!(
            cfg.policy.grant_ttl_minutes,
            PolicyConfig::default().grant_ttl_minutes
        );
    }

    #[test]
    fn parses_full_yaml_and_fills_missing_keys_with_defaults() {
        let full: GatewayConfig = serde_yaml::from_str(
            "port: 8080\ndefault_vault: /tmp/v1\nvaults:\n  ob-1: /tmp/v1\n  work: /tmp/v2\n\
             public_url: https://gw.example.com\n",
        )
        .unwrap();
        assert_eq!(full.port, 8080);
        assert_eq!(
            full.default_vault.as_deref(),
            Some(std::path::Path::new("/tmp/v1"))
        );
        assert_eq!(full.vaults.len(), 2);
        assert_eq!(full.public_url.as_deref(), Some("https://gw.example.com"));

        let sparse: GatewayConfig = serde_yaml::from_str("vaults:\n  ob-1: /tmp/v1\n").unwrap();
        assert_eq!(
            sparse.port, DEFAULT_GATEWAY_PORT,
            "missing port must default"
        );
        assert!(sparse.default_vault.is_none());
        assert!(
            sparse.public_url.is_none(),
            "missing public_url must default to None"
        );
        assert_eq!(
            sparse.policy.read_only,
            PolicyMode::Auto,
            "missing policy block must default"
        );
    }

    /// A `policy:` block parses into `GatewayConfig.policy`, and (mirroring
    /// `PolicyConfig`'s own `policy_config_parses_from_yaml_and_fills_missing_fields_with_defaults`
    /// unit test) a PARTIAL `policy:` block fills only the fields it omits —
    /// proven here at the `GatewayConfig` level, not just `PolicyConfig`'s
    /// own standalone deserialization, since `#[serde(default)]` on the
    /// `policy` field only kicks in when the KEY itself is absent, not when
    /// it's present-but-partial.
    #[test]
    fn gateway_config_parses_a_policy_block_and_fills_its_missing_fields() {
        let cfg: GatewayConfig =
            serde_yaml::from_str("policy:\n  mutating: deny\n  grant_ttl_minutes: 5\n").unwrap();
        assert_eq!(cfg.policy.mutating, PolicyMode::Deny);
        assert_eq!(cfg.policy.grant_ttl_minutes, 5);
        assert_eq!(
            cfg.policy.read_only,
            PolicyMode::Auto,
            "field omitted from the policy block must still default"
        );
        assert_eq!(cfg.policy.destructive, PolicyMode::AskAlways);
    }

    /// Windows-CI regression guard (`gateway_http.rs`'s happy path failed
    /// here): the config path MUST follow the `$HOME` / `%USERPROFILE%`
    /// override, because the integration test writes `gateway.yml` into a
    /// sandbox home and spawns the binary pointed at it. Plain
    /// `dirs::home_dir()` reads the Known Folder API on Windows and walked
    /// past the sandbox into the runner's real profile, so no `gateway.yml`
    /// was ever found, `default_vault` stayed `None`, and every vault-needing
    /// tool call answered `E_VAULT_NOT_FOUND`. Asserted on both platforms —
    /// the Unix arm passed all along, and that is exactly why the break only
    /// ever showed up on `windows-latest`.
    #[test]
    fn config_path_follows_the_home_env_override() {
        let d = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_vars(&[
            ("HOME", d.path().as_os_str()),
            ("USERPROFILE", d.path().as_os_str()),
        ]);
        assert_eq!(
            gateway_config_path().unwrap(),
            d.path().join(".onebrain").join("gateway.yml"),
        );
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
