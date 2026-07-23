//! Managed Codex plugin installation. The marker is deliberately vault-local:
//! only an explicit opt-in authorizes later OneBrain updates to refresh Codex.

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const RETRY: &str = "codex plugin marketplace add <VAULT> && codex plugin add onebrain@onebrain";
const REFRESH_RETRY: &str =
    "codex plugin remove onebrain@onebrain && codex plugin add onebrain@onebrain";
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
                RETRY.replace("<VAULT>", &vault.to_string_lossy())
            );
            return Err(error).with_context(|| format!("failed to spawn {}", codex.display()));
        }
    };
    if !marketplace.success() {
        eprintln!(
            "plugin install: Codex marketplace registration failed\nretry: {}",
            RETRY.replace("<VAULT>", &vault.to_string_lossy())
        );
        return Ok(marketplace.code().unwrap_or(1));
    }

    let add = match Command::new(&codex)
        .args(["plugin", "add", "onebrain@onebrain"])
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "plugin install: failed to spawn {}\nretry: {}",
                codex.display(),
                RETRY.replace("<VAULT>", &vault.to_string_lossy())
            );
            return Err(error).with_context(|| format!("failed to spawn {}", codex.display()));
        }
    };
    if !add.success() {
        eprintln!(
            "plugin install: Codex plugin add failed\nretry: {}",
            RETRY.replace("<VAULT>", &vault.to_string_lossy())
        );
        return Ok(add.code().unwrap_or(1));
    }

    merge_codex_config()?;
    write_marker(vault)?;
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
    refresh_if_managed_with_bin(vault, dry_run, &codex)
}

pub fn uninstall(vault: &Path, dry_run: bool) -> Result<i32> {
    let marker = vault.join(".codex/onebrain-plugin.json");
    if !marker.is_file() {
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
    fs::remove_file(marker)?;
    println!("plugin uninstall: codex · removed onebrain@onebrain");
    Ok(0)
}

fn refresh_if_managed_with_bin(vault: &Path, dry_run: bool, codex: &Path) -> Result<Option<i32>> {
    if !vault.join(".codex/onebrain-plugin.json").is_file() {
        return Ok(None);
    }
    if dry_run {
        return Ok(Some(0));
    }

    for args in [
        &["plugin", "remove", "onebrain@onebrain"][..],
        &["plugin", "add", "onebrain@onebrain"][..],
    ] {
        let status = match Command::new(codex).args(args).status() {
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
    }
    Ok(Some(0))
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .map(|p| p.join(".codex"))
        .ok_or_else(|| anyhow!("cannot resolve CODEX_HOME"))
}

fn merge_codex_config() -> Result<()> {
    let path = codex_home()?.join("config.toml");
    let text = merge_feature_flags(&fs::read_to_string(&path).unwrap_or_default());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)?;
    Ok(())
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
    fs::create_dir_all(path.parent().expect("marker has parent"))?;
    let body = serde_json::to_vec_pretty(&json!({
        "managed": true,
        "plugin": "onebrain@onebrain"
    }))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{merge_feature_flags, refresh_if_managed_with_bin};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
        let missing_bin = vault.path().join("must-not-run");
        assert_eq!(
            refresh_if_managed_with_bin(vault.path(), false, &missing_bin).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_refresh_removes_then_readds_plugin() {
        let vault = tempfile::tempdir().unwrap();
        fs::create_dir_all(vault.path().join(".codex")).unwrap();
        fs::write(
            vault.path().join(".codex/onebrain-plugin.json"),
            r#"{"managed":true}"#,
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
            refresh_if_managed_with_bin(vault.path(), false, &bin).unwrap(),
            Some(0)
        );
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "plugin remove onebrain@onebrain\nplugin add onebrain@onebrain\n"
        );
    }
}
