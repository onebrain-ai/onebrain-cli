//! `onebrain search model list` / `onebrain search model set` / bare
//! `onebrain search model` — inspect and switch the vault's embedding model.
//!
//! `list` is pure metadata (the registry in [`onebrain_search::embed`]) plus
//! a `current` flag from config — it MUST NOT open the engine or trigger a
//! model download. `set` persists the new model name to `onebrain.yml`
//! (reusing the vault's config-write + backup pattern — see
//! `commands::doctor::fix_vault_yml_keys` for the sibling implementation
//! this mirrors) and then opens the engine to re-embed the index.
//!
//! Bare `model` (no subcommand) is an interactive picker on a real TTY
//! (`inquire::Select` + a re-embed confirm, both reusing the exact same
//! apply logic as `set`) and a non-hanging informational fallback
//! otherwise — see [`run_bare`].

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
    // Marker column is 2 chars wide: `●` for the active model, `⭐` for the
    // registry default (first entry — see `embed::model_registry`). A model
    // that is both active and default shows `●` (active wins the slot).
    let mut lines = vec![format!(
        "{:<4}{:<24}{:<10}{:<6}{:<7}{}",
        "", "MODEL", "SIZE", "DIM", "THAI", "NOTE"
    )];
    for (i, m) in d.models.iter().enumerate() {
        let marker = if m.current {
            "●"
        } else if i == 0 {
            "⭐"
        } else {
            ""
        };
        let thai = m
            .thai_miracl
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(format!(
            "{:<4}{:<24}{:<10}{:<6}{:<7}{}",
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
        bail!(
            "unsupported embedding model '{}': supported names are {}",
            args.name,
            supported_model_names()
        );
    }

    let resolved = crate::vault_ctx::require(vault_flag)?;
    let envelope = apply_model_change(resolved, &args.name)?;
    emit(&envelope, mode, std::io::stdout().lock(), render_set_text)?;
    Ok(())
}

/// Comma-joined list of every registry model name — used in "unsupported
/// model" error messages and the non-TTY fallback hint.
fn supported_model_names() -> String {
    model_registry()
        .iter()
        .map(|m| m.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Apply a model switch for an already-resolved, already-validated vault:
/// no-op if `model_name` is already current, else persist to `onebrain.yml`
/// (with a config backup) and re-embed the index via `Engine::rebuild`.
///
/// Single shared code path for both `model set <name>` ([`run_set`]) and the
/// interactive picker's confirm step ([`run_picker`]) — callers are
/// responsible for name validation before calling this.
fn apply_model_change(
    resolved: onebrain_core::ResolvedVault,
    model_name: &str,
) -> Result<Envelope<ModelSetData>> {
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    if config.search.embed_model == model_name {
        let data = ModelSetData {
            model: model_name.to_string(),
            already_current: true,
            chunks_reembedded: None,
        };
        return Ok(Envelope::ok("search.model.set", Some(vault_info), data));
    }

    // Order matters for crash-safety. Open the engine while `onebrain.yml`
    // still points at the CURRENT model, so the vector store opens at its
    // existing on-disk dims (a dims-changing switch would otherwise `bail!`
    // in `VectorStore::open`). `rebuild` then wipes+recreates the vector dir
    // at the new model's dims and re-embeds. Only AFTER the rebuild succeeds
    // do we persist the new model name — so a failed switch never leaves
    // `onebrain.yml` pointing at a model whose index can't open.
    let (mut engine, resolved) = open_engine(Some(resolved.root.as_path().to_path_buf()))?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let reembedded = engine.rebuild(model_name)?;

    persist_embed_model(resolved.root.as_path(), model_name)
        .with_context(|| format!("persisting search.embed_model = {model_name}"))?;

    let data = ModelSetData {
        model: model_name.to_string(),
        already_current: false,
        chunks_reembedded: Some(reembedded),
    };
    Ok(Envelope::ok("search.model.set", Some(vault_info), data))
}

fn render_set_text(env: &Envelope<ModelSetData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.already_current {
        format!("✅ already using {}", d.model)
    } else {
        format!(
            "✅ switched to {} · 🧠 {} chunk(s) re-embedded",
            d.model,
            d.chunks_reembedded.unwrap_or(0)
        )
    }
}

/// `onebrain search model` (bare, no subcommand) — interactive picker on a
/// real TTY, non-hanging informational fallback otherwise.
///
/// TTY gate mirrors `commands::doctor::confirm_fix` /
/// `onebrain_fs::init::wizard`'s closed-stdin contract: both stdin AND
/// stdout must be real terminals, otherwise this never prompts — piped
/// output, agent/hook invocation, and CI must always take the fallback
/// branch and return immediately.
pub fn run_bare(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    use std::io::IsTerminal;

    let resolved = crate::vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let current = config.search.embed_model.clone();

    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_picker(resolved, mode, &current)
    } else {
        run_non_tty_fallback(resolved, mode, &current)
    }
}

/// Non-TTY fallback: print the current model + available names + a hint to
/// use `list` / `set` explicitly. Never opens the engine, never prompts,
/// never blocks — safe for pipes, agents, hooks, and scheduled runs.
fn run_non_tty_fallback(
    resolved: onebrain_core::ResolvedVault,
    mode: &OutputMode,
    current: &str,
) -> Result<()> {
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let data = ModelBareFallbackData {
        model: current.to_string(),
        available: model_registry().iter().map(|m| m.name).collect(),
        hint: "run 'onebrain search model list' for details, or 'onebrain search model set <name>' to switch."
            .to_string(),
    };
    let envelope = Envelope::ok("search.model.bare", Some(vault_info), data);
    emit(
        &envelope,
        mode,
        std::io::stdout().lock(),
        render_bare_fallback_text,
    )?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct ModelBareFallbackData {
    model: String,
    available: Vec<&'static str>,
    hint: String,
}

fn render_bare_fallback_text(env: &Envelope<ModelBareFallbackData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    format!(
        "current model: {}\navailable models: {}\n{}",
        d.model,
        d.available.join(", "),
        d.hint
    )
}

/// Interactive picker (real TTY only): arrow-key `Select` over the
/// registry, current model marked; on Enter, no-op if unchanged, else a
/// `Confirm` warning that switching re-embeds the whole vault (and may
/// download the new model), then — on yes — the SAME apply path `model set`
/// uses. Esc/cancel does nothing and exits 0.
fn run_picker(
    resolved: onebrain_core::ResolvedVault,
    mode: &OutputMode,
    current: &str,
) -> Result<()> {
    let options: Vec<String> = model_registry()
        .iter()
        .map(|m| format_picker_row(m, current))
        .collect();
    let starting_cursor = model_registry()
        .iter()
        .position(|m| m.name == current)
        .unwrap_or(0);

    let selection = match inquire::Select::new("Select embedding model:", options)
        .with_starting_cursor(starting_cursor)
        .prompt()
    {
        Ok(choice) => choice,
        Err(_) => return Ok(()), // Esc / Ctrl-C / cancel — do nothing, exit 0.
    };

    let Some(chosen) = model_registry()
        .iter()
        .find(|m| format_picker_row(m, current) == selection)
    else {
        return Ok(()); // Defensive: selection didn't match a known row.
    };

    if chosen.name == current {
        println!("already using {current}");
        return Ok(());
    }

    let confirmed = inquire::Confirm::new(&format!(
        "Switch to {}? This re-embeds the whole vault (and downloads the model if not cached).",
        chosen.name
    ))
    .with_default(false)
    .prompt()
    .unwrap_or(false);

    if !confirmed {
        return Ok(());
    }

    let envelope = apply_model_change(resolved, chosen.name)?;
    emit(&envelope, mode, std::io::stdout().lock(), render_set_text)?;
    Ok(())
}

/// `MODEL · SIZE · DIM · THAI · NOTE`, current model marked with `●`.
fn format_picker_row(info: &ModelInfo, current: &str) -> String {
    let marker = if info.name == current { "●" } else { " " };
    let thai = info
        .thai_miracl
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "—".to_string());
    format!(
        "{marker} {} · {} · {}d · {} · {}",
        info.name, info.approx_size, info.dims, thai, info.note
    )
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

    onebrain_fs::atomic_write_text(&path, &serialized)
        .with_context(|| format!("writing {}", path.display()))
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
            marked_line.trim_start().starts_with('●'),
            "expected current-model marker on: {marked_line}"
        );
    }

    #[test]
    fn list_text_marks_default_model_when_not_current() {
        // Current is bge-m3, so the default (first registry entry) is not the
        // active one — it should carry the ⭐ default marker instead.
        let s = render_list_text(&list_env("bge-m3"));
        let default_name = model_registry()[0].name;
        let default_line = s
            .lines()
            .find(|l| l.contains(default_name))
            .expect("default row present");
        assert!(
            default_line.trim_start().starts_with('⭐'),
            "expected default-model marker on: {default_line}"
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
        assert_eq!(render_set_text(&env), "✅ already using bge-m3");
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
