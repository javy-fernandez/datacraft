//! Results panel: data grid (virtualized TableBuilder) with inline editing,
//! output log and CSV export.

use crate::db::{Cell, ColumnInfo, ResultSet};
use crate::i18n::{tr, trf};
use crate::ui::editing::{EditState, EditTarget, RowRef};
use crate::ui::icons::{self, Ico};
use crate::ui::theme::{self, pal};
use egui::text::{CCursor, CCursorRange};
use egui::{Color32, FontId, Key, Modifiers, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Local filter of a column (header funnel): the values checked in the list (`None` = all)
/// and the text typed in the search box (case-insensitive substring).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnFilter {
    pub allowed: Option<HashSet<Cell>>,
    pub text: String,
}

impl ColumnFilter {
    fn matches(&self, v: &Cell) -> bool {
        self.allowed.as_ref().is_none_or(|a| a.contains(v)) && (self.text.is_empty() || matches_text(v, &self.text.to_lowercase()))
    }

    fn is_empty(&self) -> bool {
        self.allowed.is_none() && self.text.is_empty()
    }
}

/// Text shown for a value in the funnel list (also what the search box matches).
fn value_label(v: &Cell) -> String {
    match v {
        None => "<null>".to_string(),
        Some(s) if s.is_empty() => tr("<empty>").to_string(),
        Some(s) => s.chars().take(80).collect(),
    }
}

/// `needle` must already be lowercase. Matches the full value, not just the shown label.
fn matches_text(v: &Cell, needle: &str) -> bool {
    match v {
        Some(s) if !s.is_empty() => s.to_lowercase().contains(needle),
        _ => value_label(v).to_lowercase().contains(needle),
    }
}

/// Funnel popup open on a column.
struct FilterPopup {
    col: usize,
    anchor: egui::Pos2,
    search: String,
    /// Distinct values of the column with their occurrence count (sorted).
    values: Vec<(Cell, usize)>,
    /// The click that opened it must not close it in the same frame.
    just_opened: bool,
}

/// Distinct values of `col` with their count, in grid order (numbers as numbers).
fn distinct_values(rows: &[Vec<Cell>], col: usize) -> Vec<(Cell, usize)> {
    let mut counts: HashMap<&Cell, usize> = HashMap::new();
    for r in rows {
        if let Some(v) = r.get(col) {
            *counts.entry(v).or_default() += 1;
        }
    }
    let mut out: Vec<(Cell, usize)> = counts.into_iter().map(|(v, n)| (v.clone(), n)).collect();
    out.sort_by(|a, b| compare_cells(Some(&a.0), Some(&b.0)));
    out
}

/// Visible original rows: those passing every filter, in the requested order.
fn compute_order(rows: &[Vec<Cell>], sort: Option<(usize, SortDir)>, filters: &BTreeMap<usize, ColumnFilter>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len())
        .filter(|&r| filters.iter().all(|(c, f)| rows[r].get(*c).is_some_and(|v| f.matches(v))))
        .collect();
    if let Some((c, dir)) = sort {
        order.sort_by(|&a, &b| {
            let ord = compare_cells(rows[a].get(c), rows[b].get(c));
            if dir == SortDir::Desc { ord.reverse() } else { ord }
        });
    }
    order
}

/// Inline editor open on a cell.
struct InlineEdit {
    row: RowRef,
    col: usize,
    buffer: String,
    /// Request focus (and select all) on the next frame.
    focus: bool,
    select_all: bool,
    changed: bool,
}

type FindKey = (String, usize, Option<(usize, SortDir)>, usize);

/// Edit state of an editable result.
pub struct EditSession {
    pub target: EditTarget,
    pub state: EditState,
    editing: Option<InlineEdit>,
}

/// A result set with view state (order, selection and editing).
struct CellEditor {
    row: RowRef,
    col: usize,
    targets: Vec<(RowRef, usize)>,
    buffer: String,
    null: bool,
    changed: bool,
    hex: bool,
}

pub struct ResultView {
    cell_editor: Option<CellEditor>,
    pub rs: ResultSet,
    pub title: String,
    /// Row permutation for the current order.
    order: Vec<usize>,
    sort: Option<(usize, SortDir)>,
    /// Local per-column filters (header funnel).
    filters: BTreeMap<usize, ColumnFilter>,
    filter_popup: Option<FilterPopup>,
    /// Selected cell (visual row, column).
    selected: Option<(usize, usize)>,
    selected_cells: BTreeSet<(usize, usize)>,
    cell_anchor: Option<(usize, usize)>,
    drag_anchor: Option<(usize, usize, bool)>,
    /// Rows belonging to the pending edit batch, independent of cursor selection.
    edit_rows: BTreeSet<RowRef>,
    /// Selected visual rows (to delete several).
    selected_rows: BTreeSet<usize>,
    anchor: Option<usize>,
    scroll_to: Option<usize>,
    find_open: bool,
    find_query: String,
    find_focus: bool,
    /// Match count shown in the find bar, with what it was counted for: counting scans every
    /// loaded cell, so it is redone when the query or the view changes, and at most once a second.
    find_count: Option<(FindKey, std::time::Instant, usize)>,
    /// Salt for the table id (changes on every run to reset widths).
    salt: u64,
    /// Some if the result is editable.
    pub edit: Option<EditSession>,
    /// Visual row shown in the record view (the row as a vertical form).
    record_view: Option<usize>,
    /// Why it is read-only (tooltip).
    pub readonly_reason: Option<String>,
    /// true once the app has decided whether it is editable.
    pub edit_checked: bool,
}

impl ResultView {
    pub fn selected_cell(&self) -> Option<(usize, usize, Cell)> {
        let (r, c) = self.selected?;
        Some((r, c, self.display_value(self.row_ref(r)?, c).flatten()))
    }

    pub fn row_position(&self) -> Option<(usize, usize)> { self.selected.map(|(r, _)| (r + 1, self.n_view_rows())) }

    pub fn selected_record(&self) -> Option<(usize, Vec<(&str, Cell)>)> {
        let (r, _) = self.selected?;
        let row = self.row_ref(r)?;
        Some((r + 1, self.rs.columns.iter().enumerate().map(|(c, name)| (name.as_str(), self.display_value(row, c).flatten())).collect()))
    }

    fn commit_cell_editor(&mut self) {
        if let Some(editor) = self.cell_editor.take().filter(|e| e.changed) {
            let value = if editor.null { None } else { Some(editor.buffer) };
            self.apply_cell_values(editor.targets.into_iter().map(|(r, c)| (r, c, value.clone())).collect());
        }
    }

