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

use crate::cli::{ModelSortCol, SearchModelListArgs, SearchModelRemoveArgs, SearchModelSetArgs};
use crate::commands::search_common::{collection_cache_dir, collection_for, open_engine};
use crate::output::{emit, Envelope, OutputMode};
use onebrain_core::load_vault_config;
use onebrain_search::embed::{
    is_supported_model, model_download_status, model_registry, ModelInfo,
};

#[derive(Debug, Serialize)]
struct ModelListEntry {
    name: &'static str,
    dims: usize,
    approx_size: &'static str,
    context: usize,
    thai_miracl: Option<f32>,
    note: &'static str,
    current: bool,
    /// Whether the model's `models--*` dir exists under the cache dir.
    downloaded: bool,
    /// On-disk byte size of the downloaded model, `None` if not downloaded.
    disk_bytes: Option<u64>,
}

impl ModelListEntry {
    fn from_info(info: &ModelInfo, current_model: &str, cache_dir: &Path) -> Self {
        let status = model_download_status(info, cache_dir);
        Self {
            name: info.name,
            dims: info.dims,
            approx_size: info.approx_size,
            context: info.context,
            thai_miracl: info.thai_miracl,
            note: info.note,
            current: info.name == current_model,
            downloaded: status.downloaded,
            disk_bytes: status.disk_size,
        }
    }
}

#[derive(Debug, Serialize)]
struct ModelListData {
    models: Vec<ModelListEntry>,
    /// Collection cache dir where downloaded models live (footer + JSON).
    cache_dir: PathBuf,
}

/// `onebrain search model list` — never opens the engine / downloads
/// anything: pure registry metadata + a `current` flag from config, plus
/// per-model download status (`downloaded` / `disk_bytes`) computed by a pure
/// `std::fs` scan of the collection cache dir. Optional `--sort <col>`
/// reorders the rows.
pub fn run_list(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchModelListArgs,
) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let collection = collection_for(&resolved).context("resolve collection")?;
    let cache_dir = collection_cache_dir(&collection);

    let mut models: Vec<ModelListEntry> = model_registry()
        .iter()
        .map(|m| ModelListEntry::from_info(m, &config.search.embed_model, &cache_dir))
        .collect();

    if let Some(col) = args.sort {
        sort_rows(&mut models, col, args.desc);
    }

    let envelope = Envelope::ok(
        "search.model.list",
        Some(vault_info),
        ModelListData { models, cache_dir },
    );
    emit(&envelope, mode, std::io::stdout().lock(), render_list_text)?;
    Ok(())
}

/// Sort list rows by `col`. Default (unsorted) is registry order; this is
/// only called when `--sort` is passed. Models with no Thai score always
/// sort last regardless of direction, so `--desc` never floats a `—` to the
/// top. `disk` sorts by the DISPLAYED size — real on-disk bytes when
/// downloaded, the parsed registry approx otherwise — so the order always
/// matches the column.
fn sort_rows(rows: &mut [ModelListEntry], col: ModelSortCol, desc: bool) {
    use std::cmp::Ordering;
    // Approx registry size is a human string ("~470 MB", "~1.1 GB"); parse it
    // to bytes for a meaningful numeric sort.
    rows.sort_by(|a, b| {
        let ord = match col {
            ModelSortCol::Name => a.name.cmp(b.name),
            ModelSortCol::Dim => a.dims.cmp(&b.dims),
            ModelSortCol::Size => {
                parse_approx_size(a.approx_size).cmp(&parse_approx_size(b.approx_size))
            }
            ModelSortCol::Thai => cmp_option_last(a.thai_miracl, b.thai_miracl, desc),
            ModelSortCol::Disk => {
                let ab = a
                    .disk_bytes
                    .unwrap_or_else(|| parse_approx_size(a.approx_size));
                let bb = b
                    .disk_bytes
                    .unwrap_or_else(|| parse_approx_size(b.approx_size));
                ab.cmp(&bb).then_with(|| a.name.cmp(b.name))
            }
            // Ascending = downloaded first (✓ on top), ties break by name.
            ModelSortCol::Downloaded => b
                .downloaded
                .cmp(&a.downloaded)
                .then_with(|| a.name.cmp(b.name)),
        };
        // For the "missing sorts last" column we've already folded direction
        // into `cmp_option_last`; don't reverse it again.
        match col {
            ModelSortCol::Thai => ord,
            _ if desc => ord.reverse(),
            _ => ord,
        }
        .then(Ordering::Equal)
    });
}

