//! Editor tabs: SQL console (editor with highlighting, line numbers, autocompletion
//! and results) and table data editor (toolbar, WHERE / ORDER BY filters and grid).

use crate::db::{ColumnInfo, Driver, ObjectKind, SchemaInfo};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::icons::{self, Ico};
use crate::ui::results::{self, OutputEntry, ResultView};
use crate::ui::theme::{self, pal, palette};
use crate::ui::{ConnStatusKind, ConnSummary, byte_to_char, char_to_byte, quote_ident};
use crate::worker::ConnId;
use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{Event, FontId, Key, Modifiers, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultTab {
    Output,
    Result(usize),
}

/// Which result tabs to close (Output is never closed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseResults {
    One(usize),
    /// All but this one.
    Others(usize),
    Left(usize),
    Right(usize),
    All,
}

impl CloseResults {
    /// Indices (ascending) closed out of `n` results.
    pub fn indices(self, n: usize) -> Vec<usize> {
        match self {
            CloseResults::One(i) => (i < n).then_some(i).into_iter().collect(),
            CloseResults::Others(i) => (0..n).filter(|&j| j != i).collect(),
            CloseResults::Left(i) => (0..i.min(n)).collect(),
            CloseResults::Right(i) => (i.saturating_add(1)..n).collect(),
            CloseResults::All => (0..n).collect(),
        }
    }
}

/// New index of view `i` after removing `removed` (ascending); `None` if it was removed.
pub fn remap_index(i: usize, removed: &[usize]) -> Option<usize> {
    if removed.contains(&i) {
        return None;
    }
    Some(i - removed.iter().filter(|&&r| r < i).count())
}