    pub fn show_cell_editor(&mut self, ui: &mut Ui) {
        let Some((r, col, value)) = self.selected_cell() else { ui.label(tr("Select a cell to edit its value")); return; };
        let Some(row) = self.row_ref(r) else { return; };
        if self.cell_editor.as_ref().is_none_or(|e| e.row != row || e.col != col) {
            self.commit_cell_editor();
            self.cell_editor = Some(CellEditor { row, col, targets: self.selected_cell_refs(), buffer: value.clone().unwrap_or_default(), null: value.is_none(), changed: false, hex: false });
        }
        let targets = self.selected_cell_refs();
        if let Some(editor) = &mut self.cell_editor && !editor.changed { editor.targets = targets; }
        let editable = self.edit.as_ref().is_some_and(|e| e.state.cell_editable(&self.rs, &e.target, row, col));
        let mut apply = false; let mut reset = false;
        if let Some(editor) = self.cell_editor.as_mut() {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut editor.hex, false, tr("Text")); ui.selectable_value(&mut editor.hex, true, tr("Hex"));
                ui.label(egui::RichText::new(&self.rs.columns[col]).strong());
                if editable {
                    editor.changed |= ui.checkbox(&mut editor.null, "NULL").changed();
                    apply = ui.add_enabled(editor.changed, egui::Button::new(tr("Apply"))).clicked();
                    reset = ui.button(tr("Reset")).clicked();
                } else { ui.label(tr("Read-only")); }
                if ui.button(tr("Copy value")).clicked() { ui.ctx().copy_text(editor.buffer.clone()); }
            });
            egui::ScrollArea::both().id_salt(("cell_editor_scroll", self.salt)).show(ui, |ui| {
                if editor.hex {
                    let hex = editor.buffer.as_bytes().chunks(16).map(|bytes| bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n");
                    ui.label(tr("UTF-8 bytes")); ui.monospace(hex);
                } else {
                    ui.add_enabled_ui(editable && !editor.null, |ui| {
                        editor.changed |= ui.add(egui::TextEdit::multiline(&mut editor.buffer).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY).desired_rows(4)).changed();
                    });
                }
            });
        }
        if apply { self.commit_cell_editor(); }
        if reset { self.cell_editor = None; }
    }

    pub fn open_find(&mut self) {
        self.find_open = true;
        self.find_focus = true;
    }

    fn find_matches(&self) -> Vec<(usize, usize)> {
        if self.find_query.is_empty() { return Vec::new(); }
        let query = self.find_query.to_lowercase();
        (0..self.n_view_rows()).flat_map(|r| {
            let query = &query;
            (0..self.rs.columns.len()).filter_map(move |c| {
                let row = self.row_ref(r)?;
                self.display_value(row, c).flatten()
                    .filter(|v| v.to_lowercase().contains(query)).map(|_| (r, c))
            })
        }).collect()
    }

    fn find_count(&mut self) -> usize {
        let key: FindKey = (self.find_query.clone(), self.n_view_rows(), self.sort, self.filters.len());
        if let Some((k, at, n)) = &self.find_count
            && *k == key
            && at.elapsed() < std::time::Duration::from_secs(1)
        {
            return *n;
        }
        let n = self.find_matches().len();
        self.find_count = Some((key, std::time::Instant::now(), n));
        n
    }

    fn find_step(&mut self, forward: bool) {
        let matches = self.find_matches();
        let next = if forward {
            matches.iter().find(|&&cell| self.selected.is_none_or(|s| cell > s)).or(matches.first())
        } else {
            matches.iter().rev().find(|&&cell| self.selected.is_none_or(|s| cell < s)).or(matches.last())
        }.copied();
        if let Some((r, c)) = next {
            self.commit_edit();
            self.select_cell(r, c, Modifiers::NONE);
            self.scroll_to = Some(r);
        }
    }

    pub fn new(rs: ResultSet, salt: u64) -> Self {
        let title = statement_title(&rs.statement);
        let order = (0..rs.rows.len()).collect();
        Self {
            cell_editor: None,
            rs,
            title,
            order,
            sort: None,
            filters: BTreeMap::new(),
            filter_popup: None,
            selected: None,
            selected_cells: BTreeSet::new(),
            cell_anchor: None,
            drag_anchor: None,
            edit_rows: BTreeSet::new(),
            selected_rows: BTreeSet::new(),
            anchor: None,
            scroll_to: None,
            find_open: false,
            find_query: String::new(),
            find_focus: false,
            find_count: None,
            salt,
            edit: None,
            record_view: None,
            readonly_reason: None,
            edit_checked: false,
        }
    }

    pub fn set_edit_target(&mut self, target: Result<EditTarget, String>) {
        self.edit_checked = true;
        match target {
            Ok(target) => {
                self.edit = Some(EditSession { target, state: EditState::default(), editing: None });
                self.readonly_reason = None;
            }
            Err(reason) => {
                self.edit = None;
                self.readonly_reason = Some(reason);
            }
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.cell_editor.as_ref().is_some_and(|e| e.changed) || self.edit
            .as_ref()
            .is_some_and(|e| e.state.is_dirty() || e.editing.as_ref().is_some_and(|ed| ed.changed))
    }

    pub fn can_undo(&self) -> bool {
        self.cell_editor.as_ref().is_some_and(|e| e.changed) || self.edit.as_ref().is_some_and(|e| e.state.can_undo())
    }

    /// Discards pending changes and any edit in progress.
    pub fn discard_edits(&mut self) {
        self.cell_editor = None;
        self.edit_rows.clear();
        if let Some(e) = self.edit.as_mut() {
            e.state.clear();
            e.editing = None;
        }
        self.clamp_selection();
    }

    /// Keep saved key changes in the cache so refresh can follow the same record.
    pub fn mark_saved(&mut self) {
        if let Some(edit) = &self.edit {
            for (&(r, c), value) in &edit.state.changes {
                if let Some(cell) = self.rs.rows.get_mut(r).and_then(|row| row.get_mut(c)) {
                    *cell = value.clone();
                }
            }
        }
        self.discard_edits();
    }

    pub fn undo(&mut self) {
        if self.cell_editor.take().is_some_and(|e| e.changed) { return; }
        if let Some(e) = self.edit.as_mut() {
            e.editing = None;
            e.state.undo();
            if !e.state.is_dirty() { self.edit_rows.clear(); }
        }
        self.clamp_selection();
    }

    pub fn revert_all(&mut self) {
        self.cell_editor = None;
        self.edit_rows.clear();
        if let Some(e) = self.edit.as_mut() {
            e.editing = None;
            e.state.revert_all();
        }
        self.clamp_selection();
    }

    /// Visible rows: the original ones (in the current order), then the new ones.
    fn n_view_rows(&self) -> usize {
        self.order.len() + self.edit.as_ref().map_or(0, |e| e.state.inserted.len())
    }

    fn row_ref(&self, view_row: usize) -> Option<RowRef> {
        match self.order.get(view_row) {
            Some(&r) => Some(RowRef::Existing(r)),
            None => {
                let i = view_row - self.order.len();
                (i < self.edit.as_ref().map_or(0, |e| e.state.inserted.len())).then_some(RowRef::New(i))
            }
        }
    }

    fn clamp_selection(&mut self) {
        let n = self.n_view_rows();
        self.selected_rows.retain(|&r| r < n);
        self.selected_cells
            .retain(|&(r, c)| r < n && c < self.rs.columns.len());
        if self.selected.is_some_and(|(r, _)| r >= n) {
            self.selected = None;
        }
    }

    /// Appends a new row and opens the editor on its first editable column.
    pub fn add_row(&mut self) {
        self.commit_edit();
        let n_cols = self.rs.columns.len();
        let base = self.order.len();
        let Some(e) = self.edit.as_mut() else { return };
        let i = e.state.add_row(n_cols);
        let vr = base + i;
        let col = (0..n_cols)
            .find(|&c| {
                e.state
                    .cell_editable(&self.rs, &e.target, RowRef::New(i), c)
            })
            .unwrap_or(0);
        e.editing = Some(InlineEdit {
            row: RowRef::New(i),
            col,
            buffer: String::new(),
            focus: true,
            select_all: true,
            changed: false,
        });
        self.selected = Some((vr, col));
        self.selected_cells = BTreeSet::from([(vr, col)]);
        self.cell_anchor = self.selected;
        self.selected_rows = BTreeSet::from([vr]);
        self.anchor = Some(vr);
        self.scroll_to = Some(vr);
    }

    /// Appends a copy of the selected row (DataGrip's "Clone Row"). The primary key and
    /// binary columns are left to their defaults so the copy can be inserted.
    pub fn clone_selected_row(&mut self) {
        self.commit_edit();
        let Some(vr) = self.selected.map(|(r, _)| r).or_else(|| self.selected_rows.iter().next().copied()) else { return };
        let Some(row) = self.row_ref(vr) else { return };
        let n_cols = self.rs.columns.len();
        let values: Vec<Option<Cell>> = (0..n_cols).map(|c| self.display_value(row, c)).collect();
        let base = self.order.len();
        let Some(e) = self.edit.as_mut() else { return };
        e.editing = None;
        let values = values
            .into_iter()
            .enumerate()
            .map(|(c, v)| {
                let skip = (e.target.has_pk && e.target.key_cols.contains(&c)) || e.target.binary_cols.get(c).copied().unwrap_or(false);
                if skip { None } else { v }
            })
            .collect();
        let i = e.state.add_row_with(values);
        let vr = base + i;
        let col = self.selected.map_or(0, |(_, c)| c);
        self.selected = Some((vr, col));
        self.selected_cells = BTreeSet::from([(vr, col)]);
        self.cell_anchor = self.selected;
        self.selected_rows = BTreeSet::from([vr]);
        self.anchor = Some(vr);
        self.scroll_to = Some(vr);
    }

    /// Selected rows as references to original/new rows.
    fn selected_refs(&self) -> Vec<RowRef> {
        let mut rows: BTreeSet<usize> = self.selected_rows.clone();
        rows.extend(self.selected_cells.iter().map(|(r, _)| *r));
        if let Some((r, _)) = self.selected {
            rows.insert(r);
        }
        rows.into_iter().filter_map(|r| self.row_ref(r)).collect()
    }

    pub fn has_selection(&self) -> bool {
        self.selected.is_some() || !self.selected_rows.is_empty()
    }

    /// Marks the selected rows for deletion (new ones are removed directly).
    pub fn delete_selected(&mut self) {
        let refs = self.selected_refs();
        let Some(e) = self.edit.as_mut() else { return };
        e.editing = None;
        if e.state.delete_rows(&self.rs, &e.target, &refs) && refs.iter().any(|r| matches!(r, RowRef::New(_))) {
            self.selected = None;
            self.selected_rows.clear();
            self.selected_cells.clear();
        }
        self.clamp_selection();
    }

    /// Opens the inline editor on visual cell `(vr, col)` if it is editable.
    fn start_edit(&mut self, vr: usize, col: usize) -> bool {
        let Some(row) = self.row_ref(vr) else { return false };
        let Some(e) = self.edit.as_mut() else { return false };
        if !e.state.cell_editable(&self.rs, &e.target, row, col) {
            return false;
        }
        let buffer = e
            .state
            .current(&self.rs, row, col)
            .flatten()
            .unwrap_or_default();
        e.editing = Some(InlineEdit {
            row,
            col,
            buffer,
            focus: true,
            select_all: true,
            changed: false,
        });
        self.selected = Some((vr, col));
        true
    }

    /// Commits the inline editor without changing the selected cell.
    fn commit_edit(&mut self) {
        self.commit_cell_editor();
        let Some(ed) = self.edit.as_mut().and_then(|e| e.editing.take()) else {
            return;
        };
        if !ed.changed {
            return;
        }
        // The selection only gets the value when the editor is one of its cells; otherwise
        // (cursor moved by the record view, a reverted new row…) only the edited cell does.
        let mut cells = self.selected_cell_refs();
        if !cells.contains(&(ed.row, ed.col)) {
            cells = vec![(ed.row, ed.col)];
        }
        self.apply_cell_values(cells.into_iter().map(|(row, col)| (row, col, Some(ed.buffer.clone()))).collect());
    }

    fn toggle_sort(&mut self, col: usize) {
        self.sort = match self.sort {
            Some((c, SortDir::Asc)) if c == col => Some((col, SortDir::Desc)),
            Some((c, SortDir::Desc)) if c == col => None,
            _ => Some((col, SortDir::Asc)),
        };
        self.rebuild_order();
    }

    fn rebuild_order(&mut self) {
        self.order = compute_order(&self.rs.rows, self.sort, &self.filters);
    }

    /// Original rows hidden by the column filters.
    pub fn filtered_out(&self) -> usize {
        self.rs.rows.len() - self.order.len()
    }

    fn set_filter(&mut self, col: usize, filter: Option<ColumnFilter>) {
        self.commit_edit();
        match filter {
            Some(f) => self.filters.insert(col, f),
            None => self.filters.remove(&col),
        };
        self.rebuild_order();
        self.selected = None;
        self.selected_rows.clear();
        self.selected_cells.clear();
        self.cell_anchor = None;
        self.anchor = None;
    }

    /// Displayed value (with pending changes). `None` = untouched new cell.
    fn display_value(&self, row: RowRef, col: usize) -> Option<Cell> {
        match &self.edit {
            Some(e) => e.state.current(&self.rs, row, col),
            None => match row {
                RowRef::Existing(r) => self.rs.rows.get(r).and_then(|x| x.get(col)).cloned(),
                RowRef::New(_) => None,
            },
        }
    }

    fn row_tsv(&self, view_row: usize) -> String {
        let Some(row) = self.row_ref(view_row) else { return String::new() };
        let vals: Vec<Cell> = (0..self.rs.columns.len()).map(|c| self.display_value(row, c).flatten()).collect();
        row_to_tsv(&vals)
    }

    pub fn row_count(&self) -> usize {
        self.rs.rows.len()
    }

    /// Opens the inline editor on visual cell `(vr, col)` (demo mode and shortcuts).
    pub fn begin_edit(&mut self, vr: usize, col: usize) -> bool {
        self.start_edit(vr, col)
    }

    /// Commits the inline editor if one is open (before previewing or saving).
    pub fn finish_inline_edit(&mut self) {
        self.commit_edit();
    }

    fn selected_cell_refs(&self) -> Vec<(RowRef, usize)> {
        if self.selected_cells.is_empty() {
            return self
                .selected
                .and_then(|(r, c)| self.row_ref(r).map(|row| vec![(row, c)]))
                .unwrap_or_default();
        }
        self.selected_cells
            .iter()
            .filter_map(|&(r, c)| self.row_ref(r).map(|row| (row, c)))
            .collect()
    }

    fn cell_selected(&self, r: usize, c: usize) -> bool {
        self.selected_cells.contains(&(r, c))
            || (self.selected_cells.is_empty() && self.selected == Some((r, c)))
    }

    fn select_cell(&mut self, r: usize, c: usize, mods: Modifiers) {
        if self.row_ref(r).is_none() || c >= self.rs.columns.len() {
            return;
        }
        if mods.shift {
            let (ar, ac) = self.cell_anchor.or(self.selected).unwrap_or((r, c));
            self.cell_anchor = Some((ar, ac));
            if !mods.command {
                self.selected_cells.clear();
            }
            for vr in ar.min(r)..=ar.max(r) {
                self.selected_cells
                    .extend((ac.min(c)..=ac.max(c)).map(|col| (vr, col)));
            }
        } else if mods.command {
            if self.selected_cells.is_empty()
                && let Some(cell) = self.selected
            {
                self.selected_cells.insert(cell);
            }
            if !self.selected_cells.remove(&(r, c)) {
                self.selected_cells.insert((r, c));
            }
            self.cell_anchor = Some((r, c));
        } else {
            self.selected_cells = BTreeSet::from([(r, c)]);
            self.cell_anchor = Some((r, c));
        }
        self.selected = if self.selected_cells.contains(&(r, c)) {
            Some((r, c))
        } else {
            self.selected_cells.iter().next().copied()
        };
        self.selected_rows.clear();
        self.anchor = Some(r);
    }

    fn select_column(&mut self, col: usize, additive: bool) {
        self.commit_edit();
        let remove = additive
            && self.n_view_rows() > 0
            && (0..self.n_view_rows()).all(|r| self.selected_cells.contains(&(r, col)));
        if !additive {
            self.selected_cells.clear();
        }
        for r in 0..self.n_view_rows() {
            if remove {
                self.selected_cells.remove(&(r, col));
            } else {
                self.selected_cells.insert((r, col));
            }
        }
        self.selected = if remove {
            self.selected_cells.iter().next().copied()
        } else {
            (self.n_view_rows() > 0).then_some((0, col))
        };
        self.cell_anchor = self.selected;
        self.anchor = self.selected.map(|(r, _)| r);
        self.selected_rows.clear();
        self.scroll_to = self.selected.map(|(r, _)| r);
    }

    fn selection_tsv(&self) -> String {
        if self.selected_cells.is_empty() {
            if self.selected.is_none() {
                return self
                    .selected_rows
                    .iter()
                    .map(|&r| self.row_tsv(r))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            return self
                .selected
                .and_then(|(r, c)| {
                    self.row_ref(r)
                        .and_then(|row| self.display_value(row, c).flatten())
                })
                .unwrap_or_default();
        }
        let rows: BTreeSet<_> = self.selected_cells.iter().map(|(r, _)| *r).collect();
        let cols: BTreeSet<_> = self.selected_cells.iter().map(|(_, c)| *c).collect();
        rows.into_iter()
            .map(|r| {
                cols.iter()
                    .map(|&c| {
                        if !self.selected_cells.contains(&(r, c)) {
                            return String::new();
                        }
                        self.row_ref(r)
                            .and_then(|row| self.display_value(row, c).flatten())
                            .unwrap_or_default()
                            .replace(['\t', '\r', '\n'], " ")
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn apply_cell_values(&mut self, values: Vec<(RowRef, usize, Cell)>) {
        if let Some(edit) = self.edit.as_mut() {
            if !edit.state.is_dirty() { self.edit_rows.clear(); }
            let rows: BTreeSet<_> = values.iter().filter(|(row, col, _)| edit.state.cell_editable(&self.rs, &edit.target, *row, *col)).map(|(row, _, _)| *row).collect();
            if edit.state.set_values(&self.rs, &edit.target, values) { self.edit_rows.extend(rows); }
        }
    }

    fn fill_selection(&mut self, value: Cell) {
        self.apply_cell_values(self.selected_cell_refs().into_iter().map(|(r, c)| (r, c, value.clone())).collect());
    }

    fn paste_selection(&mut self, text: &str) -> Result<(), String> {
        let text = text.trim_end_matches(['\r', '\n']);
        if !text.contains(['\t', '\n']) {
            self.fill_selection(Some(text.into()));
            return Ok(());
        }
        let Some((r, c)) = self.selected_cells.iter().next().copied().or(self.selected) else {
            return Ok(());
        };
        let mut values = Vec::new();
        for (dr, row) in text.lines().enumerate() {
            let row_ref = self
                .row_ref(r + dr)
                .ok_or_else(|| tr("Clipboard data does not fit in the loaded grid").to_string())?;
            for (dc, value) in row.trim_end_matches('\r').split('\t').enumerate() {
                if c + dc >= self.rs.columns.len() {
                    return Err(tr("Clipboard data does not fit in the loaded grid").into());
                }
                values.push((row_ref, c + dc, Some(value.to_owned())));
            }
        }
        self.apply_cell_values(values);
        Ok(())
    }

    /// Move in visual order, including filtered/sorted results and new rows.
    fn navigate(&mut self, key: Key, mods: Modifiers, page_rows: usize) {
        let Some((r, c)) = self.selected else { return };
        let rows = self.n_view_rows();
        let cols = self.rs.columns.len();
        if rows == 0 || cols == 0 {
            return;
        }
        self.commit_edit();
        let last_row = rows - 1;
        let last_col = cols - 1;
        let (nr, nc) = match key {
            Key::ArrowUp if mods.command => (0, c),
            Key::ArrowDown if mods.command => (last_row, c),
            Key::ArrowLeft if mods.command => (r, 0),
            Key::ArrowRight if mods.command => (r, last_col),
            Key::Home if mods.command => (0, c),
            Key::End if mods.command => (last_row, c),
            Key::Home => (r, 0),
            Key::End => (r, last_col),
            Key::ArrowUp => (r.saturating_sub(1), c),
            Key::ArrowDown => ((r + 1).min(last_row), c),
            Key::ArrowLeft => (r, c.saturating_sub(1)),
            Key::ArrowRight => (r, (c + 1).min(last_col)),
            Key::PageUp if mods.command => (0, c),
            Key::PageDown if mods.command => (last_row, c),
            Key::PageUp => (r.saturating_sub(page_rows.max(1)), c),
            Key::PageDown => (r.saturating_add(page_rows.max(1)).min(last_row), c),
            Key::Enter if mods.shift => (r.saturating_sub(1), c),
            Key::Enter => ((r + 1).min(last_row), c),
            Key::Tab => {
                let cell = r * cols + c;
                let next = if mods.shift {
                    cell.saturating_sub(1)
                } else {
                    (cell + 1).min(rows * cols - 1)
                };
                (next / cols, next % cols)
            }
            _ => return,
        };
        let extend = mods.shift && key != Key::Tab && key != Key::Enter;
        self.select_cell(
            nr,
            nc,
            if extend {
                Modifiers::SHIFT
            } else {
                Modifiers::NONE
            },
        );
        self.scroll_to = Some(nr);
    }

    fn selected_row(&self) -> Option<RowRef> {
        self.selected
            .map(|(r, _)| r)
            .or_else(|| self.selected_rows.iter().next().copied())
            .and_then(|r| self.row_ref(r))
    }

    fn cancel_row(&mut self) {
        self.cell_editor = None;
        let row = self.selected_row();
        if let Some(row) = row { self.edit_rows.remove(&row); }
        if let Some(e) = self.edit.as_mut() {
            e.editing = None;
            if let Some(row) = row {
                e.state.revert_row(row);
            }
        }
        // Reverting a new row shifts the ones after it: `New(j)` becomes `New(j - 1)`, and so do
        // their view rows.
        if let Some(RowRef::New(i)) = row {
            self.edit_rows = std::mem::take(&mut self.edit_rows)
                .into_iter()
                .map(|r| match r {
                    RowRef::New(j) if j > i => RowRef::New(j - 1),
                    other => other,
                })
                .collect();
            let removed = self.order.len() + i;
            let shift = |r: usize| if r > removed { r - 1 } else { r };
            self.selected_cells = std::mem::take(&mut self.selected_cells)
                .into_iter()
                .filter(|&(r, _)| r != removed)
                .map(|(r, c)| (shift(r), c))
                .collect();
            self.selected_rows = std::mem::take(&mut self.selected_rows).into_iter().map(shift).collect();
        }
        self.clamp_selection();
    }

    fn row_in_edit_batch(&self, row: RowRef) -> bool {
        self.edit_rows.contains(&row) || self.edit.as_ref().is_some_and(|edit| match row {
            RowRef::Existing(r) => edit.state.deleted.contains(&r) || edit.state.changes.keys().any(|(changed, _)| *changed == r),
            RowRef::New(i) => i < edit.state.inserted.len(),
        })
    }

    fn row_change_needs_save(&self, previous: Option<RowRef>) -> bool {
        // A newly added row must be filled before it is submitted.
        previous.is_none_or(|row| self.row_in_edit_batch(row))
            && self
                .selected_row()
                .is_some_and(|r| matches!(r, RowRef::Existing(_)) && Some(r) != previous && !self.row_in_edit_batch(r))
            && self.is_dirty()
    }

    /// Refresh data without resetting the grid's column widths, filters or cursor.
    pub fn refresh(&mut self, rs: ResultSet) {
        self.cell_editor = None;
        let selection = self.selected.map(|(r, c)| {
            let keys = self
                .edit
                .as_ref()
                .filter(|e| e.target.has_pk)
                .and_then(|e| {
                    let row = self.row_ref(r)?;
                    Some(
                        e.target
                            .key_cols
                            .iter()
                            .map(|&k| (k, self.display_value(row, k).flatten()))
                            .collect::<Vec<_>>(),
                    )
                });
            (r, c, keys)
        });
        self.rs = rs;
        self.edit_rows.clear();
        self.edit = None;
        self.edit_checked = false;
        self.readonly_reason = None;
        self.rebuild_order();
        self.selected_rows.clear();
        self.selected = selection.and_then(|(r, c, keys)| {
            if c >= self.rs.columns.len() || self.order.is_empty() {
                return None;
            }
            let row = keys
                .and_then(|keys| {
                    self.order
                        .iter()
                        .position(|&i| keys.iter().all(|(k, v)| self.rs.rows[i].get(*k) == Some(v)))
                })
                .unwrap_or_else(|| r.min(self.order.len() - 1));
            self.selected_rows.insert(row);
            self.anchor = Some(row);
            self.scroll_to = Some(row);
            Some((row, c))
        });
        self.selected_cells = self.selected.into_iter().collect();
        self.cell_anchor = self.selected;
        self.drag_anchor = None;
    }
}

fn row_to_tsv(row: &[Cell]) -> String {
    row.iter()
        .map(|c| c.as_deref().unwrap_or("").replace(['\t', '\n'], " "))
        .collect::<Vec<_>>()
        .join("\t")
}

fn compare_cells(a: Option<&Cell>, b: Option<&Cell>) -> Ordering {
    let a = a.and_then(|c| c.as_deref());
    let b = b.and_then(|c| c.as_deref());
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => match (x.trim().parse::<f64>(), y.trim().parse::<f64>()) {
            (Ok(nx), Ok(ny)) => nx.partial_cmp(&ny).unwrap_or(Ordering::Equal),
            _ => x.cmp(y),
        },
    }
}

/// Short title from the statement.
pub fn statement_title(sql: &str) -> String {
    let one_line = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&one_line, 28)
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

/// Execution log entry.
pub struct OutputEntry {
    pub time: String,
    pub statement: String,
    pub message: String,
    pub is_error: bool,
}

pub fn affected_message(rs: &ResultSet) -> String {
    match rs.affected {
        Some(1) => tr("1 row affected").into(),
        Some(n) => trf("{n} rows affected", &[("n", &n)]),
        None => tr("OK").into(),
    }
}

pub fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1000.0 { format!("{ms:.0} ms") } else { format!("{:.2} s", ms / 1000.0) }
}

/// Escapes a value for CSV (RFC 4180).
pub fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub fn to_csv(view: &ResultView) -> String {
    let rows: Vec<&Vec<Cell>> = view.order.iter().filter_map(|&r| view.rs.rows.get(r)).collect();
    csv_text(&view.rs.columns, &rows)
}

fn csv_text(columns: &[String], rows: &[&Vec<Cell>]) -> String {
    let mut out = String::new();
    out.push_str(&columns.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(","));
    out.push_str("\r\n");
    for row in rows {
        let line: Vec<String> = row.iter().map(|c| c.as_deref().map(csv_escape).unwrap_or_default()).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

/// Masking for an export: one rule per result column, and the key of the deterministic ones.
#[derive(Clone, Copy)]
pub struct ExportMasks<'a> {
    pub masks: &'a [crate::db::copy::Mask],
    pub key: &'a str,
}

/// Extraction formats (DataGrip's "CSV ⌄" selector).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExportFormat {
    #[default]
    Csv,
    Tsv,
    Json,
    SqlInsert,
    Markdown,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 5] =
        [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Json, ExportFormat::SqlInsert, ExportFormat::Markdown];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::SqlInsert => tr("SQL Inserts"),
            ExportFormat::Markdown => "Markdown",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert => "sql",
            ExportFormat::Markdown => "md",
        }
    }
}

fn json_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for ch in v.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Visible rows (current filter and order) as text in `fmt`. `insert_target` is the already
/// quoted table for "SQL Inserts".
pub fn format_rows(view: &ResultView, fmt: ExportFormat, insert_target: &str) -> String {
    format_rows_masked(view, fmt, insert_target, None)
}

/// `format_rows` with each column's mask applied to the values (never to the grid).
pub fn format_rows_masked(view: &ResultView, fmt: ExportFormat, insert_target: &str, masks: Option<ExportMasks>) -> String {
    let cols = &view.rs.columns;
    let mut owned: Vec<Vec<Cell>> = view.order.iter().filter_map(|&r| view.rs.rows.get(r).cloned()).collect();
    if let Some(m) = masks {
        for row in &mut owned {
            for (cell, mask) in row.iter_mut().zip(m.masks) {
                *cell = mask.apply(m.key.as_bytes(), cell.take(), None);
            }
        }
    }
    let rows: Vec<&Vec<Cell>> = owned.iter().collect();
    match fmt {
        ExportFormat::Csv => csv_text(cols, &rows),
        ExportFormat::Tsv => {
            let clean = |v: &str| v.replace(['\t', '\n', '\r'], " ");
            let mut out = cols.iter().map(|c| clean(c)).collect::<Vec<_>>().join("\t");
            out.push('\n');
            for row in rows {
                out.push_str(&row.iter().map(|c| c.as_deref().map(clean).unwrap_or_default()).collect::<Vec<_>>().join("\t"));
                out.push('\n');
            }
            out
        }
        ExportFormat::Json => {
            let items: Vec<String> = rows
                .iter()
                .map(|row| {
                    let fields: Vec<String> = cols
                        .iter()
                        .zip(row.iter())
                        .map(|(c, v)| format!("{}: {}", json_string(c), v.as_deref().map_or("null".into(), json_string)))
                        .collect();
                    format!("  {{{}}}", fields.join(", "))
                })
                .collect();
            if items.is_empty() { "[]\n".into() } else { format!("[\n{}\n]\n", items.join(",\n")) }
        }
        ExportFormat::SqlInsert => {
            let col_list = cols.iter().map(|c| format!("\"{}\"", c.replace('"', "\"\""))).collect::<Vec<_>>().join(", ");
            let mut out = String::new();
            for row in rows {
                let vals = row
                    .iter()
                    .map(|v| v.as_deref().map_or("NULL".into(), |s| format!("'{}'", s.replace('\'', "''"))))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("INSERT INTO {insert_target} ({col_list}) VALUES ({vals});\n"));
            }
            out
        }
        ExportFormat::Markdown => {
            let cell = |v: &str| v.replace('|', "\\|").replace(['\n', '\r'], " ");
            let mut out = format!("| {} |\n", cols.iter().map(|c| cell(c)).collect::<Vec<_>>().join(" | "));
            out.push_str(&format!("|{}\n", " --- |".repeat(cols.len())));
            for row in rows {
                out.push_str(&format!("| {} |\n", row.iter().map(|v| v.as_deref().map(cell).unwrap_or_default()).collect::<Vec<_>>().join(" | ")));
            }
            out
        }
    }
}