/// Compare two `Option`s so that `None` always sorts LAST, while `Some`
/// values honour `desc`. Keeps not-downloaded / no-score rows at the bottom
/// of both ascending and descending sorts.
pub(crate) fn cmp_option_last<T: PartialOrd + Copy>(
    a: Option<T>,
    b: Option<T>,
    desc: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            // Total order over the compared values (NaN treated as Equal —
            // registry scores are never NaN, this is just defensive).
            let base = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
            if desc {
                base.reverse()
            } else {
                base
            }
        }
    }
}

/// Parse a registry `approx_size` string like `"~470 MB"` / `"~1.1 GB"` /
/// `"~180 MB"` into an approximate byte count for sorting. Unparseable
/// strings sort as `0`.
pub(crate) fn parse_approx_size(s: &str) -> u64 {
    let cleaned = s.trim_start_matches('~').trim();
    let (num_part, unit) = match cleaned.split_once(' ') {
        Some((n, u)) => (n, u.trim().to_ascii_uppercase()),
        None => (cleaned, String::new()),
    };
    let Ok(num) = num_part.parse::<f64>() else {
        return 0;
    };
    let mult = match unit.as_str() {
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "MB" => 1024.0 * 1024.0,
        "KB" => 1024.0,
        _ => 1.0,
    };
    (num * mult) as u64
}

/// Human-readable byte size (`471 MB`, `1.2 GB`, `840 KB`, `12 B`). Mirrors
/// `search_status::format_size`.
pub(crate) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn render_list_text(env: &Envelope<ModelListData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    // Fixed-width columns: marker · MODEL · DOWNLOADED · DISK · DIM · THAI ·
    // NOTE. The marker column is `●` for the active model, blank otherwise.
    const MARKER: usize = 2;
    const NAME: usize = 24;
    const DL: usize = 12;
    const DISK: usize = 10;
    const DIM: usize = 6;
    const THAI: usize = 6;

    let header = format!(
        "{:<MARKER$}{:<NAME$}{:<DL$}{:<DISK$}{:<DIM$}{:<THAI$}{}",
        "", "MODEL", "DOWNLOADED", "DISK", "DIM", "THAI", "NOTE"
    );
    // Light underline header (hand-rolled, no deps).
    let underline = "─".repeat(header.chars().count());

    let mut lines = vec![header, underline];
    for m in &d.models {
        let marker = if m.current { "●" } else { "" };
        // Plain "—" for not-downloaded: ⬜ renders as a huge white block in
        // some terminal fonts.
        let downloaded = if m.downloaded { "✓" } else { "—" };
        let disk = disk_cell(m.downloaded, m.disk_bytes, m.approx_size);
        let thai = m
            .thai_miracl
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(format!(
            "{:<MARKER$}{:<NAME$}{:<DL$}{:<DISK$}{:<DIM$}{:<THAI$}{}",
            marker, m.name, downloaded, disk, m.dims, thai, m.note
        ));
    }
    lines.push(format!("📁 models: {}", d.cache_dir.display()));
    lines.join("\n")
}