/// Active tab after removing `removed`: if the active one was closed, the one taking its place
/// (or the last one); with no results, Output.
pub fn tab_after_close(tab: ResultTab, removed: &[usize], remaining: usize) -> ResultTab {
    let ResultTab::Result(i) = tab else { return tab };
    if remaining == 0 {
        return ResultTab::Output;
    }
    match remap_index(i, removed) {
        Some(j) => ResultTab::Result(j.min(remaining - 1)),
        None => {
            let pos = i - removed.iter().filter(|&&r| r < i).count();
            ResultTab::Result(pos.min(remaining - 1))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ItemKind {
    Column,
    Table,
    View,
    Schema,
    Keyword,
}

struct CompletionItem {
    text: String,
    kind: ItemKind,
    detail: String,
    /// Order given by the context (lower first).
    rank: u8,
}

struct Completion {
    /// Position (in chars) where the replaced prefix starts.
    start: usize,
    selected: usize,
    items: Vec<CompletionItem>,
    scroll_to_selected: bool,
    /// Opened by typing (not ⌃Space or a dot): closes when only the typed word is left.
    auto: bool,
}

pub struct Console {
    pub id: u64,
    pub title: String,
    pub text: String,
    pub conn: Option<ConnId>,
    /// Database of the tab (`None` = the connection's default).
    pub db: Option<String>,
    pub running: bool,
    pub run_started: Option<Instant>,
    /// Statements to run once connected.
    pub pending: Option<Vec<String>>,
    pub last_statements: Vec<String>,
    pub results: Vec<ResultView>,
    pub log: Vec<OutputEntry>,
    pub result_tab: ResultTab,
    /// Selection range (chars, sorted) from the last frame.
    pub cursor: Option<(usize, usize)>,
    hl_cache: Option<(u64, LayoutJob)>,
    completion: Option<Completion>,
    /// If Some, the tab is a table data editor instead of a console.
    pub table: Option<TableTab>,
    /// After saving changes: the next run replaces only this result.
    pub replace_view: Option<usize>,
    /// Format chosen in the data editor's "CSV ⌄" selector.
    pub export_format: results::ExportFormat,
    /// Find / replace bar of the editor (open if Some).
    find: Option<FindState>,
    /// Query history popup (open if Some).
    history: Option<HistoryState>,
    /// Bottom-left corner of the toolbar's history button (popup anchor).
    history_anchor: Option<egui::Pos2>,
    /// Char position the editor scrolls to on the next frame.
    scroll_to: Option<usize>,
    /// The `DATACRAFT_DEMO_*` hooks of the console already ran.
    demo_done: bool,
}

/// State of the editor's find / replace bar.
struct FindState {
    query: String,
    replace: String,
    show_replace: bool,
    case_sensitive: bool,
    /// Give the search field the focus on the next frame.
    focus: bool,
}

/// State of the query history popup.
struct HistoryState {
    filter: String,
    focus: bool,
}

/// Editor commands (shortcuts, toolbar and context menu).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditCmd {
    Format,
    ToggleComment,
    Duplicate,
    DeleteLine,
    Explain,
    /// Runs the statement and shows the plan with actual rows and time (asks first if it writes).
    ExplainAnalyze,
    History,
    Find,
    Replace,
    FindNext,
    FindPrev,
    ReplaceOne,
    ReplaceAll,
    CloseFind,
}

/// State of a table data editor.
pub struct TableTab {
    pub view: TableViewOptions,
    pub filter_sort: crate::ui::filter_sort::FilterSort,
    pub offset: usize,
    pub page_size: usize,
    page_input: String,
    pub counting_page: bool,
    pub info_requested: bool,
    pub info_generation: u64,
    pub info_loading: bool,
    pub info: Option<Result<crate::db::object_info::ObjectInfo, String>>,
    inspector_mode: crate::ui::inspector::InspectorMode,
    pub schema: String,
    pub table: String,
    pub where_clause: String,
    pub order_by: String,
    /// false until the first load (restored tabs load when activated).
    pub loaded: bool,
    /// Opt-in: submit pending grid changes when leaving the edited rows.
    pub save_on_row_change: bool,
}

impl TableTab {
    pub fn page_input_reset(&mut self) { self.page_input.clear(); }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TableViewOptions { pub cell_editor: bool, pub filter_sort: bool, pub inspector: bool }
impl Default for TableViewOptions {
    fn default() -> Self { Self { cell_editor: false, filter_sort: false, inspector: true } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageMove { First, Previous, Next, Last, Number(usize), Size(usize) }

/// Actions the console asks the app to perform.
pub enum ConsoleAction {
    ApplyFilter,
    Page(PageMove),
    Inspect(bool),
    Run,
    RunAll,
    /// Run these statements as they are (e.g. the explain plan of the current statement).
    RunStatements(Vec<String>),
    /// "Explain Analyze" of `statement`: `explain` runs it, so the app asks first if it writes.
    ExplainAnalyze { statement: String, explain: String },
    /// Cancel the running query.
    Cancel,
    /// Reload the table editor data (with the current filters).
    Reload,
    /// Generate the DDL of the data editor's table.
    Ddl,
    Connect(ConnId),
    Error(String),
    /// Open the preview of the given result's pending changes.
    Preview(usize),
    /// Save the given result's pending changes directly.
    Submit(usize),
    /// Close result tabs (the app confirms if they have unsaved changes).
    CloseResults(CloseResults),
    /// Open "Export with Masking" for the last result.
    ExportMasked,
}

fn edit_action(a: results::EditAction, view: usize) -> ConsoleAction {
    match a {
        results::EditAction::Submit => ConsoleAction::Submit(view),
        results::EditAction::Preview => ConsoleAction::Preview(view),
    }
}

/// Read-only context the console needs.
pub struct ConsoleCtx<'a> {
    pub connections: &'a [ConnSummary],
    pub schemas: Option<&'a [SchemaInfo]>,
    pub driver: Driver,
    pub dark: bool,
    pub font_size: f32,
    /// Columns of the open table (data editor), to mark primary keys.
    pub columns: Option<&'a [ColumnInfo]>,
    /// Show completions while typing (Settings).
    pub auto_complete: bool,
}

impl Console {
    pub fn new(id: u64, title: String, text: String, conn: Option<ConnId>) -> Self {
        Self {
            id,
            title,
            text,
            conn,
            db: None,
            running: false,
            run_started: None,
            pending: None,
            last_statements: Vec::new(),
            results: Vec::new(),
            log: Vec::new(),
            result_tab: ResultTab::Output,
            cursor: None,
            hl_cache: None,
            completion: None,
            table: None,
            replace_view: None,
            export_format: results::ExportFormat::Csv,
            find: None,
            history: None,
            history_anchor: None,
            scroll_to: None,
            demo_done: false,
        }
    }

    /// Data editor tab for `schema.table`.
    pub fn new_table(id: u64, conn: ConnId, db: Option<String>, schema: String, table: String) -> Self {
        let mut c = Self::new(id, table.clone(), String::new(), Some(conn));
        c.db = db;
        c.table = Some(TableTab {
            view: TableViewOptions::default(), filter_sort: crate::ui::filter_sort::FilterSort::default(), offset: 0, page_size: 1000, page_input: String::new(),
            counting_page: false, info_requested: false, info_generation: 0, info_loading: false, info: None, inspector_mode: crate::ui::inspector::InspectorMode::default(),
            schema,
            table,
            where_clause: String::new(),
            order_by: String::new(),
            loaded: false,
            save_on_row_change: false,
        });
        c
    }

    pub fn is_busy(&self) -> bool {
        self.running || self.pending.is_some() || self.table.as_ref().is_some_and(|t| t.counting_page)
    }

    /// Result whose changes are being edited: the last one in the data editor or the
    /// visible result in a console.
    pub fn edit_view_index(&self) -> Option<usize> {
        if self.table.is_some() {
            return self.results.len().checked_sub(1);
        }
        match self.result_tab {
            ResultTab::Result(i) if i < self.results.len() => Some(i),
            _ => None,
        }
    }

    pub fn has_pending_edits(&self) -> bool {
        self.results.iter().any(|r| r.is_dirty())
    }

    /// true if any of the views `which` would close has unsaved changes.
    pub fn closing_dirty(&self, which: CloseResults) -> bool {
        which.indices(self.results.len()).iter().any(|&i| self.results[i].is_dirty())
    }

    /// Closes result tabs and adjusts the active one and `replace_view`. Returns the
    /// removed indices (ascending) so the app can adjust its own.
    pub fn close_results(&mut self, which: CloseResults) -> Vec<usize> {
        let removed = which.indices(self.results.len());
        for &i in removed.iter().rev() {
            self.results.remove(i);
        }
        if !removed.is_empty() {
            self.result_tab = tab_after_close(self.result_tab, &removed, self.results.len());
            self.replace_view = self.replace_view.and_then(|v| remap_index(v, &removed));
        }
        removed
    }

    pub fn discard_edits(&mut self) {
        for r in &mut self.results {
            r.discard_edits();
        }
    }

    /// 1-based (line, column) of the editor cursor.
    pub fn line_col(&self) -> (usize, usize) {
        let c = self.cursor.map(|(_, b)| b).unwrap_or(0);
        let mut line = 1;
        let mut col = 1;
        for ch in self.text.chars().take(c) {
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    pub fn editor_id(&self) -> egui::Id {
        egui::Id::new(("console_editor", self.id))
    }

    /// Selected text (if any).
    pub fn selection(&self) -> Option<&str> {
        let (a, b) = self.cursor?;
        if a == b {
            return None;
        }
        let s = char_to_byte(&self.text, a);
        let e = char_to_byte(&self.text, b);
        self.text.get(s..e).filter(|t| !t.trim().is_empty())
    }

    pub fn cursor_byte(&self) -> usize {
        let c = self.cursor.map(|(_, b)| b).unwrap_or(self.text.chars().count());
        char_to_byte(&self.text, c)
    }

    /// Moves the editor cursor to the end and appends text.
    pub fn append_text(&mut self, ctx: &egui::Context, extra: &str) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        let start = self.text.chars().count();
        self.text.push_str(extra);
        let end = self.text.chars().count();
        self.set_cursor(ctx, start, end);
    }

    fn set_cursor(&mut self, ctx: &egui::Context, a: usize, b: usize) {
        let id = self.editor_id();
        let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
        state.store(ctx, id);
        self.cursor = Some((a.min(b), a.max(b)));
    }

    /// Toolbar + editor of a console (inside the editor island).
    pub fn show_console(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>) -> Vec<ConsoleAction> {
        let mut actions = Vec::new();
        let mut cmds = self.demo_hooks();
        cmds.extend(self.shortcut_cmds(ui));
        self.console_toolbar(ui, cx, &mut actions, &mut cmds);
        hline(ui);
        if self.find.is_some() {
            self.find_bar(ui, &mut cmds);
            hline(ui);
        }
        self.editor(ui, cx, &mut cmds);
        let ctx = ui.ctx().clone();
        for c in cmds {
            self.apply_cmd(&ctx, cx, c, &mut actions);
        }
        if let Some(stmt) = self.history_popup(ui) {
            self.insert_text(&ctx, &stmt);
        }
        actions
    }

    /// Opens the find bar (with the selection as the search text if it is a single line).
    /// `replace` also shows the replace row. For the app's Find / Replace menu commands.
    pub fn open_find(&mut self, replace: bool) {
        if self.table.is_some() {
            if let Some(i) = self.edit_view_index() { self.results[i].open_find(); }
            return;
        }
        let selected = self.selection().filter(|s| !s.contains('\n')).map(str::to_string);
        let prev = self.find.take();
        let (query, repl, case_sensitive, shown) = match prev {
            Some(f) => (f.query, f.replace, f.case_sensitive, f.show_replace),
            None => (String::new(), String::new(), false, false),
        };
        self.find = Some(FindState {
            query: selected.unwrap_or(query),
            replace: repl,
            show_replace: replace || shown,
            case_sensitive,
            focus: true,
        });
    }

    /// Debug hooks for screenshots (demo mode only, read once):
    /// `DATACRAFT_DEMO_FORMAT=1` reformats the console text, `DATACRAFT_DEMO_FIND=<text>`
    /// opens find / replace searching `<text>` and `DATACRAFT_DEMO_HISTORY=1` opens the
    /// query history popup.
    fn demo_hooks(&mut self) -> Vec<EditCmd> {
        let mut cmds = Vec::new();
        if self.demo_done {
            return cmds;
        }
        self.demo_done = true;
        if std::env::var_os("DATACRAFT_DEMO").is_none() {
            return cmds;
        }
        if std::env::var("DATACRAFT_DEMO_FORMAT").is_ok_and(|v| v == "1") {
            self.cursor = None;
            cmds.push(EditCmd::Format);
        }
        if let Ok(q) = std::env::var("DATACRAFT_DEMO_FIND") {
            self.open_find(true);
            if let Some(f) = self.find.as_mut() {
                f.query = q;
                f.focus = false;
            }
        }
        if std::env::var("DATACRAFT_DEMO_HISTORY").is_ok_and(|v| v == "1") {
            self.history = Some(HistoryState { filter: String::new(), focus: false });
        }
        cmds
    }

    /// Editor shortcuts, consumed before the editor sees them.
    fn shortcut_cmds(&mut self, ui: &Ui) -> Vec<EditCmd> {
        let mut cmds = Vec::new();
        let editor_id = self.editor_id();
        let (find_id, replace_id) = self.find_ids();
        let (editor, find_q, find_r, none) = ui.memory(|m| {
            (m.has_focus(editor_id), m.has_focus(find_id), m.has_focus(replace_id), m.focused().is_none())
        });
        if !(editor || find_q || find_r || none) {
            return cmds;
        }
        let find_open = self.find.is_some();
        let completing = self.completion.is_some();
        let cmd = Modifiers::COMMAND;
        let shift_cmd = Modifiers::COMMAND | Modifiers::SHIFT;
        let alt_cmd = Modifiers::COMMAND | Modifiers::ALT;
        ui.input_mut(|i| {
            let mut take = |mods: Modifiers, key: Key, c: EditCmd| {
                if take_key(i, mods, key) {
                    cmds.push(c);
                }
            };
            take(alt_cmd, Key::L, EditCmd::Format);
            take(alt_cmd, Key::E, EditCmd::History);
            take(shift_cmd, Key::E, EditCmd::Explain);
            take(cmd, Key::F, EditCmd::Find);
            take(cmd, Key::R, EditCmd::Replace);
            if find_open {
                take(shift_cmd, Key::G, EditCmd::FindPrev);
                take(cmd, Key::G, EditCmd::FindNext);
                take(Modifiers::SHIFT, Key::F3, EditCmd::FindPrev);
                take(Modifiers::NONE, Key::F3, EditCmd::FindNext);
                if find_q {
                    take(Modifiers::SHIFT, Key::Enter, EditCmd::FindPrev);
                    take(Modifiers::NONE, Key::Enter, EditCmd::FindNext);
                }
                if find_r {
                    take(Modifiers::NONE, Key::Enter, EditCmd::ReplaceOne);
                }
                if find_q || find_r || (editor && !completing) {
                    take(Modifiers::NONE, Key::Escape, EditCmd::CloseFind);
                }
            }
            if editor {
                take(cmd, Key::Slash, EditCmd::ToggleComment);
                take(shift_cmd, Key::Slash, EditCmd::ToggleComment);
                take(cmd, Key::D, EditCmd::Duplicate);
                // DataGrip: ⌘⌫ on macOS, Ctrl+Y elsewhere (redo stays on Ctrl+Shift+Z).
                take(cmd, if cfg!(target_os = "macos") { Key::Backspace } else { Key::Y }, EditCmd::DeleteLine);
            }
        });
        cmds
    }

    fn find_ids(&self) -> (egui::Id, egui::Id) {
        (egui::Id::new(("find_query", self.id)), egui::Id::new(("find_replace", self.id)))
    }

    fn apply_cmd(&mut self, ctx: &egui::Context, cx: &ConsoleCtx<'_>, cmd: EditCmd, actions: &mut Vec<ConsoleAction>) {
        match cmd {
            EditCmd::Format => self.reformat(ctx, cx),
            // KQL's comments are `//`.
            EditCmd::ToggleComment => self.edit_lines(ctx, if cx.driver == Driver::Kusto { toggle_slash_comment } else { toggle_line_comment }),
            EditCmd::Duplicate => self.edit_lines(ctx, duplicate_lines),
            EditCmd::DeleteLine => self.edit_lines(ctx, delete_lines),
            EditCmd::Explain | EditCmd::ExplainAnalyze => {
                if !self.is_busy() {
                    let stmt = match self.selection() {
                        Some(sel) => sql::split_statements(sel, cx.driver).into_iter().next(),
                        None => sql::statement_at(&self.text, self.cursor_byte(), cx.driver),
                    };
                    let Some(stmt) = stmt else { return };
                    // PostgreSQL, MySQL and MariaDB give a plan the results show as a tree.
                    match crate::db::plan::explain_statement(&stmt.text, cx.driver, cmd == EditCmd::ExplainAnalyze) {
                        Some(explain) if cmd == EditCmd::ExplainAnalyze => actions.push(ConsoleAction::ExplainAnalyze { statement: stmt.text, explain }),
                        Some(explain) => actions.push(ConsoleAction::RunStatements(vec![explain])),
                        None if cmd == EditCmd::Explain => actions.push(ConsoleAction::RunStatements(sql::explain_statements(&stmt.text, cx.driver))),
                        None => {}
                    }
                }
            }
            EditCmd::History => {
                self.history = match self.history {
                    Some(_) => None,
                    None => Some(HistoryState { filter: String::new(), focus: true }),
                };
            }
            EditCmd::Find => self.open_find(false),
            EditCmd::Replace => self.open_find(true),
            EditCmd::FindNext => self.find_step(ctx, true),
            EditCmd::FindPrev => self.find_step(ctx, false),
            EditCmd::ReplaceOne => self.replace_one(ctx),
            EditCmd::ReplaceAll => self.replace_all(ctx),
            EditCmd::CloseFind => {
                self.find = None;
                ctx.memory_mut(|m| m.request_focus(self.editor_id()));
            }
        }
    }

    /// Applies a line transformation (text, selection) → (text, selection) to the editor.
    fn edit_lines(&mut self, ctx: &egui::Context, f: fn(&str, usize, usize) -> (String, usize, usize)) {
        let len = self.text.chars().count();
        let (a, b) = self.cursor.unwrap_or((len, len));
        let (text, na, nb) = f(&self.text, a, b);
        self.text = text;
        self.completion = None;
        self.set_cursor(ctx, na, nb);
        self.scroll_to = Some(nb);
        ctx.memory_mut(|m| m.request_focus(self.editor_id()));
    }

    /// Reformat Code: the selection or, without one, the whole console (like DataGrip).
    fn reformat(&mut self, ctx: &egui::Context, cx: &ConsoleCtx<'_>) {
        // The SQL formatter would mangle Redis and mongosh commands.
        if cx.driver.is_nosql() {
            return;
        }
        let len = self.text.chars().count();
        let sel = self.cursor.filter(|(a, b)| a != b && self.selection().is_some());
        let (s, e) = sel.unwrap_or((0, len));
        let (bs, be) = (char_to_byte(&self.text, s), char_to_byte(&self.text, e));
        let seg = &self.text[bs..be];
        let core_start = bs + (seg.len() - seg.trim_start().len());
        let core_end = be - (seg.len() - seg.trim_end().len());
        if core_start >= core_end {
            return;
        }
        let core = &self.text[core_start..core_end];
        let line_start = self.text[..core_start].rfind('\n').map_or(0, |i| i + 1);
        let indent = self.text[line_start..core_start].chars().count();
        let names: std::collections::HashSet<String> = cx
            .schemas
            .unwrap_or(&[])
            .iter()
            .flat_map(|s| s.tables.iter())
            .flat_map(|t| std::iter::once(t.name.to_lowercase()).chain(t.columns.iter().map(|c| c.name.to_lowercase())))
            .collect();
        let is_ident = |w: &str| names.contains(w);
        let formatted = sql::format_sql(core, cx.driver, indent, &is_ident);
        if formatted == core {
            return;
        }
        let caret = self.cursor.map(|(_, b)| b).unwrap_or(0);
        let keep = nonspace_count(&self.text, caret);
        let mut text = String::with_capacity(self.text.len() + formatted.len());
        text.push_str(&self.text[..core_start]);
        text.push_str(&formatted);
        text.push_str(&self.text[core_end..]);
        self.text = text;
        self.completion = None;
        match sel {
            // Keeps the reformatted block selected.
            Some(_) => {
                let new_e = e + self.text.chars().count() - len;
                self.set_cursor(ctx, s, new_e);
            }
            None => {
                let c = pos_of_nonspace(&self.text, keep);
                self.set_cursor(ctx, c, c);
                self.scroll_to = Some(c);
            }
        }
    }

    /// Replaces the selection (or inserts at the cursor) with `insert` and focuses the editor.
    fn insert_text(&mut self, ctx: &egui::Context, insert: &str) {
        let len = self.text.chars().count();
        let (a, b) = self.cursor.unwrap_or((len, len));
        let (bs, be) = (char_to_byte(&self.text, a), char_to_byte(&self.text, b));
        self.text.replace_range(bs..be, insert);
        let end = a + insert.chars().count();
        self.set_cursor(ctx, end, end);
        self.scroll_to = Some(end);
        ctx.memory_mut(|m| m.request_focus(self.editor_id()));
    }

    /// Matches (char ranges) of the find bar's search text.
    fn find_matches(&self) -> Vec<(usize, usize)> {
        match &self.find {
            Some(f) => find_matches(&self.text, &f.query, f.case_sensitive),
            None => Vec::new(),
        }
    }

    /// Selects the next (or previous) match after (before) the selection, wrapping around.
    fn find_step(&mut self, ctx: &egui::Context, forward: bool) {
        let matches = self.find_matches();
        let len = self.text.chars().count();
        let (a, b) = self.cursor.unwrap_or((0, 0)).min((len, len));
        let target = if forward {
            matches.iter().find(|m| m.0 >= b).or(matches.first())
        } else {
            matches.iter().rev().find(|m| m.1 <= a).or(matches.last())
        };
        if let Some(&(s, e)) = target {
            self.set_cursor(ctx, s, e);
            self.scroll_to = Some(s);
        }
    }

    /// Replaces the selected match and moves to the next one (or just finds the next one).
    fn replace_one(&mut self, ctx: &egui::Context) {
        let Some(repl) = self.find.as_ref().map(|f| f.replace.clone()) else { return };
        let matches = self.find_matches();
        if let Some((a, b)) = self.cursor
            && matches.contains(&(a, b))
        {
            let (bs, be) = (char_to_byte(&self.text, a), char_to_byte(&self.text, b));
            self.text.replace_range(bs..be, &repl);
            let end = a + repl.chars().count();
            self.set_cursor(ctx, end, end);
        }
        self.find_step(ctx, true);
    }

    fn replace_all(&mut self, ctx: &egui::Context) {
        let Some(repl) = self.find.as_ref().map(|f| f.replace.clone()) else { return };
        let matches = self.find_matches();
        if matches.is_empty() {
            return;
        }
        self.text = replace_ranges(&self.text, &matches, &repl);
        let len = self.text.chars().count();
        let c = self.cursor.map(|(_, b)| b).unwrap_or(0).min(len);
        self.set_cursor(ctx, c, c);
    }

    /// Find / replace bar above the editor.
    fn find_bar(&mut self, ui: &mut Ui, cmds: &mut Vec<EditCmd>) {
        let p = pal(ui);
        let (find_id, replace_id) = self.find_ids();
        let matches = self.find_matches();
        let current = self.cursor.and_then(|c| matches.iter().position(|m| *m == c));
        let Some(f) = self.find.as_mut() else { return };
        let mut changed = false;
        let row = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 32.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_size(vec2(w, 32.0));
                ui.add_space(6.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                add(ui);
            });
        };
        fn field<'t>(text: &'t mut String, id: egui::Id, hint: &str, color: egui::Color32) -> egui::TextEdit<'t> {
            egui::TextEdit::singleline(text)
                .id(id)
                .hint_text(RichText::new(hint).color(color))
                .font(FontId::monospace(13.0))
                .desired_width(260.0)
        }
        row(ui, &mut |ui| {
            let chevron = if f.show_replace { Ico::ChevronDown } else { Ico::ChevronRight };
            if theme::icon_button_sized(ui, chevron, true, &crate::shortcuts::format(tr("Show or hide replace (⌘R)")), 22.0).clicked() {
                f.show_replace = !f.show_replace;
            }
            let resp = ui.add(field(&mut f.query, find_id, tr("Text to find"), p.disabled));
            if f.focus {
                resp.request_focus();
                f.focus = false;
            }
            changed = resp.changed();
            let has_matches = !matches.is_empty();
            if theme::flat_button(ui, None, "↑", FontId::proportional(14.0), false, has_matches)
                .on_hover_text(crate::shortcuts::format(tr("Previous match (⇧⌘G)")))
                .clicked()
            {
                cmds.push(EditCmd::FindPrev);
            }
            if theme::flat_button(ui, None, "↓", FontId::proportional(14.0), false, has_matches)
                .on_hover_text(crate::shortcuts::format(tr("Next match (⌘G)")))
                .clicked()
            {
                cmds.push(EditCmd::FindNext);
            }
            if ui
                .add(egui::Button::selectable(f.case_sensitive, RichText::new("Aa").font(theme::bold(12.5))))
                .on_hover_text(tr("Match case"))
                .clicked()
            {
                f.case_sensitive = !f.case_sensitive;
                changed = true;
            }
            ui.add_space(6.0);
            if !f.query.is_empty() {
                let (text, color) = match (matches.len(), current) {
                    (0, _) => (tr("No matches").to_string(), p.error),
                    (n, Some(i)) => (format!("{}/{n}", i + 1), p.muted),
                    (1, None) => (tr("1 result").to_string(), p.muted),
                    (n, None) => (trf("{n} results", &[("n", &n)]), p.muted),
                };
                ui.label(RichText::new(text).color(color).size(12.5));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if theme::icon_button_sized(ui, Ico::Close, true, tr("Close (Esc)"), 22.0).clicked() {
                    cmds.push(EditCmd::CloseFind);
                }
            });
        });
        if f.show_replace {
            row(ui, &mut |ui| {
                ui.add_space(26.0);
                ui.add(field(&mut f.replace, replace_id, tr("Replace with"), p.disabled));
                let any = !matches.is_empty();
                if ui.add_enabled(any, egui::Button::new(tr("Replace"))).clicked() {
                    cmds.push(EditCmd::ReplaceOne);
                }
                if ui.add_enabled(any, egui::Button::new(tr("Replace all"))).clicked() {
                    cmds.push(EditCmd::ReplaceAll);
                }
            });
        }
        // Incremental search: the first match from the selection start.
        if changed {
            let a = self.cursor.map(|c| c.0).unwrap_or(0);
            let matches = self.find_matches();
            if let Some(&(s, e)) = matches.iter().find(|m| m.0 >= a).or(matches.first()) {
                let ctx = ui.ctx().clone();
                self.set_cursor(&ctx, s, e);
                self.scroll_to = Some(s);
            }
        }
    }

    /// Query history popup under the toolbar button. Returns the statement to insert.
    fn history_popup(&mut self, ui: &Ui) -> Option<String> {
        let anchor = self.history_anchor?;
        let h = self.history.as_mut()?;
        let p = pal(ui);
        let area_id = egui::Id::new(("query_history", self.id));
        let entries = history_entries(&self.log, &h.filter);
        let mut picked = None;
        let mut close = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let inner = egui::Area::new(area_id).order(egui::Order::Foreground).fixed_pos(anchor).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).inner_margin(8).show(ui, |ui| {
                ui.set_width(460.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr("Query History")).font(theme::bold(13.0)).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!entries.is_empty(), egui::Button::new(RichText::new(tr("Copy all queries")).size(12.0)).frame(false))
                            .clicked()
                        {
                            let all: Vec<&str> = entries.iter().rev().map(|e| e.statement.trim()).collect();
                            ui.ctx().copy_text(all.join(";\n\n") + ";\n");
                        }
                    });
                });
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut h.filter)
                        .hint_text(RichText::new(tr("Filter queries")).color(p.disabled))
                        .desired_width(f32::INFINITY),
                );
                if h.focus {
                    resp.request_focus();
                    h.focus = false;
                }
                ui.add_space(4.0);
                if entries.is_empty() {
                    let msg = if self.log.iter().any(|e| !e.statement.is_empty()) { tr("No matches") } else { tr("No queries executed yet") };
                    ui.label(RichText::new(msg).color(p.muted));
                    return;
                }
                let list_h = (entries.len() as f32 * 26.0).min(340.0);
                egui::ScrollArea::vertical().min_scrolled_height(list_h).max_height(340.0).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for e in &entries {
                        let one_line: String = e.statement.split_whitespace().collect::<Vec<_>>().join(" ");
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(rect, 5.0, p.row_hover);
                        }
                        let cy = rect.center().y;
                        ui.painter().text(pos2(rect.left() + 6.0, cy), egui::Align2::LEFT_CENTER, &e.time, FontId::proportional(11.5), p.muted);
                        let text_rect = Rect::from_min_max(pos2(rect.left() + 70.0, rect.top()), pos2(rect.right() - 6.0, rect.bottom()));
                        let galley = ui.painter().layout(one_line, FontId::monospace(12.5), p.text, f32::INFINITY);
                        ui.painter().with_clip_rect(text_rect).galley(pos2(text_rect.left(), cy - galley.size().y / 2.0), galley, p.text);
                        let resp = resp.on_hover_text(RichText::new(e.statement.trim()).monospace());
                        if resp.clicked() {
                            picked = Some(e.statement.trim().to_string());
                        }
                    }
                });
            });
        });
        let area_rect = inner.response.rect;
        let clicked_outside = ui.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|pos| !area_rect.contains(pos) && pos.distance(anchor) > 30.0)
        });
        if clicked_outside || picked.is_some() {
            close = true;
        }
        if close {
            self.history = None;
        }
        picked
    }

    /// Content of a console's results island.
    pub fn show_results(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>) -> Vec<ConsoleAction> {
        let mut actions = Vec::new();
        self.results_ui(ui, cx, &mut actions);
        actions
    }

    /// Table data editor: toolbar, filter row and grid.
    pub fn show_table(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>) -> Vec<ConsoleAction> {
        let mut actions = Vec::new();
        let p = pal(ui);
        if !cx.driver.is_nosql() && self.table.as_ref().is_some_and(|t| t.view.inspector) {
            egui::Panel::right(egui::Id::new(("table_inspector", self.id))).default_size(280.0).size_range(240.0..=460.0).resizable(true).show(ui, |ui| self.table_inspector(ui, cx, &mut actions));
        }
        egui::Panel::bottom(egui::Id::new(("table_footer", self.id))).resizable(false).show_separator_line(true).show(ui, |ui| self.table_footer(ui, cx, &mut actions));
        if self.table.as_ref().is_some_and(|t| t.view.cell_editor) {
            let busy = self.is_busy();
            egui::Panel::bottom(egui::Id::new(("cell_editor", self.id))).default_size(150.0).size_range(100.0..=450.0).resizable(true).show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| { if let Some(view) = self.results.last_mut() { view.show_cell_editor(ui); } });
            });
        }
        self.table_toolbar(ui, cx, &mut actions);
        hline(ui);
        if cx.driver.is_nosql() { self.filter_row(ui, cx.driver, &mut actions); hline(ui); }
        else if let Some(t) = &mut self.table && t.view.filter_sort {
            if ui.add_enabled_ui(!self.running, |ui| t.filter_sort.show(ui, cx.driver, cx.columns.unwrap_or(&[]))).inner { actions.push(ConsoleAction::ApplyFilter); }
            hline(ui);
        }
        if let Some(e) = self.log.last().filter(|e| e.is_error && self.result_tab == ResultTab::Output) {
            let msg = e.message.clone();
            egui::Frame::NONE
                .fill(if p.dark { egui::Color32::from_rgb(0x3F, 0x1E, 0x21) } else { egui::Color32::from_rgb(0xFF, 0xF6, 0xF5) })
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        icons::paint(
                            ui.painter(),
                            Rect::from_min_size(ui.cursor().min + vec2(0.0, 1.0), egui::Vec2::splat(16.0)),
                            Ico::Problems,
                            p.error,
                            &p,
                            true,
                        );
                        ui.add_space(22.0);
                        ui.add(egui::Label::new(RichText::new(msg).monospace().color(p.error)).wrap());
                    });
                });
            hline(ui);
        }
        let copy_requested = ui.input(|i| i.events.iter().any(|e| matches!(e, Event::Copy)))
            && ui.memory(|m| m.focused().is_none());
        let busy = self.is_busy();
        let save_on_row_change = self.table.as_ref().is_some_and(|t| t.save_on_row_change);
        let view_index = self.results.len().checked_sub(1);
        if let Some(view) = self.results.last_mut() {
            let response = ui
                .add_enabled_ui(!busy, |ui| {
                    results::show_result(ui, view, copy_requested, cx.columns, save_on_row_change, false)
                })
                .inner;
            if let Some(err) = response.error {
                actions.push(ConsoleAction::Error(err));
            }
            if response.submit
                && let Some(i) = view_index
            {
                actions.push(ConsoleAction::Submit(i));
            }
        } else if self.is_busy() {
            ui.centered_and_justified(|ui| ui.label(RichText::new(tr("Loading…")).color(p.muted)));
        }
        actions
    }

    fn busy_label(&self, ui: &mut Ui) {
        if self.is_busy() {
            let p = pal(ui);
            ui.add_space(6.0);
            ui.add(egui::Spinner::new().size(14.0).color(p.muted));
            let label = if self.pending.is_some() { tr("Connecting…") } else { tr("Running…") };
            let secs = self.run_started.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
            ui.label(RichText::new(format!("{label} {secs:.1} s")).color(p.muted));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// "Tx: Auto" dropdown.
    fn tx_dropdown(&self, ui: &mut Ui) {
        let resp = theme::flat_button(ui, None, "Tx: Auto", FontId::proportional(13.0), true, true)
            .on_hover_text(tr("Transaction mode"));
        egui::Popup::menu(&resp).show(|ui| {
            let _ = ui.add(egui::Button::new("✓  Auto").frame(false));
            ui.add_enabled(false, egui::Button::new(format!("    {}", tr("Manual"))).frame(false));
            ui.separator();
            ui.label(RichText::new(tr("Each statement is committed when run")).small().color(pal(ui).muted));
        });
    }

    /// Console "connection · database" selector (disabled while busy).
    fn connection_selector(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>, actions: &mut Vec<ConsoleAction>) {
        let busy = self.is_busy();
        let current = self.conn.and_then(|id| cx.connections.iter().find(|c| c.id == id));
        let (ico, label) = match current {
            Some(c) => (Ico::Driver(c.driver), c.label(self.db.as_deref())),
            None => (Ico::Database, tr("<no connection>").to_string()),
        };
        let resp = theme::flat_button(ui, Some(ico), &label, FontId::proportional(13.0), true, !busy)
            .on_hover_text(tr("Console data source and database"));
        let before = (self.conn, self.db.clone());
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(240.0);
            if ui.selectable_label(self.conn.is_none(), tr("<no connection>")).clicked() {
                self.conn = None;
                self.db = None;
                ui.close();
            }
            for c in cx.connections {
                let this = self.conn == Some(c.id);
                if ui.selectable_label(this && self.db.is_none(), format!("{}  ·  {}", c.name, c.driver.label())).clicked() {
                    self.conn = Some(c.id);
                    self.db = None;
                    ui.close();
                }
                if c.driver == Driver::Sqlite || c.driver == Driver::Oracle {
                    continue;
                }
                let effective = if this { self.db.clone().or_else(|| c.default_db.clone()) } else { None };
                for d in &c.databases {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        let sel = effective.as_deref() == Some(d.as_str());
                        let text = if c.default_db.as_deref() == Some(d.as_str()) { trf("{db}  (default)", &[("db", d)]) } else { d.clone() };
                        if ui.add(egui::Button::selectable(sel, text)).clicked() {
                            self.conn = Some(c.id);
                            self.db = Some(d.clone());
                            ui.close();
                        }
                    });
                }
            }
        });
        if (self.conn, self.db.clone()) != before {
            // Pending statements were for the previous connection/database.
            self.pending = None;
            self.run_started = None;
            if let Some(id) = self.conn
                && let Some(c) = cx.connections.iter().find(|c| c.id == id)
                && matches!(c.status, ConnStatusKind::Disconnected | ConnStatusKind::Error)
            {
                actions.push(ConsoleAction::Connect(id));
            }
        }
    }

    fn console_toolbar(
        &mut self,
        ui: &mut Ui,
        cx: &ConsoleCtx<'_>,
        actions: &mut Vec<ConsoleAction>,
        cmds: &mut Vec<EditCmd>,
    ) {
        let busy = self.is_busy();
        toolbar_row(ui, |ui| {
            theme::split_row(ui, |ui, side| match side {
                theme::Side::Left => {
            if theme::icon_button(ui, Ico::Play, !busy, &crate::shortcuts::format(tr("Run current statement or selection (⌘↩)"))).clicked() {
                actions.push(ConsoleAction::Run);
            }
            if theme::icon_button(ui, Ico::PlayAll, !busy, &crate::shortcuts::format(tr("Run all (⌘⇧↩)"))).clicked() {
                actions.push(ConsoleAction::RunAll);
            }
            if theme::icon_button(ui, Ico::Structure, !busy, &crate::shortcuts::format(tr("Explain plan of the current statement (⇧⌘E)"))).clicked() {
                cmds.push(EditCmd::Explain);
            }
            theme::toolbar_separator(ui);
            let hist = theme::icon_button(ui, Ico::History, true, &crate::shortcuts::format(tr("Query history (⌥⌘E)")));
            self.history_anchor = Some(hist.rect.left_bottom() + vec2(0.0, 4.0));
            if hist.clicked() {
                cmds.push(EditCmd::History);
            }
            if theme::icon_button(ui, Ico::Search, true, &crate::shortcuts::format(tr("Find and replace (⌘F)"))).clicked() {
                cmds.push(EditCmd::Find);
            }
            self.tx_dropdown(ui);
            if theme::icon_button(ui, Ico::Stop, self.running, tr("Cancel the running query")).clicked() {
                actions.push(ConsoleAction::Cancel);
            }
            self.busy_label(ui);
                }
                theme::Side::Right => {
                ui.add_space(4.0);
                self.connection_selector(ui, cx, actions);
                }
            });
        });
    }

    fn table_toolbar(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>, actions: &mut Vec<ConsoleAction>) {
        let busy = self.is_busy();
        toolbar_row(ui, |ui| {
            theme::split_row(ui, |ui, side| match side {
                theme::Side::Left => {
                    if let Some(t) = &mut self.table {
                        let was_open = t.view.cell_editor;
                        if theme::tool_button(ui, Ico::Edit, tr("Cell Editor"), t.view.cell_editor).clicked() { t.view.cell_editor = !t.view.cell_editor; }
                        if was_open && !t.view.cell_editor && let Some(view) = self.results.last_mut() { view.finish_inline_edit(); }
                        if !cx.driver.is_nosql() && theme::tool_button(ui, Ico::Filter, tr("Filter & Sort"), t.view.filter_sort).clicked() { t.view.filter_sort = !t.view.filter_sort; }
                        if !cx.driver.is_nosql() && theme::tool_button(ui, Ico::Info, tr("Information"), t.view.inspector).clicked() { t.view.inspector = !t.view.inspector; }
                    }
                    theme::toolbar_separator(ui);
                    self.tx_dropdown(ui);
                    theme::toolbar_separator(ui);
                    if theme::icon_button(ui, Ico::Ddl, cx.columns.is_some(), tr("Show table DDL"))
                        .clicked()
                    {
                        actions.push(ConsoleAction::Ddl);
                    }
                    if let Some(t) = self.table.as_mut() {
                        let editable =
                            !busy && self.results.last().is_some_and(|v| v.edit.is_some());
                        ui.add_enabled(editable, egui::Checkbox::new(&mut t.save_on_row_change, tr("Save on row change")))
                    .on_hover_text(tr("Automatically submit all pending grid changes when leaving the edited rows. Off by default."));
            }
            self.busy_label(ui);
                }
                theme::Side::Right => {
                ui.add_space(4.0);
                ui.spacing_mut().item_spacing.x = 1.0;
                let has_rows = self.results.last().is_some_and(|r| !r.rs.is_command());
                let fmt = self.export_format;
                let (target, base) = match &self.table {
                    Some(t) => (crate::db::Session::quote_table(cx.driver, &t.schema, &t.table), t.table.clone()),
                    None => (tr("table").to_string(), tr("result").to_string()),
                };
                // ⋮: copy to the clipboard.
                let more = theme::icon_button(ui, Ico::MoreVert, has_rows, tr("Copy to clipboard"));
                egui::Popup::menu(&more).show(|ui| {
                    if ui.button(trf("Copy all as {format}", &[("format", &fmt.label())])).clicked() {
                        if let Some(view) = self.results.last() {
                            ui.ctx().copy_text(results::format_rows(view, fmt, &target));
                        }
                        ui.close();
                    }
                    for other in results::ExportFormat::ALL.into_iter().filter(|f| *f != fmt) {
                        if ui.button(trf("Copy all as {format}", &[("format", &other.label())])).clicked() {
                            if let Some(view) = self.results.last() {
                                ui.ctx().copy_text(results::format_rows(view, other, &target));
                            }
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button(tr("Export with Masking…")).clicked() {
                        actions.push(ConsoleAction::ExportMasked);
                        ui.close();
                    }
                });
                // ⤓: export straight to a file in the chosen format.
                if theme::icon_button(ui, Ico::Export, has_rows, &trf("Export to {format} file…", &[("format", &fmt.label())])).clicked()
                    && let Some(view) = self.results.last()
                    && let Err(e) = results::export_to_file(view, fmt, &target, &base, None)
                {
                    actions.push(ConsoleAction::Error(trf("Export failed: {error}", &[("error", &e)])));
                }
                // "CSV ⌄": choose the extraction format.
                let selector = theme::flat_button(ui, None, fmt.label(), FontId::proportional(13.0), true, true)
                    .on_hover_text(tr("Extraction format"));
                egui::Popup::menu(&selector).show(|ui| {
                    for f in results::ExportFormat::ALL {
                        if ui.add(egui::Button::selectable(f == fmt, f.label())).clicked() {
                            self.export_format = f;
                            ui.close();
                        }
                    }
                });
                }
            });
        });
    }

    fn table_footer(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>, actions: &mut Vec<ConsoleAction>) {
        let busy = self.is_busy();
        toolbar_row(ui, |ui| {
            theme::split_row(ui, |ui, side| match side {
                theme::Side::Left => {
                    if let Some(a) = results::edit_buttons(ui, self.results.last_mut(), !busy) && let Some(i) = self.results.len().checked_sub(1) { actions.push(edit_action(a, i)); }
                    if theme::icon_button(ui, Ico::Refresh, !busy, &crate::shortcuts::format(tr("Reload page (⌘R)"))).clicked() { actions.push(ConsoleAction::Reload); }
                    if theme::icon_button(ui, Ico::Stop, self.running, tr("Cancel the running query")).clicked() { actions.push(ConsoleAction::Cancel); }
                    if let Some(view) = self.results.last() { edit_status_icon(ui, view); }
                }
                theme::Side::Right => {
                    let Some(t) = &mut self.table else { return };
                    let sql = !cx.driver.is_nosql();
                    let page = t.offset / t.page_size.max(1) + 1;
                    if t.page_input.is_empty() { t.page_input = page.to_string(); }
                    // The parent lays out this group from right to left.
                    let settings = theme::icon_button(ui, Ico::Gear, !busy && sql, tr("Page size"));
                    egui::Popup::menu(&settings).show(|ui| { for size in [100, 500, 1000, 5000] {
                        if ui.selectable_label(t.page_size == size, size.to_string()).clicked() { actions.push(ConsoleAction::Page(PageMove::Size(size))); ui.close(); }
                    } });
                    if ui.add_enabled(!busy && sql, egui::Button::new("›|")).on_hover_text(tr("Last page (counts rows)")).clicked() { actions.push(ConsoleAction::Page(PageMove::Last)); }
                    let full = self.results.last().is_some_and(|v| v.rs.rows.len() >= t.page_size);
                    if theme::icon_button(ui, Ico::ChevronRight, !busy && sql && full, tr("Next page")).clicked() { actions.push(ConsoleAction::Page(PageMove::Next)); }
                    let response = ui.add_enabled(!busy && sql, egui::TextEdit::singleline(&mut t.page_input).desired_width(40.0));
                    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        match t.page_input.parse::<usize>() { Ok(n) if n > 0 => actions.push(ConsoleAction::Page(PageMove::Number(n))), _ => actions.push(ConsoleAction::Error(tr("Enter a positive page number").into())) }
                    }
                    if theme::icon_button(ui, Ico::ChevronLeft, !busy && sql && t.offset > 0, tr("Previous page")).clicked() { actions.push(ConsoleAction::Page(PageMove::Previous)); }
                    if ui.add_enabled(!busy && sql && t.offset > 0, egui::Button::new("|‹")).on_hover_text(tr("First page")).clicked() { actions.push(ConsoleAction::Page(PageMove::First)); }
                    if let Some(view) = self.results.last() { ui.label(trf("{n}{plus} rows", &[("n", &view.row_count()), ("plus", &if view.rs.truncated { "+" } else { "" })])); }

                }
            });
        });
        let p = pal(ui);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let statement = self.last_statements.last().map(String::as_str).unwrap_or("");
            let caption = if let Some(view) = self.results.last() {
                let page = self.table.as_ref().map_or(1, |t| t.offset / t.page_size.max(1) + 1);
                match view.row_position() {
                    Some((row, _)) => trf("Record {row} · page {page}", &[("row", &(self.table.as_ref().map_or(0, |t| t.offset) + row)), ("page", &page)]),
                    None => trf("Page {page}", &[("page", &page)]),
                }
            } else { String::new() };
            theme::split_row(ui, |ui, side| match side {
                theme::Side::Left => {
                    ui.add(egui::Label::new(RichText::new(statement).monospace().size(11.0).color(p.muted)).truncate()).on_hover_text(statement);
                }
                theme::Side::Right => {
                    ui.label(RichText::new(&caption).small().color(p.muted));
                    if let Some(log) = self.log.last() { ui.label(RichText::new(trf("Last refresh: {time}", &[("time", &log.time)])).small().color(p.muted)); }
                }
            });
        });
    }

    fn table_inspector(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>, actions: &mut Vec<ConsoleAction>) {
        use crate::ui::inspector::{self, InspectorMode};
        let Some(t) = &mut self.table else { return };
        ui.set_min_width(220.0);
        ui.horizontal(|ui| {
            inspector::tabs(ui, &mut t.inspector_mode, true);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let more = theme::icon_button_sized(ui, Ico::MoreVert, !t.info_loading, tr("Actions"), 24.0);
                egui::Popup::menu(&more).show(|ui| {
                    if ui.button(tr("Count rows exactly")).clicked() { actions.push(ConsoleAction::Inspect(true)); ui.close(); }
                });
                if theme::icon_button_sized(ui, Ico::Refresh, !t.info_loading, tr("Refresh information"), 24.0).clicked() { actions.push(ConsoleAction::Inspect(false)); }
            });
        });
        let connection = cx.connections.iter().find(|c| Some(c.id) == self.conn).map_or("", |c| c.name.as_str());
        let db = self.db.as_deref().unwrap_or("");
        egui::ScrollArea::vertical().id_salt(("inspector_fields", self.id)).auto_shrink([false, false]).show(ui, |ui| {
            match t.inspector_mode {
                InspectorMode::Columns => {
                    let selected = self.results.last().and_then(|v| v.selected_cell().map(|(_, c, _)| &v.rs.columns[c]));
                    if let Some(name) = selected {
                        inspector::header(ui, name, tr("Column"), connection, db, Ico::Column);
                        let column = cx.columns.and_then(|cols| cols.iter().find(|c| c.name == *name));
                        inspector::property(ui, tr("Type"), column.map(|c| c.data_type.as_str()));
                        inspector::property(ui, tr("Not null"), column.map(|c| if c.nullable { tr("No") } else { tr("Yes") }));
                        inspector::property(ui, tr("Primary key"), column.map(|c| if c.primary_key { tr("Yes") } else { tr("No") }));
                    } else { ui.label(tr("Select a cell to see column information")); }
                }
                InspectorMode::Record => {
                    if let Some((row, values)) = self.results.last().and_then(ResultView::selected_record) {
                        let title = trf("Record {row}", &[("row", &(t.offset + row))]);
                        inspector::header(ui, &title, &t.table, connection, db, Ico::Record);
                        for (name, value) in values {
                            let p = pal(ui);
                            ui.label(RichText::new(name).size(12.0).color(p.muted));
                            let text = value.as_deref().unwrap_or("NULL");
                            let response = ui.add(egui::Label::new(text).truncate());
                            response.context_menu(|ui| { if ui.button(tr("Copy value")).clicked() { ui.ctx().copy_text(text.into()); ui.close(); } });
                            ui.add_space(12.0);
                        }
                    } else { ui.label(tr("Select a row to see its values")); }
                }
                InspectorMode::Information => {
                    let kind = cx.schemas.and_then(|schemas| schemas.iter().find(|schema| schema.name == t.schema)).and_then(|schema| schema.tables.iter().find(|table| table.name == t.table)).map_or(ObjectKind::Table, |table| table.kind);
                    inspector::header(ui, &t.table, if kind == ObjectKind::View { tr("View") } else { tr("Table") }, connection, db, if kind == ObjectKind::View { Ico::View } else { Ico::Table });
                    if t.info_loading { ui.spinner(); }
                    if let Some(Err(error)) = &t.info { ui.colored_label(ui.visuals().error_fg_color, error); }
                    inspector::table_fields(ui, t.info.as_ref().and_then(|r| r.as_ref().ok()));
                }
            }
        });
    }

    /// "WHERE | ORDER BY" row of the data editor.
    fn filter_row(&mut self, ui: &mut Ui, driver: Driver, actions: &mut Vec<ConsoleAction>) {
        let p = pal(ui);
        let Some(t) = self.table.as_mut() else { return };
        let id = self.id;
        let h = 32.0;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
        let half = (rect.width() / 2.0).round();
        let left = Rect::from_min_max(rect.min, pos2(rect.left() + half, rect.bottom()));
        let right = Rect::from_min_max(pos2(rect.left() + half, rect.top()), rect.max);
        ui.painter().vline(left.right(), rect.y_range().shrink(6.0), Stroke::new(1.0, p.grid));
        let mut submit = false;
        // InfluxDB: a Flux predicate on `r` and the time range (see `influx::table_query`). Kusto:
        // a KQL predicate and sort, piped into `where` and `order by` (see `kusto::table_query`).
        let (where_hint, order_hint) = match driver {
            Driver::InfluxDb => ("r.host == \"web-1\"", "start: -1h"),
            Driver::Kusto => ("| where", "| order by"),
            _ => ("WHERE", "ORDER BY"),
        };
        for (r, ico, hint, text, salt) in [
            (left, Ico::Filter, where_hint, &mut t.where_clause, "where"),
            (right, Ico::Sort, order_hint, &mut t.order_by, "order"),
        ] {
            icons::paint(
                ui.painter(),
                Rect::from_min_size(pos2(r.left() + 12.0, r.center().y - 7.0), egui::Vec2::splat(14.0)),
                ico,
                p.icon,
                &p,
                true,
            );
            let edit_rect = Rect::from_min_max(pos2(r.left() + 32.0, r.top() + 4.0), pos2(r.right() - 8.0, r.bottom() - 4.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(edit_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let resp = child.add(
                egui::TextEdit::singleline(text)
                    .id(egui::Id::new((salt, id)))
                    .hint_text(RichText::new(hint).monospace().color(p.disabled))
                    .font(FontId::monospace(13.0))
                    .frame(egui::Frame::NONE)
                    .background_color(egui::Color32::TRANSPARENT)
                    .desired_width(edit_rect.width()),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                submit = true;
            }
        }
        if submit {
            actions.push(ConsoleAction::Reload);
        }
    }

    fn results_ui(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>, actions: &mut Vec<ConsoleAction>) {
        let p = pal(ui);
        // "Services" tool window header: title + (closable) result tabs.
        let busy = self.is_busy();
        let mut close = None;
        theme::clipped_row(ui, 36.0, |ui| {
            ui.add_space(14.0);
            ui.label(RichText::new(tr("Services")).font(theme::bold(13.5)).color(p.text));
            ui.add_space(10.0);
            let lock = match self.result_tab {
                ResultTab::Result(i) => self
                    .results
                    .get(i)
                    .filter(|r| r.edit.is_none() && !r.rs.is_command())
                    .map(|r| r.readonly_reason.clone().unwrap_or_else(|| tr("Read-only result").into())),
                ResultTab::Output => None,
            };
            let tabs_w = (ui.available_width() - if lock.is_some() { 38.0 } else { 8.0 }).max(0.0);
            ui.allocate_ui_with_layout(vec2(tabs_w, 36.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("result_tabs")
                    .max_width(tabs_w)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            let out_err = self.log.last().is_some_and(|e| e.is_error);
                            let out_ico = if out_err { Ico::Problems } else { Ico::Console };
                            let has_results = !self.results.is_empty();
                            let (out, _) = theme::tab(ui, self.result_tab == ResultTab::Output, Some(out_ico), tr("Output"), false);
                            if out.clicked() {
                                self.result_tab = ResultTab::Output;
                            }
                            out.context_menu(|ui| {
                                if ui.add_enabled(has_results && !busy, egui::Button::new(tr("Close all results"))).clicked() {
                                    close = Some(CloseResults::All);
                                    ui.close();
                                }
                            });
                            let n = self.results.len();
                            for (i, r) in self.results.iter().enumerate() {
                                let sel = self.result_tab == ResultTab::Result(i);
                                let (resp, closed) = theme::tab(ui, sel, Some(Ico::Table), &r.title, !busy);
                                let resp = resp.on_hover_text(&r.rs.statement);
                                if closed || (resp.middle_clicked() && !busy) {
                                    close = Some(CloseResults::One(i));
                                } else if resp.clicked() {
                                    self.result_tab = ResultTab::Result(i);
                                }
                                resp.context_menu(|ui| {
                                    if ui.add_enabled(!busy, egui::Button::new(tr("Close"))).clicked() {
                                        close = Some(CloseResults::One(i));
                                        ui.close();
                                    }
                                    if ui.add_enabled(!busy && n > 1, egui::Button::new(tr("Close others"))).clicked() {
                                        close = Some(CloseResults::Others(i));
                                        ui.close();
                                    }
                                    for (label, which, enabled) in [
                                        (tr("Close to the left"), CloseResults::Left(i), i > 0),
                                        (tr("Close to the right"), CloseResults::Right(i), i + 1 < n),
                                    ] {
                                        if ui.add_enabled(!busy && enabled, egui::Button::new(label)).clicked() { close = Some(which); ui.close(); }
                                    }
                                    if ui.add_enabled(!busy, egui::Button::new(tr("Close all"))).clicked() {
                                        close = Some(CloseResults::All);
                                        ui.close();
                                    }
                                });
                            }
                        });
                    });
            });
            if let Some(why) = lock {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), egui::Vec2::splat(14.0)), Ico::Lock, p.muted, &p, true);
                    resp.on_hover_text(trf("Read-only: {reason}", &[("reason", &why)]));
                });
            }
        });
        if let Some(which) = close {
            actions.push(ConsoleAction::CloseResults(which));
        }
        hline(ui);
        let copy_requested = ui.input(|i| i.events.iter().any(|e| matches!(e, Event::Copy)))
            && ui.memory(|m| m.focused().is_none());
        // Mini edit bar above the grid if the result is editable.
        if let ResultTab::Result(i) = self.result_tab
            && self.results.get(i).is_some_and(|r| r.edit.is_some())
        {
            let busy = self.is_busy();
            let mut edit = None;
            toolbar_row(ui, |ui| {
                edit = results::edit_buttons(ui, self.results.get_mut(i), !busy);
                if let Some(v) = self.results.get(i) {
                    edit_status_icon(ui, v);
                }
            });
            if let Some(a) = edit {
                actions.push(edit_action(a, i));
            }
            hline(ui);
        }
        match self.result_tab {
            ResultTab::Result(i) if i < self.results.len() => {
                let response = ui
                    .add_enabled_ui(!busy, |ui| {
                        results::show_result(
                            ui,
                            &mut self.results[i],
                            copy_requested,
                            cx.columns,
                            false,
                            true,
                        )
                    })
                    .inner;
                if let Some(err) = response.error {
                    actions.push(ConsoleAction::Error(err));
                }
            }
            _ => {
                egui::Frame::NONE.inner_margin(egui::Margin { left: 12, right: 8, top: 6, bottom: 4 }).show(ui, |ui| {
                    results::show_output(ui, &self.log);
                });
            }
        }
    }

    fn editor(&mut self, ui: &mut Ui, cx: &ConsoleCtx<'_>, cmds: &mut Vec<EditCmd>) {
        let pal = palette(cx.dark);
        let font = FontId::monospace(cx.font_size);
        let editor_id = self.editor_id();
        let focused = ui.memory(|m| m.has_focus(editor_id));
        let busy = self.is_busy();

        // Autocompletion keys: consumed before the TextEdit sees them.
        let mut accept = false;
        if focused {
            if self.completion.is_some() {
                ui.input_mut(|i| {
                    let n = self.completion.as_ref().map(|c| c.items.len()).unwrap_or(0);
                    if let Some(c) = self.completion.as_mut() {
                        if i.consume_key(Modifiers::NONE, Key::ArrowDown) && n > 0 {
                            c.selected = (c.selected + 1) % n;
                            c.scroll_to_selected = true;
                        }
                        if i.consume_key(Modifiers::NONE, Key::ArrowUp) && n > 0 {
                            c.selected = (c.selected + n - 1) % n;
                            c.scroll_to_selected = true;
                        }
                    }
                    if i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab) {
                        accept = true;
                    }
                    if i.consume_key(Modifiers::NONE, Key::Escape) {
                        self.completion = None;
                    }
                });
            }
            let open_req = ui.input_mut(|i| {
                let before = i.events.len();
                i.events.retain(|e| {
                    !matches!(e, Event::Key { key: Key::Space, pressed: true, modifiers, .. } if modifiers.ctrl)
                });
                // Some systems also emit the text " " with Ctrl held.
                let removed = before != i.events.len();
                if removed {
                    i.events.retain(|e| !matches!(e, Event::Text(t) if t == " "));
                }
                removed
            });
            if open_req && let Some((_, c)) = self.cursor {
                self.open_completion(c, cx, false);
            }
        }
        if accept {
            self.accept_completion(ui.ctx(), cx.driver);
        }

        let caret_line = self.line_col().0;
        // Layouter caching the highlighted LayoutJob.
        let dark = cx.dark;
        let driver = cx.driver;
        let font_size = cx.font_size;
        let hl_cache = &mut self.hl_cache;
        // Find bar matches are painted with a background (the selected one stronger).
        let search = self.find.as_ref().filter(|f| !f.query.is_empty()).map(|f| (f.query.clone(), f.case_sensitive));
        let selected = self.cursor;
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| -> Arc<egui::Galley> {
            let text = buf.as_str();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            dark.hash(&mut h);
            driver.hash(&mut h);
            font_size.to_bits().hash(&mut h);
            search.hash(&mut h);
            if search.is_some() {
                selected.hash(&mut h);
            }
            let key = h.finish();
            let job = match hl_cache {
                Some((k, job)) if *k == key => job.clone(),
                _ => {
                    let mut job = sql::highlight_for(text, driver, dark, FontId::monospace(font_size));
                    job.wrap.max_width = f32::INFINITY;
                    if let Some((q, case)) = &search {
                        let (other, current) = match_colors(dark);
                        let ranges: Vec<_> = find_matches(text, q, *case)
                            .into_iter()
                            .map(|m| {
                                let color = if Some(m) == selected { current } else { other };
                                (char_to_byte(text, m.0)..char_to_byte(text, m.1), color)
                            })
                            .collect();
                        mark_ranges(&mut job, &ranges);
                    }
                    *hl_cache = Some((key, job.clone()));
                    job
                }
            };
            ui.fonts_mut(|f| f.layout_job(job))
        };

        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let line_count = self.text.bytes().filter(|&b| b == b'\n').count() + 1;
        let digits = line_count.to_string().len().max(2) as f32;
        let gutter_w = digits * cx.font_size * 0.62 + 22.0;

        let mut typed_dot = false;
        let mut typed_letter = false;
        let mut new_cursor = None;
        let scroll_target = self.scroll_to.take();
        let mut popup_anchor = None;

        egui::Frame::NONE.fill(pal.editor_bg).show(ui, |ui| {
            let avail = ui.available_size();
            egui::ScrollArea::both()
                .id_salt(("editor_scroll", self.id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let caret_bg = ui.painter().add(egui::Shape::Noop);
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let content_h = (line_count as f32 * row_h + 8.0).max(avail.y - 4.0);
                        let (gutter_rect, _) = ui.allocate_exact_size(vec2(gutter_w, content_h), Sense::hover());
                        ui.add_space(14.0);
                        let width = (avail.x - gutter_w - 18.0).max(100.0);
                        let output = egui::TextEdit::multiline(&mut self.text)
                            .id(editor_id)
                            .font(font.clone())
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .margin(vec2(0.0, 6.0))
                            .background_color(egui::Color32::TRANSPARENT)
                            .desired_width(width)
                            .min_size(vec2(width, content_h))
                            .lock_focus(true)
                            .layouter(&mut layouter)
                            .show(ui);

                        let mut line = 1;
                        let mut start_of_line = true;
                        for row in &output.galley.rows {
                            if start_of_line {
                                let y = output.galley_pos.y + row.pos.y;
                                let on_caret = line == caret_line;
                                if on_caret && focused {
                                    let r = Rect::from_min_max(
                                        pos2(gutter_rect.left(), y),
                                        pos2(gutter_rect.left() + avail.x.max(gutter_w + width), y + row.height()),
                                    );
                                    ui.painter().set(caret_bg, egui::Shape::rect_filled(r, 0.0, pal.caret_row));
                                }
                                ui.painter().text(
                                    egui::pos2(gutter_rect.right() - 4.0, y),
                                    egui::Align2::RIGHT_TOP,
                                    line.to_string(),
                                    font.clone(),
                                    if on_caret { pal.gutter_fg_caret } else { pal.gutter_fg },
                                );
                                line += 1;
                            }
                            start_of_line = row.ends_with_newline;
                        }

                        if let Some(range) = output.cursor_range {
                            let r = range.as_sorted_char_range();
                            new_cursor = Some((r.start.0, r.end.0, range.primary));
                        }
                        if output.response.changed() {
                            typed_dot = ui.input(|i| {
                                i.events.iter().any(|e| matches!(e, Event::Text(t) if t.ends_with('.')))
                            });
                            typed_letter = ui.input(|i| {
                                i.events.iter().any(|e| matches!(e, Event::Text(t) if t.chars().count() == 1 && t.chars().all(is_ident_char)))
                            });
                        }
                        if self.completion.is_some()
                            && let Some((_, _, primary)) = new_cursor
                        {
                            let r = output.galley.pos_from_cursor(primary);
                            popup_anchor = Some(output.galley_pos + r.left_bottom().to_vec2() + vec2(0.0, 2.0));
                        }
                        if let Some(c) = scroll_target {
                            let r = output.galley.pos_from_cursor(CCursor::new(c)).translate(output.galley_pos.to_vec2());
                            ui.scroll_to_rect(r.expand2(vec2(40.0, row_h)), Some(egui::Align::Center));
                        }
                        output.response.context_menu(|ui| editor_menu(ui, busy, crate::db::plan::explain_statement("x", driver, true).is_some(), cmds));
                    });
                });
        });

        if let Some((a, b, _)) = new_cursor {
            self.cursor = Some((a, b));
        }
        if typed_dot && let Some((_, c)) = self.cursor {
            self.open_completion(c, cx, false);
        } else if typed_letter
            && cx.auto_complete
            && !cx.driver.is_nosql()
            && self.completion.is_none()
            && let Some((a, c)) = self.cursor
            && a == c
            && auto_trigger(&self.text, c)
        {
            self.open_completion(c, cx, true);
        }
        if self.completion.is_some() {
            self.refresh_completion(cx);
        }
        if let Some(anchor) = popup_anchor
            && self.completion.is_some()
            && self.completion_popup(ui, anchor)
        {
            self.accept_completion(ui.ctx(), cx.driver);
            ui.memory_mut(|m| m.request_focus(editor_id));
        }
        let popup_id = egui::Id::new(("completion", self.id));
        let over_popup = ui
            .input(|i| i.pointer.hover_pos())
            .and_then(|p| ui.ctx().layer_id_at(p))
            .is_some_and(|l| l.id == popup_id);
        if !ui.memory(|m| m.has_focus(editor_id)) && !over_popup {
            self.completion = None;
        }
    }

    /// Draws the popup; returns true if an item was clicked.
    fn completion_popup(&mut self, ui: &Ui, anchor: egui::Pos2) -> bool {
        let pal = palette(ui.visuals().dark_mode);
        let Some(comp) = self.completion.as_mut() else { return false };
        let mut clicked = false;
        egui::Area::new(egui::Id::new(("completion", self.id)))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).inner_margin(4).show(ui, |ui| {
                    ui.set_min_width(300.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // Without a minimum the list shrinks to the scroll area's default 64 px.
                    let list_h = (comp.items.len() as f32 * 24.0).min(240.0);
                    egui::ScrollArea::vertical().min_scrolled_height(list_h).max_height(240.0).show(ui, |ui| {
                        for (i, item) in comp.items.iter().enumerate() {
                            let sel = i == comp.selected;
                            let ico = match item.kind {
                                ItemKind::Keyword => None,
                                ItemKind::Table => Some(Ico::Table),
                                ItemKind::View => Some(Ico::View),
                                ItemKind::Column => Some(Ico::Column),
                                ItemKind::Schema => Some(Ico::Schema),
                            };
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(300.0), 24.0), Sense::click());
                            if sel {
                                ui.painter().rect_filled(rect, 5.0, pal.selection);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(rect, 5.0, pal.row_hover);
                            }
                            let cy = rect.center().y;
                            if let Some(ico) = ico {
                                icons::paint(ui.painter(), Rect::from_min_size(pos2(rect.left() + 6.0, cy - 8.0), egui::Vec2::splat(16.0)), ico, pal.icon, &pal, true);
                            }
                            let font = if item.kind == ItemKind::Keyword { theme::mono_bold(13.0) } else { FontId::monospace(13.0) };
                            let color = if item.kind == ItemKind::Keyword {
                                if pal.dark { egui::Color32::from_rgb(0xCF, 0x8E, 0x6D) } else { egui::Color32::from_rgb(0x00, 0x33, 0xB3) }
                            } else {
                                pal.text
                            };
                            ui.painter().text(pos2(rect.left() + 28.0, cy), egui::Align2::LEFT_CENTER, &item.text, font, color);
                            if !item.detail.is_empty() {
                                ui.painter().text(
                                    pos2(rect.right() - 8.0, cy),
                                    egui::Align2::RIGHT_CENTER,
                                    &item.detail,
                                    FontId::proportional(12.0),
                                    pal.muted,
                                );
                            }
                            if sel && comp.scroll_to_selected {
                                resp.scroll_to_me(None);
                            }
                            if resp.clicked() {
                                comp.selected = i;
                                clicked = true;
                            }
                        }
                    });
                });
            });
        comp.scroll_to_selected = false;
        clicked
    }

    fn open_completion(&mut self, cursor: usize, cx: &ConsoleCtx<'_>, auto: bool) {
        let chars: Vec<char> = self.text.chars().collect();
        let cursor = cursor.min(chars.len());
        let mut start = cursor;
        while start > 0 && is_ident_char(chars[start - 1]) {
            start -= 1;
        }
        self.completion = Some(Completion { start, selected: 0, items: Vec::new(), scroll_to_selected: false, auto });
        self.refresh_completion(cx);
    }

    fn refresh_completion(&mut self, cx: &ConsoleCtx<'_>) {
        let Some((_, cursor)) = self.cursor else {
            self.completion = None;
            return;
        };
        let Some(comp) = self.completion.as_ref() else { return };
        let chars: Vec<char> = self.text.chars().collect();
        let start = comp.start;
        if cursor < start || cursor > chars.len() || chars[start..cursor].iter().any(|c| !is_ident_char(*c)) {
            self.completion = None;
            return;
        }
        let prefix: String = chars[start..cursor].iter().collect();
        let qualifier = if start > 0 && chars[start - 1] == '.' {
            let mut q = start - 1;
            while q > 0 && (is_ident_char(chars[q - 1]) || matches!(chars[q - 1], '"' | '[' | ']' | '`')) {
                q -= 1;
            }
            let raw: String = chars[q..start - 1].iter().collect();
            Some(raw.trim_matches(|c| matches!(c, '"' | '[' | ']' | '`')).to_string())
        } else {
            None
        };
        let before: String = chars[..start].iter().collect();
        let context = sql_context(&scan(&before));
        let items = build_items(&prefix, qualifier.as_deref(), &self.text, cx.schemas.unwrap_or(&[]), context);
        let only_typed = items.len() == 1 && items[0].text.eq_ignore_ascii_case(&prefix);
        if items.is_empty() || (comp.auto && only_typed) {
            self.completion = None;
            return;
        }
        if let Some(c) = self.completion.as_mut() {
            c.selected = c.selected.min(items.len() - 1);
            c.items = items;
        }
    }

    fn accept_completion(&mut self, ctx: &egui::Context, driver: Driver) {
        let Some(comp) = self.completion.take() else { return };
        let Some(item) = comp.items.get(comp.selected) else { return };
        let Some((_, cursor)) = self.cursor else { return };
        let insert = match item.kind {
            ItemKind::Keyword => item.text.clone(),
            _ => {
                if item.text.chars().all(is_ident_char) && !item.text.starts_with(|c: char| c.is_ascii_digit()) {
                    item.text.clone()
                } else {
                    quote_ident(driver, &item.text)
                }
            }
        };
        let s = char_to_byte(&self.text, comp.start);
        let e = char_to_byte(&self.text, cursor.max(comp.start));
        self.text.replace_range(s..e, &insert);
        let new_pos = byte_to_char(&self.text, s + insert.len());
        self.set_cursor(ctx, new_pos, new_pos);
        self.hl_cache = None;
    }
}