/// Saves the visible rows to a file chosen by the user. Ok(false) if cancelled.
pub fn export_to_file(view: &ResultView, fmt: ExportFormat, insert_target: &str, base_name: &str, masks: Option<ExportMasks>) -> Result<bool, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title(trf("Export to {format}", &[("format", &fmt.label())]))
        .add_filter(fmt.label(), &[fmt.extension()])
        .set_file_name(format!("{base_name}.{}", fmt.extension()))
        .save_file()
    else {
        return Ok(false);
    };
    std::fs::write(&path, format_rows_masked(view, fmt, insert_target, masks)).map(|_| true).map_err(|e| e.to_string())
}

pub fn export_csv(view: &ResultView) -> Result<bool, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title(trf("Export to {format}", &[("format", &"CSV")]))
        .add_filter("CSV", &["csv"])
        .set_file_name(format!("{}.csv", tr("result")))
        .save_file()
    else {
        return Ok(false);
    };
    std::fs::write(&path, to_csv(view)).map(|_| true).map_err(|e| e.to_string())
}

/// Grid row and header height.
const ROW_H: f32 = 24.0;
const HEADER_H: f32 = 30.0;

fn is_numeric(v: &str) -> bool {
    let t = v.trim();
    !t.is_empty() && t.len() < 40 && t.parse::<f64>().is_ok()
}

/// What the grid asks to do after drawing (applied outside the TableBuilder).
enum GridAction {
    Sort(usize),
    OpenFilter(usize, egui::Pos2),
    Select { vr: usize, col: Option<usize>, mods: Modifiers },
    SelectColumn(usize, bool),
    DragStart(usize, usize, bool),
    DragTo(usize, usize),
    Type(String),
    Paste(String),
    ClearSelection,
    StartEdit(usize, usize),
    Commit,
    Navigate(Key, Modifiers),
    AddRow,
    Cancel,
    SetNull(RowRef, usize),
    Revert(RowRef, usize),
    DeleteRows,
    CloneRow,
    RecordView(usize),
    Copy(String),
}

#[derive(Default)]
pub struct GridResponse {
    pub error: Option<String>,
    pub submit: bool,
}

