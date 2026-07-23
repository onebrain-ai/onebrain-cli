//! Managed Codex plugin installation.
//!
//! A vault-local marker alone is untrusted (a repository can contain one).
//! Managed operations require it to match a receipt stored in `CODEX_HOME`
//! and bound to the vault's canonical path.

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

const REFRESH_RETRY: &str = "codex plugin add onebrain@onebrain";
const REMOVE_RETRY: &str = "codex plugin remove onebrain@onebrain";

pub fn install(vault: &Path, dry_run: bool) -> Result<i32> {
    if dry_run {
        println!(
            "plugin install: dry-run · would run `codex plugin marketplace add {}` then `codex plugin add onebrain@onebrain`",
            vault.display()
        );
        return Ok(0);
    }

    let codex = std::env::var_os("CODEX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let marketplace = match Command::new(&codex)
        .args(["plugin", "marketplace", "add"])
        .arg(vault)
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "plugin install: failed to spawn {}\nretry: {}",
                codex.display(),
                retry_command(vault)
            );
            return Err(error).with_context(|| format!("failed to spawn {}", codex.display()));
        }
    };
    if !marketplace.success() {
        eprintln!(
            "plugin install: Codex marketplace registration failed\nretry: {}",
            retry_command(vault)
        );
        return Ok(marketplace.code().unwrap_or(1));
    }

    let was_managed = has_managed_installation(vault);
    let config_backup = merge_codex_config()?;
    if !was_managed {
        write_receipt(vault, "pending")?;
    }

    let add = match Command::new(&codex)
        .args(["plugin", "add", "onebrain@onebrain"])
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            rollback_install(vault, &codex, Some(&config_backup), was_managed);
            eprintln!(
                "plugin install: failed to spawn {}\nretry: {}",
                codex.display(),
                retry_command(vault)
            );
            return Err(error).with_context(|| format!("failed to spawn {}", codex.display()));
        }
    };
    if !add.success() {
        rollback_install(vault, &codex, Some(&config_backup), was_managed);
        eprintln!(
            "plugin install: Codex plugin add failed\nretry: {}",
            retry_command(vault)
        );
        return Ok(add.code().unwrap_or(1));
    }

    if let Err(error) = write_marker(vault).and_then(|_| write_receipt(vault, "installed")) {
        rollback_install(vault, &codex, Some(&config_backup), was_managed);
        return Err(error).context(format!(
            "failed to finalize managed Codex installation; rolled back\nretry: {}",
            retry_command(vault)
        ));
    }
    println!("plugin install: codex · installed onebrain@onebrain");
    Ok(0)
}

/// Refresh an explicitly managed Codex installation after vault sync.
///
/// Absence of the vault-local marker means the user did not opt in, so Codex
/// global state must remain untouched.
pub fn refresh_if_managed(vault: &Path, dry_run: bool) -> Result<Option<i32>> {
    let codex = std::env::var_os("CODEX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    refresh_if_managed_with_bin(vault, dry_run, &codex, &codex_home()?)
}

pub fn uninstall(vault: &Path, dry_run: bool) -> Result<i32> {
    let marker = vault.join(".codex/onebrain-plugin.json");
    let receipt = receipt_path(vault)?;
    if !has_managed_installation(vault) && !has_cleanup_receipt(vault) {
        println!("plugin uninstall: codex · no managed installation");
        return Ok(0);
    }
    if dry_run {
        println!("plugin uninstall: dry-run · would run `{REMOVE_RETRY}`");
        return Ok(0);
    }

    let codex = std::env::var_os("CODEX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let status = match Command::new(&codex)
        .args(["plugin", "remove", "onebrain@onebrain"])
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "plugin uninstall: failed to spawn {}\nretry: {REMOVE_RETRY}",
                codex.display()
            );
            return Err(error).with_context(|| format!("failed to spawn {}", codex.display()));
        }
    };
    if !status.success() {
        eprintln!("plugin uninstall: Codex plugin remove failed\nretry: {REMOVE_RETRY}");
        return Ok(status.code().unwrap_or(1));
    }
    remove_if_exists(&marker)?;
    remove_if_exists(&receipt)?;
    println!("plugin uninstall: codex · removed onebrain@onebrain");
    Ok(0)
}

fn refresh_if_managed_with_bin(
    vault: &Path,
    dry_run: bool,
    codex: &Path,
    home: &Path,
) -> Result<Option<i32>> {
    if !has_managed_installation_in(vault, home) {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(0));
    }

    let status = match Command::new(codex)
        .args(["plugin", "add", "onebrain@onebrain"])
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "plugin update: failed to spawn {}\nretry: {REFRESH_RETRY}",
                codex.display()
            );
            return Err(error).with_context(|| format!("failed to spawn {}", codex.display()));
        }
    };
    if !status.success() {
        eprintln!("plugin update: managed Codex refresh failed\nretry: {REFRESH_RETRY}");
        return Ok(Some(status.code().unwrap_or(1)));
    }
    Ok(Some(0))
}