/// Removes a key press with exactly these modifiers (⌘/Ctrl, ⌥, ⇧) from the frame's events.
fn take_key(i: &mut egui::InputState, mods: Modifiers, key: Key) -> bool {
    let before = i.events.len();
    i.events.retain(|e| {
        !matches!(e, Event::Key { key: k, pressed: true, modifiers: m, .. }
            if *k == key && m.command == mods.command && m.alt == mods.alt && m.shift == mods.shift
                && (mods.command || !m.ctrl))
    });
    before != i.events.len()
}

/// Editor context menu (DataGrip's editor popup, with its shortcuts).
fn editor_menu(ui: &mut Ui, busy: bool, analyze: bool, cmds: &mut Vec<EditCmd>) {
    ui.set_min_width(250.0);
    let delete_line = if cfg!(target_os = "macos") { "⌘⌫" } else { "Ctrl+Y" };
    let items: [(Option<EditCmd>, &str, &str, bool); 11] = [
        (Some(EditCmd::Explain), tr("Explain Plan"), "⇧⌘E", !busy),
        (Some(EditCmd::ExplainAnalyze), tr("Explain Analyze"), "", !busy && analyze),
        (None, "", "", true),
        (Some(EditCmd::Format), tr("Reformat Code"), "⌥⌘L", true),
        (Some(EditCmd::ToggleComment), tr("Comment with Line Comment"), "⌘/", true),
        (Some(EditCmd::Duplicate), tr("Duplicate Line or Selection"), "⌘D", true),
        (Some(EditCmd::DeleteLine), tr("Delete Line"), delete_line, true),
        (None, "", "", true),
        (Some(EditCmd::Find), tr("Find…"), "⌘F", true),
        (Some(EditCmd::Replace), tr("Replace…"), "⌘R", true),
        (Some(EditCmd::History), tr("Query History"), "⌥⌘E", true),
    ];
    for (cmd, label, shortcut, enabled) in items {
        match cmd {
            None => {
                ui.separator();
            }
            Some(c) => {
                if ui.add_enabled(enabled, egui::Button::new(label).shortcut_text(crate::shortcuts::format(shortcut))).clicked() {
                    cmds.push(c);
                    ui.close();
                }
            }
        }
    }
}

