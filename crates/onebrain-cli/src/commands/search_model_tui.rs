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
//! - `s` — cycle the sort column (name → disk → dim → thai → back to name)
//! - `r` — reverse the sort direction
//! - `Enter` — switch the active model to the selected row (reusing the SHARED
//!   [`search_model::apply_model_change`] path — persist AFTER a successful
//!   rebuild, never before); no-op with a footer hint if already active
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
use crate::commands::search_common::{collection_cache_dir, collection_for};
use crate::commands::search_model::{apply_model_change, cmp_option_last, format_size};
use onebrain_core::load_vault_config;
use onebrain_search::embed::{model_download_status, model_registry};

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
                dims: m.dims,
                thai: m.thai_miracl,
                note: m.note,
                path: status.path,
            }
        })
        .collect()
}

/// The next sort column in the `s`-key cycle: name → disk → dim → thai → name.
pub fn next_sort(col: ModelSortCol) -> ModelSortCol {
    match col {
        ModelSortCol::Name => ModelSortCol::Disk,
        ModelSortCol::Disk => ModelSortCol::Dim,
        ModelSortCol::Dim => ModelSortCol::Thai,
        ModelSortCol::Thai => ModelSortCol::Name,
        // `Size` isn't part of the TUI cycle (no approx-size column shown); if
        // it ever leaks in, fold it back to the start of the cycle.
        ModelSortCol::Size => ModelSortCol::Name,
    }
}

