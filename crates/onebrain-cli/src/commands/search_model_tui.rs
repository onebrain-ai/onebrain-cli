//! `onebrain search model` (bare, on a TTY) — a compact interactive ratatui
//! table of the embedding-model registry, drawn inline right below the
//! command (no alternate screen / full-screen takeover); the final frame
//! stays in the scrollback after quitting.
//!
//! Columns: current-marker (`●`) · MODEL · DOWNLOADED (`✓`/`⬜`) · DISK · DIM ·
//! THAI · NOTE — matching the static `search model list` table's emoji/columns
//! for visual consistency.
//!
//! Keybindings:
//! - `↑`/`↓` (or `k`/`j`) — move the selection
//! - `s` — cycle the sort column following the on-screen order (MODEL →
//!   DOWNLOADED → DISK → DIM → THAI → back to MODEL); the active column shows
//!   an ↑/↓ indicator in the header. Default sort: MODEL name, ascending.
//! - `r` — reverse the sort direction (ASC ↔ DESC) on the active column
//! - `Enter` — switch the active model to the selected row: downloads the
//!   model IMMEDIATELY if missing (an in-table progress bar with % replaces
//!   the row's NOTE while the `models--*` dir fills), then re-embeds via the
//!   SHARED [`search_model::apply_model_change`] path (persist AFTER a
//!   successful rebuild, never before); no-op with a footer hint if already
//!   active
//! - `d` — delete the selected model's on-disk download, with an inline y/n
//!   confirm row; refuses to delete the ACTIVE model (footer warning)
//! - `q` / `Esc` — quit
//!
//! ## Testability
//!
//! The raw-mode event loop ([`run`], [`event_loop`]) is TTY-only and lives in
//! the coverage exclusion allowlist. Everything it depends on — building the
//! row model from the registry + download status ([`build_rows`]), sorting
//! ([`sort_rows`]) and sort-column cycling ([`next_sort`]), the footer/legend
//! text ([`footer_text`]), and the pure state transitions on [`AppState`]
//! (`move_up`/`move_down`/`cycle_sort`/`toggle_desc`/`begin_delete`/…) — is a
//! plain function or method with unit tests, so the interesting logic is
//! covered without driving a terminal.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::ModelSortCol;
use crate::commands::search_common::format_size;
use crate::commands::search_common::{
    collection_cache_dir, collection_for, reconcile_missing_model,
};
#[cfg(feature = "semantic")]
use crate::commands::search_model::apply_model_change;
use crate::commands::search_model::{cmp_option_last, disk_cell};
use onebrain_core::load_vault_config;
use onebrain_search::embed::{dir_size_bytes, model_download_status, model_registry};

/// One row in the interactive model table — a flattened, self-contained view
/// of a registry entry plus its on-disk download status, carrying everything
/// the render + keybindings need (including the model dir `path` so `d` can
/// delete it without recomputing).
#[derive(Debug, Clone, PartialEq)]
pub struct TuiRow {
    pub name: &'static str,
    pub current: bool,
    pub downloaded: bool,
    pub disk_bytes: Option<u64>,
    /// Registry approx download size (`~470 MB`) — shown in DISK until the
    /// model is actually on disk.
    pub approx_size: &'static str,
    /// Registry approx download size in bytes — the DISK sort key while the
    /// model isn't downloaded (so the sort matches what the column shows).
    pub approx_bytes: u64,
    pub dims: usize,
    pub thai: Option<f32>,
    pub note: &'static str,
    /// The model's `models--*` dir (whether or not it exists on disk).
    pub path: PathBuf,
}

/// Build the table rows from the registry, flagging the `current` model and
/// filling per-row download status (downloaded / disk size / model dir path)
/// from a pure `std::fs` scan of `cache_dir`. Never opens the engine or
/// downloads anything.
pub fn build_rows(current: &str, cache_dir: &std::path::Path) -> Vec<TuiRow> {
    model_registry()
        .iter()
        .map(|m| {
            let status = model_download_status(m, cache_dir);
            TuiRow {
                name: m.name,
                current: m.name == current,
                downloaded: status.downloaded,
                disk_bytes: status.disk_size,
                approx_size: m.approx_size,
                approx_bytes: m.approx_bytes,
                dims: m.dims,
                thai: m.thai_miracl,
                note: m.note,
                path: status.path,
            }
        })
        .collect()
}

/// The next sort column in the `s`-key cycle, following the on-screen column
/// order: MODEL → DOWNLOADED → DISK → DIM → THAI → back to MODEL.
pub fn next_sort(col: ModelSortCol) -> ModelSortCol {
    match col {
        ModelSortCol::Name => ModelSortCol::Downloaded,
        ModelSortCol::Downloaded => ModelSortCol::Disk,
        ModelSortCol::Disk => ModelSortCol::Dim,
        ModelSortCol::Dim => ModelSortCol::Thai,
        ModelSortCol::Thai => ModelSortCol::Name,
        // `Size` isn't part of the TUI cycle (no approx-size column shown); if
        // it ever leaks in, fold it back to the start of the cycle.
        ModelSortCol::Size => ModelSortCol::Name,
    }
}

/// Sort `rows` by `col`/`desc`. `thai` keeps missing values last in BOTH
/// directions (via the shared [`cmp_option_last`]); `disk` sorts by the
/// DISPLAYED size — real on-disk bytes when downloaded, registry
/// `approx_bytes` otherwise — so the order always matches the column.
pub fn sort_rows(rows: &mut [TuiRow], col: ModelSortCol, desc: bool) {
    rows.sort_by(|a, b| {
        let ord = match col {
            ModelSortCol::Name => a.name.cmp(b.name),
            ModelSortCol::Dim => a.dims.cmp(&b.dims),
            ModelSortCol::Thai => cmp_option_last(a.thai, b.thai, desc),
            ModelSortCol::Disk => {
                let ab = a.disk_bytes.unwrap_or(a.approx_bytes);
                let bb = b.disk_bytes.unwrap_or(b.approx_bytes);
                ab.cmp(&bb).then_with(|| a.name.cmp(b.name))
            }
            // Ascending = downloaded first (✓ on top), ties break by name.
            ModelSortCol::Downloaded => b
                .downloaded
                .cmp(&a.downloaded)
                .then_with(|| a.name.cmp(b.name)),
            // Not a TUI sort column — treat as name so the comparator is total.
            ModelSortCol::Size => a.name.cmp(b.name),
        };
        match col {
            // Direction already folded into cmp_option_last for this one.
            ModelSortCol::Thai => ord,
            _ if desc => ord.reverse(),
            _ => ord,
        }
    });
}