/// Background of the find matches (other ones, selected one).
fn match_colors(dark: bool) -> (egui::Color32, egui::Color32) {
    use egui::Color32;
    if dark {
        (Color32::from_rgb(0x32, 0x59, 0x3D), Color32::from_rgb(0x2E, 0x6E, 0x9E))
    } else {
        (Color32::from_rgb(0xFF, 0xEB, 0x99), Color32::from_rgb(0xF5, 0xC2, 0x4C))
    }
}

/// Paints the background of byte ranges of `job`, splitting its sections where needed.
fn mark_ranges(job: &mut LayoutJob, ranges: &[(std::ops::Range<usize>, egui::Color32)]) {
    use egui::text::{ByteIndex, LayoutSection};
    if ranges.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(job.sections.len() + ranges.len() * 2);
    for sec in job.sections.drain(..) {
        let (s, e) = (sec.byte_range.start.0, sec.byte_range.end.0);
        let mut cuts = vec![s, e];
        for (r, _) in ranges {
            for x in [r.start, r.end] {
                if x > s && x < e {
                    cuts.push(x);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let mut format = sec.format.clone();
            if let Some((_, c)) = ranges.iter().find(|(r, _)| r.start <= w[0] && w[1] <= r.end) {
                format.background = *c;
            }
            out.push(LayoutSection {
                leading_space: if w[0] == s { sec.leading_space } else { 0.0 },
                byte_range: ByteIndex(w[0])..ByteIndex(w[1]),
                format,
            });
        }
    }
    job.sections = out;
}

/// Non-overlapping occurrences (char ranges) of `needle` in `text`.
fn find_matches(text: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let fold = |c: char| if case_sensitive { c } else { c.to_lowercase().next().unwrap_or(c) };
    let hay: Vec<char> = text.chars().map(fold).collect();
    let pat: Vec<char> = needle.chars().map(fold).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + pat.len() <= hay.len() {
        if hay[i..i + pat.len()] == pat[..] {
            out.push((i, i + pat.len()));
            i += pat.len();
        } else {
            i += 1;
        }
    }
    out
}

/// `text` with the (sorted, non-overlapping) char ranges replaced by `with`.
fn replace_ranges(text: &str, ranges: &[(usize, usize)], with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for &(s, e) in ranges {
        let (bs, be) = (char_to_byte(text, s), char_to_byte(text, e));
        out.push_str(&text[last..bs]);
        out.push_str(with);
        last = be;
    }
    out.push_str(&text[last..]);
    out
}

/// Number of non-whitespace chars before char position `pos`.
fn nonspace_count(text: &str, pos: usize) -> usize {
    text.chars().take(pos).filter(|c| !c.is_whitespace()).count()
}

/// Char position right after the `n`-th non-whitespace char (inverse of [`nonspace_count`]).
fn pos_of_nonspace(text: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, c) in text.chars().enumerate() {
        if !c.is_whitespace() {
            seen += 1;
            if seen == n {
                return i + 1;
            }
        }
    }
    text.chars().count()
}

/// Char ranges `[start, end)` (without the `\n`) of the lines touched by the selection `a..b`.
/// A selection ending at the start of a line doesn't touch that line.
fn touched_lines(chars: &[char], a: usize, b: usize) -> Vec<(usize, usize)> {
    let (a, mut b) = (a.min(chars.len()), b.min(chars.len()));
    if b > a && chars[b - 1] == '\n' {
        b -= 1;
    }
    let mut start = a;
    while start > 0 && chars[start - 1] != '\n' {
        start -= 1;
    }
    let mut lines = Vec::new();
    loop {
        let end = chars[start..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| start + i);
        lines.push((start, end));
        if end >= b || end >= chars.len() {
            break;
        }
        start = end + 1;
    }
    lines
}

/// Start of the line containing char `pos`.
fn line_start(chars: &[char], pos: usize) -> usize {
    chars[..pos.min(chars.len())].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1)
}