fn has_managed_marker(vault: &Path) -> bool {
    let path = vault.join(".codex/onebrain-plugin.json");
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|value| {
            value.get("managed").and_then(|v| v.as_bool()) == Some(true)
                && value.get("plugin").and_then(|v| v.as_str()) == Some("onebrain@onebrain")
        })
}

pub(crate) fn has_managed_installation(vault: &Path) -> bool {
    codex_home()
        .ok()
        .is_some_and(|home| has_managed_installation_in(vault, &home))
}

fn has_managed_installation_in(vault: &Path, home: &Path) -> bool {
    has_managed_marker(vault) && receipt_matches_at(vault, home, Some("installed"))
}

fn has_cleanup_receipt(vault: &Path) -> bool {
    receipt_matches(vault, None)
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .map(|p| p.join(".codex"))
        .ok_or_else(|| anyhow!("cannot resolve CODEX_HOME"))
}

fn canonical_vault(vault: &Path) -> Result<PathBuf> {
    fs::canonicalize(vault)
        .with_context(|| format!("cannot resolve vault path {}", vault.display()))
}

fn receipt_path(vault: &Path) -> Result<PathBuf> {
    receipt_path_at(vault, &codex_home()?)
}

fn receipt_path_at(vault: &Path, home: &Path) -> Result<PathBuf> {
    let canonical = canonical_vault(vault)?;
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    Ok(home
        .join("onebrain-managed")
        .join(format!("{digest:x}.json")))
}

fn receipt_matches(vault: &Path, required_state: Option<&str>) -> bool {
    let Ok(home) = codex_home() else {
        return false;
    };
    receipt_matches_at(vault, &home, required_state)
}

fn receipt_matches_at(vault: &Path, home: &Path, required_state: Option<&str>) -> bool {
    let Ok(canonical) = canonical_vault(vault) else {
        return false;
    };
    let Ok(path) = receipt_path_at(vault, home) else {
        return false;
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|value| {
            value.get("managed").and_then(|v| v.as_bool()) == Some(true)
                && value.get("plugin").and_then(|v| v.as_str()) == Some("onebrain@onebrain")
                && value.get("vault").and_then(|v| v.as_str())
                    == Some(canonical.to_string_lossy().as_ref())
                && required_state
                    .is_none_or(|state| value.get("state").and_then(|v| v.as_str()) == Some(state))
        })
}

fn write_receipt(vault: &Path, state: &str) -> Result<()> {
    let canonical = canonical_vault(vault)?;
    write_json_atomic(
        &receipt_path(vault)?,
        &json!({
            "managed": true,
            "plugin": "onebrain@onebrain",
            "vault": canonical,
            "state": state
        }),
    )
}

#[derive(Debug)]
struct ConfigBackup {
    path: PathBuf,
    existed: bool,
}

fn merge_codex_config() -> Result<ConfigBackup> {
    let path = codex_home()?.join("config.toml");
    let original = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let text = merge_feature_flags(original.as_deref().unwrap_or_default());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let backup = path.with_extension("toml.onebrain.bak");
    if let Some(original) = &original {
        fs::write(&backup, original)?;
    } else {
        remove_if_exists(&backup)?;
    }
    replace_file(&path, text.as_bytes())?;
    Ok(ConfigBackup {
        path,
        existed: original.is_some(),
    })
}

fn merge_feature_flags(input: &str) -> String {
    let mut lines: Vec<String> = input.lines().map(str::to_string).collect();
    let start = lines.iter().position(|line| line.trim() == "[features]");
    let insert_at = if let Some(start) = start {
        let mut end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, line)| line.trim().starts_with('['))
            .map(|(index, _)| index)
            .unwrap_or(lines.len());
        while end > start + 1 && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        for key in ["hooks", "multi_agent"] {
            let prefix = format!("{key} =");
            if let Some(index) =
                (start + 1..end).find(|&index| lines[index].trim().starts_with(&prefix))
            {
                lines[index] = format!("{key} = true");
            } else {
                lines.insert(end, format!("{key} = true"));
                end += 1;
            }
        }
        None
    } else {
        Some(lines.len())
    };
    if let Some(index) = insert_at {
        if index > 0 && !lines[index - 1].is_empty() {
            lines.push(String::new());
        }
        lines.extend([
            "[features]".to_string(),
            "hooks = true".to_string(),
            "multi_agent = true".to_string(),
        ]);
    }
    let mut output = lines.join("\n");
    output.push('\n');
    output
}

fn write_marker(vault: &Path) -> Result<()> {
    let path = vault.join(".codex/onebrain-plugin.json");
    write_json_atomic(
        &path,
        &json!({
        "managed": true,
        "plugin": "onebrain@onebrain"
        }),
    )
}

fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<()> {
    replace_file(path, &serde_json::to_vec_pretty(value)?)
}