/// Human label for a sort column (footer display).
pub fn sort_label(col: ModelSortCol) -> &'static str {
    match col {
        ModelSortCol::Name => "name",
        ModelSortCol::Size => "size",
        ModelSortCol::Dim => "dim",
        ModelSortCol::Thai => "thai",
        ModelSortCol::Disk => "disk",
        ModelSortCol::Downloaded => "downloaded",
    }
}

/// Header label for a column, with the active sort column carrying an
/// inline direction indicator (e.g. `MODEL ↑`).
pub fn header_label(base: &str, active: bool, desc: bool) -> String {
    if active {
        format!("{base} {}", if desc { "↓" } else { "↑" })
    } else {
        base.to_string()
    }
}

/// The footer legend + current sort indicator shown under the table. When
/// `no_active_model` is set (no row is both `current` and `downloaded`), a
/// warning is prepended so the user knows to select a row and download it.
pub fn footer_text(col: ModelSortCol, desc: bool, no_active_model: bool) -> String {
    let dir = if desc { "↓" } else { "↑" };
    let legend = format!(
        "↑/↓ move · s sort · r reverse · enter switch · d delete · q quit    sort: {} {dir}",
        sort_label(col)
    );
    if no_active_model {
        format!("⚠️  No model downloaded — select a row and press enter to download    {legend}")
    } else {
        legend
    }
}

/// The interactive TUI's mutable state: the sorted rows, current selection,
/// sort column/direction, a transient `status` line, and (when the user hits
/// `d`) a `pending_delete` index awaiting the inline y/n confirm.
///
/// All transitions here are pure (no I/O, no terminal) so they're unit-tested
/// directly; the event loop just calls them and re-renders.
#[derive(Debug)]
pub struct AppState {
    pub rows: Vec<TuiRow>,
    pub selected: usize,
    pub sort: ModelSortCol,
    pub desc: bool,
    /// Transient message shown in the status line (hints, warnings, results).
    pub status: Option<String>,
    /// Set to the selected row index while an inline delete confirm is up.
    pub pending_delete: Option<usize>,
    /// Flipped by [`AppState::request_quit`]; the event loop reads it to exit.
    pub should_quit: bool,
    /// The collection's model cache dir — kept so a successful switch can
    /// re-scan download status (DOWNLOADED/DISK go stale otherwise).
    pub cache_dir: PathBuf,
    /// Set while an Enter-triggered download is running: the row named here
    /// renders an in-table progress bar instead of its NOTE text.
    pub downloading: Option<DownloadUi>,
    /// Set while a switch's re-embed phase is running: the row named here
    /// renders an in-table progress bar driven by chunk counts.
    pub reembed: Option<ReembedUi>,
}

/// In-table re-embed progress for one model: exact chunk counts streamed
/// from `Engine::rebuild_with_progress`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReembedUi {
    pub name: &'static str,
    pub done: usize,
    pub total: usize,
}

/// Re-embed percentage from exact chunk counts (0–100; `total == 0` → 100,
/// treated as already done).
pub fn reembed_pct(done: usize, total: usize) -> u8 {
    if total == 0 {
        return 100;
    }
    ((done * 100 / total).min(100)) as u8
}

/// In-table download progress for one model: the row to decorate, the
/// expected total (registry `approx_bytes`), and the last polled percentage.
/// The event loop refreshes `pct` from a disk scan each tick; render is pure.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadUi {
    pub name: &'static str,
    pub expected_bytes: u64,
    /// The model's `models--*` dir being polled.
    pub model_dir: PathBuf,
    /// Last computed progress percentage (0–99 while running).
    pub pct: u8,
    /// Last polled on-disk byte count (for the footer's "X / ~Y" line).
    pub bytes: u64,
}

/// Download percentage from polled `current` bytes vs the registry's
/// `expected` total, capped at 99 — only a finished download reads 100%
/// (the estimate is approximate, so the bar must never claim done early).
pub fn download_pct(current: u64, expected: u64) -> u8 {
    if expected == 0 {
        return 0;
    }
    // u128 math: `current * 100` can't overflow, even at u64::MAX.
    ((current as u128 * 100 / expected as u128).min(99)) as u8
}

/// A fixed-width text progress bar like `▓▓▓▓▓░░░░░░░  42%`.
pub fn progress_bar(pct: u8, width: usize) -> String {
    let filled = (pct as usize * width) / 100;
    let mut bar = String::with_capacity(width + 6);
    for i in 0..width {
        bar.push(if i < filled { '▓' } else { '░' });
    }
    format!("{bar} {pct:>3}%")
}

impl AppState {
    /// Build initial state from rows, selecting the CURRENT model if present
    /// (else the first row). Default sort: MODEL name, ascending.
    pub fn new(mut rows: Vec<TuiRow>, cache_dir: PathBuf) -> Self {
        sort_rows(&mut rows, ModelSortCol::Name, false);
        let selected = rows.iter().position(|r| r.current).unwrap_or(0);
        Self {
            rows,
            selected,
            sort: ModelSortCol::Name,
            desc: false,
            status: None,
            pending_delete: None,
            should_quit: false,
            cache_dir,
            downloading: None,
            reembed: None,
        }
    }

    /// After a successful model switch: rebuild every row from a fresh
    /// cache-dir scan (download status may have changed — a dims-changing
    /// rebuild downloads the new model), mark `name` current, and re-apply
    /// the active sort while keeping the same model selected.
    pub fn refresh_after_switch(&mut self, name: &str) {
        let selected_name = self.selected_row().map(|r| r.name);
        self.rows = build_rows(name, &self.cache_dir);
        sort_rows(&mut self.rows, self.sort, self.desc);
        if let Some(sel) = selected_name {
            if let Some(idx) = self.rows.iter().position(|r| r.name == sel) {
                self.selected = idx;
            }
        }
    }

    /// Currently-selected row, if any (the table is never empty in practice —
    /// the registry always has entries — but this stays defensive).
    pub fn selected_row(&self) -> Option<&TuiRow> {
        self.rows.get(self.selected)
    }

    /// Move the selection down one row (saturating at the last row).
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    /// Move the selection up one row (saturating at the first row).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Cycle to the next sort column and re-sort, keeping the same selected
    /// MODEL highlighted (selection follows the row, not the index).
    pub fn cycle_sort(&mut self) {
        self.sort = next_sort(self.sort);
        self.resort_preserving_selection();
        self.status = None;
    }