/// Comment / uncomment with `-- ` the lines touched by the selection (DataGrip's ⌘/): if
/// every non-blank line is commented they are uncommented, otherwise `-- ` is added at the
/// start of each one. Without a selection the caret moves to the next line.
fn toggle_line_comment(text: &str, a: usize, b: usize) -> (String, usize, usize) {
    toggle_comment(text, a, b, "-- ")
}

/// `toggle_line_comment` with `// ` (KQL).
fn toggle_slash_comment(text: &str, a: usize, b: usize) -> (String, usize, usize) {
    toggle_comment(text, a, b, "// ")
}

/// `marker`: the two comment characters and a space.
fn toggle_comment(text: &str, a: usize, b: usize, marker: &'static str) -> (String, usize, usize) {
    let mark: Vec<char> = marker.chars().collect();
    let chars: Vec<char> = text.chars().collect();
    let (a, b) = (a.min(b).min(chars.len()), a.max(b).min(chars.len()));
    let lines = touched_lines(&chars, a, b);
    let content = |&(s, e): &(usize, usize)| chars[s..e].iter().any(|c| !c.is_whitespace());
    let mut targets: Vec<(usize, usize)> = lines.iter().copied().filter(content).collect();
    if targets.is_empty() {
        targets = lines.clone();
    }
    let commented = |&(s, e): &(usize, usize)| {
        let first = (s..e).find(|&i| !chars[i].is_whitespace());
        first.is_some_and(|i| i + 1 < e && chars[i] == mark[0] && chars[i + 1] == mark[1])
    };
    let uncomment = targets.iter().filter(|l| content(l)).count() > 0 && targets.iter().all(commented);
    // Edits (position, removed chars, inserted text), ascending.
    let edits: Vec<(usize, usize, &str)> = targets
        .iter()
        .map(|&(s, e)| {
            if uncomment {
                let i = (s..e).find(|&i| !chars[i].is_whitespace()).unwrap_or(s);
                let del = if i + 2 < e && chars[i + 2] == ' ' { 3 } else { 2 };
                (i, del, "")
            } else {
                (s, 0, marker)
            }
        })
        .collect();
    let mut out = String::with_capacity(text.len() + edits.len() * 3);
    let mut last = 0;
    for &(pos, del, ins) in &edits {
        out.extend(&chars[last..pos]);
        out.push_str(ins);
        last = pos + del;
    }
    out.extend(&chars[last..]);
    let map = |p: usize, after: bool| -> usize {
        let mut acc: isize = 0;
        for &(pos, del, ins) in &edits {
            let ins = ins.chars().count() as isize;
            if p < pos || (p == pos && del == 0 && !after) {
                break;
            }
            if p < pos + del {
                return (pos as isize + acc) as usize;
            }
            acc += ins - del as isize;
        }
        (p as isize + acc) as usize
    };
    if a != b {
        return (out, map(a, false), map(b, true));
    }
    // No selection: the caret goes down one line, same column (like DataGrip).
    let new: Vec<char> = out.chars().collect();
    let c = map(a, true);
    let col = c - line_start(&new, c);
    let next = new[c..].iter().position(|&ch| ch == '\n').map(|i| c + i + 1);
    let caret = match next {
        Some(ns) => {
            let ne = new[ns..].iter().position(|&ch| ch == '\n').map_or(new.len(), |i| ns + i);
            ns + col.min(ne - ns)
        }
        None => c,
    };
    (out, caret, caret)
}

