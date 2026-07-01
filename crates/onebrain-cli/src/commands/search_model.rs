//! `onebrain search model list` / `onebrain search model set` — inspect and
//! switch the vault's embedding model.
//!
//! `list` is pure metadata (the registry in [`onebrain_search::embed`]) plus
//! a `current` flag from config — it MUST NOT open the engine or trigger a
//! model download. `set` persists the new model name to `onebrain.yml`
//! (reusing the vault's config-write + backup pattern — see
//! `commands::doctor::fix_vault_yml_keys` for the sibling implementation
//! this mirrors) and then opens the engine to re-embed the index.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::cli::SearchModelSetArgs;
use crate::commands::search_common::open_engine;
use crate::output::{emit, Envelope, OutputMode};
use onebrain_core::load_vault_config;
use onebrain_search::embed::{is_supported_model, model_registry, ModelInfo};

#[derive(Debug, Serialize)]
struct ModelListEntry {
    name: &'static str,
    dims: usize,
    approx_size: &'static str,
    context: usize,
    thai_miracl: Option<f32>,
    note: &'static str,
    current: bool,
}

impl ModelListEntry {
    fn from_info(info: &ModelInfo, current_model: &str) -> Self {
        Self {
            name: info.name,
            dims: info.dims,
            approx_size: info.approx_size,
            context: info.context,
            thai_miracl: info.thai_miracl,
            note: info.note,
            current: info.name == current_model,
        }
    }
}

#[derive(Debug, Serialize)]
struct ModelListData {
    models: Vec<ModelListEntry>,
}

/// `onebrain search model list` — never opens the engine / downloads
/// anything: pure registry metadata + a `current` flag from config.
pub fn run_list(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    let models: Vec<ModelListEntry> = model_registry()
        .iter()
        .map(|m| ModelListEntry::from_info(m, &config.search.embed_model))
        .collect();

    let envelope = Envelope::ok(
        "search.model.list",
        Some(vault_info),
        ModelListData { models },
    );
    emit(&envelope, mode, std::io::stdout().lock(), render_list_text)?;
    Ok(())
}

fn render_list_text(env: &Envelope<ModelListData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let mut lines = vec![format!(
        "{:<3}{:<24}{:<10}{:<6}{:<7}{}",
        "", "MODEL", "SIZE", "DIM", "THAI", "NOTE"
    )];
    for m in &d.models {
        let marker = if m.current { "*" } else { " " };
        let thai = m
            .thai_miracl
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(format!(
            "{:<3}{:<24}{:<10}{:<6}{:<7}{}",
            marker, m.name, m.approx_size, m.dims, thai, m.note
        ));
    }
    lines.join("\n")
}

#[derive(Debug, Serialize)]
struct ModelSetData {
    model: String,
    already_current: bool,
    chunks_reembedded: Option<usize>,
}

/// `onebrain search model set <name>` — validate, no-op if already active,
/// else persist to `onebrain.yml` (with a config backup) and re-embed the
/// index via `Engine::rebuild`.
pub fn run_set(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchModelSetArgs,
) -> Result<()> {
    if !is_supported_model(&args.name) {
        let supported = model_registry()
            .iter()
            .map(|m| m.name)
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unsupported embedding model '{}': supported names are {supported}",
            args.name
        );
    }

    let resolved = crate::vault_ctx::require(vault_flag.clone())?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    if config.search.embed_model == args.name {
        let data = ModelSetData {
            model: args.name.clone(),
            already_current: true,
            chunks_reembedded: None,
        };
        let envelope = Envelope::ok("search.model.set", Some(vault_info), data);
        emit(&envelope, mode, std::io::stdout().lock(), render_set_text)?;
        return Ok(());
    }

    persist_embed_model(resolved.root.as_path(), &args.name)
        .with_context(|| format!("persisting search.embed_model = {}", args.name))?;

    let (mut engine, resolved) = open_engine(Some(resolved.root.as_path().to_path_buf()))?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let reembedded = engine.rebuild(&args.name)?;

    let data = ModelSetData {
        model: args.name.clone(),
        already_current: false,
        chunks_reembedded: Some(reembedded),
    };
    let envelope = Envelope::ok("search.model.set", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_set_text)?;
    Ok(())
}

fn render_set_text(env: &Envelope<ModelSetData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.already_current {
        format!("already using {}", d.model)
    } else {
        format!(
            "switched to {} · {} chunk(s) re-embedded",
            d.model,
            d.chunks_reembedded.unwrap_or(0)
        )
    }
}