    /// Reverse the sort direction and re-sort, keeping the selected model.
    pub fn toggle_desc(&mut self) {
        self.desc = !self.desc;
        self.resort_preserving_selection();
        self.status = None;
    }

    /// Re-sort `rows` by the current column/direction while keeping the
    /// same MODEL selected (find its new index after the sort).
    fn resort_preserving_selection(&mut self) {
        let selected_name = self.selected_row().map(|r| r.name);
        sort_rows(&mut self.rows, self.sort, self.desc);
        if let Some(name) = selected_name {
            if let Some(idx) = self.rows.iter().position(|r| r.name == name) {
                self.selected = idx;
            }
        }
    }

    /// Signal the event loop to exit.
    pub fn request_quit(&mut self) {
        self.should_quit = true;
    }

    /// Handle `d`: stage an inline delete confirm for the selected row, unless
    /// it's the active model (refused with a footer warning) or not downloaded
    /// (nothing to delete). Returns `true` when a confirm row was staged.
    pub fn begin_delete(&mut self) -> bool {
        let Some(row) = self.selected_row() else {
            return false;
        };
        if row.current {
            self.status = Some("⚠️  Can't delete the active model — switch away first".to_string());
            return false;
        }
        if !row.downloaded {
            self.status = Some(format!("{} isn't downloaded — nothing to delete", row.name));
            return false;
        }
        self.pending_delete = Some(self.selected);
        self.status = Some(format!(
            "Delete {}'s files? (y/n)",
            self.rows[self.selected].name
        ));
        true
    }

    /// Cancel a staged delete confirm.
    pub fn cancel_delete(&mut self) {
        self.pending_delete = None;
        self.status = Some("Delete cancelled".to_string());
    }
}

/// Whether a delete confirm is currently awaiting a y/n keypress.
impl AppState {
    pub fn awaiting_delete_confirm(&self) -> bool {
        self.pending_delete.is_some()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Raw-mode event loop (TTY-only; excluded from coverage — see docs/coverage.md)
// ─────────────────────────────────────────────────────────────────────────

/// Height of the inline viewport: table borders (2) + header (1) + one line
/// per registry row + the legend line + the status line.
pub fn viewport_height(row_count: usize) -> u16 {
    (row_count as u16).saturating_add(5)
}

/// Launch the interactive model TUI for `vault_flag`'s vault. Resolves the
/// collection + cache dir, builds the row model, enters raw mode, runs the
/// event loop, and always restores the terminal on the way out (even on
/// error). Draws in a small INLINE viewport right below the command — no
/// alternate screen — so the last frame stays visible after quitting.
/// Caller guarantees a real TTY (see `search_model::run_bare`).
pub fn run(vault_flag: Option<PathBuf>) -> Result<()> {
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use ratatui::{TerminalOptions, Viewport};

    let resolved = crate::vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;
    let current = config.search.embed_model.clone();
    let collection = collection_for(&resolved).context("resolve collection")?;
    let cache_dir = collection_cache_dir(&collection);

    // Drop a stale choice whose download was purged before we open, so no row
    // renders as the active model and the user re-selects + downloads.
    reconcile_missing_model(resolved.root.as_path(), &cache_dir, &current);

    let rows = build_rows(&current, &cache_dir);
    let height = viewport_height(rows.len());
    let mut state = AppState::new(rows, cache_dir);

    enable_raw_mode().context("entering raw mode")?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("building terminal")?;

    let loop_result = event_loop(&mut terminal, &mut state, resolved);

    // Always restore the terminal, regardless of how the loop ended. The
    // inline viewport's final frame intentionally stays in the scrollback;
    // just drop the cursor onto a fresh line below it for the shell prompt.
    disable_raw_mode().ok();
    terminal.show_cursor().ok();
    println!();

    loop_result
}

/// The blocking event loop: draw, read a key, mutate state, repeat until quit.
/// Split from [`run`] so the terminal setup/teardown stays a thin wrapper.
fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut AppState,
    resolved: onebrain_core::ResolvedVault,
) -> Result<()> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

    loop {
        terminal.draw(|f| render(f, state))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // While a delete confirm is up, only y/n/Esc are meaningful.
        if state.awaiting_delete_confirm() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    perform_delete(state);
                }
                _ => state.cancel_delete(),
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => state.request_quit(),
            KeyCode::Down | KeyCode::Char('j') => state.move_down(),
            KeyCode::Up | KeyCode::Char('k') => state.move_up(),
            KeyCode::Char('s') => state.cycle_sort(),
            KeyCode::Char('r') => state.toggle_desc(),
            KeyCode::Char('d') => {
                state.begin_delete();
            }
            KeyCode::Enter => perform_switch(terminal, state, &resolved),
            _ => {}
        }

        if state.should_quit {
            return Ok(());
        }
    }
}

/// Messages from the background switch worker back to the UI loop.
enum SwitchMsg {
    /// The model files are on disk (either they already were, or the forced
    /// download just finished); the re-embed phase is starting. Only sent by
    /// the semantic-build worker (a lex-only build refuses the switch outright),
    /// though the UI loop still matches it in both builds.
    #[cfg_attr(not(feature = "semantic"), allow(dead_code))]
    DownloadDone,
    /// Live re-embed progress from `Engine::rebuild_with_progress`:
    /// `(chunks done, total chunks)`. Semantic-build only (see `DownloadDone`).
    #[cfg_attr(not(feature = "semantic"), allow(dead_code))]
    Reembed { done: usize, total: usize },
    /// The whole switch finished (config persisted on success). Boxed: the
    /// envelope is much larger than the other variant.
    Finished(
        Box<anyhow::Result<crate::output::Envelope<crate::commands::search_model::ModelSetData>>>,
    ),
}

/// The three things `Enter` can do to the selected row, decided purely from
/// its `current`/`downloaded` flags (see [`enter_action`]).
#[derive(Debug, PartialEq)]
pub(crate) enum EnterAction {
    /// Active model with files on disk — nothing to do.
    NoOp,
    /// Different model — normal switch (download if needed + rebuild).
    Switch,
    /// Active model whose files are gone (e.g. OS purged the cache, #114):
    /// re-download ONLY. The index was embedded by this same model, so no
    /// rebuild/re-embed is needed.
    RedownloadActive,
}

/// Pure decision function for the `Enter` keybinding: given whether the
/// selected row is the active model and whether its files are on disk,
/// decide what `Enter` should do. See [`EnterAction`] for what each variant
/// means.
pub(crate) fn enter_action(is_current: bool, is_downloaded: bool) -> EnterAction {
    match (is_current, is_downloaded) {
        (true, true) => EnterAction::NoOp,
        (true, false) => EnterAction::RedownloadActive,
        (false, _) => EnterAction::Switch,
    }
}