/// Duplicates the selection after itself (selecting the copy) or, without one, the caret's
/// line below it (caret on the same column of the copy).
fn duplicate_lines(text: &str, a: usize, b: usize) -> (String, usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let (a, b) = (a.min(b).min(chars.len()), a.max(b).min(chars.len()));
    let mut out: String;
    if a != b {
        out = chars[..b].iter().collect();
        out.extend(&chars[a..b]);
        out.extend(&chars[b..]);
        return (out, b, b + (b - a));
    }
    let (s, e) = touched_lines(&chars, a, a)[0];
    out = chars[..e].iter().collect();
    out.push('\n');
    out.extend(&chars[s..e]);
    out.extend(&chars[e..]);
    let caret = a + (e - s) + 1;
    (out, caret, caret)
}

/// Deletes the lines touched by the selection (or the caret's line), with their line break.
/// The caret stays at the same column of the line that takes their place.
fn delete_lines(text: &str, a: usize, b: usize) -> (String, usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let (a, b) = (a.min(b).min(chars.len()), a.max(b).min(chars.len()));
    let lines = touched_lines(&chars, a, b);
    let (s, e) = (lines[0].0, lines[lines.len() - 1].1);
    let col = a - s;
    let (rs, re) = if e < chars.len() {
        (s, e + 1)
    } else if s > 0 {
        (s - 1, e)
    } else {
        (s, e)
    };
    let mut out: String = chars[..rs].iter().collect();
    out.extend(&chars[re..]);
    let new: Vec<char> = out.chars().collect();
    let ls = line_start(&new, rs.min(new.len()));
    let le = new[ls..].iter().position(|&c| c == '\n').map_or(new.len(), |i| ls + i);
    let caret = ls + col.min(le - ls);
    (out, caret, caret)
}

/// Executed statements for the history popup: most recent first, without repeats
/// (whitespace-insensitive) and containing `filter` (case-insensitive).
fn history_entries<'a>(log: &'a [OutputEntry], filter: &str) -> Vec<&'a OutputEntry> {
    let filter = filter.trim().to_lowercase();
    let mut seen = std::collections::HashSet::new();
    log.iter()
        .rev()
        .filter(|e| !e.statement.trim().is_empty())
        .filter(|e| seen.insert(e.statement.split_whitespace().collect::<Vec<_>>().join(" ")))
        .filter(|e| filter.is_empty() || e.statement.to_lowercase().contains(&filter))
        .collect()
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn matches_prefix(candidate: &str, prefix: &str) -> bool {
    prefix.is_empty() || candidate.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Where the word being completed sits, to list the likely candidates first.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SqlContext {
    Any,
    /// Right after FROM / JOIN / UPDATE / INTO / TABLE, or a comma in that list.
    Tables,
    /// In SELECT, WHERE, ON, ORDER / GROUP BY, SET, HAVING…
    Columns,
}

/// Token before the word being completed.
#[derive(Debug, PartialEq)]
enum Prev {
    Start,
    Word(String),
    Punct(char),
}

/// The text before the word being completed: whether it ends inside a string or comment,
/// its last token and its last clause keyword.
struct Scan {
    in_literal: bool,
    prev: Prev,
    clause: Option<&'static str>,
}

const TABLE_CLAUSES: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE"];
const COLUMN_CLAUSES: &[&str] =
    &["SELECT", "WHERE", "ON", "BY", "SET", "HAVING", "AND", "OR", "NOT", "DISTINCT", "WHEN", "THEN", "ELSE", "CASE"];

fn scan(before: &str) -> Scan {
    let chars: Vec<char> = before.chars().collect();
    let literal = |prev, clause| Scan { in_literal: true, prev, clause };
    let (mut i, mut prev, mut clause) = (0, Prev::Start, None);
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            match chars[i..].iter().position(|&x| x == '\n') {
                Some(n) => i += n + 1,
                None => return literal(prev, clause),
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            match chars[i + 2..].windows(2).position(|w| w == ['*', '/']) {
                Some(n) => i += n + 4,
                None => return literal(prev, clause),
            }
        } else if matches!(c, '\'' | '"' | '`' | '[') {
            let close = if c == '[' { ']' } else { c };
            match chars[i + 1..].iter().position(|&x| x == close) {
                // A string or a quoted name: an operand, like a word that is not a keyword.
                Some(n) => {
                    prev = Prev::Word(c.to_string());
                    i += n + 2;
                }
                None => return literal(prev, clause),
            }
        } else if is_ident_char(c) {
            let from = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[from..i].iter().collect();
            let upper = word.to_ascii_uppercase();
            if let Some(k) = TABLE_CLAUSES.iter().chain(COLUMN_CLAUSES).find(|k| **k == upper) {
                clause = Some(*k);
            }
            prev = Prev::Word(word);
        } else {
            if !c.is_whitespace() {
                prev = Prev::Punct(c);
                if c == ';' {
                    clause = None;
                }
            }
            i += 1;
        }
    }
    Scan { in_literal: false, prev, clause }
}

fn sql_context(scan: &Scan) -> SqlContext {
    match scan.clause {
        Some(k) if TABLE_CLAUSES.contains(&k) => {
            let after_keyword = matches!(&scan.prev, Prev::Word(w) if w.eq_ignore_ascii_case(k));
            if after_keyword || scan.prev == Prev::Punct(',') { SqlContext::Tables } else { SqlContext::Any }
        }
        Some(_) => SqlContext::Columns,
        None => SqlContext::Any,
    }
}

/// Whether typing at `cursor` should open the completion list by itself: at the start of a
/// word that follows a keyword or an operator, not after a table name (where an alias goes),
/// inside strings or comments, or in the middle of a word.
fn auto_trigger(text: &str, cursor: usize) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if cursor == 0 || cursor > chars.len() || chars.get(cursor).is_some_and(|c| is_ident_char(*c)) {
        return false;
    }
    let mut start = cursor;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    if chars[start].is_ascii_digit() || (start > 0 && chars[start - 1] == '.') {
        return false;
    }
    let before: String = chars[..start].iter().collect();
    let scan = scan(&before);
    !scan.in_literal
        && match &scan.prev {
            Prev::Start => true,
            Prev::Punct(c) => !matches!(c, '.' | ')' | ']'),
            Prev::Word(w) => !w.eq_ignore_ascii_case("AS") && sql::KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(w)),
        }
}

/// Tables named anywhere in the text (lowercase), whose columns go first in column positions.
fn referenced_tables(text: &str, schemas: &[SchemaInfo]) -> std::collections::HashSet<String> {
    let words: std::collections::HashSet<String> =
        text.split(|c: char| !is_ident_char(c)).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    schemas.iter().flat_map(|s| &s.tables).map(|t| t.name.to_lowercase()).filter(|t| words.contains(t)).collect()
}