/// Sort `rows` by `col`/`desc`. `disk` and `thai` keep missing values last in
/// BOTH directions (via the shared [`cmp_option_last`]); `name`/`dim` are a
/// plain total order reversed for `desc`.
pub fn sort_rows(rows: &mut [TuiRow], col: ModelSortCol, desc: bool) {
    rows.sort_by(|a, b| {
        let ord = match col {
            ModelSortCol::Name => a.name.cmp(b.name),
            ModelSortCol::Dim => a.dims.cmp(&b.dims),
            ModelSortCol::Thai => cmp_option_last(a.thai, b.thai, desc),
            ModelSortCol::Disk => cmp_option_last(a.disk_bytes, b.disk_bytes, desc),
            // Not a TUI sort column — treat as name so the comparator is total.
            ModelSortCol::Size => a.name.cmp(b.name),
        };
        match col {
            // Direction already folded into cmp_option_last for these.
            ModelSortCol::Thai | ModelSortCol::Disk => ord,
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
    }
}

/// The footer legend + current sort indicator shown under the table.
pub fn footer_text(col: ModelSortCol, desc: bool) -> String {
    let dir = if desc { "↓" } else { "↑" };
    format!(
        "↑/↓ move · s sort · r reverse · enter switch · d delete · q quit    sort: {} {dir}",
        sort_label(col)
    )
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
}

impl AppState {
    /// Build initial state from rows, selecting the CURRENT model if present
    /// (else the first row). Starts unsorted (registry order), ascending.
    pub fn new(rows: Vec<TuiRow>) -> Self {
        let selected = rows.iter().position(|r| r.current).unwrap_or(0);
        Self {
            rows,
            selected,
            sort: ModelSortCol::Name,
            desc: false,
            status: None,
            pending_delete: None,
            should_quit: false,
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
            self.status = Some("⚠️  can't delete the active model — switch away first".to_string());
            return false;
        }
        if !row.downloaded {
            self.status = Some(format!("{} isn't downloaded — nothing to delete", row.name));
            return false;
        }
        self.pending_delete = Some(self.selected);
        self.status = Some(format!(
            "delete {}'s files? (y/n)",
            self.rows[self.selected].name
        ));
        true
    }

    /// Cancel a staged delete confirm.
    pub fn cancel_delete(&mut self) {
        self.pending_delete = None;
        self.status = Some("delete cancelled".to_string());
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
/// per registry row + the single footer line.
pub fn viewport_height(row_count: usize) -> u16 {
    (row_count as u16).saturating_add(4)
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

    let rows = build_rows(&current, &cache_dir);
    let height = viewport_height(rows.len());
    let mut state = AppState::new(rows);

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

/// Enter-key handler: switch the active model to the selected row via the
/// SHARED `apply_model_change` path (open-old → rebuild → persist ordering
/// preserved), showing a "switching…" status while the rebuild runs. No-op
/// with a footer hint if the row is already active.
fn perform_switch<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut AppState,
    resolved: &onebrain_core::ResolvedVault,
) {
    let Some(row) = state.selected_row() else {
        return;
    };
    if row.current {
        state.status = Some(format!("already using {}", row.name));
        return;
    }
    let name = row.name;

    // Surface a rebuild-in-progress line before the (potentially long,
    // download+re-embed) apply call blocks the loop.
    state.status = Some(format!("⏳ switching to {name} — re-embedding…"));
    let _ = terminal.draw(|f| render(f, state));

    match apply_model_change(resolved.clone(), name) {
        Ok(_) => {
            // Reflect the new active model in the table + selection.
            for r in state.rows.iter_mut() {
                r.current = r.name == name;
            }
            state.status = Some(format!("✅ switched to {name}"));
        }
        Err(e) => {
            state.status = Some(format!("⚠️  switch failed: {e}"));
        }
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
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    let header = Row::new([
        Cell::from(""),
        Cell::from("MODEL"),
        Cell::from("DOWNLOADED"),
        Cell::from("DISK"),
        Cell::from("DIM"),
        Cell::from("THAI"),
        Cell::from("NOTE"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = if r.current { "●" } else { "" };
            let downloaded = if r.downloaded { "✓" } else { "⬜" };
            let disk = r
                .disk_bytes
                .map(format_size)
                .unwrap_or_else(|| "—".to_string());
            let thai = r
                .thai
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".to_string());
            let style = if i == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new([
                Cell::from(marker),
                Cell::from(r.name),
                Cell::from(downloaded),
                Cell::from(disk),
                Cell::from(r.dims.to_string()),
                Cell::from(thai),
                Cell::from(r.note),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(24),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Min(20),
    ];
    let table = Table::new(body, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" embedding models "),
    );
    f.render_widget(table, chunks[0]);

    let footer_line = state
        .status
        .clone()
        .unwrap_or_else(|| footer_text(state.sort, state.desc));
    let footer = ratatui::widgets::Paragraph::new(footer_line)
        .style(Style::default().add_modifier(Modifier::DIM));
    f.render_widget(footer, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn rows_for(current: &str) -> Vec<TuiRow> {
        build_rows(current, Path::new("/nonexistent-cache"))
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
    fn next_sort_cycles_name_disk_dim_thai_back() {
        assert_eq!(next_sort(ModelSortCol::Name), ModelSortCol::Disk);
        assert_eq!(next_sort(ModelSortCol::Disk), ModelSortCol::Dim);
        assert_eq!(next_sort(ModelSortCol::Dim), ModelSortCol::Thai);
        assert_eq!(next_sort(ModelSortCol::Thai), ModelSortCol::Name);
        // Size (not in the cycle) folds back to the start.
        assert_eq!(next_sort(ModelSortCol::Size), ModelSortCol::Name);
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
            assert_eq!(
                rows.last().unwrap().name,
                "embeddinggemma-300m-q",
                "None-thai model sorts last (desc={desc})"
            );
        }
    }

    #[test]
    fn sort_rows_disk_keeps_not_downloaded_last() {
        let mut rows = rows_for("bge-m3");
        sort_rows(&mut rows, ModelSortCol::Disk, true);
        assert!(rows.iter().all(|r| r.disk_bytes.is_none()));
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
    fn viewport_height_is_rows_plus_chrome() {
        // 2 table borders + 1 header + N rows + 1 footer line.
        assert_eq!(viewport_height(5), 9);
        assert_eq!(viewport_height(0), 4);
        assert_eq!(viewport_height(usize::MAX), u16::MAX);
    }

    #[test]
    fn footer_text_shows_legend_and_sort() {
        let f = footer_text(ModelSortCol::Disk, true);
        assert!(f.contains("s sort"));
        assert!(f.contains("enter switch"));
        assert!(f.contains("d delete"));
        assert!(f.contains("q quit"));
        assert!(f.contains("sort: disk ↓"), "{f}");
        let up = footer_text(ModelSortCol::Name, false);
        assert!(up.contains("sort: name ↑"), "{up}");
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
        let st = AppState::new(rows_for("bge-m3"));
        assert_eq!(st.selected_row().unwrap().name, "bge-m3");
        assert!(!st.should_quit);
        assert!(!st.awaiting_delete_confirm());
    }

    #[test]
    fn app_new_defaults_to_first_when_no_current() {
        // A current name not in the registry → first row selected.
        let st = AppState::new(rows_for("not-a-real-model"));
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn move_down_and_up_saturate() {
        let mut st = AppState::new(rows_for("multilingual-e5-small"));
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
        let mut st = AppState::new(rows_for("bge-m3"));
        let before = st.selected_row().unwrap().name;
        st.cycle_sort();
        assert_eq!(st.sort, ModelSortCol::Disk);
        assert_eq!(
            st.selected_row().unwrap().name,
            before,
            "selection follows the model across a re-sort"
        );
    }

    #[test]
    fn toggle_desc_flips_and_keeps_selection() {
        let mut st = AppState::new(rows_for("bge-m3"));
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
        let mut st = AppState::new(rows_for("bge-m3"));
        st.request_quit();
        assert!(st.should_quit);
    }

    #[test]
    fn begin_delete_refuses_active_model() {
        let mut st = AppState::new(rows_for("bge-m3"));
        // Selection starts on the active model.
        assert!(!st.begin_delete());
        assert!(!st.awaiting_delete_confirm());
        assert!(st.status.as_ref().unwrap().contains("active model"));
    }

    #[test]
    fn begin_delete_refuses_not_downloaded() {
        let mut st = AppState::new(rows_for("bge-m3"));
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

        let mut st = AppState::new(build_rows("bge-m3", cache.path()));
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
        let mut st = AppState::new(rows_for("bge-m3"));
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

        let mut st = AppState::new(build_rows("bge-m3", cache.path()));
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
        let mut st = AppState::new(rows_for("bge-m3"));
        let idx = st.rows.iter().position(|r| !r.current).unwrap();
        st.rows[idx].downloaded = true; // pretend downloaded
        st.pending_delete = Some(idx);
        perform_delete(&mut st);
        assert!(st.status.as_ref().unwrap().contains("couldn't remove"));
    }

    #[test]
    fn perform_delete_noop_when_nothing_pending() {
        let mut st = AppState::new(rows_for("bge-m3"));
        st.pending_delete = None;
        perform_delete(&mut st);
        // No panic, status untouched.
        assert!(st.status.is_none());
    }
}