/// Enter-key handler: switch the active model to the selected row, or (see
/// [`enter_action`]) re-download the active model's missing files, or no-op.
/// The download (forced via [`onebrain_search::embed::new_quiet`], so even an
/// empty index downloads immediately) and the re-embed both run on a worker
/// thread; this loop keeps drawing, polling the model dir's on-disk size into
/// an in-table progress bar with % until the worker reports back. Keys are
/// swallowed while the switch/download runs. A genuine switch is the SHARED
/// [`apply_model_change`] path (open-old → rebuild → persist ordering
/// preserved); re-downloading the ACTIVE model's files skips `apply_model_change`
/// entirely — the config and vector store already reflect this model, so
/// only the download phase runs.
fn perform_switch<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut AppState,
    resolved: &onebrain_core::ResolvedVault,
) {
    use ratatui::crossterm::event;
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;

    let Some(row) = state.selected_row() else {
        return;
    };
    let name = row.name;
    match enter_action(row.current, row.downloaded) {
        EnterAction::NoOp => {
            state.status = Some(format!("Already using {name} ✓"));
            return;
        }
        EnterAction::RedownloadActive => {
            perform_redownload_active(terminal, state, name);
            return;
        }
        EnterAction::Switch => {}
    }
    let row = state.selected_row().expect("checked above");
    let needs_download = !row.downloaded;
    let model_dir = row.path.clone();
    let expected = model_registry()
        .iter()
        .find(|m| m.name == name)
        .map(|m| m.approx_bytes)
        .unwrap_or(0);

    let (tx, rx) = std::sync::mpsc::channel();
    let worker_resolved = resolved.clone();
    let worker_cache = state.cache_dir.clone();
    std::thread::spawn(move || {
        // Lex-only build (no `semantic` feature): there's no embedder to switch
        // to, so refuse the switch cleanly rather than pretend to download.
        #[cfg(not(feature = "semantic"))]
        {
            let _ = (&worker_cache, &worker_resolved, name, needs_download);
            let _ = tx.send(SwitchMsg::Finished(Box::new(Err(anyhow::anyhow!(
                onebrain_search::engine::SEMANTIC_UNAVAILABLE
            )))));
        }
        #[cfg(feature = "semantic")]
        {
            if needs_download {
                // Force the download up front (quiet: no stdout bar — we're in
                // raw mode). With the cache warm, apply's own embedder init is a
                // no-op download-wise.
                if let Err(e) = onebrain_search::embed::new_quiet(name, &worker_cache) {
                    let _ = tx.send(SwitchMsg::Finished(Box::new(Err(e))));
                    return;
                }
            }
            let _ = tx.send(SwitchMsg::DownloadDone);
            let progress_tx = tx.clone();
            let _ = tx.send(SwitchMsg::Finished(Box::new(apply_model_change(
                worker_resolved,
                name,
                &mut |done, total| {
                    let _ = progress_tx.send(SwitchMsg::Reembed { done, total });
                },
            ))));
        }
    });

    if needs_download {
        state.downloading = Some(DownloadUi {
            name,
            expected_bytes: expected,
            model_dir,
            pct: 0,
            bytes: 0,
        });
        state.status = Some(format!("⏬  Downloading {name}…"));
    } else {
        state.status = Some(format!("🧠  Re-embedding with {name}…"));
    }

    // Nested UI loop while the worker runs: poll disk → redraw → drain keys.
    loop {
        if let Some(dl) = state.downloading.as_mut() {
            dl.bytes = dir_size_bytes(&dl.model_dir);
            dl.pct = download_pct(dl.bytes, dl.expected_bytes);
            state.status = Some(format!(
                "⏬  Downloading {name} · {} / {} ({}%)",
                format_size(dl.bytes),
                format_size(dl.expected_bytes),
                dl.pct
            ));
        }
        let _ = terminal.draw(|f| render(f, state));

        // Swallow input while busy (no cancel mid-switch — config only
        // persists after a successful rebuild, so killing the worker would
        // at worst leave a partial cache, but half-handled keys are worse).
        if event::poll(Duration::from_millis(150)).unwrap_or(false) {
            let _ = event::read();
        }

        // Drain everything queued since the last tick (re-embed events can
        // arrive faster than the 150ms redraw cadence).
        loop {
            match rx.try_recv() {
                Ok(SwitchMsg::DownloadDone) => {
                    if state.downloading.take().is_some() {
                        state.status =
                            Some(format!("✅  Downloaded · 🧠  re-embedding with {name}…"));
                    }
                }
                Ok(SwitchMsg::Reembed { done, total }) => {
                    state.downloading = None;
                    state.reembed = Some(ReembedUi { name, done, total });
                    state.status = Some(format!(
                        "🧠  Re-embedding with {name} — {done}/{total} chunk(s) ({}%)",
                        reembed_pct(done, total)
                    ));
                }
                Ok(SwitchMsg::Finished(result)) => {
                    state.downloading = None;
                    state.reembed = None;
                    match *result {
                        Ok(envelope) => {
                            // Re-scan download status and reflect the new
                            // active model in the table.
                            state.refresh_after_switch(name);
                            let chunks = envelope.data.and_then(|d| d.chunks_reembedded);
                            state.status = Some(switch_status(name, chunks));
                        }
                        Err(e) => {
                            state.status = Some(format!("⚠️  Switch failed: {e}"));
                        }
                    }
                    return;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    state.downloading = None;
                    state.reembed = None;
                    state.status = Some("⚠️  Switch worker exited unexpectedly".to_string());
                    return;
                }
            }
        }
    }
}