fn replace_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("onebrain.tmp");
    fs::write(&tmp, contents)?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn restore_config(backup: &ConfigBackup) -> Result<()> {
    let backup_path = backup.path.with_extension("toml.onebrain.bak");
    if backup.existed {
        let contents = fs::read(&backup_path)?;
        replace_file(&backup.path, &contents)?;
    } else {
        remove_if_exists(&backup.path)?;
    }
    Ok(())
}

fn rollback_install(
    vault: &Path,
    codex: &Path,
    config_backup: Option<&ConfigBackup>,
    was_managed: bool,
) {
    if !was_managed {
        let _ = Command::new(codex)
            .args(["plugin", "remove", "onebrain@onebrain"])
            .status();
        let _ = fs::remove_file(vault.join(".codex/onebrain-plugin.json"));
        if let Ok(receipt) = receipt_path(vault) {
            let _ = fs::remove_file(receipt);
        }
    }
    if let Some(backup) = config_backup {
        let _ = restore_config(backup);
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn retry_command(vault: &Path) -> String {
    format!(
        "codex plugin marketplace add {} && codex plugin add onebrain@onebrain",
        shell_quote(vault.to_string_lossy().as_ref())
    )
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{
        has_managed_installation_in, has_managed_marker, merge_feature_flags, receipt_path_at,
        refresh_if_managed_with_bin, retry_command,
    };
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn feature_merge_preserves_other_tables_and_enables_flags() {
        let input =
            "model = \"gpt-5\"\n\n[features]\nhooks = false\n\n[mcp_servers.x]\ncommand = \"x\"\n";
        let output = merge_feature_flags(input);
        assert!(output.contains("[features]\nhooks = true\nmulti_agent = true\n"));
        assert!(output.contains("[mcp_servers.x]\ncommand = \"x\""));
        assert_eq!(merge_feature_flags(&output), output);
    }

    #[test]
    fn refresh_skips_vault_without_managed_marker() {
        let vault = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let missing_bin = vault.path().join("must-not-run");
        assert_eq!(
            refresh_if_managed_with_bin(vault.path(), false, &missing_bin, home.path()).unwrap(),
            None
        );
    }

    #[test]
    fn marker_requires_managed_true_and_expected_plugin() {
        let vault = tempfile::tempdir().unwrap();
        fs::create_dir_all(vault.path().join(".codex")).unwrap();
        let marker = vault.path().join(".codex/onebrain-plugin.json");
        for invalid in [
            "{}",
            r#"{"managed":false,"plugin":"onebrain@onebrain"}"#,
            r#"{"managed":true,"plugin":"other@onebrain"}"#,
            "not json",
        ] {
            fs::write(&marker, invalid).unwrap();
            assert!(!has_managed_marker(vault.path()));
        }
        fs::write(marker, r#"{"managed":true,"plugin":"onebrain@onebrain"}"#).unwrap();
        assert!(has_managed_marker(vault.path()));
    }

    #[cfg(unix)]
    #[test]
    fn managed_refresh_readds_plugin_in_place() {
        let vault = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(vault.path().join(".codex")).unwrap();
        fs::write(
            vault.path().join(".codex/onebrain-plugin.json"),
            r#"{"managed":true,"plugin":"onebrain@onebrain"}"#,
        )
        .unwrap();
        let receipt = receipt_path_at(vault.path(), home.path()).unwrap();
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(
            receipt,
            format!(
                r#"{{"managed":true,"plugin":"onebrain@onebrain","vault":"{}","state":"installed"}}"#,
                fs::canonicalize(vault.path()).unwrap().display()
            ),
        )
        .unwrap();
        let bin = vault.path().join("codex");
        let log = vault.path().join("argv.log");
        fs::write(
            &bin,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            refresh_if_managed_with_bin(vault.path(), false, &bin, home.path()).unwrap(),
            Some(0)
        );
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "plugin add onebrain@onebrain\n"
        );
    }

    #[test]
    fn vault_marker_without_matching_global_receipt_is_not_managed() {
        let vault = tempfile::tempdir().unwrap();
        let other_vault = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(vault.path().join(".codex")).unwrap();
        fs::write(
            vault.path().join(".codex/onebrain-plugin.json"),
            r#"{"managed":true,"plugin":"onebrain@onebrain"}"#,
        )
        .unwrap();
        assert!(!has_managed_installation_in(vault.path(), home.path()));

        let receipt = receipt_path_at(vault.path(), home.path()).unwrap();
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(
            receipt,
            format!(
                r#"{{"managed":true,"plugin":"onebrain@onebrain","vault":"{}","state":"installed"}}"#,
                fs::canonicalize(other_vault.path()).unwrap().display()
            ),
        )
        .unwrap();
        assert!(!has_managed_installation_in(vault.path(), home.path()));
    }

    #[cfg(not(windows))]
    #[test]
    fn retry_command_quotes_vault_paths_and_single_quotes() {
        let command = retry_command(Path::new("/tmp/brain's vault"));
        assert_eq!(
            command,
            "codex plugin marketplace add '/tmp/brain'\"'\"'s vault' && codex plugin add onebrain@onebrain"
        );
    }
}