/// Draws a result set as a DataGrip-style data grid. `columns_info` marks primary keys in
/// the header. If the view is editable (`view.edit`), allows inline editing (double click,
/// Enter or F2). Returns an error message if exporting fails.
pub fn show_result(
    ui: &mut Ui,
    view: &mut ResultView,
    copy_requested: bool,
    columns_info: Option<&[ColumnInfo]>,
    save_on_row_change: bool,
    floating_summary: bool,
) -> GridResponse {
    let p = pal(ui);
    let mut error = None;
    let area_rect = ui.max_rect();

    if view.rs.is_command() {
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new(format!("{} · {}", affected_message(&view.rs), format_duration(view.rs.elapsed))).color(p.muted),
            )
        });
        return GridResponse::default();
    }

    let mut actions: Vec<GridAction> = Vec::new();
    let previous_row = view.selected_row();
    if ui.is_enabled() && !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
        view.open_find();
    }
    if view.find_open {
        let mut step = None;
        let mut restart = false;
        ui.horizontal(|ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut view.find_query)
                .id_salt(("grid_find", view.salt)).hint_text(tr("Find in loaded rows…")).desired_width(240.0));
            if std::mem::take(&mut view.find_focus) { response.request_focus(); }
            if response.changed() {
                restart = true;
                step = Some(true);
            }
            let enter = (response.has_focus() || response.lost_focus()) && ui.input(|i| i.key_pressed(Key::Enter));
            if enter { response.request_focus(); }
            if ui.button(tr("Previous")).clicked() { step = Some(false); }
            if ui.button(tr("Next")).clicked() || enter { step = Some(!ui.input(|i| i.modifiers.shift)); }
            let matches = view.find_count();
            ui.label(if matches == 1 { tr("1 match").into() } else { trf("{n} matches", &[("n", &matches)]) });
            let escape = (response.has_focus() || response.lost_focus())
                && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
            if theme::icon_button(ui, Ico::Close, true, tr("Close (Esc)")).clicked()
                || escape {
                view.find_open = false;
                response.surrender_focus();
            }
        });
        if let Some(forward) = step {
            let previous = view.selected;
            if restart { view.selected = None; }
            view.find_step(forward);
            if view.selected.is_none() { view.selected = previous; }
        }
    }
    let editable = view.edit.is_some();
    demo_filter(view, area_rect);
    demo_grid(view);
    let filters_active: Vec<usize> = view.filters.keys().copied().collect();
    let editing_now = view.edit.as_ref().is_some_and(|e| e.editing.is_some());
    let edit_id = egui::Id::new(("cell_edit", view.salt));
    let page_rows = (area_rect.height() / ROW_H).floor().max(1.0) as usize;
    let owns_focus = ui.memory(|m| m.focused().is_none() || m.has_focus(edit_id));
    let editor_pending_focus = view.edit.as_ref().and_then(|e| e.editing.as_ref()).is_some_and(|ed| ed.focus);
    let keyboard_enabled = ui.is_enabled() && !egui::Popup::is_any_open(ui.ctx());

    if keyboard_enabled && editable && editing_now && (owns_focus || editor_pending_focus) {
        let action = ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Escape) { Some(GridAction::Cancel) }
            else if i.consume_key(Modifiers::SHIFT, Key::Tab) { Some(GridAction::Navigate(Key::Tab, Modifiers::SHIFT)) }
            else if i.consume_key(Modifiers::NONE, Key::Tab) { Some(GridAction::Navigate(Key::Tab, Modifiers::NONE)) }
            else if i.consume_key(Modifiers::COMMAND, Key::Enter) || i.consume_key(Modifiers::SHIFT, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Enter) { Some(GridAction::Commit) }
            else if i.consume_key(Modifiers::NONE, Key::ArrowUp) { Some(GridAction::Navigate(Key::ArrowUp, Modifiers::NONE)) }
            else if i.consume_key(Modifiers::NONE, Key::ArrowDown) { Some(GridAction::Navigate(Key::ArrowDown, Modifiers::NONE)) }
            else { None }
        });
        if let Some(action) = action { actions.push(action); }
        let (add, delete) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::N)
                    || i.consume_key(Modifiers::NONE, Key::Insert),
                i.consume_key(Modifiers::COMMAND, Key::Delete),
            )
        });
        if add {
            actions.push(GridAction::AddRow);
        }
        if delete {
            actions.push(GridAction::DeleteRows);
        }
    }

    // Retain grid ownership when an inline editor was removed before this frame.
    if keyboard_enabled && owns_focus && !editing_now
    {
        if (copy_requested || ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))) && view.has_selection() {
            ui.ctx().copy_text(view.selection_tsv());
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
            view.selected_cells = (0..view.n_view_rows())
                .flat_map(|r| (0..view.rs.columns.len()).map(move |c| (r, c)))
                .collect();
            view.selected = view.selected_cells.iter().next().copied();
            view.cell_anchor = view.selected;
            view.selected_rows.clear();
            view.scroll_to = view.selected.map(|(r, _)| r);
        }
        if let Some((r, _)) = view.selected
            && ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter))
        {
            actions.push(GridAction::RecordView(r));
        }
        if editable && let Some((r, c)) = view.selected {
            let pasted = ui.input_mut(|i| {
                let text = i.events.iter().find_map(|e| {
                    if let egui::Event::Paste(s) = e {
                        Some(s.clone())
                    } else {
                        None
                    }
                });
                if text.is_some() {
                    i.events.retain(|e| !matches!(e, egui::Event::Paste(_)));
                }
                text
            });
            if let Some(text) = pasted {
                actions.push(GridAction::Paste(text));
            }
            let typed = ui.input_mut(|i| {
                if i.modifiers.command || i.modifiers.alt {
                    return String::new();
                }
                let text = i
                    .events
                    .iter()
                    .filter_map(|e| {
                        if let egui::Event::Text(s) = e {
                            Some(s.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<String>();
                if !text.is_empty() {
                    i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
                }
                text
            });
            if !typed.is_empty() {
                actions.push(GridAction::Type(typed));
            }
            let (edit_key, undo, clone) = ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::COMMAND, Key::Enter)
                        || i.consume_key(Modifiers::NONE, Key::Enter)
                        || i.consume_key(Modifiers::NONE, Key::F2),
                    i.consume_key(Modifiers::COMMAND, Key::Z),
                    i.consume_key(Modifiers::COMMAND, Key::D),
                )
            });
            if edit_key {
                actions.push(GridAction::StartEdit(r, c));
            }
            if undo {
                view.undo();
            }
            if clone {
                actions.push(GridAction::CloneRow);
            }
        }
        if editable {
            let (add, delete, cancel, clear) = ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::COMMAND, Key::N)
                        || i.consume_key(Modifiers::NONE, Key::Insert),
                    i.consume_key(Modifiers::COMMAND, Key::Delete),
                    i.consume_key(Modifiers::NONE, Key::Escape),
                    i.consume_key(Modifiers::NONE, Key::Delete),
                )
            });
            if add {
                actions.push(GridAction::AddRow);
            }
            if delete {
                actions.push(GridAction::DeleteRows);
            }
            if cancel {
                actions.push(GridAction::Cancel);
            }
            if clear {
                actions.push(GridAction::ClearSelection);
            }
        }
        if view.selected.is_some() {
            let navigation = ui.input_mut(|i| {
                [
                    Key::ArrowUp,
                    Key::ArrowDown,
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::Home,
                    Key::End,
                    Key::PageUp,
                    Key::PageDown,
                    Key::Tab,
                ]
                .into_iter()
                .find_map(|key| {
                    let mods = i.modifiers;
                    (!mods.alt && i.consume_key(mods, key)).then_some((key, mods))
                })
            });
            if let Some((key, mods)) = navigation {
                actions.push(GridAction::Navigate(key, mods));
            }
        }
    }

    let n_rows = view.n_view_rows();
    let n_cols = view.rs.columns.len();
    let digits = n_rows.max(1).to_string().len() as f32;
    let cell_font = FontId::monospace(12.5);
    let char_w = ui.fonts_mut(|f| f.glyph_width(&cell_font, '0'));
    let index_width = 22.0 + digits * char_w;

    // Initial widths from the content of the first rows.
    let initial_widths: Vec<f32> = (0..n_cols)
        .map(|c| {
            let header = view.rs.columns[c].chars().count();
            let content = view.rs.rows.iter().take(50).map(|r| r.get(c).and_then(|v| v.as_deref()).map_or(6, |v| v.chars().count())).max().unwrap_or(0);
            // Header: icon (24) + bold name + funnel and sort (44).
            let header_w = header as f32 * (char_w + 0.6) + 72.0;
            let content_w = content.min(40) as f32 * char_w + 22.0;
            header_w.max(content_w).clamp(60.0, 380.0)
        })
        .collect();

    let sort = view.sort;
    let selected_rows = view.selected_rows.clone();
    let scroll_to = view.scroll_to.take();
    // The inline editor is taken out of the view so it can be edited while `view` is read.
    let mut inline = view.edit.as_mut().and_then(|e| e.editing.take());

    let pk_of = |i: usize, name: &str| -> bool {
        if let Some(e) = &view.edit
            && e.target.has_pk
        {
            return e.target.key_cols.contains(&i);
        }
        columns_info.is_some_and(|cols| cols.iter().any(|c| c.primary_key && c.name.eq_ignore_ascii_case(name)))
    };

    egui::ScrollArea::horizontal().id_salt(("result_h", view.salt)).auto_shrink([false, false]).show(ui, |ui| {
        let mut scroll_cell = None;
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
        {
            let v = ui.visuals_mut();
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.grid);
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.control_border);
            v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
            v.widgets.hovered.bg_fill = theme::blend(p.island_bg, p.row_hover);
        }
        let mut table = TableBuilder::new(ui)
            .id_salt(("result_table", view.salt, n_cols))
            .striped(false)
            .resizable(true)
            .sense(Sense::click_and_drag())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(index_width).resizable(false));
        if let Some(r) = scroll_to {
            table = table.scroll_to_row(r, None);
        }
        for w in &initial_widths {
            table = table.column(Column::initial(*w).at_least(36.0).clip(true).resizable(true));
        }
        table
            .header(HEADER_H, |mut header| {
                header.col(|ui| {
                    let r = ui.max_rect();
                    ui.painter().rect_filled(r, 0.0, p.header_bg);
                    ui.painter().hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, p.grid));
                });
                for (i, name) in view.rs.columns.iter().enumerate() {
                    let mut funnel_hit = Rect::NOTHING;
                    let mut sort_hit = Rect::NOTHING;
                    let (_, resp) = header.col(|ui| {
                        let r = ui.max_rect();
                        if scroll_to.is_some() && view.selected.is_some_and(|(_, c)| c == i) { scroll_cell = Some(r); }
                        ui.painter().rect_filled(r, 0.0, p.header_bg);
                        ui.painter().hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, p.grid));
                        let painter = ui.painter_at(r);
                        let cy = r.center().y;
                        let ico = if pk_of(i, name) { Ico::Key } else { Ico::Column };
                        icons::paint(&painter, Rect::from_min_size(pos2(r.left() + 6.0, cy - 7.0), egui::Vec2::splat(14.0)), ico, p.icon, &p, true);
                        let text_right = r.right() - 40.0;
                        let galley = painter.layout_no_wrap(name.clone(), theme::mono_bold(12.5), p.text);
                        let tp = pos2(r.left() + 24.0, cy - galley.size().y / 2.0);
                        painter.with_clip_rect(Rect::from_min_max(r.min, pos2(text_right.max(r.left() + 24.0), r.max.y))).galley(tp, galley, p.text);
                        let funnel = Rect::from_min_size(pos2(r.right() - 36.0, cy - 6.5), egui::Vec2::splat(13.0));
                        funnel_hit = funnel.expand(4.0);
                        let funnel_color = if filters_active.contains(&i) { p.accent } else { p.disabled };
                        icons::paint(&painter, funnel, Ico::Filter, funnel_color, &p, true);
                        let sort_ico = match sort {
                            Some((c, SortDir::Asc)) if c == i => Ico::SortAsc,
                            Some((c, SortDir::Desc)) if c == i => Ico::SortDesc,
                            _ => Ico::Sort,
                        };
                        let sort_color = if matches!(sort, Some((c, _)) if c == i) { p.accent } else { p.disabled };
                        let sr = Rect::from_min_size(pos2(r.right() - 20.0, cy - 6.5), egui::Vec2::splat(13.0));
                        sort_hit = sr.expand(2.0);
                        icons::paint(&painter, sr, sort_ico, sort_color, &p, true);
                    });
                    let on_funnel = resp.hover_pos().is_some_and(|pos| funnel_hit.contains(pos));
                    let on_sort = resp.hover_pos().is_some_and(|pos| sort_hit.contains(pos));
                    let hint = if on_funnel { tr("Filter by values").to_string() } else if on_sort { trf("{name}\nClick to sort", &[("name", name)]) } else { tr("Select column").to_string() };
                    let resp = resp.on_hover_text(hint);
                    if resp.clicked() {
                        if resp.interact_pointer_pos().is_some_and(|pos| funnel_hit.contains(pos)) {
                            actions.push(GridAction::OpenFilter(i, funnel_hit.left_bottom()));
                        } else if resp.interact_pointer_pos().is_some_and(|pos| sort_hit.contains(pos)) {
                            actions.push(GridAction::Sort(i));
                        } else {
                            actions.push(GridAction::SelectColumn(i, ui_mods(&resp).command));
                        }
                    }
                }
            })
            .body(|body| {
                body.rows(ROW_H, n_rows, |mut row| {
                    let vr = row.index();
                    let Some(rref) = view.row_ref(vr) else { return };
                    let edit = view.edit.as_ref();
                    let deleted = edit.is_some_and(|e| e.state.is_deleted(rref));
                    let is_new = matches!(rref, RowRef::New(_));
                    let row_sel = selected_rows.contains(&vr);
                    let row_bg = if deleted {
                        Some(p.row_deleted)
                    } else if is_new {
                        Some(p.row_added)
                    } else {
                        None
                    };
                    let (_, gresp) = row.col(|ui| {
                        let r = ui.max_rect();
                        if row_sel {
                            ui.painter().rect_filled(r, 0.0, p.selection_inactive);
                        } else if let Some(bg) = row_bg {
                            ui.painter().rect_filled(r, 0.0, bg);
                        }
                        ui.painter().hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, p.grid));
                        let (label, color) = match rref {
                            RowRef::New(_) => ("+".to_string(), p.ok),
                            RowRef::Existing(_) if deleted => ((vr + 1).to_string(), p.error),
                            RowRef::Existing(_) => ((vr + 1).to_string(), p.gutter_fg),
                        };
                        ui.painter().text(pos2(r.left() + 8.0, r.center().y), egui::Align2::LEFT_CENTER, label, cell_font.clone(), color);
                    });
                    if gresp.clicked() || gresp.secondary_clicked() {
                        let mods = ui_mods(&gresp);
                        actions.push(GridAction::Select { vr, col: None, mods });
                    }
                    for c in 0..n_cols {
                        let is_sel = view.cell_selected(vr, c);
                        let value = view.display_value(rref, c);
                        let can_edit = edit.is_some_and(|e| e.state.cell_editable(&view.rs, &e.target, rref, c));
                        let editing_here = inline.as_ref().is_some_and(|ed| ed.row == rref && ed.col == c);
                        let modified = (edit.is_some_and(|e| e.state.is_modified(rref, c)) && !is_new)
                            || inline.as_ref().is_some_and(|ed| editing_here && ed.changed);
                        let (_, resp) = row.col(|ui| {
                            let r = ui.max_rect();
                            let bg = if modified && !deleted {
                                Some(p.cell_modified)
                            } else if is_sel && !editing_here {
                                Some(p.selection)
                            } else if row_sel {
                                Some(p.selection_inactive)
                            } else {
                                row_bg
                            };
                            if let Some(bg) = bg {
                                ui.painter().rect_filled(r, 0.0, bg);
                            }
                            if modified && is_sel && !editing_here {
                                let width = if view.selected == Some((vr, c)) { 2.0 } else { 1.0 };
                                ui.painter().rect_stroke(r.shrink(0.5), 0.0, Stroke::new(width, p.accent), egui::StrokeKind::Inside);
                            }
                            ui.painter().hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, p.grid));
                            if editing_here && let Some(ed) = inline.as_mut() {
                                if ed.focus && ui.is_enabled() {
                                    ed.focus = false;
                                    ui.memory_mut(|m| m.request_focus(edit_id));
                                    let mut st = egui::text_edit::TextEditState::load(ui.ctx(), edit_id).unwrap_or_default();
                                    let n = ed.buffer.chars().count();
                                    st.cursor.set_char_range(Some(if ed.select_all { CCursorRange::two(CCursor::new(0), CCursor::new(n)) } else { CCursorRange::one(CCursor::new(n)) }));
                                    st.store(ui.ctx(), edit_id);
                                }
                                ui.painter().rect_stroke(r.shrink(0.5), 2.0, Stroke::new(1.5, p.accent), egui::StrokeKind::Inside);
                                let edit_rect = r.shrink2(egui::vec2(5.0, 2.0));
                                let te = ui.put(
                                    edit_rect,
                                    egui::TextEdit::singleline(&mut ed.buffer)
                                        .id(edit_id)
                                        .font(cell_font.clone())
                                        .frame(egui::Frame::NONE)
                                        .margin(egui::vec2(2.0, 2.0))
                                        .background_color(Color32::TRANSPARENT)
                                        .vertical_align(egui::Align::Center)
                                        .desired_width(edit_rect.width()),
                                );
                                ed.changed |= te.changed();
                                if te.lost_focus() && !actions.iter().any(|a| matches!(a, GridAction::Commit | GridAction::Navigate(..) | GridAction::Cancel)) {
                                    actions.push(GridAction::Commit);
                                }
                                return;
                            }
                            let painter = ui.painter_at(r.shrink2(egui::vec2(6.0, 0.0)));
                            let text_rect = match &value {
                                Some(Some(v)) => {
                                    let shown: String =
                                        v.chars().take(300).map(|ch| if ch == '\n' || ch == '\r' { '⏎' } else { ch }).collect();
                                    let (pos, align) = if is_numeric(v) {
                                        (pos2(r.right() - 7.0, r.center().y), egui::Align2::RIGHT_CENTER)
                                    } else {
                                        (pos2(r.left() + 7.0, r.center().y), egui::Align2::LEFT_CENTER)
                                    };
                                    painter.text(pos, align, shown, cell_font.clone(), if deleted { p.muted } else { p.text })
                                }
                                Some(None) => painter.text(
                                    pos2(r.left() + 7.0, r.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "<null>",
                                    cell_font.clone(),
                                    p.disabled,
                                ),
                                None => painter.text(
                                    pos2(r.left() + 7.0, r.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "<default>",
                                    cell_font.clone(),
                                    p.disabled,
                                ),
                            };
                            if deleted {
                                painter.hline(text_rect.x_range(), text_rect.center().y, Stroke::new(1.0, p.error));
                            }
                        });
                        if editing_here {
                            continue;
                        }
                        if resp.drag_started() { actions.push(GridAction::DragStart(vr, c, ui_mods(&resp).command)); }
                        if view.drag_anchor.is_some() && resp.contains_pointer() && resp.ctx.input(|i| i.pointer.primary_down()) { actions.push(GridAction::DragTo(vr, c)); }
                        if resp.clicked() || (resp.secondary_clicked() && !is_sel) {
                            let mods = ui_mods(&resp);
                            actions.push(GridAction::Select { vr, col: Some(c), mods });
                        }
                        if resp.double_clicked() && can_edit {
                            actions.push(GridAction::StartEdit(vr, c));
                        }
                        let resp = if editable && !can_edit && resp.hovered() && edit.is_some_and(|e| !e.state.is_deleted(rref)) {
                            resp.on_hover_text(tr("Read-only cell"))
                        } else {
                            resp
                        };
                        resp.context_menu(|ui| {
                            if editable {
                                if ui.add_enabled(can_edit, egui::Button::new(tr("Edit")).shortcut_text("F2")).clicked() {
                                    actions.push(GridAction::StartEdit(vr, c));
                                    ui.close();
                                }
                                let is_null = matches!(value, Some(None));
                                if ui.add_enabled(can_edit && !is_null, egui::Button::new(tr("Set NULL"))).clicked() {
                                    actions.push(GridAction::SetNull(rref, c));
                                    ui.close();
                                }
                                let revertible = deleted || edit.is_some_and(|e| e.state.is_modified(rref, c));
                                if ui.add_enabled(revertible, egui::Button::new(tr("Revert change"))).clicked() {
                                    actions.push(GridAction::Revert(rref, c));
                                    ui.close();
                                }
                                // Insert-only targets (ClickHouse) can't delete rows already stored.
                                let insert_only = edit.is_some_and(|e| e.target.insert_only) && matches!(rref, RowRef::Existing(_));
                                if ui.add_enabled(!deleted && !insert_only, egui::Button::new(tr("Delete row"))).clicked() {
                                    actions.push(GridAction::DeleteRows);
                                    ui.close();
                                }
                                if ui.add(egui::Button::new(tr("Clone Row")).shortcut_text("⌘D")).clicked() {
                                    actions.push(GridAction::Select { vr, col: Some(c), mods: Modifiers::NONE });
                                    actions.push(GridAction::CloneRow);
                                    ui.close();
                                }
                                ui.separator();
                            }
                            if ui.add(egui::Button::new(tr("Record View")).shortcut_text("⇧↩")).clicked() {
                                actions.push(GridAction::RecordView(vr));
                                ui.close();
                            }
                            if ui.button(tr("Copy value")).clicked() {
                                actions.push(GridAction::Copy(value.clone().flatten().unwrap_or_default()));
                                ui.close();
                            }
                            if ui.button(tr("Copy row (TSV)")).clicked() {
                                actions.push(GridAction::Copy(view.row_tsv(vr)));
                                ui.close();
                            }
                            if ui.button(tr("Copy row with headers")).clicked() {
                                actions.push(GridAction::Copy(format!("{}\n{}", view.rs.columns.join("\t"), view.row_tsv(vr))));
                                ui.close();
                            }
                        });
                    }
                });
            });
        // The inner vertical scroll area consumes scroll targets on both axes.
        // Request horizontal visibility after it has finished.
        if let Some(rect) = scroll_cell { ui.scroll_to_rect(rect, None); }
    });

    if let Some(e) = view.edit.as_mut() {
        e.editing = inline;
    }

    if floating_summary && let Some(e) = row_count_pill(ui, view, area_rect) {
        error = Some(e);
    }
    filter_popup_ui(ui, view);

    for a in actions {
        match a {
            GridAction::OpenFilter(c, anchor) => {
                let search = view.filters.get(&c).map(|f| f.text.clone()).unwrap_or_default();
                view.filter_popup = Some(FilterPopup { col: c, anchor, search, values: distinct_values(&view.rs.rows, c), just_opened: true });
            }
            GridAction::Sort(c) => {
                view.commit_edit();
                view.toggle_sort(c);
                view.selected = None;
                view.selected_rows.clear();
                view.selected_cells.clear();
                view.cell_anchor = None;
                view.anchor = None;
            }
            GridAction::Select { vr, col, mods } => {
                view.commit_edit();
                if let Some(col) = col {
                    view.select_cell(vr, col, mods);
                } else if mods.shift
                    && let Some(a) = view.anchor
                {
                    view.selected_rows = (a.min(vr)..=a.max(vr)).collect();
                } else if mods.command {
                    if !view.selected_rows.remove(&vr) {
                        view.selected_rows.insert(vr);
                    }
                    view.anchor = Some(vr);
                } else {
                    view.selected_rows = BTreeSet::from([vr]);
                    view.anchor = Some(vr);
                }
                if col.is_none() {
                    view.selected = None;
                    view.selected_cells.clear();
                    view.cell_anchor = None;
                }
                ui.ctx().memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            GridAction::SelectColumn(c, additive) => {
                view.select_column(c, additive);
                ui.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            GridAction::DragStart(r, c, additive) => {
                view.commit_edit();
                view.select_cell(
                    r,
                    c,
                    if additive {
                        Modifiers::COMMAND
                    } else {
                        Modifiers::NONE
                    },
                );
                view.drag_anchor = Some((r, c, additive));
                view.cell_anchor = Some((r, c));
            }
            GridAction::DragTo(r, c) => {
                if let Some((ar, ac, additive)) = view.drag_anchor {
                    view.cell_anchor = Some((ar, ac));
                    view.select_cell(
                        r,
                        c,
                        if additive {
                            Modifiers::SHIFT | Modifiers::COMMAND
                        } else {
                            Modifiers::SHIFT
                        },
                    );
                    ui.ctx().request_repaint();
                }
            }
            GridAction::Type(text) => {
                if let Some((r, c)) = view.selected
                    && view.start_edit(r, c)
                {
                    if let Some(ed) = view.edit.as_mut().and_then(|e| e.editing.as_mut()) {
                        ed.buffer = text;
                        ed.select_all = false;
                        ed.changed = true;
                    }
                    ui.ctx().request_repaint();
                }
            }
            GridAction::Paste(text) => {
                if let Err(e) = view.paste_selection(&text) {
                    error = Some(e);
                }
            }
            GridAction::ClearSelection => view.fill_selection(None),
            GridAction::StartEdit(vr, c) => {
                view.commit_edit();
                view.start_edit(vr, c);
            }
            GridAction::Commit => {
                ui.memory_mut(|m| m.surrender_focus(edit_id));
                view.commit_edit();
            }
            GridAction::Navigate(key, mods) => {
                ui.memory_mut(|m| m.surrender_focus(edit_id));
                view.navigate(key, mods, page_rows);
                if editing_now
                    && key == Key::Tab
                    && let Some((r, c)) = view.selected
                {
                    view.start_edit(r, c);
                }
                ui.ctx().request_repaint();
            }
            GridAction::AddRow => {
                view.add_row();
            }
            GridAction::Cancel => {
                ui.memory_mut(|m| m.surrender_focus(edit_id));
                view.cancel_row();
            }
            GridAction::SetNull(row, c) => {
                if !view.selected_cells.is_empty() {
                    view.fill_selection(None);
                } else {
                    view.apply_cell_values(vec![(row, c, None)]);
                }
            }
            GridAction::Revert(row, c) => {
                if let Some(e) = view.edit.as_mut() {
                    e.state.revert_cell(row, c);
                }
            }
            GridAction::DeleteRows => view.delete_selected(),
            GridAction::CloneRow => view.clone_selected_row(),
            GridAction::RecordView(vr) => view.record_view = Some(vr),
            GridAction::Copy(t) => ui.ctx().copy_text(t),
        }
    }
    if ui.input(|i| !i.pointer.primary_down()) {
        view.drag_anchor = None;
    } else if view.drag_anchor.is_some()
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
    {
        let direction = if pos.y > area_rect.bottom() - ROW_H {
            Some(Key::ArrowDown)
        } else if pos.y < area_rect.top() + HEADER_H + ROW_H {
            Some(Key::ArrowUp)
        } else if pos.x > area_rect.right() - 12.0 {
            Some(Key::ArrowRight)
        } else if pos.x < area_rect.left() + index_width + 12.0 {
            Some(Key::ArrowLeft)
        } else {
            None
        };
        if let Some(key) = direction {
            view.navigate(key, Modifiers::SHIFT, page_rows);
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
        }
    }
    record_view_ui(ui, view, columns_info);
    if view.selected_cells.len() > 1 {
        ui.painter().text(
            pos2(area_rect.left() + 8.0, area_rect.bottom() - 10.0),
            egui::Align2::LEFT_BOTTOM,
            trf("{n} cells selected", &[("n", &view.selected_cells.len())]),
            FontId::proportional(11.0),
            p.muted,
        );
    }
    GridResponse {
        error,
        submit: save_on_row_change && view.row_change_needs_save(previous_row),
    }
}

/// Record view: the row as a vertical list of column / value, with previous / next.
fn record_view_ui(ui: &Ui, view: &mut ResultView, columns_info: Option<&[ColumnInfo]>) {
    let Some(vr) = view.record_view else { return };
    let n = view.n_view_rows();
    let Some(row) = view.row_ref(vr) else {
        view.record_view = None;
        return;
    };
    let p = pal(ui);
    let mut open = true;
    let mut go: Option<usize> = None;
    let values: Vec<Cell> = (0..view.rs.columns.len()).map(|c| view.display_value(row, c).flatten()).collect();
    egui::Window::new(tr("Record View"))
        .id(egui::Id::new(("record_view", view.salt)))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(460.0)
        .default_height(380.0)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ui.ctx().content_rect().center())
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                if theme::icon_button(ui, Ico::ChevronLeft, vr > 0, tr("Previous row")).clicked() {
                    go = Some(vr - 1);
                }
                if theme::icon_button(ui, Ico::ChevronRight, vr + 1 < n, tr("Next row")).clicked() {
                    go = Some(vr + 1);
                }
                ui.label(RichText::new(trf("Row {n} of {total}", &[("n", &(vr + 1)), ("total", &n)])).color(p.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(tr("Copy row with headers")).clicked() {
                        let text = view.rs.columns.iter().zip(&values).map(|(c, v)| format!("{c}\t{}", v.clone().unwrap_or_default())).collect::<Vec<_>>().join("\n");
                        ui.ctx().copy_text(text);
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let label_w = 150.0;
                for (c, name) in view.rs.columns.iter().enumerate() {
                    let info = columns_info.and_then(|cols| cols.iter().find(|x| x.name == *name));
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(vec2(label_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.set_width(label_w);
                            ui.add(egui::Label::new(RichText::new(name).strong().color(p.text)).truncate());
                            if let Some(info) = info {
                                ui.label(RichText::new(info.data_type.to_lowercase()).small().color(p.muted));
                            }
                        });
                        let text = match &values[c] {
                            None => RichText::new("<null>").monospace().color(p.muted),
                            Some(v) => RichText::new(v).monospace().color(p.text),
                        };
                        let resp = ui.add(egui::Label::new(text).wrap().sense(egui::Sense::click()));
                        resp.context_menu(|ui| {
                            if ui.button(tr("Copy value")).clicked() {
                                ui.ctx().copy_text(values[c].clone().unwrap_or_default());
                                ui.close();
                            }
                        });
                    });
                    ui.separator();
                }
            });
        });
    if let Some(r) = go {
        view.record_view = Some(r);
        let col = view.selected.map_or(0, |(_, c)| c);
        view.selected = Some((r, col));
        view.selected_rows = BTreeSet::from([r]);
        view.selected_cells = BTreeSet::from([(r, col)]);
        view.scroll_to = Some(r);
    }
    if !open || ui.input(|i| i.key_pressed(Key::Escape)) {
        view.record_view = None;
    }
}