/// Persist `search.embed_model = model_name` into the vault's config file
/// (`onebrain.yml`, or legacy `vault.yml` if that's what's present),
/// preserving every other key. Mirrors
/// `commands::doctor::fix_vault_yml_keys`'s read → mutate mapping →
/// backup → atomic-write pattern: `onebrain_fs::backup_config_file` is a
/// hard precondition (no write proceeds without a successful backup, or a
/// confirmed-absent file for a fresh vault).
fn persist_embed_model(vault_root: &Path, model_name: &str) -> Result<()> {
    use onebrain_core::{find_config_file, CONFIG_FILENAME};

    let path = find_config_file(vault_root).unwrap_or_else(|| vault_root.join(CONFIG_FILENAME));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let mut yaml: serde_yaml::Value = if text.trim().is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    };
    if !yaml.is_mapping() {
        yaml = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let mapping = yaml.as_mapping_mut().expect("normalized to mapping above");

    let search_key = serde_yaml::Value::String("search".to_string());
    let needs_replace = match mapping.get(&search_key) {
        Some(v) => !v.is_mapping(),
        None => true,
    };
    if needs_replace {
        mapping.insert(
            search_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let search = mapping
        .get_mut(&search_key)
        .and_then(|v| v.as_mapping_mut())
        .expect("search key was just ensured to be a mapping");
    search.insert(
        serde_yaml::Value::String("embed_model".to_string()),
        serde_yaml::Value::String(model_name.to_string()),
    );

    let serialized = serde_yaml::to_string(&yaml).context("serializing updated config")?;

    // Defense-in-depth: back up the existing config before overwriting it.
    // Hard precondition — refuse the write if the backup couldn't be made.
    onebrain_fs::backup_config_file(&path)
        .with_context(|| format!("backing up {} before write", path.display()))?;

    atomic_write_text(&path, &serialized).with_context(|| format!("writing {}", path.display()))
}

/// Atomic text write: `path.tmp` → rename. Same pattern as
/// `commands::doctor::atomic_write_text`; duplicated locally rather than
/// shared across command modules since it's a two-line helper with no
/// existing shared home. Creates parent dirs as needed.
fn atomic_write_text(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn list_env(current: &str) -> Envelope<ModelListData> {
        let models: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, current))
            .collect();
        Envelope::ok("search.model.list", None, ModelListData { models })
    }

    #[test]
    fn list_text_marks_current_model() {
        let s = render_list_text(&list_env("bge-m3"));
        let marked_line = s
            .lines()
            .find(|l| l.contains("bge-m3"))
            .expect("bge-m3 row present");
        assert!(
            marked_line.trim_start().starts_with('*'),
            "expected current-model marker on: {marked_line}"
        );
    }

    #[test]
    fn list_text_has_header_and_all_models() {
        let s = render_list_text(&list_env("multilingual-e5-small"));
        assert!(s.contains("MODEL"));
        for m in model_registry() {
            assert!(s.contains(m.name), "missing {} in rendered list", m.name);
        }
    }

    #[test]
    fn set_text_reports_noop_when_already_current() {
        let env = Envelope::ok(
            "search.model.set",
            None,
            ModelSetData {
                model: "bge-m3".to_string(),
                already_current: true,
                chunks_reembedded: None,
            },
        );
        assert_eq!(render_set_text(&env), "already using bge-m3");
    }

    #[test]
    fn set_text_reports_switch_with_count() {
        let env = Envelope::ok(
            "search.model.set",
            None,
            ModelSetData {
                model: "bge-m3".to_string(),
                already_current: false,
                chunks_reembedded: Some(7),
            },
        );
        let s = render_set_text(&env);
        assert!(s.contains("switched to bge-m3"));
        assert!(s.contains("7 chunk(s) re-embedded"));
    }

    #[test]
    fn persist_embed_model_creates_fresh_config() {
        let dir = tempdir().unwrap();
        persist_embed_model(dir.path(), "bge-m3").unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("embed_model: bge-m3"));
    }

    #[test]
    fn persist_embed_model_preserves_other_keys() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: my-vault\n  embed_model: multilingual-e5-small\nfolders:\n  inbox: 00-inbox\n",
        )
        .unwrap();
        persist_embed_model(dir.path(), "bge-m3").unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("onebrain.yml")).unwrap();
        assert!(yaml.contains("embed_model: bge-m3"));
        assert!(yaml.contains("collection: my-vault"));
        assert!(yaml.contains("inbox: 00-inbox"));
    }

    #[test]
    fn persist_embed_model_writes_backup() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  embed_model: multilingual-e5-small\n",
        )
        .unwrap();
        persist_embed_model(dir.path(), "bge-m3").unwrap();
        let backup_dir = dir.path().join(".onebrain-backups");
        assert!(
            backup_dir.is_dir(),
            "expected a config backup to be written"
        );
        let mut entries = std::fs::read_dir(&backup_dir).unwrap();
        assert!(
            entries.next().is_some(),
            "expected at least one backup file"
        );
    }
}