fn build_items(prefix: &str, qualifier: Option<&str>, text: &str, schemas: &[SchemaInfo], context: SqlContext) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let table_kind = |k: ObjectKind| if k == ObjectKind::Table { ItemKind::Table } else { ItemKind::View };
    let item = |text: &str, kind: ItemKind, detail: String, rank: u8| CompletionItem { text: text.to_string(), kind, detail, rank };

    if let Some(q) = qualifier {
        // schema.<table>
        for s in schemas.iter().filter(|s| s.name.eq_ignore_ascii_case(q)) {
            for t in &s.tables {
                if matches_prefix(&t.name, prefix) {
                    items.push(item(&t.name, table_kind(t.kind), s.name.clone(), 0));
                }
            }
        }
        // table.<column> or alias.<column>
        let table_name = resolve_alias(q, text, schemas).unwrap_or_else(|| q.to_string());
        for s in schemas {
            for t in s.tables.iter().filter(|t| t.name.eq_ignore_ascii_case(&table_name)) {
                for c in &t.columns {
                    if matches_prefix(&c.name, prefix) {
                        items.push(item(&c.name, ItemKind::Column, c.data_type.clone(), 0));
                    }
                }
            }
        }
    } else {
        let (object_rank, keyword_rank) = match context {
            SqlContext::Any => (0, 0),
            SqlContext::Tables => (0, 2),
            SqlContext::Columns => (1, 2),
        };
        for s in schemas {
            if schemas.len() > 1 && matches_prefix(&s.name, prefix) {
                items.push(item(&s.name, ItemKind::Schema, tr("schema").into(), object_rank.max(1)));
            }
            for t in &s.tables {
                if matches_prefix(&t.name, prefix) {
                    items.push(item(&t.name, table_kind(t.kind), s.name.clone(), object_rank));
                }
            }
        }
        if context != SqlContext::Tables {
            // Columns of the tables in the query first, so their names win the dedup.
            let referenced = referenced_tables(text, schemas);
            let mut seen_cols = std::collections::HashSet::new();
            for in_query in [true, false] {
                let rank = match context {
                    SqlContext::Columns if in_query => 0,
                    SqlContext::Columns => 3,
                    _ => 0,
                };
                let listed = !prefix.is_empty() || (context == SqlContext::Columns && in_query);
                for t in schemas.iter().flat_map(|s| &s.tables).filter(|t| referenced.contains(&t.name.to_lowercase()) == in_query) {
                    for c in &t.columns {
                        if listed && matches_prefix(&c.name, prefix) && seen_cols.insert(c.name.to_lowercase()) {
                            items.push(item(&c.name, ItemKind::Column, t.name.clone(), rank));
                        }
                    }
                }
            }
        }
        for kw in sql::KEYWORDS {
            if matches_prefix(kw, prefix) {
                items.push(item(kw, ItemKind::Keyword, String::new(), keyword_rank));
            }
        }
    }
    items.sort_by(|a, b| {
        let a_exact = a.text.eq_ignore_ascii_case(prefix);
        let b_exact = b.text.eq_ignore_ascii_case(prefix);
        b_exact
            .cmp(&a_exact)
            .then(a.rank.cmp(&b.rank))
            .then(a.kind.cmp(&b.kind))
            .then_with(|| a.text.to_lowercase().cmp(&b.text.to_lowercase()))
    });
    items.dedup_by(|a, b| a.text == b.text && a.kind == b.kind);
    items.truncate(200);
    items
}

/// Looks for `<table> [AS] <alias>` in the text to resolve an alias to its table.
fn resolve_alias(alias: &str, text: &str, schemas: &[SchemaInfo]) -> Option<String> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';'))
        .filter(|w| !w.is_empty())
        .collect();
    let is_table = |w: &str| -> Option<String> {
        let last = w.rsplit('.').next()?.trim_matches(|c| matches!(c, '"' | '[' | ']' | '`'));
        schemas
            .iter()
            .flat_map(|s| &s.tables)
            .find(|t| t.name.eq_ignore_ascii_case(last))
            .map(|t| t.name.clone())
    };
    for i in 0..words.len() {
        let next = words.get(i + 1).copied();
        let next2 = words.get(i + 2).copied();
        let aliased = next.is_some_and(|n| n.eq_ignore_ascii_case(alias))
            || (next.is_some_and(|n| n.eq_ignore_ascii_case("as")) && next2.is_some_and(|n| n.eq_ignore_ascii_case(alias)));
        if aliased && let Some(t) = is_table(words[i]) {
            return Some(t);
        }
    }
    None
}

/// Pending changes counter and warning for tables without a primary key.
fn edit_status_icon(ui: &mut Ui, v: &ResultView) {
    let p = pal(ui);
    let Some(e) = &v.edit else { return };
    if !e.target.has_pk {
        let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
        icons::paint(ui.painter(), Rect::from_center_size(rect.center(), egui::Vec2::splat(15.0)), Ico::Problems, p.warn, &p, true);
        resp.on_hover_text(tr(
            "The table has no primary key: each row is identified by all of its original \
             values. If a change does not affect exactly one row, nothing is saved.",
        ));
    }
    let n = e.state.pending_count();
    if n > 0 {
        ui.add_space(4.0);
        let text = if n == 1 { tr("1 change").to_string() } else { trf("{n} changes", &[("n", &n)]) };
        ui.label(RichText::new(text).color(p.accent).size(12.5));
    }
}

/// Thin full-width horizontal line (separator inside an island).
fn hline(ui: &mut Ui) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.island_border));
}