fn ui_mods(resp: &egui::Response) -> Modifiers {
    resp.ctx.input(|i| i.modifiers)
}

/// Edit bar action the app must resolve.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditAction {
    /// ⇧: save the changes directly.
    Submit,
    /// Open the window with the SQL of the changes.
    Preview,
}

/// Data edit buttons (+, −, undo, revert, submit) and "Preview".
pub fn edit_buttons(ui: &mut Ui, view: Option<&mut ResultView>, enabled: bool) -> Option<EditAction> {
    let editable = enabled && view.as_ref().is_some_and(|v| v.edit.is_some());
    let dirty = editable && view.as_ref().is_some_and(|v| v.is_dirty());
    let can_undo = editable && view.as_ref().is_some_and(|v| v.can_undo());
    let has_sel = editable && view.as_ref().is_some_and(|v| v.has_selection());
    let add = theme::icon_button(ui, Ico::Plus, editable, tr("Add row"));
    let del = theme::icon_button(ui, Ico::Minus, has_sel, tr("Delete selected rows"));
    let undo = theme::icon_button(ui, Ico::Undo, can_undo, tr("Undo (⌘Z)"));
    let revert = theme::icon_button(ui, Ico::Revert, dirty, tr("Revert all changes"));
    let submit = theme::icon_button(ui, Ico::Submit, dirty, tr("Submit changes (⌘S)"));
    let pv = theme::flat_button(ui, Some(Ico::Eye), tr("Preview"), FontId::proportional(13.0), false, dirty)
        .on_hover_text(tr("Preview the SQL of the pending changes"));
    if let Some(v) = view {
        if add.clicked() {
            v.add_row();
        }
        if del.clicked() {
            v.delete_selected();
        }
        if undo.clicked() {
            v.undo();
        }
        if revert.clicked() {
            v.revert_all();
        }
    }
    if submit.clicked() {
        Some(EditAction::Submit)
    } else if pv.clicked() {
        Some(EditAction::Preview)
    } else {
        None
    }
}