/// `EnterAction::RedownloadActive` handler: the active model's files are
/// missing from disk (e.g. the OS purged the cache — #114), so re-download
/// them ONLY. Reuses the exact quiet-download call
/// ([`onebrain_search::embed::new_quiet`]) and the same in-table
/// progress-bar polling loop the [`Switch`](EnterAction::Switch) path uses,
/// but deliberately does NOT call [`apply_model_change`] — the vector index
/// was already embedded by this same model (same dims), so there's nothing
/// to rebuild; restoring the files alone restores query capability.
fn perform_redownload_active<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut AppState,
    name: &'static str,
) {
    use ratatui::crossterm::event;
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;

    let model_dir = state.cache_dir.join(
        model_registry()
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.cache_dir_name())
            .unwrap_or_default(),
    );
    let expected = model_registry()
        .iter()
        .find(|m| m.name == name)
        .map(|m| m.approx_bytes)
        .unwrap_or(0);

    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
    let worker_cache = state.cache_dir.clone();
    std::thread::spawn(move || {
        #[cfg(not(feature = "semantic"))]
        {
            let _ = (&worker_cache, name);
            let _ = tx.send(Err(anyhow::anyhow!(
                onebrain_search::engine::SEMANTIC_UNAVAILABLE
            )));
        }
        #[cfg(feature = "semantic")]
        {
            // Same quiet-download call the Switch path uses — no stdout bar,
            // we're in raw mode.
            let result = onebrain_search::embed::new_quiet(name, &worker_cache).map(|_| ());
            let _ = tx.send(result);
        }
    });

    state.downloading = Some(DownloadUi {
        name,
        expected_bytes: expected,
        model_dir,
        pct: 0,
        bytes: 0,
    });
    state.status = Some(format!("⏬  Re-downloading {name}…"));

    loop {
        if let Some(dl) = state.downloading.as_mut() {
            dl.bytes = dir_size_bytes(&dl.model_dir);
            dl.pct = download_pct(dl.bytes, dl.expected_bytes);
            state.status = Some(format!(
                "⏬  Re-downloading {name} · {} / {} ({}%)",
                format_size(dl.bytes),
                format_size(dl.expected_bytes),
                dl.pct
            ));
        }
        let _ = terminal.draw(|f| render(f, state));

        if event::poll(Duration::from_millis(150)).unwrap_or(false) {
            let _ = event::read();
        }

        match rx.try_recv() {
            Ok(Ok(())) => {
                state.downloading = None;
                // Refresh just this row's download status from disk — no
                // rebuild happened, so `current` and every other row is
                // untouched.
                if let (Some(idx), Some(info)) = (
                    state.rows.iter().position(|r| r.name == name),
                    model_registry().iter().find(|m| m.name == name),
                ) {
                    let st = model_download_status(info, &state.cache_dir);
                    if let Some(r) = state.rows.get_mut(idx) {
                        r.downloaded = st.downloaded;
                        r.disk_bytes = st.disk_size;
                    }
                }
                state.status = Some(format!("✅  re-downloaded {name}"));
                return;
            }
            Ok(Err(e)) => {
                state.downloading = None;
                state.status = Some(format!("⚠️  Re-download failed: {e}"));
                return;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                state.downloading = None;
                state.status = Some("⚠️  Download worker exited unexpectedly".to_string());
                return;
            }
        }
    }
}

/// Post-switch status line: says what actually happened and what to do next.
/// A rebuild over an EMPTY index embeds nothing (and downloads nothing —
/// the embedder is lazy), so point the user at `search reindex`; otherwise
/// report how many chunks were re-embedded with the new model.
pub fn switch_status(name: &str, chunks_reembedded: Option<usize>) -> String {
    match chunks_reembedded {
        Some(0) | None => format!(
            "✅  Switched to {name} · index is empty — run `onebrain search reindex` to download + index"
        ),
        Some(n) => format!("✅  Switched to {name} · 🧠  {n} chunk(s) re-embedded with the new model"),
    }
}

/// y-confirm handler for `d`: remove the pending row's `models--*` dir (same
/// `remove_dir_all` the `search model remove` verb uses), then refresh the
/// row's download status in place.
fn perform_delete(state: &mut AppState) {
    let Some(idx) = state.pending_delete.take() else {
        return;
    };
    let Some(row) = state.rows.get(idx) else {
        return;
    };
    let path = row.path.clone();
    let name = row.name;
    let freed = row.disk_bytes.unwrap_or(0);

    match std::fs::remove_dir_all(&path) {
        Ok(()) => {
            if let Some(r) = state.rows.get_mut(idx) {
                r.downloaded = false;
                r.disk_bytes = None;
            }
            state.status = Some(format!("🗑️  removed {name} · freed {}", format_size(freed)));
        }
        Err(e) => {
            state.status = Some(format!("⚠️  couldn't remove {name}: {e}"));
        }
    }
}