/// DISK column text: the real on-disk size once downloaded, else the
/// registry's approximate download size (`~470 MB`) so the user can see how
/// big a model is BEFORE choosing to download it. Shared by the static list
/// table and the interactive TUI.
pub(crate) fn disk_cell(downloaded: bool, disk_bytes: Option<u64>, approx_size: &str) -> String {
    match (downloaded, disk_bytes) {
        (true, Some(b)) => format_size(b),
        _ => approx_size.to_string(),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelSetData {
    model: String,
    already_current: bool,
    pub(crate) chunks_reembedded: Option<usize>,
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
pub(crate) fn apply_model_change(
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

pub(crate) fn render_set_text(env: &Envelope<ModelSetData>) -> String {
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

#[derive(Debug, Serialize)]
struct ModelRemoveData {
    model: String,
    /// `true` when files were actually deleted from disk.
    removed: bool,
    /// Bytes freed by the removal, `None` when nothing was removed.
    freed_bytes: Option<u64>,
    /// `true` when `<name>` was the active `search.embed_model`.
    was_current: bool,
}

/// `onebrain search model remove <name>` — delete a downloaded model's cache
/// dir (`models--*`).
///
/// - Unknown name → error listing supported names (mirrors `set`).
/// - Not downloaded → friendly "nothing to remove", exit 0.
/// - Active model guard: on a TTY, `inquire::Confirm` warns before deleting;
///   on non-TTY, refuse unless `--force`.
pub fn run_remove(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchModelRemoveArgs,
) -> Result<()> {
    use std::io::IsTerminal;

    if !is_supported_model(&args.name) {
        bail!(
            "unsupported embedding model '{}': supported names are {}",
            args.name,
            supported_model_names()
        );
    }

    let resolved = crate::vault_ctx::require(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let collection = collection_for(&resolved).context("resolve collection")?;
    let cache_dir = collection_cache_dir(&collection);

    let info = model_registry()
        .iter()
        .find(|m| m.name == args.name)
        .expect("name validated as supported above");
    let status = model_download_status(info, &cache_dir);
    let was_current = config.search.embed_model == args.name;

    // Nothing on disk → friendly no-op, exit 0.
    if !status.downloaded {
        let data = ModelRemoveData {
            model: args.name.clone(),
            removed: false,
            freed_bytes: None,
            was_current,
        };
        let envelope = Envelope::ok("search.model.remove", Some(vault_info), data);
        emit(
            &envelope,
            mode,
            std::io::stdout().lock(),
            render_remove_text,
        )?;
        return Ok(());
    }

    // Active-model guard: never silently delete the model the next search
    // would use. On a TTY, confirm; on a pipe/agent/hook, refuse unless
    // `--force`.
    if was_current && !args.force {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            let confirmed = inquire::Confirm::new(&format!(
                "'{}' is the current model; removing means the next search re-downloads it — continue?",
                args.name
            ))
            .with_default(false)
            .prompt()
            .unwrap_or(false);
            if !confirmed {
                let data = ModelRemoveData {
                    model: args.name.clone(),
                    removed: false,
                    freed_bytes: None,
                    was_current,
                };
                let envelope = Envelope::ok("search.model.remove", Some(vault_info), data);
                emit(
                    &envelope,
                    mode,
                    std::io::stdout().lock(),
                    render_remove_text,
                )?;
                return Ok(());
            }
        } else {
            bail!(
                "'{}' is the current embedding model — refusing to remove it non-interactively. \
                 Re-run with --force to override (the next search will re-download it).",
                args.name
            );
        }
    }

    let freed = status.disk_size.unwrap_or(0);
    std::fs::remove_dir_all(&status.path)
        .with_context(|| format!("removing {}", status.path.display()))?;

    let data = ModelRemoveData {
        model: args.name.clone(),
        removed: true,
        freed_bytes: Some(freed),
        was_current,
    };
    let envelope = Envelope::ok("search.model.remove", Some(vault_info), data);
    emit(
        &envelope,
        mode,
        std::io::stdout().lock(),
        render_remove_text,
    )?;
    Ok(())
}

fn render_remove_text(env: &Envelope<ModelRemoveData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.removed {
        format!(
            "🗑️  removed {} · freed {}",
            d.model,
            format_size(d.freed_bytes.unwrap_or(0))
        )
    } else {
        format!("nothing to remove — {} is not downloaded", d.model)
    }
}

/// `onebrain search model` (bare, no subcommand) — full-screen interactive
/// ratatui TUI on a real TTY (browse / sort / switch / delete models), or the
/// static `search model list` table otherwise.
///
/// TTY gate mirrors `commands::doctor::confirm_fix` /
/// `onebrain_fs::init::wizard`'s closed-stdin contract: both stdin AND stdout
/// must be real terminals, otherwise the TUI never launches — piped output,
/// agent/hook invocation, and CI fall straight through to the static table
/// (same output as the explicit `search model list`), which never blocks and
/// stays script-stable. Structured `--json` / `--yaml` also take the static
/// path so the machine-readable envelope is unchanged.
pub fn run_bare(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    use std::io::IsTerminal;

    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if tty && !mode.is_structured() {
        crate::commands::search_model_tui::run(vault_flag)
    } else {
        // Non-TTY / structured: identical to `search model list` with no sort
        // — a stable static table (or JSON/YAML envelope), never the TUI.
        run_list(vault_flag, mode, &SearchModelListArgs::bare())
    }
}

/// Prompt the user to choose a model via an `inquire::Select` picker
/// (registry rows, `current` marked, starting cursor on the current model).
/// Returns the chosen model's registry name, or `None` on Esc / Ctrl-C /
/// cancel.
///
/// Shared entry point so `search reindex`'s first-run "pick a model before we
/// download anything" prompt (see `search_reindex`) reuses the exact picker
/// wording and row format, rather than reinventing a second `Select`. Callers
/// are responsible for the TTY gate — this always prompts.
pub(crate) fn prompt_pick_model(current: &str) -> Option<&'static str> {
    let options: Vec<String> = model_registry()
        .iter()
        .map(|m| format_picker_row(m, current))
        .collect();
    let starting_cursor = model_registry()
        .iter()
        .position(|m| m.name == current)
        .unwrap_or(0);

    let selection = inquire::Select::new("Select embedding model:", options)
        .with_starting_cursor(starting_cursor)
        .prompt()
        .ok()?;

    model_registry()
        .iter()
        .find(|m| format_picker_row(m, current) == selection)
        .map(|m| m.name)
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
/// preserving every other key. Thin wrapper over the shared
/// `onebrain_fs::persist_search_key` (read → mutate `search.*` → backup →
/// atomic-write; `backup_config_file` is a hard precondition), which
/// `search_common::persist_collection` also uses.
fn persist_embed_model(vault_root: &Path, model_name: &str) -> Result<()> {
    onebrain_fs::persist_search_key(vault_root, "embed_model", model_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn list_env(current: &str) -> Envelope<ModelListData> {
        // Empty cache dir → nothing downloaded (pure-render tests).
        let cache_dir = PathBuf::from("/tmp/onebrain-test-cache/my-collection");
        let models: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, current, &cache_dir))
            .collect();
        Envelope::ok(
            "search.model.list",
            None,
            ModelListData { models, cache_dir },
        )
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
    fn list_text_only_current_row_has_marker() {
        let s = render_list_text(&list_env("bge-m3"));
        // The non-active default (first registry entry) row must not carry a
        // marker anymore — only the active model does.
        let default_name = model_registry()[0].name;
        assert_ne!(default_name, "bge-m3");
        let default_line = s
            .lines()
            .find(|l| l.contains(default_name))
            .expect("default row present");
        assert!(
            !default_line.trim_start().starts_with('●'),
            "non-active row should not carry the active marker: {default_line}"
        );
    }

    #[test]
    fn list_text_has_header_footer_and_all_models() {
        let s = render_list_text(&list_env("multilingual-e5-small"));
        assert!(s.contains("MODEL"));
        assert!(s.contains("DOWNLOADED"));
        assert!(s.contains("DISK"));
        assert!(s.contains("📁 models:"), "footer with cache dir present");
        for m in model_registry() {
            assert!(s.contains(m.name), "missing {} in rendered list", m.name);
        }
    }

    #[test]
    fn list_text_shows_downloaded_and_disk_for_present_model() {
        let cache = tempdir().unwrap();
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-small")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 3 * 1024 * 1024]).unwrap();

        let models: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, "bge-m3", cache.path()))
            .collect();
        let env = Envelope::ok(
            "search.model.list",
            None,
            ModelListData {
                models,
                cache_dir: cache.path().to_path_buf(),
            },
        );
        let s = render_list_text(&env);
        let row = s
            .lines()
            .find(|l| l.contains("multilingual-e5-small"))
            .unwrap();
        assert!(row.contains('✓'), "downloaded check missing: {row}");
        assert!(row.contains("3 MB"), "disk size missing: {row}");
        // Not-downloaded models show a plain "—" marker (no ⬜ — it renders
        // as a huge white block in some terminal fonts) and the registry's
        // approx size in DISK.
        let other = s.lines().find(|l| l.contains("bge-m3")).unwrap();
        assert!(
            other.contains('—'),
            "not-downloaded marker missing: {other}"
        );
        assert!(!other.contains('⬜'), "white-block glyph banned: {other}");
        assert!(other.contains("~2.2 GB"), "approx size missing: {other}");
    }

    #[test]
    fn sort_by_dim_ascending_and_descending() {
        let mut rows: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, "bge-m3", Path::new("/nope")))
            .collect();
        sort_rows(&mut rows, ModelSortCol::Dim, false);
        let dims: Vec<usize> = rows.iter().map(|r| r.dims).collect();
        let mut sorted = dims.clone();
        sorted.sort_unstable();
        assert_eq!(dims, sorted, "ascending dim sort");

        sort_rows(&mut rows, ModelSortCol::Dim, true);
        let dims_desc: Vec<usize> = rows.iter().map(|r| r.dims).collect();
        sorted.reverse();
        assert_eq!(dims_desc, sorted, "descending dim sort");
    }

    #[test]
    fn sort_by_name_ascending() {
        let mut rows: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, "bge-m3", Path::new("/nope")))
            .collect();
        sort_rows(&mut rows, ModelSortCol::Name, false);
        let names: Vec<&str> = rows.iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn sort_by_thai_keeps_none_last_in_both_directions() {
        for desc in [false, true] {
            let mut rows: Vec<ModelListEntry> = model_registry()
                .iter()
                .map(|m| ModelListEntry::from_info(m, "bge-m3", Path::new("/nope")))
                .collect();
            sort_rows(&mut rows, ModelSortCol::Thai, desc);
            // embeddinggemma has thai_miracl == None → must be last.
            assert_eq!(
                rows.last().unwrap().name,
                "embeddinggemma-300m-q",
                "None-thai model should sort last (desc={desc})"
            );
        }
    }

    #[test]
    fn sort_by_disk_uses_displayed_size_for_not_downloaded() {
        let mut rows: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, "bge-m3", Path::new("/nope")))
            .collect();
        // Nothing is downloaded → the DISK column shows approx sizes, so the
        // sort follows them: descending puts the biggest approx first.
        sort_rows(&mut rows, ModelSortCol::Disk, true);
        assert_eq!(rows.first().unwrap().name, "bge-m3", "~2.2 GB first desc");
        assert_eq!(
            rows.last().unwrap().name,
            "embeddinggemma-300m-q",
            "~180 MB last desc"
        );
        sort_rows(&mut rows, ModelSortCol::Disk, false);
        assert_eq!(rows.first().unwrap().name, "embeddinggemma-300m-q");
    }

    #[test]
    fn sort_by_size_parses_registry_strings() {
        let mut rows: Vec<ModelListEntry> = model_registry()
            .iter()
            .map(|m| ModelListEntry::from_info(m, "bge-m3", Path::new("/nope")))
            .collect();
        sort_rows(&mut rows, ModelSortCol::Size, false);
        // Smallest approx size first: embeddinggemma (~180 MB).
        assert_eq!(rows.first().unwrap().name, "embeddinggemma-300m-q");
    }

    #[test]
    fn parse_approx_size_units() {
        assert_eq!(parse_approx_size("~470 MB"), 470 * 1024 * 1024);
        assert_eq!(
            parse_approx_size("~1.1 GB"),
            (1.1 * 1024.0 * 1024.0 * 1024.0) as u64
        );
        assert_eq!(parse_approx_size("~180 MB"), 180 * 1024 * 1024);
        assert_eq!(parse_approx_size("garbage"), 0);
    }

    #[test]
    fn remove_text_reports_freed_size() {
        let env = Envelope::ok(
            "search.model.remove",
            None,
            ModelRemoveData {
                model: "bge-m3".to_string(),
                removed: true,
                freed_bytes: Some(2 * 1024 * 1024),
                was_current: false,
            },
        );
        let s = render_remove_text(&env);
        assert!(s.contains("removed bge-m3"), "{s}");
        assert!(s.contains("2 MB"), "{s}");
    }

    #[test]
    fn remove_text_reports_nothing_to_remove() {
        let env = Envelope::ok(
            "search.model.remove",
            None,
            ModelRemoveData {
                model: "bge-m3".to_string(),
                removed: false,
                freed_bytes: None,
                was_current: false,
            },
        );
        assert!(render_remove_text(&env).contains("nothing to remove"));
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