/// Demo mode (`DATACRAFT_DEMO_FILTER=<column>`): opens that column's funnel and unchecks
/// its first value, to capture the filter without interacting with the app.
/// `<column>=<text>` types that text in the search box instead.
fn demo_filter(view: &mut ResultView, area: Rect) {
    if view.filters.is_empty()
        && view.filter_popup.is_none()
        && let Ok(spec) = std::env::var("DATACRAFT_DEMO_FILTER")
        && let (col_name, text) = spec.split_once('=').unwrap_or((spec.as_str(), ""))
        && let Some(c) = view.rs.columns.iter().position(|n| n == col_name)
    {
        let values = distinct_values(&view.rs.rows, c);
        let filter = if text.is_empty() {
            let mut allowed: HashSet<Cell> = values.iter().map(|(v, _)| v.clone()).collect();
            if let Some((first, _)) = values.first() {
                allowed.remove(first);
            }
            ColumnFilter { allowed: Some(allowed), text: String::new() }
        } else {
            ColumnFilter { allowed: None, text: text.to_string() }
        };
        view.set_filter(c, Some(filter));
        let anchor = area.left_top() + egui::vec2(120.0 * c as f32 + 60.0, 30.0);
        view.filter_popup = Some(FilterPopup { col: c, anchor, search: text.to_string(), values, just_opened: true });
    }
}

/// Demo mode (`DATACRAFT_DEMO_GRID=record|clone`): opens the record view of the first row,
/// or clones it (in memory, never saved), once per run.
fn demo_grid(view: &mut ResultView) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Ok(mode) = std::env::var("DATACRAFT_DEMO_GRID") else { return };
    if view.rs.rows.is_empty() || (mode == "clone" && view.edit.is_none()) || DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    view.selected = Some((0, 1.min(view.rs.columns.len().saturating_sub(1))));
    match mode.as_str() {
        "record" => view.record_view = Some(0),
        "clone" => view.clone_selected_row(),
        _ => {}
    }
}

/// Funnel popup: search, check values, all/none and remove filter.
fn filter_popup_ui(ui: &Ui, view: &mut ResultView) {
    let Some(popup) = view.filter_popup.as_mut() else { return };
    let p = pal(ui);
    let col = popup.col;
    let current = view.filters.get(&col).cloned();
    let mut new_filter: Option<Option<ColumnFilter>> = None;
    let mut close = ui.input(|i| i.key_pressed(Key::Escape));
    let col_name = view.rs.columns.get(col).cloned().unwrap_or_default();
    let all_values = || popup.values.iter().map(|(v, _)| v.clone()).collect::<HashSet<Cell>>();
    let area = egui::Area::new(egui::Id::new(("col_filter", view.salt)))
        .order(egui::Order::Foreground)
        .fixed_pos(popup.anchor + egui::vec2(0.0, 4.0))
        .constrain(true)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(260.0);
                ui.label(RichText::new(trf("Filter “{column}”", &[("column", &col_name)])).font(theme::bold(13.0)).color(p.text));
                ui.add_space(4.0);
                let search = ui.add(
                    egui::TextEdit::singleline(&mut popup.search).hint_text(tr("Search value…")).desired_width(f32::INFINITY),
                );
                if popup.just_opened || ui.memory(|m| m.focused().is_none()) {
                    search.request_focus();
                }
                // The typed text filters the grid as well as the list.
                if search.changed() {
                    let mut f = current.clone().unwrap_or_default();
                    f.text = popup.search.trim().to_string();
                    new_filter = Some(Some(f));
                }
                let needle = popup.search.trim().to_lowercase();
                let shown: Vec<usize> = popup
                    .values
                    .iter()
                    .enumerate()
                    .filter(|(_, (v, _))| needle.is_empty() || matches_text(v, &needle))
                    .map(|(i, _)| i)
                    .collect();
                let base = new_filter.clone().flatten().or_else(|| current.clone()).unwrap_or_default();
                ui.horizontal(|ui| {
                    if ui.small_button(tr("All")).clicked() {
                        let mut f = base.clone();
                        if let Some(a) = f.allowed.as_mut() {
                            a.extend(shown.iter().map(|&i| popup.values[i].0.clone()));
                        }
                        new_filter = Some(Some(f));
                    }
                    if ui.small_button(tr("None")).clicked() {
                        let mut f = base.clone();
                        let mut allowed = f.allowed.take().unwrap_or_else(all_values);
                        for &i in &shown {
                            allowed.remove(&popup.values[i].0);
                        }
                        f.allowed = Some(allowed);
                        new_filter = Some(Some(f));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(current.is_some(), egui::Button::new(tr("Remove filter")).small()).clicked() {
                            new_filter = Some(None);
                            close = true;
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show_rows(
                    ui,
                    20.0,
                    shown.len(),
                    |ui, range| {
                        for &i in &shown[range] {
                            let (value, count) = &popup.values[i];
                            let mut on = base.allowed.as_ref().is_none_or(|a| a.contains(value));
                            let text = RichText::new(value_label(value)).monospace().color(if value.is_none() { p.muted } else { p.text });
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut on, text).changed() {
                                    let mut f = base.clone();
                                    let mut allowed = f.allowed.take().unwrap_or_else(all_values);
                                    if on {
                                        allowed.insert(value.clone());
                                    } else {
                                        allowed.remove(value);
                                    }
                                    f.allowed = Some(allowed);
                                    new_filter = Some(Some(f));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(count.to_string()).small().color(p.muted));
                                });
                            });
                        }
                    },
                );
                if shown.is_empty() {
                    ui.label(RichText::new(tr("No matches")).color(p.muted));
                }
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    close = true;
                }
            });
        });
    let just_opened = std::mem::take(&mut popup.just_opened);
    if area.response.clicked_elsewhere() && !just_opened {
        close = true;
    }
    if let Some(f) = new_filter {
        // A filter that checks every value and has no text is redundant.
        let total = all_values_of(view);
        let f = f.map(|mut f| {
            if f.allowed.as_ref().is_some_and(|a| total.iter().all(|v| a.contains(v))) {
                f.allowed = None;
            }
            f
        });
        view.set_filter(col, f.filter(|f| !f.is_empty()));
    }
    if close {
        view.filter_popup = None;
    }
}

fn all_values_of(view: &ResultView) -> Vec<Cell> {
    view.filter_popup.as_ref().map(|p| p.values.iter().map(|(v, _)| v.clone()).collect()).unwrap_or_default()
}

/// Floating pill centred at the bottom of the grid: "N rows ⌄  ⋮".
fn row_count_pill(ui: &Ui, view: &ResultView, area: Rect) -> Option<String> {
    let p = pal(ui);
    let n = view.rs.rows.len();
    let plus = if view.rs.truncated { "+" } else { "" };
    let text = match (view.filtered_out(), n) {
        (0, 1) => tr("1 row").to_string(),
        (0, n) => trf("{n}{plus} rows", &[("n", &n), ("plus", &plus)]),
        (_, n) => trf("{shown} of {n}{plus} rows", &[("shown", &view.order.len()), ("n", &n), ("plus", &plus)]),
    };
    let font = FontId::proportional(12.5);
    let tw = ui.fonts_mut(|f| f.layout_no_wrap(text.clone(), font.clone(), p.text).size().x);
    let size = egui::vec2(10.0 + tw + 18.0 + 9.0 + 24.0 + 4.0, 28.0);
    let pos = pos2(area.center().x - size.x / 2.0, area.bottom() - size.y - 12.0);
    let mut error = None;
    egui::Area::new(egui::Id::new(("rows_pill", view.salt)))
        .order(egui::Order::Middle)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            ui.painter().add(
                egui::Shadow { offset: [0, 2], blur: 8, spread: 0, color: egui::Color32::from_black_alpha(if p.dark { 60 } else { 18 }) }
                    .as_shape(rect, 8.0),
            );
            ui.painter().rect(rect, 8.0, p.island_bg, Stroke::new(1.0, p.control_border), egui::StrokeKind::Inside);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(2.0, 1.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
            child.spacing_mut().item_spacing.x = 0.0;
            let rows_btn = theme::flat_button(&mut child, None, &text, font.clone(), true, true)
                .on_hover_text(trf("Query executed in {time}", &[("time", &format_duration(view.rs.elapsed))]));
            theme::toolbar_separator(&mut child);
            let more = theme::icon_button_sized(&mut child, Ico::MoreVert, true, "", 24.0);
            for resp in [rows_btn, more] {
                egui::Popup::menu(&resp).show(|ui| {
                    if ui.button(tr("Export to CSV…")).clicked() {
                        if let Err(e) = export_csv(view) {
                            error = Some(trf("Export failed: {error}", &[("error", &e)]));
                        }
                        ui.close();
                    }
                    if ui.button(tr("Copy all (CSV)")).clicked() {
                        ui.ctx().copy_text(to_csv(view));
                        ui.close();
                    }
                });
            }
        });
    error
}

/// Execution log.
pub fn show_output(ui: &mut Ui, log: &[OutputEntry]) {
    let pal = pal(ui);
    egui::ScrollArea::both()
        .id_salt("output_log")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if log.is_empty() {
                ui.label(RichText::new(tr("No executions yet.")).color(pal.muted));
            }
            for e in log {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("[{}]", e.time)).monospace().color(pal.muted));
                    if !e.statement.is_empty() {
                        ui.label(RichText::new(statement_title_long(&e.statement)).monospace());
                    }
                });
                let color: Color32 = if e.is_error { pal.error } else { pal.ok };
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.add(egui::Label::new(RichText::new(&e.message).monospace().color(color)).wrap());
                });
            }
        });
}