/// Render the table + footer into the frame. Pure w.r.t. state (reads only);
/// still terminal-coupled via ratatui types, so it rides with the excluded
/// event-loop file.
fn render(f: &mut ratatui::Frame, state: &AppState) {
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Modifier, Style};
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let (col, desc) = (state.sort, state.desc);
    let header = Row::new([
        Cell::from(""),
        Cell::from(header_label("MODEL", col == ModelSortCol::Name, desc)),
        Cell::from(header_label(
            "DOWNLOADED",
            col == ModelSortCol::Downloaded,
            desc,
        )),
        Cell::from(header_label("DISK", col == ModelSortCol::Disk, desc)),
        Cell::from(header_label("DIM", col == ModelSortCol::Dim, desc)),
        Cell::from(header_label("THAI", col == ModelSortCol::Thai, desc)),
        Cell::from("NOTE"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let dl = state.downloading.as_ref().filter(|d| d.name == r.name);
            let re = state.reembed.as_ref().filter(|d| d.name == r.name);
            let active = r.current && r.downloaded;
            let marker = if active { "●" } else { "" };
            // Plain "—" for not-downloaded: ⬜ renders as a huge white block
            // in some terminal fonts.
            let downloaded = if dl.is_some() {
                "⏬"
            } else if re.is_some() {
                "🧠"
            } else if r.downloaded {
                "✓"
            } else {
                "—"
            };
            let disk = match dl {
                // Live: the dir is filling up — show it growing.
                Some(d) => format_size(d.bytes),
                None => disk_cell(r.downloaded, r.disk_bytes, r.approx_size),
            };
            let thai = r
                .thai
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".to_string());
            // While this row downloads or re-embeds, its NOTE column becomes
            // the progress bar.
            let note = match (dl, re) {
                (Some(d), _) => progress_bar(d.pct, 14),
                (None, Some(e)) => progress_bar(reembed_pct(e.done, e.total), 14),
                (None, None) => r.note.to_string(),
            };
            // The ACTIVE model (●) always stands out: bold + green (reads well
            // on dark terminals and doesn't clash with the REVERSED selection
            // highlight). The selected row keeps REVERSED; when the selection
            // sits on the active row, the modifiers combine.
            let mut style = Style::default();
            if active {
                style = style
                    .fg(ratatui::style::Color::Green)
                    .add_modifier(Modifier::BOLD);
            }
            if i == state.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Row::new([
                Cell::from(marker),
                Cell::from(r.name),
                Cell::from(downloaded),
                Cell::from(disk),
                Cell::from(r.dims.to_string()),
                Cell::from(thai),
                Cell::from(note),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(24),
        Constraint::Length(13),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Min(20),
    ];
    // Full plain border: single thin verticals (│) matching the horizontal
    // line weight. (History: the full-screen-era plain verticals looked
    // dashed in one terminal and QuadrantOutside looked too chunky, so the
    // sides were dropped for a while — the user has since switched terminals
    // and asked for the plain single-line box back.)
    let table = Table::new(body, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Plain)
            .title(" Embedding Model Selection "),
    );
    f.render_widget(table, chunks[0]);

    // The keybinding legend gets its own permanent line so a transient
    // status (switch result, re-embed progress, delete confirm) never
    // hides the shortcuts.
    let no_active_model = !state.rows.iter().any(|r| r.current && r.downloaded);
    let legend =
        ratatui::widgets::Paragraph::new(footer_text(state.sort, state.desc, no_active_model))
            .style(Style::default().add_modifier(Modifier::DIM));
    f.render_widget(legend, chunks[1]);
    let status = ratatui::widgets::Paragraph::new(state.status.clone().unwrap_or_default());
    f.render_widget(status, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn rows_for(current: &str) -> Vec<TuiRow> {
        build_rows(current, Path::new("/nonexistent-cache"))
    }

    #[test]
    fn enter_on_active_downloaded_is_noop() {
        assert!(matches!(enter_action(true, true), EnterAction::NoOp));
    }

    #[test]
    fn enter_on_active_missing_files_redownloads() {
        assert!(matches!(
            enter_action(true, false),
            EnterAction::RedownloadActive
        ));
    }

    #[test]
    fn enter_on_other_model_switches() {
        assert!(matches!(enter_action(false, true), EnterAction::Switch));
        assert!(matches!(enter_action(false, false), EnterAction::Switch));
    }

    fn state_for(current: &str) -> AppState {
        AppState::new(rows_for(current), PathBuf::from("/nonexistent-cache"))
    }

    #[test]
    fn build_rows_flags_current_and_undownloaded() {
        let rows = rows_for("bge-m3");
        assert_eq!(rows.len(), model_registry().len());
        let bge = rows.iter().find(|r| r.name == "bge-m3").unwrap();
        assert!(bge.current, "bge-m3 should be current");
        assert!(!bge.downloaded, "empty cache → not downloaded");
        assert_eq!(bge.disk_bytes, None);
        // Non-current rows carry no marker.
        assert!(rows.iter().filter(|r| r.current).count() == 1);
        // Path is the models--* dir even when absent.
        assert!(bge.path.ends_with("models--BAAI--bge-m3"));
    }

    #[test]
    fn build_rows_reads_disk_size_when_present() {
        let cache = tempfile::tempdir().unwrap();
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-small")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 5 * 1024 * 1024]).unwrap();

        let rows = build_rows("bge-m3", cache.path());
        let e5 = rows
            .iter()
            .find(|r| r.name == "multilingual-e5-small")
            .unwrap();
        assert!(e5.downloaded);
        assert_eq!(e5.disk_bytes, Some(5 * 1024 * 1024));
    }

    #[test]
    fn next_sort_cycles_in_on_screen_column_order() {
        assert_eq!(next_sort(ModelSortCol::Name), ModelSortCol::Downloaded);
        assert_eq!(next_sort(ModelSortCol::Downloaded), ModelSortCol::Disk);
        assert_eq!(next_sort(ModelSortCol::Disk), ModelSortCol::Dim);
        assert_eq!(next_sort(ModelSortCol::Dim), ModelSortCol::Thai);
        assert_eq!(next_sort(ModelSortCol::Thai), ModelSortCol::Name);
        // Size (not in the cycle) folds back to the start.
        assert_eq!(next_sort(ModelSortCol::Size), ModelSortCol::Name);
    }

    #[test]
    fn sort_rows_downloaded_puts_downloaded_first_ties_by_name() {
        let cache = tempfile::tempdir().unwrap();
        // Download exactly one model.
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-base")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 64]).unwrap();

        let mut rows = build_rows("bge-m3", cache.path());
        sort_rows(&mut rows, ModelSortCol::Downloaded, false);
        assert_eq!(rows[0].name, "multilingual-e5-base", "✓ first ascending");
        // The undownloaded tail is name-ordered (tie-break).
        let tail: Vec<&str> = rows[1..].iter().map(|r| r.name).collect();
        let mut sorted_tail = tail.clone();
        sorted_tail.sort_unstable();
        assert_eq!(tail, sorted_tail);

        sort_rows(&mut rows, ModelSortCol::Downloaded, true);
        assert_eq!(
            rows.last().unwrap().name,
            "multilingual-e5-base",
            "descending flips ✓ to the bottom"
        );
    }

    #[test]
    fn header_label_marks_only_active_column() {
        assert_eq!(header_label("MODEL", true, false), "MODEL ↑");
        assert_eq!(header_label("MODEL", true, true), "MODEL ↓");
        assert_eq!(header_label("DISK", false, true), "DISK");
    }

    #[test]
    fn app_new_default_sorts_by_name_ascending() {
        let st = state_for("bge-m3");
        let names: Vec<&str> = st.rows.iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "initial rows are name-sorted");
        assert_eq!(st.sort, ModelSortCol::Name);
        assert!(!st.desc);
        // Selection still lands on the current model after the initial sort.
        assert_eq!(st.selected_row().unwrap().name, "bge-m3");
    }

    #[test]
    fn sort_rows_by_dim_ascending_and_descending() {
        let mut rows = rows_for("bge-m3");
        sort_rows(&mut rows, ModelSortCol::Dim, false);
        let dims: Vec<usize> = rows.iter().map(|r| r.dims).collect();
        let mut sorted = dims.clone();
        sorted.sort_unstable();
        assert_eq!(dims, sorted);

        sort_rows(&mut rows, ModelSortCol::Dim, true);
        let dims_desc: Vec<usize> = rows.iter().map(|r| r.dims).collect();
        sorted.reverse();
        assert_eq!(dims_desc, sorted);
    }

    #[test]
    fn sort_rows_by_name_ascending() {
        let mut rows = rows_for("bge-m3");
        sort_rows(&mut rows, ModelSortCol::Name, false);
        let names: Vec<&str> = rows.iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn sort_rows_thai_keeps_none_last_both_directions() {
        for desc in [false, true] {
            let mut rows = rows_for("bge-m3");
            sort_rows(&mut rows, ModelSortCol::Thai, desc);
            // Both None-thai gemma variants sort after every scored model, in
            // both directions (stable sort keeps their registry order).
            let last_two: Vec<&str> = rows.iter().rev().take(2).map(|r| r.name).collect();
            assert_eq!(
                last_two,
                vec!["embeddinggemma-300m-q4", "embeddinggemma-300m-q"],
                "None-thai models sort last (desc={desc})"
            );
        }
    }

    #[test]
    fn sort_rows_disk_uses_displayed_size_for_not_downloaded() {
        // Nothing downloaded → DISK shows approx sizes and sorts by them.
        let mut rows = rows_for("bge-m3");
        sort_rows(&mut rows, ModelSortCol::Disk, false);
        assert_eq!(
            rows.first().unwrap().name,
            "embeddinggemma-300m-q4",
            "~200 MB first ascending"
        );
        assert_eq!(rows.last().unwrap().name, "bge-m3", "~2.2 GB last asc");
        sort_rows(&mut rows, ModelSortCol::Disk, true);
        assert_eq!(rows.first().unwrap().name, "bge-m3", "r flips to desc");
    }

    #[test]
    fn sort_rows_disk_mixes_real_and_approx_sizes() {
        let cache = tempfile::tempdir().unwrap();
        // Give e5-small a REAL on-disk size bigger than gemma-q4's approx but
        // smaller than everything else. (NB: writing into e5-small's own
        // cache dir — the gemma pair share one dir, so faking THEIR size
        // would flip both.)
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-small")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 250 * 1024 * 1024]).unwrap();

        let mut rows = build_rows("bge-m3", cache.path());
        sort_rows(&mut rows, ModelSortCol::Disk, false);
        let names: Vec<&str> = rows.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            vec![
                "embeddinggemma-300m-q4", // ~200 MB approx
                "multilingual-e5-small",  // 250 MB real
                "embeddinggemma-300m-q",  // ~310 MB approx
                "multilingual-e5-base",   // ~1.1 GB approx
                "multilingual-e5-large",  // ~2.1 GB approx
                "bge-m3",                 // ~2.2 GB approx
            ]
        );
    }

    #[test]
    fn sort_rows_size_column_is_stable_by_name() {
        // Size isn't a TUI sort column; the comparator falls back to name so
        // it stays total (no panic, deterministic order).
        let mut rows = rows_for("bge-m3");
        sort_rows(&mut rows, ModelSortCol::Size, false);
        let names: Vec<&str> = rows.iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn disk_cell_shows_approx_size_until_downloaded() {
        assert_eq!(disk_cell(false, None, "~470 MB"), "~470 MB");
        assert_eq!(disk_cell(true, Some(1024 * 1024), "~470 MB"), "1 MB");
        // Downloaded but size unreadable → still fall back to approx.
        assert_eq!(disk_cell(true, None, "~470 MB"), "~470 MB");
    }

    #[test]
    fn reembed_pct_exact_counts() {
        assert_eq!(reembed_pct(0, 200), 0);
        assert_eq!(reembed_pct(64, 200), 32);
        assert_eq!(reembed_pct(200, 200), 100);
        assert_eq!(reembed_pct(0, 0), 100, "empty re-embed is already done");
    }

    #[test]
    fn download_pct_caps_at_99_and_handles_zero_expected() {
        assert_eq!(download_pct(0, 1000), 0);
        assert_eq!(download_pct(420, 1000), 42);
        assert_eq!(download_pct(1000, 1000), 99, "capped until actually done");
        assert_eq!(download_pct(5000, 1000), 99, "overshoot stays capped");
        assert_eq!(download_pct(123, 0), 0, "zero expected → 0, no div-by-zero");
        // No overflow on huge byte counts (saturating mul).
        assert_eq!(download_pct(u64::MAX, u64::MAX), 99);
    }

    #[test]
    fn progress_bar_fills_by_pct_and_shows_percent() {
        let b = progress_bar(0, 10);
        assert!(b.starts_with("░░░░░░░░░░"), "{b}");
        assert!(b.ends_with("  0%"), "{b}");
        let b = progress_bar(50, 10);
        assert!(b.starts_with("▓▓▓▓▓░░░░░"), "{b}");
        assert!(b.contains("50%"), "{b}");
        let b = progress_bar(99, 10);
        assert!(b.starts_with("▓▓▓▓▓▓▓▓▓░"), "{b}");
        assert!(b.contains("99%"), "{b}");
    }

    #[test]
    fn registry_has_positive_approx_bytes_for_every_model() {
        for m in model_registry() {
            assert!(m.approx_bytes > 0, "{} needs approx_bytes", m.name);
        }
    }

    #[test]
    fn switch_status_points_at_reindex_when_index_empty() {
        for chunks in [Some(0), None] {
            let s = switch_status("bge-m3", chunks);
            assert!(s.contains("Switched to bge-m3"), "{s}");
            assert!(s.contains("search reindex"), "{s}");
        }
    }

    #[test]
    fn switch_status_reports_reembedded_chunks() {
        let s = switch_status("multilingual-e5-base", Some(42));
        assert!(s.contains("Switched to multilingual-e5-base"), "{s}");
        assert!(s.contains("42 chunk(s) re-embedded"), "{s}");
        assert!(!s.contains("search reindex"), "{s}");
    }

    #[test]
    fn refresh_after_switch_updates_current_and_download_status() {
        let cache = tempfile::tempdir().unwrap();
        // Start with an empty cache: nothing downloaded, bge-m3 active.
        let mut st = AppState::new(
            build_rows("bge-m3", cache.path()),
            cache.path().to_path_buf(),
        );
        st.selected = st
            .rows
            .iter()
            .position(|r| r.name == "multilingual-e5-base")
            .unwrap();

        // Simulate the switch having downloaded the new model on disk.
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-base")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 4096]).unwrap();

        st.refresh_after_switch("multilingual-e5-base");

        let row = st.selected_row().unwrap();
        assert_eq!(row.name, "multilingual-e5-base", "selection follows model");
        assert!(row.current, "switched model is marked current");
        assert!(row.downloaded, "download status re-scanned from disk");
        assert_eq!(row.disk_bytes, Some(4096));
        assert_eq!(
            st.rows.iter().filter(|r| r.current).count(),
            1,
            "exactly one current model after refresh"
        );
    }

    #[test]
    fn viewport_height_is_rows_plus_chrome() {
        // 2 table borders + 1 header + N rows + legend + status lines.
        assert_eq!(viewport_height(5), 10);
        assert_eq!(viewport_height(0), 5);
        assert_eq!(viewport_height(usize::MAX), u16::MAX);
    }

    #[test]
    fn footer_text_shows_legend_and_sort() {
        let f = footer_text(ModelSortCol::Disk, true, false);
        assert!(f.contains("s sort"));
        assert!(f.contains("enter switch"));
        assert!(f.contains("d delete"));
        assert!(f.contains("q quit"));
        assert!(f.contains("sort: disk ↓"), "{f}");
        assert!(!f.contains("No model downloaded"), "{f}");
        let up = footer_text(ModelSortCol::Name, false, false);
        assert!(up.contains("sort: name ↑"), "{up}");
    }

    #[test]
    fn footer_text_warns_when_no_active_model() {
        let f = footer_text(ModelSortCol::Name, false, true);
        assert!(f.contains("No model downloaded"), "{f}");
        assert!(f.contains("press enter to download"), "{f}");
        assert!(f.contains("q quit"), "{f}"); // legend still present
    }

    #[test]
    fn sort_label_covers_every_column() {
        assert_eq!(sort_label(ModelSortCol::Name), "name");
        assert_eq!(sort_label(ModelSortCol::Size), "size");
        assert_eq!(sort_label(ModelSortCol::Dim), "dim");
        assert_eq!(sort_label(ModelSortCol::Thai), "thai");
        assert_eq!(sort_label(ModelSortCol::Disk), "disk");
    }

    #[test]
    fn app_new_selects_current_model() {
        let st = state_for("bge-m3");
        assert_eq!(st.selected_row().unwrap().name, "bge-m3");
        assert!(!st.should_quit);
        assert!(!st.awaiting_delete_confirm());
    }

    #[test]
    fn app_new_defaults_to_first_when_no_current() {
        // A current name not in the registry → first row selected.
        let st = state_for("not-a-real-model");
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn move_down_and_up_saturate() {
        let mut st = state_for("multilingual-e5-small");
        st.selected = 0;
        st.move_up(); // already at top
        assert_eq!(st.selected, 0);
        let last = st.rows.len() - 1;
        for _ in 0..st.rows.len() + 5 {
            st.move_down();
        }
        assert_eq!(st.selected, last, "saturates at last row");
        st.move_up();
        assert_eq!(st.selected, last - 1);
    }

    #[test]
    fn cycle_sort_keeps_selected_model() {
        let mut st = state_for("bge-m3");
        let before = st.selected_row().unwrap().name;
        st.cycle_sort();
        assert_eq!(st.sort, ModelSortCol::Downloaded);
        assert_eq!(
            st.selected_row().unwrap().name,
            before,
            "selection follows the model across a re-sort"
        );
    }

    #[test]
    fn toggle_desc_flips_and_keeps_selection() {
        let mut st = state_for("bge-m3");
        let before = st.selected_row().unwrap().name;
        assert!(!st.desc);
        st.toggle_desc();
        assert!(st.desc);
        assert_eq!(st.selected_row().unwrap().name, before);
        st.toggle_desc();
        assert!(!st.desc);
    }

    #[test]
    fn request_quit_sets_flag() {
        let mut st = state_for("bge-m3");
        st.request_quit();
        assert!(st.should_quit);
    }

    #[test]
    fn begin_delete_refuses_active_model() {
        let mut st = state_for("bge-m3");
        // Selection starts on the active model.
        assert!(!st.begin_delete());
        assert!(!st.awaiting_delete_confirm());
        assert!(st.status.as_ref().unwrap().contains("active model"));
    }

    #[test]
    fn begin_delete_refuses_not_downloaded() {
        let mut st = state_for("bge-m3");
        // Move to a non-active, not-downloaded row.
        let idx = st
            .rows
            .iter()
            .position(|r| !r.current && !r.downloaded)
            .unwrap();
        st.selected = idx;
        assert!(!st.begin_delete());
        assert!(!st.awaiting_delete_confirm());
        assert!(st.status.as_ref().unwrap().contains("nothing to delete"));
    }

    #[test]
    fn begin_delete_stages_confirm_for_downloaded_inactive() {
        let cache = tempfile::tempdir().unwrap();
        // Download a NON-active model on disk.
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-small")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 1024]).unwrap();

        let mut st = AppState::new(
            build_rows("bge-m3", cache.path()),
            cache.path().to_path_buf(),
        );
        let idx = st
            .rows
            .iter()
            .position(|r| r.name == "multilingual-e5-small")
            .unwrap();
        st.selected = idx;
        assert!(st.begin_delete());
        assert!(st.awaiting_delete_confirm());
        assert!(st.status.as_ref().unwrap().contains("(y/n)"));
    }

    #[test]
    fn cancel_delete_clears_pending() {
        let mut st = state_for("bge-m3");
        st.pending_delete = Some(0);
        st.cancel_delete();
        assert!(!st.awaiting_delete_confirm());
        assert!(st.status.as_ref().unwrap().contains("cancelled"));
    }

    #[test]
    fn perform_delete_removes_dir_and_refreshes_row() {
        let cache = tempfile::tempdir().unwrap();
        let info = model_registry()
            .iter()
            .find(|m| m.name == "multilingual-e5-small")
            .unwrap();
        let mdir = cache.path().join(info.cache_dir_name());
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(mdir.join("model.onnx"), vec![0u8; 2048]).unwrap();

        let mut st = AppState::new(
            build_rows("bge-m3", cache.path()),
            cache.path().to_path_buf(),
        );
        let idx = st
            .rows
            .iter()
            .position(|r| r.name == "multilingual-e5-small")
            .unwrap();
        st.selected = idx;
        assert!(st.begin_delete());
        perform_delete(&mut st);

        assert!(!mdir.exists(), "model dir should be removed");
        let row = &st.rows[idx];
        assert!(!row.downloaded);
        assert_eq!(row.disk_bytes, None);
        assert!(st.status.as_ref().unwrap().contains("removed"));
        assert!(!st.awaiting_delete_confirm());
    }

    #[test]
    fn perform_delete_reports_error_when_dir_missing() {
        // pending_delete points at a downloaded-flagged row whose dir doesn't
        // actually exist → remove_dir_all errors, surfaced in the status line.
        let mut st = state_for("bge-m3");
        let idx = st.rows.iter().position(|r| !r.current).unwrap();
        st.rows[idx].downloaded = true; // pretend downloaded
        st.pending_delete = Some(idx);
        perform_delete(&mut st);
        assert!(st.status.as_ref().unwrap().contains("couldn't remove"));
    }

    #[test]
    fn perform_delete_noop_when_nothing_pending() {
        let mut st = state_for("bge-m3");
        st.pending_delete = None;
        perform_delete(&mut st);
        // No panic, status untouched.
        assert!(st.status.is_none());
    }
}