/// Compact toolbar row with room for 26 px icon buttons.
fn toolbar_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(vec2(w, 32.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(w, 32.0));
        ui.add_space(8.0);
        ui.spacing_mut().item_spacing.x = 2.0;
        add(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnInfo, TableInfo};

    struct TableState {
        console: Console,
        submits: usize,
    }

    fn table_harness() -> egui_kittest::Harness<'static, TableState> {
        let mut console = Console::new_table(1, 1, None, "main".into(), "notes".into());
        let mut view = ResultView::new(
            crate::db::ResultSet {
                columns: vec!["id".into(), "note".into()],
                rows: vec![
                    vec![Some("1".into()), Some("First note".into())],
                    vec![Some("2".into()), Some("Second note".into())],
                ],
                ..Default::default()
            },
            1,
        );
        view.set_edit_target(Ok(crate::ui::editing::EditTarget {
            schema: "main".into(),
            table: "notes".into(),
            col_names: view.rs.columns.clone(),
            key_cols: vec![0],
            binary_cols: vec![false, false],
            has_pk: true,
            insert_only: false,
        }));
        view.begin_edit(0, 1);
        console.results.push(view);
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1000.0, 500.0))
            .build_ui_state(
                move |ui, state: &mut TableState| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), false);
                        return;
                    }
                    let cx = ConsoleCtx {
                        connections: &[],
                        schemas: None,
                        driver: Driver::Sqlite,
                        dark: false,
                        font_size: 14.0,
                        columns: None,
                        auto_complete: true,
                    };
                    for action in state.console.show_table(ui, &cx) {
                        if let ConsoleAction::Submit(0) = action {
                            state.submits += 1;
                        }
                    }
                },
                TableState {
                    console,
                    submits: 0,
                },
            );
        h.run_steps(3);
        h
    }

    #[test]
    fn cell_editor_stages_the_same_pending_values_and_row_change_save_as_the_grid() {
        use egui_kittest::kittest::Queryable;
        let mut h = table_harness();
        h.state_mut().console.table.as_mut().unwrap().save_on_row_change = true;
        h.get_by_label("Cell Editor").click(); h.run_steps(3);
        h.get_all_by_value("First note").next().unwrap().click(); h.run_steps(2);
        h.key_press_modifiers(Modifiers::COMMAND, Key::A); h.run_steps(1);
        h.input_mut().events.push(Event::Text("Panel edit".into())); h.run_steps(2);
        assert!(h.state().console.has_pending_edits());
        h.get_by_label("Apply").click(); h.run_steps(2);
        assert_eq!(h.state().console.results[0].edit.as_ref().unwrap().state.changes.get(&(0, 1)), Some(&Some("Panel edit".into())));
        h.key_press(Key::ArrowDown); h.run_steps(3);
        assert_eq!(h.state().submits, 1);
    }

    #[test]
    fn record_inspector_follows_selection_and_pending_values_without_changing_the_grid() {
        use egui_kittest::kittest::Queryable;
        let mut h = table_harness();
        h.state_mut().console.results[0].finish_inline_edit();
        let view = &mut h.state_mut().console.results[0];
        view.edit.as_mut().unwrap().state.set_value(&view.rs, crate::ui::editing::RowRef::Existing(0), 1, Some("Pending value".into()));
        h.get_by_label("Record").click(); h.run_steps(3);
        assert_eq!(h.state().console.table.as_ref().unwrap().inspector_mode, crate::ui::inspector::InspectorMode::Record);
        assert!(h.query_by_label("Record 1").is_some());
        assert!(h.query_all_by_label("Pending value").next().is_some());
        h.key_press(Key::ArrowDown); h.run_steps(3);
        assert!(h.query_by_label("Record 2").is_some());
        assert!(h.query_by_label("Second note").is_some());
        assert_eq!(h.state().submits, 0);
        assert!(h.state().console.has_pending_edits());
    }

    #[test]
    fn table_view_toggles_roundtrip_and_inspector_is_available_without_editing() {
        let view = TableViewOptions { cell_editor: true, filter_sort: true, inspector: false };
        let view: TableViewOptions = serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
        assert!(view.cell_editor && view.filter_sort && !view.inspector);
    }

    #[test]
    fn table_save_on_row_change_is_an_explicit_opt_in() {
        use egui_kittest::kittest::Queryable;
        let mut h = table_harness();
        assert!(!h.state().console.table.as_ref().unwrap().save_on_row_change);
        h.get_by_label("Save on row change").click();
        h.run_steps(3);
        assert!(h.state().console.table.as_ref().unwrap().save_on_row_change);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
        h.run_steps(3);
        h.input_mut()
            .events
            .push(Event::Text("Changed note".into()));
        h.run_steps(2);
        h.key_press(Key::ArrowDown);
        h.run_steps(3);
        assert_eq!(h.state().submits, 1);
        assert!(h.state().console.results[0].is_dirty());
    }

    #[test]
    #[ignore = "Requires a GPU; renders the database workspace using synthetic data"]
    fn render_database_workspace() {
        use crate::app::{ConnState, ConnStatus};
        use crate::db::{ConnectionConfig, DatabaseInfo};
        use crate::ui::explorer::{self, ExplorerState};
        use crate::ui::filter_sort::{Condition, Node, Operator, SortRule};
        use egui_kittest::kittest::Queryable;
        let columns: Vec<_> = [("id", "int"), ("customer", "varchar(120)"), ("status", "varchar(20)"), ("total", "decimal(10,2)"), ("created_at", "datetime"), ("note", "text")]
            .into_iter().map(|(name, ty)| ColumnInfo { name: name.into(), data_type: ty.into(), nullable: name == "note", primary_key: name == "id" }).collect();
        let mut console = Console::new_table(1, 1, Some("shop".into()), "shop".into(), "orders".into());
        let rows = (1..=48).map(|id| vec![Some(id.to_string()), Some(format!("Customer {id:02}")), Some(if id % 3 == 0 { "pending" } else { "paid" }.into()), Some(format!("{}.00", id * 25)), Some("2026-10-23 12:00:00".into()), Some("Delivery instructions\nLeave at reception.".into())]).collect();
        let mut view = ResultView::new(crate::db::ResultSet { columns: columns.iter().map(|c| c.name.clone()).collect(), rows, ..Default::default() }, 42);
        view.set_edit_target(Ok(crate::ui::editing::EditTarget { schema: "shop".into(), table: "orders".into(), col_names: view.rs.columns.clone(), key_cols: vec![0], binary_cols: vec![false; 6], has_pk: true, insert_only: false }));
        view.begin_edit(0, 5); view.finish_inline_edit();
        console.results.push(view);
        console.last_statements.push("SELECT * FROM `shop`.`orders` ORDER BY `id` LIMIT 1000".into());
        let table = console.table.as_mut().unwrap();
        table.loaded = true; table.save_on_row_change = true;
        table.info = Some(Ok(crate::db::object_info::ObjectInfo { rows: Some(48), engine: Some("InnoDB".into()), auto_increment: Some(49), row_format: Some("Dynamic".into()), created: Some("2026-10-01 09:30:00".into()), modified: Some("2026-10-23 12:00:00".into()), data_bytes: Some(16384), index_bytes: Some(16384), collation: Some("utf8mb4_unicode_ci".into()), comment: Some("Customer orders".into()), ..Default::default() }));
        table.filter_sort.filter.children = vec![Node::Condition(Condition { enabled: true, column: "status".into(), operator: Operator::Equal, value: "paid".into(), second: String::new() }).into(), Node::Condition(Condition { enabled: true, column: "total".into(), operator: Operator::GreaterEqual, value: "100".into(), second: String::new() }).into()];
        table.filter_sort.sort.push(SortRule { enabled: true, column: "total".into(), descending: true });
        let schemas = vec![SchemaInfo { name: "shop".into(), tables: ["customers", "orders", "products"].into_iter().map(|name| TableInfo { name: name.into(), kind: ObjectKind::Table, columns: columns.clone() }).collect() }];
        let mut cfg = ConnectionConfig::new(1, Driver::MariaDb); cfg.name = "Development".into(); cfg.database = "shop".into();
        let mut conn = ConnState::new(cfg); conn.status = ConnStatus::Connected;
        conn.databases = vec![DatabaseInfo { name: "shop".into(), current: true, schemas: Some(schemas) }, DatabaseInfo { name: "analytics".into(), current: false, schemas: None }];
        let connections = vec![ConnSummary { id: 1, name: "Development".into(), driver: Driver::MariaDb, status: ConnStatusKind::Connected, databases: vec!["shop".into()], default_db: Some("shop".into()) }];
        let mut explorer = ExplorerState::default(); explorer.expanded.extend(["c:1".into(), "d:1:shop".into()]);
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1600.0, 1050.0)).build_ui_state(move |ui, console: &mut Console| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            egui::Panel::left("fixture_explorer").exact_size(250.0).show(ui, |ui| { explorer::show(ui, &mut explorer, std::slice::from_ref(&conn)); });
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| { ui.label("Objects"); ui.separator(); ui.strong("orders"); });
                ui.separator();
                console.show_table(ui, &ConsoleCtx { connections: &connections, schemas: conn.databases[0].schemas.as_deref(), driver: Driver::MariaDb, dark: true, font_size: 14.0, columns: Some(&columns), auto_complete: true });
            });
        }, console);
        h.run_steps(4);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/workspace-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(h.query_by_label("customers").is_none(), "table folders start collapsed");
        assert!(h.query_by_label("Backups").is_some());
        h.render().unwrap().save(dir.join("workspace.png")).unwrap();
        h.get_by_label("Filter & Sort").click(); h.run_steps(4);
        assert!(h.query_by_label("Sort By").is_some());
        h.render().unwrap().save(dir.join("filter-sort.png")).unwrap();
        h.get_by_label("Filter & Sort").click(); h.get_by_label("Cell Editor").click(); h.run_steps(4);
        assert!(h.query_by_value("Delivery instructions\nLeave at reception.").is_some());
        h.render().unwrap().save(dir.join("cell-editor.png")).unwrap();
        h.get_by_label("Cell Editor").click(); h.get_by_label("Record").click(); h.run_steps(4);
        h.render().unwrap().save(dir.join("record-inspector.png")).unwrap();
        h.get_by_label("Column").click(); h.run_steps(4);
        h.render().unwrap().save(dir.join("column-inspector.png")).unwrap();
    }

    #[test]
    #[ignore = "Requires a GPU; writes the completion list opened while typing to target/completion-snapshots"]
    fn render_completion_while_typing() {
        let connections = vec![ConnSummary { id: 1, name: "Development".into(), driver: Driver::Postgres, status: ConnStatusKind::Connected, databases: vec!["shop".into()], default_db: Some("shop".into()) }];
        let col = |name: &str, ty: &str| ColumnInfo { name: name.into(), data_type: ty.into(), nullable: true, primary_key: false };
        let table = |name: &str, kind| TableInfo { name: name.into(), kind, columns: vec![col("id", "integer"), col("fecha", "date"), col("cliente", "text")] };
        let schemas = vec![SchemaInfo {
            name: "public".into(),
            tables: vec![table("Albaranes", ObjectKind::Table), table("Archivos", ObjectKind::Table), table("Articulos", ObjectKind::Table), table("Agenda_semanal", ObjectKind::View), table("Clientes", ObjectKind::Table), table("Facturas", ObjectKind::Table)],
        }];
        let (mut first, mut focus) = (true, true);
        let mut h = egui_kittest::Harness::builder().with_size(vec2(900.0, 420.0)).build_ui_state(move |ui, console: &mut Console| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            console.show_console(ui, &ConsoleCtx { connections: &connections, schemas: Some(&schemas), driver: Driver::Postgres, dark: true, font_size: 14.0, columns: None, auto_complete: true });
            if std::mem::take(&mut focus) { ui.memory_mut(|m| m.request_focus(console.editor_id())); }
        }, Console::new(1, "Query 1".into(), String::new(), Some(1)));
        h.run_steps(2);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/completion-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for c in "select * from A".chars() {
            h.input_mut().events.push(Event::Text(c.to_string()));
            h.run_steps(1);
        }
        h.run_steps(2);
        let items: Vec<String> = h.state().completion.as_ref().expect("opened while typing").items.iter().map(|i| i.text.clone()).collect();
        assert_eq!(items[..4], ["Albaranes", "Archivos", "Articulos", "Agenda_semanal"]);
        h.render().unwrap().save(dir.join("tables-after-from.png")).unwrap();
        for c in "lbaranes where f".chars() {
            h.input_mut().events.push(Event::Text(c.to_string()));
            h.run_steps(1);
        }
        h.run_steps(2);
        assert_eq!(h.state().completion.as_ref().expect("opened after WHERE").items[0].text, "fecha");
        h.render().unwrap().save(dir.join("columns-after-where.png")).unwrap();
    }

    #[test]
    #[ignore = "Requires a GPU; writes a table editor preview to target/table-editor-snapshots"]
    fn render_table_keyboard_navigation() {
        let mut h = table_harness();
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/table-editor-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        h.render()
            .expect("render")
            .save(dir.join("improve-ux.png"))
            .unwrap();
    }

    fn schemas() -> Vec<SchemaInfo> {
        vec![SchemaInfo {
            name: "main".into(),
            tables: vec![TableInfo {
                name: "users".into(),
                kind: ObjectKind::Table,
                columns: vec![ColumnInfo { name: "email".into(), data_type: "TEXT".into(), nullable: true, primary_key: false }],
            }],
        }]
    }

    #[test]
    fn alias_resolution() {
        let s = schemas();
        assert_eq!(resolve_alias("u", "SELECT u. FROM users u", &s).as_deref(), Some("users"));
        assert_eq!(resolve_alias("x", "select * from main.users AS x", &s).as_deref(), Some("users"));
        assert!(resolve_alias("z", "select 1", &s).is_none());
    }

    fn shop() -> Vec<SchemaInfo> {
        let col = |name: &str| ColumnInfo { name: name.into(), data_type: "TEXT".into(), nullable: true, primary_key: false };
        let table = |name: &str, cols: &[&str]| TableInfo { name: name.into(), kind: ObjectKind::Table, columns: cols.iter().map(|c| col(c)).collect() };
        vec![SchemaInfo {
            name: "main".into(),
            tables: vec![
                table("Albaranes", &["numero", "fecha"]),
                table("Archivos", &["ruta", "nombre"]),
                table("Clientes", &["nombre", "alta"]),
            ],
        }]
    }

    /// Items for the text where `|` marks the caret, as the editor computes them.
    fn complete(marked: &str) -> Vec<String> {
        let cursor = marked.find('|').unwrap();
        let text = marked.replace('|', "");
        let before = &text[..cursor];
        let start = before.rfind(|c: char| !is_ident_char(c)).map(|i| i + 1).unwrap_or(0);
        let context = sql_context(&scan(&text[..start]));
        build_items(&text[start..cursor], None, &text, &shop(), context).into_iter().map(|i| i.text).collect()
    }

    #[test]
    fn tables_first_after_from() {
        let items = complete("select * from A|");
        assert_eq!(items[..2], ["Albaranes", "Archivos"]);
        assert!(items[2..].iter().all(|i| sql::KEYWORDS.contains(&i.as_str())));
        assert_eq!(complete("SELECT * FROM Clientes c JOIN A|")[..2], ["Albaranes", "Archivos"]);
        assert_eq!(complete("select * from Clientes, a|")[..2], ["Albaranes", "Archivos"]);
        // No columns where a table goes.
        assert!(!complete("update n|").contains(&"nombre".to_string()));
    }

    #[test]
    fn query_columns_first_after_select_and_where() {
        // Columns of Clientes (in the query) before the other tables' columns.
        let items = complete("select | from Clientes");
        assert_eq!(items[..5], ["alta", "nombre", "Albaranes", "Archivos", "Clientes"]);
        assert!(!items.contains(&"ruta".to_string()));
        let items = complete("select * from Archivos where r|");
        assert_eq!(items[0], "ruta");
        let items = complete("select * from Albaranes where f|");
        assert_eq!(items[0], "fecha");
    }

    #[test]
    fn opens_by_itself_only_where_a_name_goes() {
        let at = |marked: &str| auto_trigger(&marked.replace('|', ""), marked.find('|').unwrap());
        assert!(at("select * from A|"));
        assert!(at("s|"));
        assert!(at("select a, b|"));
        assert!(at("select * from t where x = y|"));
        // After a table name (an alias goes there), AS, a dot, digits, inside words.
        assert!(!at("select * from Clientes c|"));
        assert!(!at("select count(*) as t|"));
        assert!(!at("select c.n|"));
        assert!(!at("select 1|"));
        assert!(!at("select * from A|rchivos"));
        // Strings and comments.
        assert!(!at("select 'A|"));
        assert!(!at("-- from A|"));
        assert!(!at("/* from A|"));
        assert!(at("/* note */ select * from A|"));
        assert!(at("select 'x', A|"));
    }

    #[test]
    fn column_items_after_dot() {
        let s = schemas();
        let items = build_items("em", Some("users"), "", &s, SqlContext::Any);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "email");
    }

    /// Runs a (text, a, b) → (text, a, b) transformation on a text where `|` marks the caret
    /// or `[` `]` the selection, and returns the result marked the same way.
    fn run(f: fn(&str, usize, usize) -> (String, usize, usize), marked: &str) -> String {
        let mut text = String::new();
        let (mut a, mut b) = (None, None);
        for c in marked.chars() {
            let n = text.chars().count();
            match c {
                '|' => (a, b) = (Some(n), Some(n)),
                '[' => a = Some(n),
                ']' => b = Some(n),
                _ => text.push(c),
            }
        }
        let (out, na, nb) = f(&text, a.unwrap(), b.unwrap());
        let mut res = String::new();
        for (i, c) in out.chars().chain(std::iter::once('\0')).enumerate() {
            if na == nb && i == na {
                res.push('|');
            } else if na != nb && i == na {
                res.push('[');
            } else if na != nb && i == nb {
                res.push(']');
            }
            if c != '\0' {
                res.push(c);
            }
        }
        res
    }

    #[test]
    fn toggle_comment_lines() {
        let t = toggle_line_comment;
        // Caret: comments the line and moves down keeping the column.
        assert_eq!(run(t, "sel|ect 1\nfrom t"), "-- select 1\nfrom t|");
        assert_eq!(run(t, "-- sel|ect 1\nfrom t"), "select 1\nfro|m t");
        assert_eq!(run(t, "select 1\nfr|om"), "select 1\n-- fr|om");
        // Selection over several lines (blank ones untouched), kept around the same text.
        assert_eq!(run(t, "[a\n\n  b\nc]\nd"), "[-- a\n\n--   b\n-- c]\nd");
        assert_eq!(run(t, "[-- a\n\n  --b\n-- c]\nd"), "[a\n\n  b\nc]\nd");
        // Mixed lines: comments all of them; a selection ending at column 0 skips that line.
        assert_eq!(run(t, "[-- a\nb\n]c"), "[-- -- a\n-- b\n]c");
        // Empty text / blank line.
        assert_eq!(run(t, "|"), "-- |");
        assert_eq!(run(t, "a\n|\nb"), "a\n-- \nb|");
        // KQL: `//`, and `--` is not a comment there.
        let k = toggle_slash_comment;
        assert_eq!(run(k, "[T\n  take 1]"), "[// T\n//   take 1]");
        assert_eq!(run(k, "[// T\n  //take 1]"), "[T\n  take 1]");
        assert_eq!(run(k, "-- x|"), "// -- x|");
    }

    #[test]
    fn duplicate_and_delete_lines() {
        assert_eq!(run(duplicate_lines, "ab|c\nd"), "abc\nab|c\nd");
        assert_eq!(run(duplicate_lines, "x\nlast|"), "x\nlast\nlast|");
        assert_eq!(run(duplicate_lines, "a[bc]d"), "abc[bc]d");
        assert_eq!(run(delete_lines, "one\ntw|o\nthree"), "one\nth|ree");
        assert_eq!(run(delete_lines, "one\nlast|"), "one|");
        assert_eq!(run(delete_lines, "on|ly"), "|");
        assert_eq!(run(delete_lines, "a\n[b\nc]\nd"), "a\n|d");
        assert_eq!(run(delete_lines, "a\n[b\n]c"), "a\n|c");
    }

    #[test]
    fn find_and_replace_text() {
        assert_eq!(find_matches("Select select SELECT", "select", false), vec![(0, 6), (7, 13), (14, 20)]);
        assert_eq!(find_matches("Select select SELECT", "select", true), vec![(7, 13)]);
        assert_eq!(find_matches("aaaa", "aa", true), vec![(0, 2), (2, 4)]);
        assert!(find_matches("abc", "", false).is_empty());
        assert_eq!(find_matches("añoÑo", "ño", false), vec![(1, 3), (3, 5)]);
        assert_eq!(replace_ranges("añoÑo x", &[(1, 3), (3, 5)], "N"), "aNN x");
        // Caret mapping across reformatting (only whitespace changes).
        let (a, b) = ("select  a ,b", "SELECT a,\n       b");
        let n = nonspace_count(a, 10);
        assert_eq!(n, 7);
        assert_eq!(pos_of_nonspace(b, n), 8);
        assert_eq!(pos_of_nonspace(b, 0), 0);
    }

    #[test]
    fn history_dedup_and_filter() {
        let e = |t: &str, s: &str| OutputEntry { time: t.into(), statement: s.into(), message: String::new(), is_error: false };
        let log = vec![
            e("10:00:00", "select 1"),
            e("10:00:01", ""),
            e("10:00:02", "SELECT * FROM users"),
            e("10:00:03", "select  1"),
        ];
        let h = history_entries(&log, "");
        assert_eq!(h.iter().map(|e| e.time.as_str()).collect::<Vec<_>>(), ["10:00:03", "10:00:02"]);
        let h = history_entries(&log, " users ");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].statement, "SELECT * FROM users");
    }

    #[test]
    fn match_backgrounds_split_sections() {
        let mut job = sql::highlight("select abc from t", false, FontId::monospace(13.0));
        let c = egui::Color32::RED;
        mark_ranges(&mut job, &[(8..10, c)]);
        let text: String = job.sections.iter().map(|s| &job.text[s.byte_range.start.0..s.byte_range.end.0]).collect();
        assert_eq!(text, "select abc from t");
        let marked: Vec<_> = job
            .sections
            .iter()
            .filter(|s| s.format.background == c)
            .map(|s| &job.text[s.byte_range.start.0..s.byte_range.end.0])
            .collect();
        assert_eq!(marked, ["bc"]);
    }

    #[test]
    fn indices_to_close() {
        assert_eq!(CloseResults::One(1).indices(3), vec![1]);
        assert!(CloseResults::One(5).indices(3).is_empty());
        assert_eq!(CloseResults::Others(1).indices(3), vec![0, 2]);
        assert_eq!(CloseResults::Left(2).indices(4), vec![0, 1]);
        assert_eq!(CloseResults::Right(1).indices(4), vec![2, 3]);
        assert!(CloseResults::Right(usize::MAX).indices(4).is_empty());
        assert_eq!(CloseResults::All.indices(2), vec![0, 1]);
    }

    #[test]
    fn reindex_after_close() {
        assert_eq!(remap_index(0, &[1]), Some(0));
        assert_eq!(remap_index(1, &[1]), None);
        assert_eq!(remap_index(3, &[0, 2]), Some(1));
        // The active one is closed: moves to the one taking its place or to the last one.
        assert_eq!(tab_after_close(ResultTab::Result(1), &[1], 2), ResultTab::Result(1));
        assert_eq!(tab_after_close(ResultTab::Result(2), &[2], 2), ResultTab::Result(1));
        assert_eq!(tab_after_close(ResultTab::Result(2), &[0], 2), ResultTab::Result(1));
        assert_eq!(tab_after_close(ResultTab::Result(0), &[0, 1], 0), ResultTab::Output);
        assert_eq!(tab_after_close(ResultTab::Output, &[0], 1), ResultTab::Output);
    }

    #[test]
    fn close_console_results() {
        let rs = |stmt: &str| crate::db::ResultSet { statement: stmt.into(), ..Default::default() };
        let mut c = Console::new(1, "c".into(), String::new(), None);
        c.results = (0..4).map(|i| ResultView::new(rs(&format!("select {i}")), i)).collect();
        c.result_tab = ResultTab::Result(2);
        c.replace_view = Some(3);
        assert_eq!(c.close_results(CloseResults::One(0)), vec![0]);
        assert_eq!(c.result_tab, ResultTab::Result(1));
        assert_eq!(c.replace_view, Some(2));
        assert_eq!(c.results[0].rs.statement, "select 1");
        c.close_results(CloseResults::Others(1));
        assert_eq!(c.results.len(), 1);
        assert_eq!(c.results[0].rs.statement, "select 2");
        assert_eq!(c.result_tab, ResultTab::Result(0));
        assert_eq!(c.replace_view, None);
        c.close_results(CloseResults::All);
        assert!(c.results.is_empty());
        assert_eq!(c.result_tab, ResultTab::Output);
    }
}