fn statement_title_long(sql: &str) -> String {
    truncate_chars(&sql.split_whitespace().collect::<Vec<_>>().join(" "), 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_escaping() {
        assert_eq!(csv_escape("abc"), "abc");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("x\ny"), "\"x\ny\"");
    }

    #[test]
    fn numeric_sort() {
        let a: Cell = Some("10".into());
        let b: Cell = Some("9".into());
        assert_eq!(compare_cells(Some(&a), Some(&b)), Ordering::Greater);
        let n: Cell = None;
        assert_eq!(compare_cells(Some(&n), Some(&b)), Ordering::Less);
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn rows() -> Vec<Vec<Cell>> {
        let c = |v: Option<&str>| v.map(str::to_string);
        vec![
            vec![c(Some("1")), c(Some("b"))],
            vec![c(Some("10")), c(None)],
            vec![c(Some("2")), c(Some("a"))],
            vec![c(Some("2")), c(Some("b"))],
        ]
    }

    #[test]
    fn distinct_values_with_count_and_numeric_order() {
        let d = distinct_values(&rows(), 0);
        let labels: Vec<_> = d.iter().map(|(v, n)| (v.clone().unwrap_or_default(), *n)).collect();
        assert_eq!(labels, [("1".into(), 1), ("2".into(), 2), ("10".into(), 1)]);
        assert_eq!(distinct_values(&rows(), 1).len(), 3);
    }

    #[test]
    fn combined_filter_and_sort() {
        let mut filters = BTreeMap::new();
        filters.insert(1, ColumnFilter { allowed: Some(HashSet::from([Some("b".to_string()), None])), text: String::new() });
        assert_eq!(compute_order(&rows(), None, &filters), vec![0, 1, 3]);
        assert_eq!(compute_order(&rows(), Some((0, SortDir::Desc)), &filters), vec![1, 3, 0]);
        assert_eq!(compute_order(&rows(), None, &BTreeMap::new()), vec![0, 1, 2, 3]);
    }

    #[test]
    fn text_filter_hides_rows() {
        let mut filters = BTreeMap::new();
        filters.insert(1, ColumnFilter { allowed: None, text: "B".into() });
        assert_eq!(compute_order(&rows(), None, &filters), vec![0, 3]);
        filters.insert(1, ColumnFilter { allowed: None, text: "null".into() });
        assert_eq!(compute_order(&rows(), None, &filters), vec![1]);
        // Text and checked values combine.
        filters.insert(0, ColumnFilter { allowed: Some(HashSet::from([Some("2".to_string())])), text: String::new() });
        filters.insert(1, ColumnFilter { allowed: None, text: "b".into() });
        assert_eq!(compute_order(&rows(), None, &filters), vec![3]);
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    fn wide_table_scenario(render: bool) {
        let mut view = ResultView::new(ResultSet {
            columns: (0..30).map(|c| format!("column_{c:02}")).collect(),
            rows: (0..200).map(|r| (0..30).map(|c| Some(format!("value_{r}_{c}"))).collect()).collect(),
            ..Default::default()
        }, 991);
        view.select_cell(0, 0, Modifiers::NONE);
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(900.0, 460.0)).build_ui_state(
            move |ui, state: &mut (ResultView, f32)| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), true);
                    return;
                }
                let id = ui.make_persistent_id(egui::IdSalt::new(("result_h", state.0.salt)));
                show_result(ui, &mut state.0, false, None, false, true);
                state.1 = egui::scroll_area::State::load(ui.ctx(), id).expect("horizontal scroll state").offset.x;
            }, (view, 0.0));
        h.run_steps(3);
        h.key_press_modifiers(Modifiers::COMMAND, Key::ArrowRight);
        h.run_steps(20);
        assert_eq!(h.state().0.selected, Some((0, 29)));
        assert!(h.state().1 > 1000.0, "cursor must reveal the last column: {}", h.state().1);
        if render {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/navigation-snapshots");
            std::fs::create_dir_all(&dir).unwrap();
            h.render().unwrap().save(dir.join("wide-table.png")).unwrap();
        }
        h.key_press_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
        h.run_steps(20);
        assert_eq!(h.state().0.selected, Some((0, 0)));
        assert!(h.state().1 < 60.0, "cursor must reveal the first column, offset {}", h.state().1);
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run_steps(2);
        h.input_mut().events.push(egui::Event::Text("value_199_29".into()));
        h.run_steps(20);
        assert_eq!(h.state().0.selected, Some((199, 29)));
        assert!(h.state().1 > 1000.0, "find must also reveal the matched column");
        if render {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/navigation-snapshots");
            h.render().unwrap().save(dir.join("table-find.png")).unwrap();
        }
    }

    #[test]
    fn horizontal_keyboard_navigation_reveals_the_selected_column() { wide_table_scenario(false); }

    #[test]
    #[ignore = "Requires a GPU; renders horizontal navigation on test data"]
    fn render_wide_table() { wide_table_scenario(true); }

    #[test]
    fn ctrl_f_finds_loaded_values_and_wraps_without_changing_data() {
        let mut h = harness(false, false);
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run_steps(2);
        assert!(h.state().view.find_open);
        h.input_mut().events.push(egui::Event::Text("C".into()));
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((2, 1)));
        assert!(!h.state().view.is_dirty());
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((2, 1)));
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(!h.state().view.find_open);
    }

    #[test]
    fn finding_outside_an_edited_batch_respects_opt_in_auto_save() {
        let mut h = harness(true, true);
        h.input_mut().events.push(egui::Event::Text("edited".into()));
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert!(h.state().view.is_dirty());
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run_steps(2);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().view.is_dirty(), "closing find must not discard pending row changes");
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run_steps(2);
        h.input_mut().events.push(egui::Event::Text("c".into()));
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((2, 1)));
        assert_eq!(h.state().submits, 1);
    }

    fn view() -> ResultView {
        let mut view = ResultView::new(
            ResultSet {
                columns: vec!["id".into(), "note".into()],
                rows: vec![
                    vec![Some("1".into()), Some("a".into())],
                    vec![Some("2".into()), None],
                    vec![Some("3".into()), Some("c".into())],
                ],
                ..Default::default()
            },
            42,
        );
        view.set_edit_target(Ok(EditTarget {
            schema: "main".into(),
            table: "notes".into(),
            col_names: view.rs.columns.clone(),
            key_cols: vec![0],
            binary_cols: vec![false, false],
            has_pk: true,
            insert_only: false,
        }));
        view.selected = Some((0, 1));
        view.selected_rows = BTreeSet::from([0]);
        view
    }

    struct State {
        view: ResultView,
        auto_save: bool,
        submits: usize,
        enabled: bool,
    }

    fn harness(auto_save: bool, editing: bool) -> egui_kittest::Harness<'static, State> {
        let mut view = view();
        if editing {
            view.begin_edit(0, 1);
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(900.0, 500.0))
            .build_ui_state(
                move |ui, state: &mut State| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), false);
                        return;
                    }
                    let response = ui
                        .add_enabled_ui(state.enabled, |ui| {
                            show_result(ui, &mut state.view, false, None, state.auto_save, true)
                        })
                        .inner;
                    assert!(response.error.is_none());
                    state.submits += usize::from(response.submit);
                },
                State {
                    view,
                    auto_save,
                    submits: 0,
                    enabled: true,
                },
            );
        h.run_steps(3);
        h
    }

    #[test]
    fn down_from_inline_editor_commits_and_requests_opt_in_save_once() {
        for auto_save in [false, true] {
            let mut h = harness(auto_save, true);
            h.input_mut()
                .events
                .push(egui::Event::Text("edited".into()));
            h.run_steps(2);
            h.key_press(Key::ArrowDown);
            h.run_steps(3);
            let state = h.state();
            assert_eq!(state.view.selected, Some((1, 1)));
            assert_eq!(
                state.view.edit.as_ref().unwrap().state.changes.get(&(0, 1)),
                Some(&Some("edited".into()))
            );
            assert!(state.view.edit.as_ref().unwrap().editing.is_none());
            assert_eq!(state.submits, usize::from(auto_save));
        }
    }

    fn batch_view() -> ResultView {
        let mut v = ResultView::new(
            ResultSet {
                columns: (0..7).map(|c| format!("field_{c}")).collect(),
                rows: (0..20)
                    .map(|r| (0..7).map(|c| Some(format!("value_{r}_{c}"))).collect())
                    .collect(),
                ..Default::default()
            },
            43,
        );
        v.rs.rows[4][2] = Some("0".into());
        v.set_edit_target(Ok(EditTarget {
            schema: "main".into(),
            table: "sample".into(),
            col_names: v.rs.columns.clone(),
            key_cols: vec![0],
            binary_cols: vec![false; 7],
            has_pk: true,
            insert_only: false,
        }));
        v.select_cell(0, 2, Modifiers::NONE);
        v.select_cell(9, 2, Modifiers::SHIFT);
        v
    }

    #[test]
    fn bulk_edit_stays_put_and_saves_only_after_leaving_its_rows() {
        let mut h = harness(true, false);
        h.state_mut().view = batch_view();
        h.input_mut().events.push(egui::Event::Text("0".into()));
        h.run_steps(3);
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert_eq!(
            h.state().view.selected,
            Some((9, 2)),
            "Enter must not advance out of the edited block"
        );
        assert_eq!(h.state().submits, 0);
        assert_eq!(h.state().view.edit.as_ref().unwrap().state.changes.len(), 9);
        for key in [
            Key::ArrowRight,
            Key::ArrowLeft,
            Key::ArrowUp,
            Key::ArrowDown,
        ] {
            h.key_press(key);
            h.run_steps(2);
            assert_eq!(
                h.state().submits,
                0,
                "navigation inside the edited rows must not submit"
            );
        }
        for _ in 0..5 {
            h.key_press(Key::ArrowUp);
            h.run_steps(2);
        }
        assert_eq!(h.state().view.selected, Some((4, 2)));
        assert_eq!(
            h.state().submits,
            0,
            "an unchanged value inside the selected block is still part of that block"
        );
        for _ in 0..6 {
            h.key_press(Key::ArrowDown);
            h.run_steps(2);
        }
        assert_eq!(h.state().view.selected, Some((10, 2)));
        assert_eq!(h.state().submits, 1);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(
            h.state().submits,
            1,
            "a failed submission must not retry on every outside row"
        );
        for _ in 0..2 {
            h.key_press(Key::ArrowUp);
            h.run_steps(2);
        }
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(
            h.state().submits,
            2,
            "returning to the batch and leaving it allows retry"
        );
    }

    #[test]
    fn editor_outside_the_selection_only_edits_its_own_cell() {
        // The record view moved the cursor to row 5 while row 3 was still the selected cell.
        let mut v = batch_view();
        v.selected_cells = BTreeSet::from([(3, 2)]);
        assert!(v.start_edit(5, 2));
        let ed = v.edit.as_mut().unwrap().editing.as_mut().unwrap();
        ed.buffer = "typed".into();
        ed.changed = true;
        v.commit_edit();
        let changes = &v.edit.as_ref().unwrap().state.changes;
        assert_eq!(changes.get(&(5, 2)), Some(&Some("typed".into())));
        assert!(!changes.contains_key(&(3, 2)), "the stale selection must not get the value");
    }

    #[test]
    fn reverting_a_new_row_shifts_the_later_ones() {
        let mut v = batch_view();
        for _ in 0..3 {
            v.add_row();
            v.commit_edit();
        }
        let first_new = v.order.len();
        v.edit_rows.extend((0..3).map(RowRef::New));
        v.selected = Some((first_new, 1));
        v.selected_cells = BTreeSet::from([(first_new, 1), (first_new + 2, 1)]);
        v.cancel_row();
        assert_eq!(v.edit.as_ref().unwrap().state.inserted.len(), 2);
        assert!(v.edit_rows.iter().all(|r| !matches!(r, RowRef::New(i) if *i >= 2)));
        assert!(v.edit_rows.contains(&RowRef::New(1)), "the old third new row is now the second");
        assert_eq!(v.selected_cells, BTreeSet::from([(first_new + 1, 1)]));
    }

    #[test]
    fn undo_preserves_the_remaining_batch_until_save() {
        let mut v = batch_view();
        v.fill_selection(Some("0".into()));
        v.select_cell(9, 3, Modifiers::NONE);
        v.fill_selection(Some("another".into()));
        v.undo();
        assert!(v.row_in_edit_batch(RowRef::Existing(4)));
        assert_eq!(v.edit.as_ref().unwrap().state.changes.len(), 9);
        v.mark_saved();
        assert!(v.edit_rows.is_empty());
        assert!(!v.is_dirty());
    }

    #[test]
    fn clicking_outside_after_sort_clears_selection_still_saves_the_batch() {
        let mut v = batch_view();
        v.fill_selection(Some("0".into()));
        v.toggle_sort(0);
        v.selected = None;
        v.selected_cells.clear();
        v.selected_rows.clear();
        let outside = v.order.iter().position(|&r| r == 10).unwrap();
        v.select_cell(outside, 2, Modifiers::NONE);
        assert!(v.row_change_needs_save(None));
    }

    fn batch_console_scenario(render: bool) {
        use crate::ui::console::{Console, ConsoleAction, ConsoleCtx};
        struct ConsoleState {
            console: Console,
            db: rusqlite::Connection,
            submits: usize,
        }
        let v = batch_view();
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE sample(field_0 TEXT PRIMARY KEY, field_1 TEXT, field_2 TEXT, field_3 TEXT, field_4 TEXT, field_5 TEXT, field_6 TEXT)").unwrap();
        for row in &v.rs.rows {
            db.execute(
                "INSERT INTO sample VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params_from_iter(row.iter()),
            )
            .unwrap();
        }
        let mut console = Console::new_table(1, 1, None, "main".into(), "sample".into());
        console.table.as_mut().unwrap().save_on_row_change = true;
        console.results.push(v);
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1280.0, 760.0))
            .build_ui_state(
                move |ui, state: &mut ConsoleState| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), true);
                        return;
                    }
                    let cx = ConsoleCtx {
                        connections: &[],
                        schemas: None,
                        driver: crate::db::Driver::Sqlite,
                        dark: true,
                        font_size: 14.0,
                        columns: None,
                    };
                    for action in state.console.show_table(ui, &cx) {
                        if let ConsoleAction::Submit(i) = action {
                            state.submits += 1;
                            let view = &mut state.console.results[i];
                            view.finish_inline_edit();
                            let edit = view.edit.as_ref().unwrap();
                            let target = edit.target.clone();
                            let changes = edit.state.build_changes(&view.rs, &target).unwrap();
                            let transaction = state.db.transaction().unwrap();
                            for change in changes {
                                let sql = crate::edit::change_sql(
                                    crate::db::Driver::Sqlite,
                                    &target.schema,
                                    &target.table,
                                    &change,
                                );
                                assert_eq!(transaction.execute(&sql, []).unwrap(), 1);
                            }
                            transaction.commit().unwrap();
                            let mut statement = state
                                .db
                                .prepare("SELECT * FROM sample ORDER BY rowid")
                                .unwrap();
                            let rows = statement
                                .query_map([], |row| {
                                    (0..7)
                                        .map(|c| row.get::<_, Option<String>>(c))
                                        .collect::<rusqlite::Result<Vec<_>>>()
                                })
                                .unwrap()
                                .collect::<rusqlite::Result<Vec<_>>>()
                                .unwrap();
                            let mut fresh = view.rs.clone();
                            fresh.rows = rows;
                            view.mark_saved();
                            view.refresh(fresh);
                            view.set_edit_target(Ok(target));
                        } else if let ConsoleAction::Error(error) = action {
                            panic!("{error}");
                        }
                    }
                },
                ConsoleState {
                    console,
                    db,
                    submits: 0,
                },
            );
        h.run_steps(3);
        h.input_mut().events.push(egui::Event::Text("0".into()));
        h.run_steps(3);
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert_eq!(h.state().submits, 0);
        assert_eq!(h.state().console.results[0].selected, Some((9, 2)));
        assert_eq!(
            h.state().console.results[0]
                .edit
                .as_ref()
                .unwrap()
                .state
                .changes
                .len(),
            9
        );
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/edit-range-snapshots");
        if render {
            std::fs::create_dir_all(&directory).unwrap();
            h.render()
                .unwrap()
                .save(directory.join("pending.png"))
                .unwrap();
        }
        for key in [Key::ArrowRight, Key::ArrowLeft] {
            h.key_press(key);
            h.run_steps(2);
        }
        assert_eq!(h.state().submits, 0);
        h.key_press(Key::ArrowDown);
        h.run_steps(3);
        assert_eq!(h.state().submits, 1);
        assert_eq!(h.state().console.results[0].selected, Some((10, 2)));
        assert!(!h.state().console.results[0].is_dirty());
        let count: i64 = h
            .state()
            .db
            .query_row(
                "SELECT COUNT(*) FROM sample WHERE field_2 = '0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 10);
        if render {
            h.render()
                .unwrap()
                .save(directory.join("saved.png"))
                .unwrap();
        }
    }

    #[test]
    fn leaving_the_batch_submits_the_sqlite_transaction() {
        batch_console_scenario(false);
    }

    #[test]
    #[ignore = "Requires a GPU; renders the pending batch and its saved result on test data"]
    fn render_edit_batch() {
        batch_console_scenario(true);
    }

    #[test]
    fn navigation_accepts_a_stale_inline_focus_id() {
        let mut h = harness(false, false);
        let id = egui::Id::new(("cell_edit", h.state().view.salt));
        h.ctx.memory_mut(|m| m.request_focus(id));
        h.key_press(Key::ArrowDown);
        h.run_steps(1);
        assert_eq!(h.state().view.selected, Some((1, 1)));
    }

    #[test]
    fn text_and_down_are_handled_before_the_editor_has_its_first_focus_frame() {
        let mut h = harness(true, false);
        h.state_mut().view.begin_edit(0, 1);
        h.input_mut().events.push(egui::Event::Text("quick".into()));
        h.key_press(Key::ArrowDown);
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((1, 1)));
        assert_eq!(
            h.state()
                .view
                .edit
                .as_ref()
                .unwrap()
                .state
                .changes
                .get(&(0, 1)),
            Some(&Some("quick".into()))
        );
        assert_eq!(h.state().submits, 1);
    }

    #[test]
    fn tab_wraps_and_shift_tab_returns_without_changing_null() {
        let mut h = harness(true, true);
        h.key_press(Key::Tab);
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((1, 0)));
        assert!(!h.state().view.is_dirty());
        h.key_press_modifiers(Modifiers::SHIFT, Key::Tab);
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((0, 1)));
        assert_eq!(h.state().submits, 0);
        assert_eq!(h.state().view.rs.rows[1][1], None);
    }

    #[test]
    fn navigation_obeys_visual_order_bounds_and_page_size() {
        let mut v = view();
        v.toggle_sort(0);
        v.toggle_sort(0);
        assert_eq!(v.selected_row(), Some(RowRef::Existing(2)));
        v.navigate(Key::ArrowDown, Modifiers::NONE, 1);
        assert_eq!(v.selected_row(), Some(RowRef::Existing(1)));
        v.navigate(Key::PageDown, Modifiers::NONE, 1);
        assert_eq!(v.selected, Some((2, 1)));
        v.navigate(Key::ArrowDown, Modifiers::NONE, 1);
        assert_eq!(v.selected, Some((2, 1)));
        v.navigate(Key::Home, Modifiers::COMMAND, 1);
        assert_eq!(v.selected, Some((0, 1)));
        v.navigate(Key::ArrowLeft, Modifiers::COMMAND, 1);
        assert_eq!(v.selected, Some((0, 0)));
        v.navigate(Key::Tab, Modifiers::SHIFT, 1);
        assert_eq!(v.selected, Some((0, 0)));
        v.set_filter(
            0,
            Some(ColumnFilter {
                allowed: None,
                text: "2".into(),
            }),
        );
        v.selected = Some((0, 0));
        v.navigate(Key::End, Modifiers::COMMAND, 1);
        assert_eq!(v.selected_row(), Some(RowRef::Existing(1)));
    }

    #[test]
    fn same_row_clean_rows_and_incomplete_new_rows_do_not_auto_save() {
        let mut v = view();
        let previous = v.selected_row();
        v.navigate(Key::ArrowDown, Modifiers::NONE, 10);
        assert!(!v.row_change_needs_save(previous));
        v.edit.as_mut().unwrap().state.set_value(
            &v.rs,
            RowRef::Existing(1),
            1,
            Some("changed".into()),
        );
        let previous = v.selected_row();
        v.navigate(Key::ArrowLeft, Modifiers::NONE, 10);
        assert!(!v.row_change_needs_save(previous));
        v.add_row();
        assert!(!v.row_change_needs_save(previous));
        v.navigate(Key::ArrowUp, Modifiers::NONE, 10);
        assert!(v.row_change_needs_save(Some(RowRef::New(0))));
    }

    #[test]
    fn escape_discards_only_the_current_record() {
        let mut v = view();
        let e = v.edit.as_mut().unwrap();
        e.state
            .set_value(&v.rs, RowRef::Existing(0), 1, Some("first".into()));
        e.state
            .set_value(&v.rs, RowRef::Existing(1), 1, Some("second".into()));
        v.begin_edit(0, 1);
        v.cancel_row();
        let changes = &v.edit.as_ref().unwrap().state.changes;
        assert!(!changes.contains_key(&(0, 1)));
        assert_eq!(changes.get(&(1, 1)), Some(&Some("second".into())));
        v.add_row();
        v.cancel_row();
        assert!(v.edit.as_ref().unwrap().state.inserted.is_empty());
    }

    #[test]
    fn refresh_preserves_sort_filter_column_width_id_and_selected_primary_key() {
        let mut v = view();
        v.toggle_sort(0);
        v.toggle_sort(0);
        v.set_filter(
            0,
            Some(ColumnFilter {
                allowed: None,
                text: "2".into(),
            }),
        );
        v.selected = Some((0, 1));
        let mut fresh = v.rs.clone();
        fresh.rows.rotate_left(1);
        v.refresh(fresh);
        assert_eq!(v.salt, 42);
        assert!(v.sort == Some((0, SortDir::Desc)));
        assert_eq!(v.filters.len(), 1);
        assert_eq!(v.selected_row(), Some(RowRef::Existing(0)));
        assert_eq!(v.selected, Some((0, 1)));
        assert!(!v.is_dirty());
        assert!(!v.edit_checked);
    }

    #[test]
    fn refresh_follows_a_saved_primary_key_change() {
        let mut v = view();
        v.select_cell(1, 1, Modifiers::NONE);
        v.edit
            .as_mut()
            .unwrap()
            .state
            .set_value(&v.rs, RowRef::Existing(1), 0, Some("20".into()));
        v.mark_saved();
        let mut fresh = v.rs.clone();
        fresh.rows.rotate_left(1);
        v.refresh(fresh);
        assert_eq!(v.selected_row(), Some(RowRef::Existing(0)));
        assert_eq!(v.rs.rows[0][0].as_deref(), Some("20"));
    }

    #[test]
    fn busy_grid_ignores_navigation_and_editing_shortcuts() {
        let mut h = harness(true, false);
        h.state_mut().enabled = false;
        h.key_press(Key::ArrowDown);
        h.key_press(Key::Insert);
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((0, 1)));
        assert!(!h.state().view.is_dirty());
        assert_eq!(h.state().submits, 0);
    }

    #[test]
    fn shift_arrow_rectangle_fills_as_one_undoable_edit() {
        let mut h = harness(false, false);
        h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowLeft);
        h.run_steps(2);
        h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(h.state().view.selected_cells.len(), 4);
        h.input_mut().events.push(egui::Event::Text("0".into()));
        h.run_steps(3);
        h.key_press(Key::Enter);
        h.run_steps(3);
        let edits = &h.state().view.edit.as_ref().unwrap().state;
        assert_eq!(edits.changes.len(), 4);
        assert!(edits.changes.values().all(|v| v.as_deref() == Some("0")));
        h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        h.run_steps(2);
        assert!(!h.state().view.is_dirty());
    }

    #[test]
    fn whole_column_fill_respects_filter_and_does_not_sort() {
        let mut v = view();
        v.set_filter(
            0,
            Some(ColumnFilter {
                allowed: None,
                text: "2".into(),
            }),
        );
        v.select_column(1, false);
        assert!(v.sort.is_none());
        assert_eq!(v.selected_cells.len(), 1);
        v.start_edit(0, 1);
        let ed = v.edit.as_mut().unwrap().editing.as_mut().unwrap();
        ed.buffer = "0".into();
        ed.changed = true;
        v.finish_inline_edit();
        assert_eq!(
            v.edit
                .as_ref()
                .unwrap()
                .state
                .changes
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![(1, 1)]
        );
    }

    #[test]
    fn ctrl_click_and_shift_click_produce_disjoint_and_rectangular_selections() {
        let mut v = view();
        v.select_cell(0, 1, Modifiers::NONE);
        v.select_cell(2, 1, Modifiers::COMMAND);
        assert_eq!(v.selected_cells.len(), 2);
        assert_eq!(v.selection_tsv(), "a\nc");
        v.select_cell(0, 0, Modifiers::NONE);
        v.select_cell(2, 1, Modifiers::SHIFT);
        assert_eq!(v.selected_cells.len(), 6);
        assert_eq!(v.selection_tsv(), "1\ta\n2\t\n3\tc");
    }

    #[test]
    fn drag_selects_a_cell_rectangle() {
        let mut h = harness(false, false);
        h.hover_at(pos2(60.0, 50.0));
        h.drag_at(pos2(60.0, 50.0));
        h.run_steps(2);
        h.hover_at(pos2(75.0, 50.0));
        h.run_steps(2);
        h.hover_at(pos2(160.0, 98.0));
        h.run_steps(2);
        h.drop_at(pos2(160.0, 98.0));
        h.run_steps(2);
        assert_eq!(h.state().view.selected_cells.len(), 6);
        assert!(h.state().view.drag_anchor.is_none());
    }

    #[test]
    fn header_name_selects_column_and_only_sort_icon_sorts() {
        let mut h = harness(false, false);
        h.hover_at(pos2(160.0, 20.0));
        h.drag_at(pos2(160.0, 20.0));
        h.drop_at(pos2(160.0, 20.0));
        h.run_steps(3);
        assert!(h.state().view.sort.is_none());
        assert_eq!(h.state().view.selected_cells.len(), 3);
        assert!(h.state().view.selected_cells.iter().all(|(_, c)| *c == 1));
        h.input_mut().events.push(egui::Event::Text("0".into()));
        h.run_steps(3);
        h.key_press(Key::Enter);
        h.run_steps(3);
        assert_eq!(h.state().view.edit.as_ref().unwrap().state.changes.len(), 3);
        h.hover_at(pos2(215.0, 20.0));
        h.drag_at(pos2(215.0, 20.0));
        h.drop_at(pos2(215.0, 20.0));
        h.run_steps(3);
        assert!(h.state().view.sort == Some((1, SortDir::Asc)));
    }

    #[test]
    fn paste_fills_selection_or_matrix_atomically_and_rejects_overflow() {
        let mut v = view();
        v.select_column(1, false);
        v.paste_selection("0").unwrap();
        assert_eq!(v.edit.as_ref().unwrap().state.changes.len(), 3);
        v.undo();
        assert!(!v.is_dirty());
        v.select_cell(0, 0, Modifiers::NONE);
        v.paste_selection("10\tfirst\r\n20\tsecond").unwrap();
        assert_eq!(v.edit.as_ref().unwrap().state.changes.len(), 4);
        v.undo();
        assert!(!v.is_dirty());
        assert!(v.paste_selection("1\t2\t3").is_err());
        assert!(!v.is_dirty());
    }

    #[test]
    fn bulk_edit_skips_binary_deleted_and_lossy_cells() {
        let mut v = view();
        v.select_cell(0, 0, Modifiers::NONE);
        v.select_cell(2, 1, Modifiers::SHIFT);
        v.edit.as_mut().unwrap().target.binary_cols[0] = true;
        v.edit.as_mut().unwrap().state.deleted.insert(1);
        v.fill_selection(Some("same".into()));
        assert_eq!(
            v.edit
                .as_ref()
                .unwrap()
                .state
                .changes
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![(0, 1), (2, 1)]
        );
        v.undo();
        assert!(v.edit.as_ref().unwrap().state.changes.is_empty());
        assert!(v.edit.as_ref().unwrap().state.deleted.contains(&1));
        v.rs.rows[2][1] = Some("0xAB…".into());
        v.fill_selection(Some("safe".into()));
        assert_eq!(
            v.edit
                .as_ref()
                .unwrap()
                .state
                .changes
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![(0, 1)]
        );
    }

    #[test]
    fn read_only_grid_can_navigate_but_never_adds_deletes_or_submits() {
        let mut h = harness(true, false);
        h.state_mut()
            .view
            .set_edit_target(Err("Read-only connection".into()));
        h.key_press(Key::Insert);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Delete);
        h.key_press(Key::ArrowDown);
        h.run_steps(3);
        assert_eq!(h.state().view.selected, Some((1, 1)));
        assert!(h.state().view.edit.is_none());
        assert_eq!(h.state().submits, 0);
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    fn view() -> ResultView {
        let rs = ResultSet {
            columns: vec!["id".into(), "note".into()],
            rows: vec![vec![Some("1".into()), Some("a \"b\"\tc|d".into())], vec![Some("2".into()), None]],
            ..Default::default()
        };
        ResultView::new(rs, 1)
    }

    #[test]
    fn masked_exports_leave_the_grid_alone() {
        use crate::db::copy::Mask;
        let v = view();
        let masks = [Mask::Keep, Mask::Email];
        let m = Some(ExportMasks { masks: &masks, key: "k" });
        let csv = format_rows_masked(&v, ExportFormat::Csv, "", m);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "id,note");
        assert!(lines[1].starts_with("1,user_") && lines[1].ends_with("@example.com"), "{csv}");
        assert_eq!(lines[2], "2,");
        assert_eq!(format_rows_masked(&v, ExportFormat::Csv, "", m), csv);
        assert_eq!(v.rs.rows[0][1].as_deref(), Some("a \"b\"\tc|d"));
        assert_eq!(format_rows_masked(&v, ExportFormat::Tsv, "", None), format_rows(&v, ExportFormat::Tsv, ""));
    }

    #[test]
    fn export_formats() {
        let v = view();
        assert_eq!(
            format_rows(&v, ExportFormat::Json, ""),
            "[\n  {\"id\": \"1\", \"note\": \"a \\\"b\\\"\\tc|d\"},\n  {\"id\": \"2\", \"note\": null}\n]\n"
        );
        assert_eq!(format_rows(&v, ExportFormat::Tsv, ""), "id\tnote\n1\ta \"b\" c|d\n2\t\n");
        assert_eq!(
            format_rows(&v, ExportFormat::SqlInsert, "[dbo].[t]"),
            "INSERT INTO [dbo].[t] (\"id\", \"note\") VALUES ('1', 'a \"b\"\tc|d');\nINSERT INTO [dbo].[t] (\"id\", \"note\") VALUES ('2', NULL);\n"
        );
        assert_eq!(format_rows(&v, ExportFormat::Markdown, ""), "| id | note |\n| --- | --- |\n| 1 | a \"b\"\tc\\|d |\n| 2 |  |\n");
    }
}
