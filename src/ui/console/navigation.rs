//! Cached execution previews and File Structure selection.
use super::*;
use std::ops::Range;
mod cache;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Structure,
    Switcher,
}
pub const ACTIONS: &[(Command, &str, &str)] = &[
    (
        Command::Structure,
        "File Structure…",
        if cfg!(target_os = "macos") {
            "⌘F12 / ⌥⌘7"
        } else {
            "Ctrl+F12 / Alt+7"
        },
    ),
    (Command::Switcher, "Recent tabs / Switcher", "Ctrl+Tab"),
];
pub(super) fn menu(ui: &mut Ui, cmds: &mut Vec<EditCmd>) {
    for &(cmd, label, key) in ACTIONS {
        if ui
            .add(egui::Button::new(tr(label)).shortcut_text(crate::shortcuts::format(key)))
            .clicked()
        {
            cmds.push(EditCmd::Navigate(cmd));
            ui.close();
        }
    }
}
#[derive(Clone)]
pub struct Saved {
    pub line_numbers: bool,
}
impl Default for Saved {
    fn default() -> Self {
        Self { line_numbers: true }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Popup {
    Structure,
}
pub(super) struct GutterRun {
    revision: u64,
    source: (Option<ConnId>, Option<String>, Driver),
    range: Range<usize>,
}
#[derive(Default)]
pub struct State {
    pub caret: Option<usize>,
    pub anchor: Option<usize>,
    pub pending_location: Option<usize>,
    pub saved: Saved,
    pub pending: Vec<Command>,
    popup: Option<Popup>,
    input: String,
    error: Option<&'static str>,
    selected: Vec<usize>,
    highlight: usize,
    focus: bool,
    snapshot: Option<u64>,
    source: Option<(Option<ConnId>, Option<String>, Driver)>,
    revision: Option<u64>,
    index: Option<Arc<cache::TextIndex>>,
    scope_key: Option<(Option<(usize, usize)>, bool, Vec<usize>)>,
    scopes: Vec<Range<usize>>,
    scope_chars: Vec<Range<usize>>,
    scope_epoch: u64,
    outline_cache: Option<cache::OutlineCache>,
    filter_key: Option<(u64, Driver, String)>,
    visible: Arc<Vec<usize>>,
    #[cfg(test)]
    pub(crate) index_builds: usize,
    #[cfg(test)]
    scope_builds: usize,
    #[cfg(test)]
    filter_builds: usize,
    #[cfg(test)]
    outline_builds: usize,
    pub(super) gutter_run: Option<GutterRun>,
}
impl State {
    pub fn is_open(&self) -> bool {
        self.popup.is_some()
    }
    fn open(&mut self, popup: Popup) {
        self.popup = Some(popup);
        self.input.clear();
        self.error = None;
        self.selected.clear();
        self.highlight = 0;
        self.focus = true;
        self.snapshot = None;
        self.source = None;
    }
    pub(super) fn execution_ranges(
        &self,
        text: &str,
        cursor: Option<(usize, usize)>,
        driver: Driver,
    ) -> Vec<Range<usize>> {
        let temporary;
        let index = if let Some(index) = self
            .index
            .as_ref()
            .filter(|i| i.revision == self.revision() && i.driver == driver)
        {
            index.as_ref()
        } else {
            temporary = cache::TextIndex::new(text, driver, self.revision());
            &temporary
        };
        if self.popup == Some(Popup::Structure) {
            return self
                .selected
                .iter()
                .filter_map(|&i| index.statements.get(i).map(|s| s.start..s.end))
                .collect();
        }
        if let Some((a, b)) = cursor.filter(|(a, b)| a != b)
            && !text[index.byte(a)..index.byte(b)].trim().is_empty()
        {
            return vec![index.byte(a)..index.byte(b)];
        }
        index
            .statement_at(text, cursor.map_or(index.characters.len() - 1, |(_, b)| b))
            .map(|i| vec![index.statements[i].start..index.statements[i].end])
            .unwrap_or_default()
    }
    pub(crate) fn sync_text(&mut self, text: &str) {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hash);
        self.sync_changed_text(text, hash.finish());
    }
    fn sync_changed_text(&mut self, text: &str, revision: u64) {
        let _ = text;
        if self.revision == Some(revision) {
            return;
        }
        self.revision = Some(revision);
        self.index = None;
        self.scope_key = None;
        self.filter_key = None;
        if self.snapshot.is_some_and(|s| s != revision) {
            self.popup = None;
            self.snapshot = None;
            self.source = None;
        }
    }
}
fn outline_chars(
    galley: &egui::Galley,
    ranges: &[Range<usize>],
    origin: egui::Pos2,
    color: egui::Color32,
) -> Vec<egui::Shape> {
    let mut offset = 0;
    let row_starts: Vec<_> = galley
        .rows
        .iter()
        .map(|row| {
            let start = offset;
            offset += row.char_count_including_newline().0;
            start
        })
        .collect();
    let cursor = |position: usize| {
        let mut row = row_starts
            .partition_point(|&s| s <= position)
            .saturating_sub(1);
        if row > 0 && position == row_starts[row] && !galley.rows[row - 1].ends_with_newline {
            row -= 1;
        }
        egui::epaint::text::cursor::LayoutCursor {
            row,
            column: egui::text::CharIndex(
                position
                    .saturating_sub(row_starts[row])
                    .min(galley.rows[row].char_count_excluding_newline().0),
            ),
        }
    };
    ranges
        .iter()
        .filter_map(|range| {
            if range.is_empty() || galley.rows.is_empty() || range.end > offset {
                return None;
            }
            let first = cursor(range.start);
            let mut last = cursor(range.end);
            // Selecting the newline does not select the next empty editor row.
            if last.column.0 == 0 && last.row > first.row {
                last.row -= 1;
                last.column = egui::text::CharIndex(galley.rows[last.row].glyphs.len());
            }
            let head = &galley.rows[first.row];
            let tail = &galley.rows[last.row];
            let start_x = head.pos.x + head.x_offset(first.column) - 0.5;
            let end_x = tail.pos.x + tail.x_offset(last.column) + 0.5;
            let top = head.min_y() - 0.5;
            let bottom = tail.max_y() + 0.5;
            let mut points = if first.row == last.row {
                vec![
                    pos2(start_x, top),
                    pos2(end_x, top),
                    pos2(end_x, bottom),
                    pos2(start_x, bottom),
                ]
            } else {
                let left = galley.rows[first.row + 1..=last.row]
                    .iter()
                    .map(|row| row.pos.x - 0.5)
                    .fold(start_x, f32::min);
                let right = galley.rows[first.row..last.row]
                    .iter()
                    .map(|row| row.rect().right() + 0.5)
                    .fold(end_x, f32::max);
                vec![
                    pos2(start_x, top),
                    pos2(right, top),
                    pos2(right, tail.min_y()),
                    pos2(end_x, tail.min_y()),
                    pos2(end_x, bottom),
                    pos2(left, bottom),
                    pos2(left, head.max_y()),
                    pos2(start_x, head.max_y()),
                ]
            };
            for point in &mut points {
                *point += origin.to_vec2();
            }
            Some(egui::Shape::closed_line(points, Stroke::new(1.0, color)))
        })
        .collect()
}
impl Console {
    pub fn navigate_to(&mut self, ctx: &egui::Context, position: usize, select_to: Option<usize>) {
        let n = self.text.chars().count();
        let position = position.min(n);
        self.set_cursor(ctx, select_to.unwrap_or(position).min(n), position);
        self.scroll_to = Some(position);
        ctx.memory_mut(|m| m.request_focus(self.editor_id()));
    }
    pub fn open_structure(&mut self, driver: Driver) {
        self.navigation.sync_text(&self.text);
        self.navigation.ensure_index(&self.text, driver);
        self.navigation.open(Popup::Structure);
        self.navigation.snapshot = Some(self.navigation.revision());
        self.navigation.source = Some((self.conn, self.db.clone(), driver));
        let index = self.navigation.index();
        self.navigation.highlight = index
            .statement_at(
                &self.text,
                self.cursor.map_or(index.characters.len() - 1, |(_, b)| b),
            )
            .unwrap_or(0);
        self.navigation.selected = vec![self.navigation.highlight];
        self.completion = None;
    }
    pub(super) fn navigation_keys(&mut self, ui: &Ui, cmds: &mut Vec<EditCmd>) {
        if self.navigation.is_open() || !ui.memory(|m| m.has_focus(self.editor_id())) {
            return;
        }
        ui.input_mut(|i| {
            if take_key(i, Modifiers::COMMAND, Key::F12)
                || take_key(
                    i,
                    if cfg!(target_os = "macos") {
                        Modifiers::COMMAND | Modifiers::ALT
                    } else {
                        Modifiers::ALT
                    },
                    Key::Num7,
                )
            {
                cmds.push(EditCmd::Navigate(Command::Structure));
            }
        });
    }
    pub fn navigation_command(
        &mut self,
        ctx: &egui::Context,
        driver: Driver,
        cmd: Command,
        actions: &mut Vec<ConsoleAction>,
    ) {
        if let Some(editor) = &mut self.routine
            && editor.page != crate::ui::routine_dialog::RoutinePage::Definition
            && cmd == Command::Structure
        {
            editor.page = crate::ui::routine_dialog::RoutinePage::Definition;
            self.navigation.pending.push(cmd);
            ctx.request_repaint();
            return;
        }
        match cmd {
            Command::Structure => self.open_structure(driver),
            Command::Switcher => actions.push(ConsoleAction::Navigate(cmd)),
        }
    }
    pub(super) fn navigation_ui(
        &mut self,
        ui: &Ui,
        cx: &ConsoleCtx<'_>,
        actions: &mut Vec<ConsoleAction>,
    ) {
        let ctx = ui.ctx();
        for cmd in std::mem::take(&mut self.navigation.pending) {
            self.navigation_command(ctx, cx.driver, cmd, actions);
        }
        if let Some(target) = self.navigation.gutter_run.take()
            && target.revision == self.navigation.revision()
            && target.source == (self.conn, self.db.clone(), cx.driver)
        {
            actions.push(ConsoleAction::RunStatements(
                self.prepare_ranges(cx.driver, vec![target.range]),
            ));
        }
        if self
            .navigation
            .source
            .as_ref()
            .is_some_and(|s| s != &(self.conn, self.db.clone(), cx.driver))
        {
            self.navigation.popup = None;
            self.navigation.source = None;
        }
        if self.navigation.popup.is_none() {
            return;
        }
        self.completion = None;
        let (mut open, mut close, mut navigate, mut execute, mut all) =
            (true, false, None, false, false);
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        egui::Window::new(tr("File Structure")).id(egui::Id::new(("editor_navigation",self.id))).open(&mut open).collapsible(false).resizable(true).default_width(540.0).anchor(egui::Align2::CENTER_TOP,vec2(0.0,85.0)).show(ctx,|ui|{
   let r=ui.add(egui::TextEdit::singleline(&mut self.navigation.input).hint_text(tr("Type to filter")).desired_width(f32::INFINITY));
   if self.navigation.focus {r.request_focus();self.navigation.focus=false;}
   if r.changed(){self.navigation.highlight=usize::MAX;self.navigation.selected.clear();}

                    let index = self.navigation.index();
                    let ss = &index.statements;
                    let visible = self.navigation.filtered_statements();
                    if (up || down) && let Some(index) = move_choice(&visible, self.navigation.highlight, up, down) { self.navigation.highlight = index; self.navigation.selected = vec![index]; }
                    if !visible.contains(&self.navigation.highlight) && let Some(&index)=visible.first(){self.navigation.highlight=index;self.navigation.selected=vec![index];}
                    ui.label(tr("Selected statements are outlined in the editor. Ctrl/Command-click selects several."));
                    egui::ScrollArea::vertical().max_height(300.0).show_rows(ui, ui.spacing().interact_size.y, visible.len(), |ui, rows| {
                        for &i in &visible[rows] {
                            let s = &ss[i];
                            let r = ui.add(egui::Button::selectable(self.navigation.selected.contains(&i), &index.structure[i].label).wrap_mode(egui::TextWrapMode::Truncate)).on_hover_text(&s.text);
                            if r.clicked() {
                                let multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                                let shift = ui.input(|i| i.modifiers.shift);
                                if shift { self.navigation.selected = (i.min(self.navigation.highlight)..=i.max(self.navigation.highlight)).collect(); }
                                else if multi { if self.navigation.selected.contains(&i) { self.navigation.selected.retain(|j| *j != i); } else { self.navigation.selected.push(i); } }
                                else { self.navigation.selected = vec![i]; }
                                self.navigation.highlight = i;
                            }
                            if r.double_clicked() { navigate = Some(index.statement_chars[i].start); }
                            if r.secondary_clicked() && !self.navigation.selected.contains(&i) {self.navigation.selected=vec![i];self.navigation.highlight=i;}
                            r.context_menu(|ui| { if ui.add_enabled(!self.is_busy(), egui::Button::new(tr("Execute selected statements"))).clicked() { execute = true; ui.close(); } });
                        }
                    });
                    ui.horizontal(|ui| {
                        if !visible.is_empty() && (enter || ui.button(tr("Go to statement")).clicked()) {
                            navigate = index.statement_chars.get(self.navigation.highlight).map(|r| r.start);
                        }
                        execute |= ui.add_enabled(!self.is_busy() && !self.navigation.selected.is_empty(), egui::Button::new(tr("Execute selected statements"))).clicked();
                        all = ui.add_enabled(!self.is_busy() && !ss.is_empty(), egui::Button::new(tr("Run all statements"))).clicked();
                    });
                    if ss.is_empty() { ui.label(tr("No executable statements")); }

   close|=ui.button(tr("Cancel / Close")).clicked();
  });
        if (execute || all) && !self.is_busy() {
            let statements = self.prepare_run(cx.driver, all);
            actions.push(ConsoleAction::RunStatements(statements));
            close = true;
        }
        if let Some(p) = navigate {
            self.navigate_to(ctx, p, None);
            close = true;
        }
        if close || esc || !open {
            self.navigation.popup = None;
            self.navigation.snapshot = None;
            self.navigation.source = None;
            ctx.memory_mut(|m| m.request_focus(self.editor_id()));
        }
    }
}
fn move_choice(rows: &[usize], current: usize, up: bool, down: bool) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let at = rows.iter().position(|&i| i == current).unwrap_or(0);
    Some(
        rows[if up {
            (at + rows.len() - 1) % rows.len()
        } else if down {
            (at + 1) % rows.len()
        } else {
            at
        }],
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;
    struct Fixture {
        console: Console,
        ran: Vec<Vec<String>>,
        dark: bool,
    }
    fn harness(text: &str, execution: bool) -> egui_kittest::Harness<'static, Fixture> {
        let mut console = Console::new(1, "Navigation test".into(), text.into(), None);
        console.cursor = Some((0, 0));
        if execution {
            console.open_structure(Driver::Postgres);
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(1000.0, 720.0))
            .build_ui_state(
                move |ui, f: &mut Fixture| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), true);
                        return;
                    }
                    let cx = ConsoleCtx {
                        connections: &[],
                        schemas: None,
                        driver: Driver::Postgres,
                        dark: f.dark,
                        font_size: 14.0,
                        columns: None,
                        auto_complete: false,
                    };
                    let actions = if f.console.routine.is_some() {
                        f.console.show_routine(ui, &cx)
                    } else {
                        f.console.show_console(ui, &cx)
                    };
                    for action in actions {
                        if let ConsoleAction::RunStatements(s) = action {
                            f.ran.push(s);
                        }
                    }
                },
                Fixture {
                    console,
                    ran: Vec::new(),
                    dark: true,
                },
            );
        h.run_steps(3);
        h
    }
    #[test]
    fn many_selected_scopes_reuse_geometry_and_only_paint_visible_rows() {
        let text: String = (0..5000).map(|i| format!("SELECT {i};\n")).collect();
        let ctx = egui::Context::default();
        let mut galley = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
            galley = Some(ctx.fonts_mut(|f| {
                f.layout_no_wrap(text.clone(), FontId::monospace(14.0), egui::Color32::WHITE)
            }));
        });
        output.textures_delta.clear();
        let galley = galley.unwrap();
        let mut state = State::new(&text);
        state.ensure_index(&text, Driver::Postgres);
        state.open(Popup::Structure);
        state.selected = (0..5000).collect();
        state.update_scope(&text, Some((0, 0)), Driver::Postgres);
        let clip = Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 100.0));
        for origin in [egui::Pos2::ZERO, pos2(0.0, -7000.0)] {
            let shapes = state.outlines(&galley, origin, egui::Color32::GREEN, clip);
            assert!(!shapes.is_empty() && shapes.len() < 16);
            assert!(
                shapes
                    .iter()
                    .all(|s| s.visual_bounding_rect().intersects(clip))
            );
        }
        assert_eq!(
            state.outline_builds, 1,
            "scrolling must reuse scope geometry"
        );
    }
    #[test]
    fn text_analysis_and_structure_filter_are_cached_across_idle_frames() {
        let script: String = (0..1000).map(|i| format!("SELECT {i};\n")).collect();
        let mut h = harness(&script, false);
        h.run_steps(3);
        let initial = h.state().console.navigation.index_builds;
        let scopes = h.state().console.navigation.scope_builds;
        let outlines = h.state().console.navigation.outline_builds;
        h.run_steps(5);
        assert_eq!(h.state().console.navigation.index_builds, initial);
        assert_eq!(h.state().console.navigation.scope_builds, scopes);
        assert_eq!(h.state().console.navigation.outline_builds, outlines);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 12, None);
        h.run_steps(2);
        assert_eq!(h.state().console.navigation.index_builds, initial);
        assert!(h.state().console.navigation.scope_builds > scopes);
        h.state_mut().console.open_structure(Driver::Postgres);
        h.run_steps(3);
        let filters = h.state().console.navigation.filter_builds;
        h.run_steps(5);
        assert_eq!(h.state().console.navigation.index_builds, initial);
        assert_eq!(h.state().console.navigation.filter_builds, filters);
        h.state_mut().console.navigation.input = "SELECT 999".into();
        h.run_steps(3);
        assert_eq!(h.state().console.navigation.filter_builds, filters + 1);
        assert_eq!(h.state().console.navigation.visible.as_ref(), &vec![999]);
    }
    #[test]
    fn a_dialect_change_rebuilds_the_index_without_rebasing_text() {
        let text = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE a SET x=1; END; SELECT 2;";
        let mut state = State::new(text);
        let revision = state.revision();
        state.sync_revision(text, Driver::Postgres, revision);
        assert_eq!(state.index().statements.len(), 3);
        let builds = state.index_builds;
        state.sync_revision(text, Driver::Sqlite, revision);
        assert_eq!(state.index().statements.len(), 2);
        assert_eq!(state.index_builds, builds + 1);
        state.sync_revision(text, Driver::Sqlite, revision);
        assert_eq!(state.index_builds, builds + 1);
    }
    #[test]
    fn structure_arrows_preview_and_execute_only_the_selected_statement_without_editing_sql() {
        let text = "SELECT 'سلام';\n\nSELECT 2;\nSELECT 3;";
        let mut h = harness(text, true);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        let console = &h.state().console;
        let r =
            console
                .navigation
                .execution_ranges(&console.text, console.cursor, Driver::Postgres);
        assert_eq!(&text[r[0].clone()], "SELECT 2");
        h.get_by_label("Execute selected statements").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 2".to_string()]]);
        assert_eq!(h.state().console.text, text);
        assert!(!h.state().console.navigation.is_open());
    }
    #[test]
    fn structure_cancel_and_source_changes_never_execute() {
        let mut h = harness("SELECT 1; SELECT 2;", true);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().ran.is_empty());
        assert!(!h.state().console.navigation.is_open());
        h.state_mut().console.open_structure(Driver::Postgres);
        h.state_mut().console.conn = Some(99);
        h.run_steps(2);
        assert!(!h.state().console.navigation.is_open());
        assert!(h.state().ran.is_empty());
    }
    #[test]
    fn gutter_button_executes_its_own_statement() {
        let mut h = harness("SELECT 1;\n\nSELECT 'سلام';", false);
        h.get_by_label("Execute statement 2").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 'سلام'".to_string()]]);
    }
    #[test]
    fn execution_duration_is_visible_beside_sql_and_disappears_when_that_sql_changes() {
        let mut h = harness("SELECT 1;\nSELECT 'سلام';", false);
        let console = &mut h.state_mut().console;
        let statements = console.prepare_run(Driver::Postgres, false);
        console.execution_started(&statements);
        console.execution_finished(
            &[],
            &[crate::worker::StatementExecution {
                statement: statements[0].clone(),
                results: 0..0,
                elapsed: std::time::Duration::from_millis(12),
                failed: false,
            }],
        );
        h.run_steps(2);
        h.get_by_label("12 ms");
        assert_eq!(h.state().console.text, "SELECT 1;\nSELECT 'سلام';");
        h.state_mut().console.text.insert_str(0, "-- سلام\n");
        h.run_steps(2);
        h.get_by_label("12 ms");
        h.state_mut().console.text = "-- سلام\nSELECT 99;\nSELECT 'سلام';".into();
        h.run_steps(2);
        assert!(h.query_by_label("12 ms").is_none());
    }
    #[test]
    fn long_script_only_builds_execution_time_labels_for_visible_lines() {
        fn labels(shape: &egui::Shape) -> usize {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().map(labels).sum(),
                egui::Shape::Text(t) => usize::from(t.galley.text() == "12 ms"),
                _ => 0,
            }
        }
        let mut h = harness(&"SELECT 1;\n".repeat(1000), false);
        let c = &mut h.state_mut().console;
        let statements = c.prepare_run(Driver::Postgres, true);
        c.execution_started(&statements);
        let executions: Vec<_> = statements
            .into_iter()
            .map(|statement| crate::worker::StatementExecution {
                statement,
                results: 0..0,
                elapsed: std::time::Duration::from_millis(12),
                failed: false,
            })
            .collect();
        c.execution_finished(&[], &executions);
        h.run_steps(2);
        let count: usize = h.output().shapes.iter().map(|s| labels(&s.shape)).sum();
        assert!(
            count > 0 && count < 64,
            "only viewport timing labels should be built, got {count}"
        );
        let ctx = h.ctx.clone();
        let end = h.state().console.text.chars().count() - 1;
        h.state_mut().console.navigate_to(&ctx, end, None);
        h.run_steps(4);
        let count: usize = h.output().shapes.iter().map(|s| labels(&s.shape)).sum();
        assert!(
            count > 0 && count < 64,
            "timing labels must remain visible after scrolling"
        );
    }
    #[test]
    fn long_script_only_builds_gutter_controls_for_visible_lines() {
        fn run_icons(shape: &egui::Shape) -> usize {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().map(run_icons).sum(),
                egui::Shape::Text(t) => usize::from(t.galley.text() == "▶"),
                _ => 0,
            }
        }
        let mut h = harness(&"SELECT 1;\n".repeat(1000), false);
        let icons: usize = h.output().shapes.iter().map(|s| run_icons(&s.shape)).sum();
        assert!(
            icons > 0 && icons < 64,
            "only viewport gutter controls should be built, got {icons}"
        );
        let ctx = h.ctx.clone();
        let end = h.state().console.text.chars().count() - 1;
        h.state_mut().console.navigate_to(&ctx, end, None);
        h.run_steps(4);
        h.get_by_label("Execute statement 1000").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 1".to_string()]]);
    }
    #[test]
    fn backspace_before_a_trailing_newline_rebuilds_gutter_offsets() {
        let text = format!("{}  \n", "SELECT 1;".repeat(6));
        assert_eq!(text.len(), 57);
        let mut h = harness(&text, false);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 56, None);
        h.run_steps(2);
        h.key_press(Key::Backspace);
        h.run_steps(2);
        assert_eq!(h.state().console.text.len(), 56);
        assert!(h.state().console.text.ends_with('\n'));
        assert!(h.state().ran.is_empty());
    }
    #[test]
    fn execution_scope_preserves_syntax_colors_and_has_an_unfilled_outline() {
        fn visit<'a>(
            shape: &'a egui::Shape,
            text: &str,
            editors: &mut Vec<&'a egui::Galley>,
            paths: &mut Vec<&'a egui::epaint::PathShape>,
        ) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        visit(shape, text, editors, paths);
                    }
                }
                egui::Shape::Text(t) if t.galley.text() == text => editors.push(&t.galley),
                egui::Shape::Path(path) => paths.push(path),
                _ => {}
            }
        }
        let text = "SELECT id,\n       email\nFROM customers;\n\nSELECT 2;";
        let mut h = harness(text, false);
        for dark in [true, false] {
            h.state_mut().dark = dark;
            theme::apply(&h.ctx, dark);
            h.run_steps(2);
            let mut editors = Vec::new();
            let mut paths = Vec::new();
            for clipped in &h.output().shapes {
                visit(&clipped.shape, text, &mut editors, &mut paths);
            }
            assert!(!editors.is_empty());
            for galley in editors {
                assert!(
                    galley
                        .job
                        .sections
                        .iter()
                        .all(|s| s.format.background == egui::Color32::TRANSPARENT),
                    "Execution scope must not fill the syntax-highlighted text background"
                );
                let plain =
                    sql::highlight_for(text, Driver::Postgres, dark, FontId::monospace(14.0));
                assert_eq!(
                    galley.job.sections, plain.sections,
                    "Execution scope must preserve the syntax colors"
                );
            }
            let border = if dark {
                egui::Color32::from_rgb(61, 122, 73)
            } else {
                egui::Color32::from_rgb(186, 168, 255)
            };
            assert!(
                paths.iter().any(|path| path.closed
                    && path.fill == egui::Color32::TRANSPARENT
                    && path.stroke.width == 1.0
                    && matches!(path.stroke.color,egui::epaint::ColorMode::Solid(c) if c==border)),
                "The current statement must have DataGrip's unfilled boxed outline"
            );
        }
    }
    #[test]
    fn unicode_insertions_do_not_reuse_offsets_inside_a_multibyte_character() {
        let mut h = harness("a\né\n", false);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 0, None);
        h.run_steps(2);
        h.input_mut().events.push(Event::Text("é".into()));
        h.run_steps(1);
        assert_eq!(h.state().console.text, "éa\né\n");
        assert!(h.state().ran.is_empty());
    }
    #[test]
    fn gutter_statement_targets_update_in_the_same_frame_as_paste() {
        let mut h = harness("SELECT 2;", false);
        let ctx = h.ctx.clone();
        h.state_mut().console.navigate_to(&ctx, 0, None);
        h.run_steps(2);
        h.input_mut()
            .events
            .push(Event::Paste("SELECT 'سلام';\n\n".into()));
        h.run_steps(1);
        assert_eq!(h.state().console.text, "SELECT 'سلام';\n\nSELECT 2;");
        h.get_by_label("Execute statement 2").click();
        h.run_steps(2);
        assert_eq!(h.state().ran, vec![vec!["SELECT 2".to_owned()]]);
    }
    #[test]
    fn mixed_unicode_edits_undo_and_redo_keep_source_ranges_valid() {
        let mut h = harness("SELECT 'سلام';\r\nSELECT 'é';\n", false);
        let ctx = h.ctx.clone();
        let mut seed = 0x5eed_u32;
        for step in 0..80 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = h.state().console.text.chars().count();
            h.state_mut()
                .console
                .navigate_to(&ctx, (seed as usize) % (len + 1), None);
            h.run_steps(1);
            match step % 6 {
                0 => h.input_mut().events.push(Event::Text("é".into())),
                1 => h.key_press(Key::Backspace),
                2 => h
                    .input_mut()
                    .events
                    .push(Event::Paste("\nSELECT 'سلام';\n".into())),
                3 => h.key_press(Key::Delete),
                4 => h.key_press_modifiers(Modifiers::COMMAND, Key::Z),
                _ => h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z),
            }
            h.run_steps(1);
            let c = &h.state().console;
            for range in c
                .navigation
                .execution_ranges(&c.text, c.cursor, Driver::Postgres)
            {
                assert!(
                    c.text.get(range).is_some(),
                    "stale source range after edit {step}"
                );
            }
            assert!(h.state().ran.is_empty());
        }
    }
    #[test]
    fn multiple_statement_scopes_have_separate_boxes_and_skip_unselected_sql() {
        fn collect<'a>(shape: &'a egui::Shape, out: &mut Vec<&'a egui::epaint::PathShape>) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, out);
                    }
                }
                egui::Shape::Path(path) if matches!(path.stroke.color,egui::epaint::ColorMode::Solid(c) if c==egui::Color32::from_rgb(61,122,73)) => {
                    out.push(path)
                }
                _ => {}
            }
        }
        let mut h = harness("SELECT 1; SELECT 2; SELECT 3;", true);
        h.state_mut().console.navigation.selected = vec![0, 2];
        h.run_steps(2);
        let mut paths = Vec::new();
        for clipped in &h.output().shapes {
            collect(&clipped.shape, &mut paths);
        }
        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .all(|p| p.fill == egui::Color32::TRANSPARENT && p.closed)
        );
        let a = Rect::from_points(&paths[0].points);
        let b = Rect::from_points(&paths[1].points);
        assert!(
            a.right() < b.left(),
            "The box must not include the unselected middle statement"
        );
    }
    #[test]
    fn structure_filter_enter_navigates_to_the_visible_statement() {
        let mut h = harness("SELECT 1;\nSELECT 99;", false);
        h.state_mut()
            .console
            .navigation
            .pending
            .push(Command::Structure);
        h.run_steps(3);
        h.input_mut().events.push(Event::Text("99".into()));
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().console.cursor, Some((10, 10)));
        assert!(h.state().ran.is_empty());
    }
    #[test]
    fn multiple_statements_execute_in_source_order_and_run_all_includes_every_statement() {
        let mut h = harness("SELECT 1; SELECT 2; SELECT 3;", true);
        h.state_mut().console.navigation.selected = vec![2, 0, 2];
        h.run_steps(2);
        assert_eq!(
            h.state().console.navigation.selected,
            vec![2, 0, 2],
            "rendering must preserve multi-selection"
        );
        h.get_by_label("Execute selected statements").click();
        h.run_steps(2);
        assert_eq!(h.state().ran[0], vec!["SELECT 1", "SELECT 3"]);
        h.state_mut().console.open_structure(Driver::Postgres);
        h.run_steps(2);
        h.get_by_label("Run all statements").click();
        h.run_steps(2);
        assert_eq!(h.state().ran[1], vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }
    #[test]
    fn sqlite_trigger_is_one_executable_statement() {
        let text = "CREATE TRIGGER audit AFTER INSERT ON notes BEGIN\n INSERT INTO log VALUES ('a;');\n UPDATE log SET n = 2;\nEND;\nSELECT 42;";
        let ss = sql::split_statements(text, Driver::Sqlite);
        assert_eq!(ss.len(), 2);
        assert!(ss[0].text.ends_with("END;"));
        assert_eq!(ss[1].text, "SELECT 42");
    }
}
