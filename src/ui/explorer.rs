//! "Database Explorer" panel: tree of connections, databases, schemas, tables and columns
//! (Islands style).

use crate::app::{ConnState, ConnStatus};
use crate::db::routines::{RoutineInfo, RoutineKind, RoutineList, RoutineRef};
use crate::db::{DatabaseInfo, Driver, ObjectKind, SchemaInfo, TableInfo};
use crate::i18n::tr;
use crate::ui::icons::{self, Ico};
use crate::ui::object_browser::ObjectScope;
use crate::ui::theme::{self, pal};
use crate::worker::ConnId;
use egui::{FontId, Rect, RichText, Sense, Stroke, Ui, Vec2, pos2, vec2};
use std::collections::HashSet;

#[derive(Default)]
pub struct ExplorerState {
    pub backups: Vec<crate::db::backup::BackupRecord>,
    pub filter: String,
    pub show_filter: bool,
    pub expanded: HashSet<String>,
    pub selected: Option<String>,
    /// Selected table (for the toolbar buttons).
    pub selected_table: Option<TableRef>,
    /// true if the last click was inside the panel ("focused" selection in blue).
    pub focused: bool,
    /// Width of the tree rows in this frame (the visible one or that of the longest row).
    row_width: f32,
    /// Width needed by the longest row drawn in this frame (horizontal scroll).
    widest_row: f32,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TableRef {
    pub conn: ConnId,
    pub db: String,
    pub schema: String,
    pub table: String,
}

pub enum ExplorerAction {
    Backup(ConnId, String),
    /// "Run SQL Script…" on the connection, or on one of its databases.
    RunScript(ConnId, Option<String>),
    NewConnection,
    Connect(ConnId),
    Disconnect(ConnId),
    Refresh(ConnId),
    Edit(ConnId),
    Duplicate(ConnId),
    Delete(ConnId),
    /// New console on the connection (and the database, if opened from a database).
    NewConsole(ConnId, Option<String>),
    /// "New Database" dialog on the connection.
    NewDatabase(ConnId),
    /// "New Table" dialog in a database (and schema, if one was chosen).
    NewTable { conn: ConnId, db: String, schema: Option<String> },
    /// "New View" dialog in a database (and schema, if one was chosen).
    NewView { conn: ConnId, db: String, schema: Option<String> },
    /// "Edit View" dialog, with the view's definition read from the server.
    EditView(TableRef),
    /// "New Procedure" / "New Function" dialog in a database (and schema, if one was chosen).
    NewRoutine { conn: ConnId, db: String, schema: Option<String>, kind: RoutineKind },
    /// Routine dialog with the routine's definition read from the server.
    EditRoutine(RoutineRef),
    /// "Execute…": the parameter form, then a console that runs the call.
    ExecuteRoutine(RoutineRef),
    RoutineDdl(RoutineRef),
    DropRoutine(RoutineRef),
    /// Confirm and drop a database.
    DropDatabase(ConnId, String),
    /// Confirm and drop a table or view.
    DropObject(TableRef, ObjectKind),
    TruncateTable(TableRef),
    ImportData(TableRef),
    /// "Copy Table To…" another connection, database or schema.
    CopyTable(TableRef),
    RenameTable(TableRef),
    /// "Modify Table…": columns, primary key, indexes and name.
    ModifyTable(TableRef),
    /// Load a database's schema when it is expanded.
    LoadDatabase(ConnId, String),
    BrowseObjects(ObjectScope),
    BrowseRoutines(ObjectScope, RoutineKind),
    InspectObject(TableRef, ObjectKind),
    OpenTable(TableRef),
    GenerateSelect(TableRef),
    GenerateInsert(TableRef),
    GenerateDdl(TableRef),
    /// Load (or reload) the "Users" / "Roles" folder.
    LoadUsers(ConnId),
    NewUser(ConnId),
    EditUser(ConnId, crate::db::users::Account),
    DropUser(ConnId, crate::db::users::Account),
    Hide,
}

const ROW_H: f32 = 24.0;
const INDENT: f32 = 18.0;

struct RowOut {
    resp: egui::Response,
    toggled: bool,
}

struct RowSpec<'a> {
    key: &'a str,
    depth: usize,
    expandable: bool,
    expanded: bool,
    icon: Ico,
    label: &'a str,
    /// Rounded gray badge after the name (e.g. number of tables).
    badge: Option<&'a str>,
    /// Gray secondary text (counters, types).
    detail: Option<(&'a str, egui::Color32)>,
    status: Option<egui::Color32>,
    label_color: Option<egui::Color32>,
}

fn tree_row(ui: &mut Ui, state: &mut ExplorerState, spec: RowSpec<'_>) -> RowOut {
    let p = pal(ui);
    let text_color = spec.label_color.unwrap_or(p.text);
    let label_galley = ui.painter().layout_no_wrap(spec.label.to_owned(), FontId::proportional(13.0), text_color);
    let x_label = 12.0 + spec.depth as f32 * INDENT + 20.0 + 16.0 + 6.0;
    let mut needed = x_label + label_galley.size().x + 6.0;
    if let Some(b) = spec.badge {
        needed += ui.painter().layout_no_wrap(b.to_owned(), FontId::proportional(11.0), p.badge_fg).size().x + 12.0 + 6.0;
    }
    if let Some((d, _)) = spec.detail {
        needed += ui.painter().layout_no_wrap(d.to_owned(), FontId::proportional(12.0), p.muted).size().x;
    }
    needed += 14.0;
    state.widest_row = state.widest_row.max(needed);
    let width = state.row_width.max(needed);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, state.selected.as_deref() == Some(spec.key), spec.label));
    if !ui.is_rect_visible(rect) {
        let mut toggled = false;
        if resp.double_clicked() && spec.expandable {
            toggled = true;
        }
        return RowOut { resp, toggled };
    }
    let selected = state.selected.as_deref() == Some(spec.key);
    let bg_rect = Rect::from_min_max(pos2(rect.left() + 6.0, rect.top() + 1.0), pos2(rect.right() - 6.0, rect.bottom() - 1.0));
    if selected {
        let fill = if state.focused { p.selection } else { p.selection_inactive };
        ui.painter().rect_filled(bg_rect, 5.0, fill);
    } else if resp.hovered() {
        ui.painter().rect_filled(bg_rect, 5.0, p.row_hover);
    }
    let x0 = rect.left() + 12.0 + spec.depth as f32 * INDENT;
    let cy = rect.center().y;
    let chevron_rect = Rect::from_min_size(pos2(x0, cy - 8.0), Vec2::splat(16.0));
    let mut toggled = false;
    if spec.expandable {
        let ico = if spec.expanded { Ico::ChevronDown } else { Ico::ChevronRight };
        icons::paint(ui.painter(), chevron_rect.shrink(1.0), ico, p.muted, &p, true);
        if resp.clicked()
            && let Some(pt) = resp.interact_pointer_pos()
            && pt.x < chevron_rect.right() + 2.0
        {
            toggled = true;
        }
    }
    let icon_rect = Rect::from_min_size(pos2(x0 + 20.0, cy - 8.0), Vec2::splat(16.0));
    icons::paint(ui.painter(), icon_rect, spec.icon, p.icon, &p, true);
    if let Some(c) = spec.status {
        let center = pos2(icon_rect.right() - 1.0, icon_rect.bottom() - 2.0);
        ui.painter().circle_filled(center, 3.6, p.island_bg);
        ui.painter().circle_filled(center, 2.6, c);
    }
    let painter = ui.painter_at(rect);
    let label_pos = pos2(icon_rect.right() + 6.0, cy - label_galley.size().y / 2.0);
    let label_right = label_pos.x + label_galley.size().x;
    painter.galley(label_pos, label_galley, text_color);
    let mut x = label_right + 6.0;
    if let Some(b) = spec.badge {
        let r = theme::paint_badge(ui, pos2(x, cy), b);
        x = r.right() + 6.0;
    }
    if let Some((d, color)) = spec.detail {
        painter.text(pos2(x, cy), egui::Align2::LEFT_CENTER, d, FontId::proportional(12.0), color);
    }
    if resp.clicked() || resp.secondary_clicked() {
        state.selected = Some(spec.key.to_string());
    }
    if resp.double_clicked() && spec.expandable {
        toggled = true;
    }
    RowOut { resp, toggled }
}

fn toggle(state: &mut ExplorerState, key: &str, open: bool) {
    if open {
        state.expanded.remove(key);
    } else {
        state.expanded.insert(key.to_string());
    }
}

fn table_matches(t: &TableInfo, f: &str) -> bool {
    f.is_empty() || t.name.to_lowercase().contains(f)
}

/// Database of the selected row (if it is a database or something inside one).
fn selected_db(state: &ExplorerState, conns: &[ConnState]) -> Option<String> {
    let key = state.selected.as_deref()?;
    let (kind, rest) = key.split_once(':')?;
    if kind == "c" {
        return None;
    }
    let conn = conn_of_key(key)?;
    let c = conns.iter().find(|c| c.cfg.id == conn)?;
    let after = rest.split_once(':')?.1;
    // The database name may contain ':'; the longest match wins.
    c.databases
        .iter()
        .filter(|d| after == d.name || after.starts_with(&format!("{}:", d.name)))
        .max_by_key(|d| d.name.len())
        .map(|d| d.name.clone())
}

/// Connection of the selected row (if any).
fn selected_conn(state: &ExplorerState) -> Option<ConnId> {
    state.selected.as_deref().and_then(conn_of_key)
}

/// Tool window header + icon bar.
fn header(ui: &mut Ui, state: &mut ExplorerState, conns: &[ConnState], actions: &mut Vec<ExplorerAction>) {
    let p = pal(ui);
    ui.add_space(4.0);
    // Clipped rows: with a narrow panel they cannot widen the island.
    theme::clipped_row(ui, 30.0, |ui| {
        ui.add_space(14.0);
        let title_w = (ui.available_width() - 66.0).max(0.0);
        ui.allocate_ui_with_layout(vec2(title_w, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(tr("Database Explorer")).font(theme::bold(13.5)).color(p.text)).truncate());
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.x = 2.0;
            if theme::icon_button(ui, Ico::Minus, true, tr("Hide")).clicked() {
                actions.push(ExplorerAction::Hide);
            }
            let more = theme::icon_button(ui, Ico::MoreVert, true, tr("Options"));
            egui::Popup::menu(&more).show(|ui| {
                if ui.button(tr("New Connection…")).clicked() {
                    actions.push(ExplorerAction::NewConnection);
                    ui.close();
                }
                if ui.button(tr("Expand Connections")).clicked() {
                    for c in conns {
                        state.expanded.insert(format!("c:{}", c.cfg.id));
                    }
                    ui.close();
                }
                if ui.button(tr("Collapse All")).clicked() {
                    state.expanded.clear();
                    ui.close();
                }
            });
        });
    });
    ui.add_space(2.0);

    let sel_conn = selected_conn(state);
    let sel_state = sel_conn.and_then(|id| conns.iter().find(|c| c.cfg.id == id));
    let connected = sel_state.is_some_and(|c| matches!(c.status, ConnStatus::Connected));
    let active = sel_state.is_some_and(|c| matches!(c.status, ConnStatus::Connected | ConnStatus::Connecting));
    let table = state.selected_table.clone().filter(|t| Some(t.conn) == sel_conn);

    theme::clipped_row(ui, 30.0, |ui| {
        ui.add_space(10.0);
        ui.spacing_mut().item_spacing.x = 1.0;
        let plus = theme::icon_button(ui, Ico::Plus, true, tr("New"));
        let sel_driver = sel_state.map(|c| c.cfg.driver);
        let sel_db = sel_conn.and_then(|_| selected_db(state, conns)).or_else(|| sel_state.and_then(|c| c.default_db().map(str::to_string)));
        egui::Popup::menu(&plus).show(|ui| {
            if ui.button(tr("New Connection…")).clicked() {
                actions.push(ExplorerAction::NewConnection);
                ui.close();
            }
            let sql = !sel_driver.is_some_and(|d| d.is_nosql());
            let can_db = connected && sql && sel_driver.is_some_and(|d| d.has_databases());
            if ui.add_enabled(can_db, egui::Button::new(tr("New Database…"))).clicked()
                && let Some(id) = sel_conn
            {
                actions.push(ExplorerAction::NewDatabase(id));
                ui.close();
            }
            if ui.add_enabled(connected && sql && sel_db.is_some(), egui::Button::new(tr("New Table…"))).clicked()
                && let (Some(conn), Some(db)) = (sel_conn, sel_db.clone())
            {
                let schema = table.as_ref().filter(|t| t.db == db).map(|t| t.schema.clone());
                actions.push(ExplorerAction::NewTable { conn, db, schema });
                ui.close();
            }
            if ui.add_enabled(connected && sql && sel_db.is_some(), egui::Button::new(tr("New View…"))).clicked()
                && let (Some(conn), Some(db)) = (sel_conn, sel_db.clone())
            {
                let schema = table.as_ref().filter(|t| t.db == db).map(|t| t.schema.clone());
                actions.push(ExplorerAction::NewView { conn, db, schema });
                ui.close();
            }
        });
        if theme::icon_button(ui, Ico::Properties, sel_conn.is_some(), tr("Data Source Properties")).clicked()
            && let Some(id) = sel_conn
        {
            actions.push(ExplorerAction::Edit(id));
        }
        theme::toolbar_separator(ui);
        if theme::icon_button(ui, Ico::Refresh, !conns.is_empty(), tr("Refresh")).clicked() {
            for c in conns {
                if matches!(c.status, ConnStatus::Connected) && sel_conn.is_none_or(|id| id == c.cfg.id) {
                    actions.push(ExplorerAction::Refresh(c.cfg.id));
                }
            }
        }
        if theme::icon_button(ui, Ico::Stop, active, tr("Disconnect")).clicked()
            && let Some(id) = sel_conn
        {
            actions.push(ExplorerAction::Disconnect(id));
        }
        theme::toolbar_separator(ui);
        if theme::icon_button(ui, Ico::JumpToConsole, sel_conn.is_some(), tr("Jump to Console")).clicked()
            && let Some(id) = sel_conn
        {
            actions.push(ExplorerAction::NewConsole(id, selected_db(state, conns)));
        }
        if theme::icon_button(ui, Ico::Table, table.is_some(), tr("Edit Table Data")).clicked()
            && let Some(t) = table.clone()
        {
            actions.push(ExplorerAction::OpenTable(t));
        }
        if theme::icon_button(ui, Ico::Ddl, table.is_some() && connected, tr("Generate DDL")).clicked()
            && let Some(t) = table.clone()
        {
            actions.push(ExplorerAction::GenerateDdl(t));
        }
        theme::toolbar_separator(ui);
        let eye = theme::icon_button(ui, Ico::Eye, true, tr("Filter Tables"));
        if state.show_filter {
            ui.painter().rect_stroke(eye.rect, 6.0, Stroke::new(1.0, p.control_border), egui::StrokeKind::Inside);
        }
        if eye.clicked() {
            state.show_filter = !state.show_filter;
            if !state.show_filter {
                state.filter.clear();
            }
        }
    });
    if state.show_filter {
        ui.add_space(2.0);
        theme::clipped_row(ui, 28.0, |ui| {
            ui.add_space(12.0);
            let w = (ui.available_width() - 12.0).max(40.0);
            ui.add(
                egui::TextEdit::singleline(&mut state.filter)
                    .hint_text(tr("Filter tables…"))
                    .desired_width(w)
                    .margin(vec2(8.0, 4.0)),
            );
        });
    }
    ui.add_space(4.0);
}

pub fn show(ui: &mut Ui, state: &mut ExplorerState, conns: &[ConnState]) -> Vec<ExplorerAction> {
    let mut actions = Vec::new();
    let p = pal(ui);
    let panel_rect = ui.max_rect();
    if ui.input(|i| i.pointer.any_pressed())
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
    {
        state.focused = panel_rect.contains(pos);
    }

    header(ui, state, conns, &mut actions);

    let filter = state.filter.trim().to_lowercase();

    // The tree fills everything below the header, with vertical and horizontal scroll.
    // It reaches the island's edge, like the grid, so the horizontal bars of both panels
    // line up.
    let max_h = ui.available_height().max(0.0);
    let viewport_w = ui.available_width();
    egui::ScrollArea::both()
        .id_salt("explorer_tree")
        .auto_shrink([false, false])
        .max_height(max_h)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            // All rows share the same width: the visible one or that of the longest row in
            // the previous frame, so the highlight covers the whole row when scrolling.
            state.row_width = viewport_w.max(state.widest_row);
            state.widest_row = 0.0;
            if conns.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(tr("Create a data source with Alt+Insert")).color(p.muted));
                    ui.add_space(6.0);
                    if theme::primary_button(ui, tr("New Connection…")).clicked() {
                        actions.push(ExplorerAction::NewConnection);
                    }
                });
            }
            for c in conns {
                connection_rows(ui, state, &mut actions, c, &filter);
            }
            ui.add_space(8.0);
        });
    if (state.row_width - viewport_w.max(state.widest_row)).abs() > 0.5 {
        ui.ctx().request_repaint();
    }
    actions
}

fn db_has_match(d: &DatabaseInfo, filter: &str) -> bool {
    d.schemas.as_ref().is_some_and(|ss| ss.iter().any(|s| s.tables.iter().any(|t| table_matches(t, filter))))
}

fn routine_matches(r: &RoutineInfo, f: &str) -> bool {
    f.is_empty() || r.name.to_lowercase().contains(f)
}

/// Routines of `schema` (all of them when the database has a single schema).
fn schema_routines<'a>(routines: Option<&'a RoutineList>, schema: Option<&'a str>) -> impl Iterator<Item = &'a RoutineInfo> {
    routines.and_then(|l| l.items.as_deref()).unwrap_or_default().iter().filter(move |r| schema.is_none_or(|s| r.schema == s))
}

/// Filtering by name also finds routines (once they are loaded).
fn db_routines_match(c: &ConnState, d: &DatabaseInfo, filter: &str) -> bool {
    !filter.is_empty() && schema_routines(c.routines.get(&d.name), None).any(|r| routine_matches(r, filter))
}

fn connection_rows(ui: &mut Ui, state: &mut ExplorerState, actions: &mut Vec<ExplorerAction>, c: &ConnState, filter: &str) {
    let p = pal(ui);
    let filtering = !filter.is_empty();
    let id = c.cfg.id;
    let name_match = filtering && c.cfg.name.to_lowercase().contains(filter);
    let any_table = c.databases.iter().any(|d| db_has_match(d, filter) || db_routines_match(c, d, filter));
    if filtering && !name_match && !any_table {
        return;
    }
    let ckey = format!("c:{id}");
    let open = state.expanded.contains(&ckey) || (filtering && any_table);
    let (dot, detail) = match &c.status {
        ConnStatus::Disconnected => (None, None),
        ConnStatus::Connecting => (Some(p.warn), Some((tr("connecting…"), p.muted))),
        ConnStatus::Connected => (Some(p.ok), None),
        ConnStatus::Error(_) => (Some(p.error), Some((tr("error"), p.error))),
    };
    let badge = (!c.databases.is_empty()).then(|| c.databases.len().to_string());
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key: &ckey,
            depth: 0,
            expandable: true,
            expanded: open,
            icon: Ico::Driver(c.cfg.driver),
            label: &c.cfg.name,
            badge: badge.as_deref(),
            detail,
            status: dot,
            label_color: None,
        },
    );
    let mut resp = row.resp;
    if let ConnStatus::Error(e) = &c.status {
        resp = resp.on_hover_text(RichText::new(e).color(p.error));
    } else {
        resp = resp.on_hover_text(connection_summary(c));
    }
    if row.toggled {
        toggle(state, &ckey, open);
        if !open && matches!(c.status, ConnStatus::Disconnected | ConnStatus::Error(_)) {
            actions.push(ExplorerAction::Connect(id));
        }
    }
    resp.context_menu(|ui| {
        let connected = matches!(c.status, ConnStatus::Connected);
        if ui.add_enabled(!connected, egui::Button::new(tr("Connect"))).clicked() {
            actions.push(ExplorerAction::Connect(id));
            state.expanded.insert(ckey.clone());
            ui.close();
        }
        if ui.add_enabled(!matches!(c.status, ConnStatus::Disconnected), egui::Button::new(tr("Disconnect"))).clicked() {
            actions.push(ExplorerAction::Disconnect(id));
            ui.close();
        }
        if ui.add_enabled(connected, egui::Button::new(tr("Refresh"))).clicked() {
            actions.push(ExplorerAction::Refresh(id));
            ui.close();
        }
        ui.separator();
        if c.cfg.driver.has_databases() && !c.cfg.driver.is_nosql() && ui.add_enabled(connected, egui::Button::new(tr("New Database…"))).clicked() {
            actions.push(ExplorerAction::NewDatabase(id));
            ui.close();
        }
        if ui.button(tr("New Console")).clicked() {
            actions.push(ExplorerAction::NewConsole(id, None));
            ui.close();
        }
        if crate::db::script::supported(c.cfg.driver) && ui.add_enabled(connected && !c.cfg.read_only, egui::Button::new(tr("Run SQL Script…"))).clicked() {
            actions.push(ExplorerAction::RunScript(id, None));
            ui.close();
        }
        ui.separator();
        if ui.button(tr("Properties…")).clicked() {
            actions.push(ExplorerAction::Edit(id));
            ui.close();
        }
        if ui.button(tr("Duplicate")).clicked() {
            actions.push(ExplorerAction::Duplicate(id));
            ui.close();
        }
        if ui.button(tr("Copy as URL")).on_hover_text(tr("The connection's URL for other tools, without the password")).clicked() {
            ui.ctx().copy_text(crate::db::url::connection_url(&c.cfg));
            ui.close();
        }
        if ui.button(RichText::new(tr("Delete…")).color(p.error)).clicked() {
            actions.push(ExplorerAction::Delete(id));
            ui.close();
        }
    });
    if !open {
        return;
    }
    if let ConnStatus::Error(e) = &c.status {
        error_line(ui, 1, e);
    }
    for d in &c.databases {
        database_rows(ui, state, actions, c, d, filter, name_match);
    }
    if !filtering && matches!(c.status, ConnStatus::Connected) && crate::db::users::UserModel::of(c.cfg.driver).is_some() {
        users_rows(ui, state, actions, c);
    }
}

/// "Users" (MySQL / MariaDB) or "Roles" (PostgreSQL) folder at server level, after the databases.
fn users_rows(ui: &mut Ui, state: &mut ExplorerState, actions: &mut Vec<ExplorerAction>, c: &ConnState) {
    let p = pal(ui);
    let id = c.cfg.id;
    let driver = c.cfg.driver;
    let roles = driver == Driver::Postgres;
    let ukey = format!("u:{id}");
    let open = state.expanded.contains(&ukey);
    let node = &c.users;
    let badge = node.list.as_ref().map(|l| l.accounts.len().to_string());
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key: &ukey,
            depth: 1,
            expandable: true,
            expanded: open,
            icon: Ico::Folder,
            label: if roles { tr("Roles") } else { tr("Users") },
            badge: badge.as_deref(),
            detail: node.loading.then(|| (tr("loading…"), p.muted)),
            status: None,
            label_color: None,
        },
    );
    if row.toggled {
        toggle(state, &ukey, open);
    }
    if (row.toggled && !open || row.resp.clicked()) && node.list.is_none() && !node.loading {
        actions.push(ExplorerAction::LoadUsers(id));
    }
    let new_label = if roles { tr("New Role…") } else { tr("New User…") };
    row.resp.context_menu(|ui| {
        if ui.button(new_label).clicked() {
            actions.push(ExplorerAction::NewUser(id));
            ui.close();
        }
        if ui.add_enabled(!node.loading, egui::Button::new(tr("Refresh"))).clicked() {
            actions.push(ExplorerAction::LoadUsers(id));
            ui.close();
        }
    });
    if !open {
        return;
    }
    if let Some(e) = &node.error {
        error_line(ui, 2, e);
    }
    let Some(list) = &node.list else { return };
    for a in &list.accounts {
        let label = if a.account.name.is_empty() { format!("{}{}", tr("(anonymous)"), a.account.label(driver)) } else { a.account.label(driver) };
        let akey = format!("ua:{id}:{label}");
        let row = tree_row(
            ui,
            state,
            RowSpec {
                key: &akey,
                depth: 2,
                expandable: false,
                expanded: false,
                icon: Ico::User,
                label: &label,
                badge: None,
                detail: (roles && !a.can_login).then(|| (tr("no login"), p.muted)),
                status: None,
                label_color: None,
            },
        );
        if row.resp.double_clicked() {
            actions.push(ExplorerAction::EditUser(id, a.account.clone()));
        }
        row.resp.context_menu(|ui| {
            if ui.button(if roles { tr("Edit Role…") } else { tr("Edit User…") }).clicked() {
                actions.push(ExplorerAction::EditUser(id, a.account.clone()));
                ui.close();
            }
            if ui.button(new_label).clicked() {
                actions.push(ExplorerAction::NewUser(id));
                ui.close();
            }
            ui.separator();
            if ui.button(RichText::new(if roles { tr("Drop Role…") } else { tr("Drop User…") }).color(p.error)).clicked() {
                actions.push(ExplorerAction::DropUser(id, a.account.clone()));
                ui.close();
            }
        });
    }
}

/// Red error message below a node.
fn error_line(ui: &mut Ui, depth: usize, msg: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.add_space(12.0 + depth as f32 * INDENT + 20.0);
        ui.add(egui::Label::new(RichText::new(msg).small().color(p.error)).wrap());
    });
}

#[allow(clippy::too_many_arguments)]
fn database_rows(
    ui: &mut Ui,
    state: &mut ExplorerState,
    actions: &mut Vec<ExplorerAction>,
    c: &ConnState,
    d: &DatabaseInfo,
    filter: &str,
    name_match: bool,
) {
    let p = pal(ui);
    let filtering = !filter.is_empty();
    let id = c.cfg.id;
    let has_match = db_has_match(d, filter) || db_routines_match(c, d, filter);
    let routines = c.routines.get(&d.name);
    if filtering && !has_match && !name_match {
        return;
    }
    let dkey = format!("d:{id}:{}", d.name);
    let open = state.expanded.contains(&dkey) || (filtering && has_match);
    let loading = c.loading.contains(&d.name);
    let multi_db = c.cfg.driver.has_databases();
    // Show the default schema directly when it is the only one.
    let flat = d.schemas.as_ref().is_some_and(|schemas| match schemas.as_slice() {
        [schema] => c.cfg.driver.flat_schema() || (c.cfg.driver == Driver::MsSql && schema.name == "dbo"),
        _ => false,
    });
    let badge = d.schemas.as_ref().map(|ss| {
        if flat { ss.iter().map(|s| s.tables.len()).sum::<usize>().to_string() } else { ss.len().to_string() }
    });
    let detail = if loading {
        Some((tr("loading…"), p.muted))
    } else if d.current && multi_db {
        Some((tr("default"), p.muted))
    } else {
        None
    };
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key: &dkey,
            depth: 1,
            expandable: true,
            expanded: open,
            icon: Ico::Database,
            label: &d.name,
            badge: badge.as_deref(),
            detail,
            status: None,
            label_color: None,
        },
    );
    if row.toggled {
        toggle(state, &dkey, open);
        if !open && d.schemas.is_none() && !loading && matches!(c.status, ConnStatus::Connected) {
            actions.push(ExplorerAction::LoadDatabase(id, d.name.clone()));
        }
    }
    let db_name = d.name.clone();
    if row.resp.clicked() {
        actions.push(ExplorerAction::BrowseObjects(ObjectScope {
            conn: id,
            db: db_name.clone(),
            schema: None,
            kind: None,
        }));
        if d.schemas.is_none() && !loading && matches!(c.status, ConnStatus::Connected) {
            actions.push(ExplorerAction::LoadDatabase(id, db_name.clone()));
        }
    }
    row.resp.context_menu(|ui| {
        if !c.cfg.driver.is_nosql() && ui.add_enabled(c.status == ConnStatus::Connected, egui::Button::new(tr("Back up database…"))).clicked() {
            actions.push(ExplorerAction::Backup(id, db_name.clone())); ui.close();
        }
        if ui.button(tr("New Console")).clicked() {
            actions.push(ExplorerAction::NewConsole(id, Some(db_name.clone())));
            ui.close();
        }
        let connected = matches!(c.status, ConnStatus::Connected);
        if crate::db::script::supported(c.cfg.driver) && ui.add_enabled(connected && !c.cfg.read_only, egui::Button::new(tr("Run SQL Script…"))).clicked() {
            actions.push(ExplorerAction::RunScript(id, Some(db_name.clone())));
            ui.close();
        }
        let sql = !c.cfg.driver.is_nosql();
        if sql && ui.add_enabled(connected, egui::Button::new(tr("New Table…"))).clicked() {
            actions.push(ExplorerAction::NewTable { conn: id, db: db_name.clone(), schema: None });
            ui.close();
        }
        if sql && ui.add_enabled(connected, egui::Button::new(tr("New View…"))).clicked() {
            actions.push(ExplorerAction::NewView { conn: id, db: db_name.clone(), schema: None });
            ui.close();
        }
        if routines.is_some() {
            new_routine_buttons(ui, actions, id, &db_name, None, connected);
        }
        if multi_db && sql && ui.add_enabled(connected, egui::Button::new(tr("New Database…"))).clicked() {
            actions.push(ExplorerAction::NewDatabase(id));
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(connected && !loading, egui::Button::new(tr("Reload Schema"))).clicked() {
            actions.push(ExplorerAction::LoadDatabase(id, db_name.clone()));
            ui.close();
        }
        if ui.button(tr("Copy Name")).clicked() {
            ui.ctx().copy_text(db_name.clone());
            ui.close();
        }
        if multi_db {
            ui.separator();
            // The connection's default database cannot be dropped from its own session.
            let btn = egui::Button::new(RichText::new(tr("Drop…")).color(p.error));
            let resp = ui.add_enabled(connected && !d.current, btn);
            let resp = if d.current { resp.on_disabled_hover_text(tr("It is the connection's default database")) } else { resp };
            if resp.clicked() {
                actions.push(ExplorerAction::DropDatabase(id, db_name.clone()));
                ui.close();
            }
        }
    });
    if !open {
        return;
    }
    if let Some(e) = c.db_errors.get(&d.name) {
        error_line(ui, 2, e);
    }
    let Some(schemas) = &d.schemas else {
        if loading {
            ui.horizontal(|ui| {
                ui.add_space(12.0 + 2.0 * INDENT + 20.0);
                ui.add(egui::Spinner::new().size(12.0).color(p.muted));
                ui.label(RichText::new(tr("loading…")).small().color(p.muted));
            });
        }
        return;
    };
    if flat {
        if let Some(s) = schemas.first() {
            object_groups(ui, state, actions, id, &d.name, s, routines, filter, 2, c.cfg.driver);
        }
        if !c.cfg.driver.is_nosql() { backup_rows(ui, state, actions, id, &d.name, 2); }
        return;
    }
    for s in schemas {
        schema_rows(ui, state, actions, id, &d.name, schemas, s, routines, filter, name_match);
    }
    if !c.cfg.driver.is_nosql() { backup_rows(ui, state, actions, id, &d.name, 2); }

}

#[allow(clippy::too_many_arguments)]
fn schema_rows(
    ui: &mut Ui,
    state: &mut ExplorerState,
    actions: &mut Vec<ExplorerAction>,
    conn: ConnId,
    db: &str,
    _all: &[SchemaInfo],
    s: &SchemaInfo,
    routines: Option<&RoutineList>,
    filter: &str,
    name_match: bool,
) {
    let filtering = !filter.is_empty();
    let skey = format!("s:{conn}:{db}:{}", s.name);
    let s_any = s.tables.iter().any(|t| table_matches(t, filter))
        || (filtering && schema_routines(routines, Some(&s.name)).any(|r| routine_matches(r, filter)));
    if filtering && !s_any && !name_match {
        return;
    }
    let s_open = state.expanded.contains(&skey) || (filtering && s_any);
    let n = s.tables.len().to_string();
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key: &skey,
            depth: 2,
            expandable: true,
            expanded: s_open,
            icon: Ico::Schema,
            label: &s.name,
            badge: None,
            detail: (!s.tables.is_empty()).then_some((n.as_str(), pal(ui).muted)),
            status: None,
            label_color: None,
        },
    );
    if row.resp.clicked() {
        actions.push(ExplorerAction::BrowseObjects(ObjectScope {
            conn,
            db: db.to_string(),
            schema: Some(s.name.clone()),
            kind: None,
        }));
    }
    row.resp.context_menu(|ui| {
        if ui.button(tr("New Table…")).clicked() {
            actions.push(ExplorerAction::NewTable { conn, db: db.to_string(), schema: Some(s.name.clone()) });
            ui.close();
        }
        if ui.button(tr("New View…")).clicked() {
            actions.push(ExplorerAction::NewView { conn, db: db.to_string(), schema: Some(s.name.clone()) });
            ui.close();
        }
        if routines.is_some() {
            new_routine_buttons(ui, actions, conn, db, Some(&s.name), true);
        }
        if ui.button(tr("Copy Name")).clicked() {
            ui.ctx().copy_text(s.name.clone());
            ui.close();
        }
    });
    if row.toggled {
        toggle(state, &skey, s_open);
    }
    if s_open {
        object_groups(ui, state, actions, conn, db, s, routines, filter, 3, Driver::Postgres);
    }
}

fn backup_rows(ui: &mut Ui, state: &mut ExplorerState, actions: &mut Vec<ExplorerAction>, conn: ConnId, db: &str, depth: usize) {
    let records: Vec<_> = state.backups.iter().filter(|r| r.conn == conn && r.db == db).cloned().collect();
    let key = format!("backups:{conn}:{db}");
    let open = state.expanded.contains(&key);
    let count = records.len().to_string();
    let row = tree_row(ui, state, RowSpec { key: &key, depth, expandable: true, expanded: open, icon: Ico::History, label: tr("Backups"), badge: None, detail: Some((&count, pal(ui).muted)), status: None, label_color: None });
    if row.toggled { toggle(state, &key, open); }
    row.resp.context_menu(|ui| { if ui.button(tr("Back up database…")).clicked() { actions.push(ExplorerAction::Backup(conn, db.into())); ui.close(); } });
    if open {
        if records.is_empty() { ui.indent(&key, |ui| { ui.label(tr("No backups yet")); }); }
        for (index, record) in records.iter().enumerate().rev() {
            let label = format!("{} · {}", record.created, record.mode.label());
            let row = tree_row(ui, state, RowSpec { key: &format!("{key}:{index}"), depth: depth + 1, expandable: false, expanded: false, icon: Ico::History, label: &label, badge: None, detail: None, status: None, label_color: None });
            let response = row.resp.on_hover_text(&record.destination);
            if response.double_clicked() { actions.push(ExplorerAction::Backup(conn, db.into())); }
            response.context_menu(|ui| { if ui.button(tr("Copy path")).clicked() { ui.ctx().copy_text(record.destination.clone()); ui.close(); } });
        }
    }
}

/// "tables" / "views" folders of a schema and their objects.
#[allow(clippy::too_many_arguments)]
fn object_groups(
    ui: &mut Ui,
    state: &mut ExplorerState,
    actions: &mut Vec<ExplorerAction>,
    conn: ConnId,
    db: &str,
    s: &SchemaInfo,
    // Procedures and functions of the database (`None` on engines without them).
    routines: Option<&RoutineList>,
    filter: &str,
    depth: usize,
    // Redis and MongoDB: their own folder names, and no DDL or import (MongoDB can copy). Schema-level
    // callers (only SQL engines get there) pass any SQL driver.
    driver: Driver,
) {
    let p = pal(ui);
    let filtering = !filter.is_empty();
    let sql = !driver.is_nosql();
    let groups: &[(ObjectKind, &str, &str)] = match driver {
        Driver::Redis => &[(ObjectKind::Table, tr("key groups"), "t")],
        Driver::MongoDb => &[(ObjectKind::Table, tr("collections"), "t"), (ObjectKind::View, tr("views"), "v")],
        Driver::InfluxDb => &[(ObjectKind::Table, tr("measurements"), "t")],
        Driver::Kusto => &[(ObjectKind::Table, tr("Tables"), "t"), (ObjectKind::View, tr("Materialized views"), "v")],
        _ => &[(ObjectKind::Table, tr("Tables"), "t"), (ObjectKind::View, tr("Views"), "v")],
    };
    for &(kind, label, gk) in groups {
        let tables: Vec<&TableInfo> = s.tables.iter().filter(|t| t.kind == kind && table_matches(t, filter)).collect();
        if filtering && tables.is_empty() {
            continue;
        }
        let gkey = format!("g:{conn}:{db}:{}:{gk}", s.name);
        let g_open = state.expanded.contains(&gkey) || filtering;
        let n = tables.len().to_string();
        let row = tree_row(
            ui,
            state,
            RowSpec {
                key: &gkey,
                depth,
                expandable: true,
                expanded: g_open,
                icon: Ico::Folder,
                label,
                badge: None,
                detail: Some((&n, p.muted)),
                status: None,
                label_color: None,
            },
        );
        if row.resp.clicked() {
            state.selected_table = None;
            actions.push(ExplorerAction::BrowseObjects(ObjectScope {
                conn,
                db: db.to_string(),
                schema: Some(s.name.clone()),
                kind: Some(kind),
            }));
        }
        if kind == ObjectKind::Table && sql {
            row.resp.context_menu(|ui| {
                if ui.button(tr("New Table…")).clicked() {
                    actions.push(ExplorerAction::NewTable { conn, db: db.to_string(), schema: Some(s.name.clone()) });
                    ui.close();
                }
            });
        } else if sql {
            row.resp.context_menu(|ui| {
                if ui.button(tr("New View…")).clicked() {
                    actions.push(ExplorerAction::NewView { conn, db: db.to_string(), schema: Some(s.name.clone()) });
                    ui.close();
                }
            });
        }
        if row.toggled {
            toggle(state, &gkey, g_open);
        }
        if !g_open {
            continue;
        }
        for t in tables {
            let tref = TableRef { conn, db: db.to_string(), schema: s.name.clone(), table: t.name.clone() };
            table_rows(ui, state, actions, tref, t, depth + 1, sql, driver.can_copy());
        }
    }
    if let Some(list) = routines {
        for kind in [RoutineKind::Procedure, RoutineKind::Function] {
            routine_group(ui, state, actions, conn, db, &s.name, list, kind, filter, depth);
        }
    }
}

fn new_routine_buttons(ui: &mut Ui, actions: &mut Vec<ExplorerAction>, conn: ConnId, db: &str, schema: Option<&str>, enabled: bool) {
    for (kind, label) in [(RoutineKind::Procedure, tr("New Procedure…")), (RoutineKind::Function, tr("New Function…"))] {
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            actions.push(ExplorerAction::NewRoutine { conn, db: db.to_string(), schema: schema.map(str::to_string), kind });
            ui.close();
        }
    }
}

/// "Procedures" / "Functions" folder of a schema and its routines.
#[allow(clippy::too_many_arguments)]
fn routine_group(
    ui: &mut Ui,
    state: &mut ExplorerState,
    actions: &mut Vec<ExplorerAction>,
    conn: ConnId,
    db: &str,
    schema: &str,
    list: &RoutineList,
    kind: RoutineKind,
    filter: &str,
    depth: usize,
) {
    let p = pal(ui);
    let filtering = !filter.is_empty();
    let items: Vec<&RoutineInfo> = schema_routines(Some(list), Some(schema)).filter(|r| r.kind == kind && routine_matches(r, filter)).collect();
    if filtering && items.is_empty() {
        return;
    }
    let (label, gk) = match kind {
        RoutineKind::Procedure => (tr("Procedures"), "p"),
        RoutineKind::Function => (tr("Functions"), "f"),
    };
    let gkey = format!("g:{conn}:{db}:{schema}:{gk}");
    let open = state.expanded.contains(&gkey) || filtering;
    let n = if list.items.is_some() { items.len().to_string() } else if list.loading { tr("loading…").to_string() } else { String::new() };
    let row = tree_row(
        ui,
        state,
        RowSpec { key: &gkey, depth, expandable: true, expanded: open, icon: Ico::Folder, label, badge: None, detail: Some((&n, p.muted)), status: None, label_color: None },
    );
    if row.resp.clicked() {
        state.selected_table = None;
        actions.push(ExplorerAction::BrowseRoutines(ObjectScope {
            conn, db: db.to_string(), schema: Some(schema.to_string()), kind: None,
        }, kind));
    }
    row.resp.context_menu(|ui| {
        let text = if kind == RoutineKind::Procedure { tr("New Procedure…") } else { tr("New Function…") };
        if ui.button(text).clicked() {
            actions.push(ExplorerAction::NewRoutine { conn, db: db.to_string(), schema: Some(schema.to_string()), kind });
            ui.close();
        }
    });
    if row.toggled {
        toggle(state, &gkey, open);
    }
    if !open {
        return;
    }
    if let Some(e) = &list.error {
        error_line(ui, depth + 1, e);
    }
    for r in items {
        routine_row(ui, state, actions, RoutineRef::new(conn, db, r), r, depth + 1);
    }
}

/// Tree key of a routine.
pub fn routine_key(r: &RoutineRef) -> String {
    format!("r:{}:{}:{}:{}({}):{:?}", r.conn, r.db, r.schema, r.name, r.signature, r.kind)
}

fn routine_row(ui: &mut Ui, state: &mut ExplorerState, actions: &mut Vec<ExplorerAction>, rref: RoutineRef, r: &RoutineInfo, depth: usize) {
    let p = pal(ui);
    let key = routine_key(&rref);
    let mut detail: String = r.arguments.chars().take(40).collect();
    if r.arguments.chars().count() > 40 {
        detail.push('…');
    }
    let detail = format!("({detail})");
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key: &key,
            depth,
            expandable: false,
            expanded: false,
            icon: if r.kind == RoutineKind::Procedure { Ico::Procedure } else { Ico::Function },
            label: &r.name,
            badge: None,
            detail: Some((&detail, p.muted)),
            status: None,
            label_color: (!r.valid).then_some(p.error),
        },
    );
    let mut tip = r.display();
    if !r.returns.is_empty() {
        tip.push_str(&format!("\n→ {}", r.returns));
    }
    if !r.language.is_empty() {
        tip.push_str(&format!("\n{}", r.language));
    }
    if !r.valid {
        tip.push_str(&format!("\n{}", tr("Invalid: it has compilation errors")));
    }
    let resp = row.resp.on_hover_text(tip);
    if resp.clicked() || resp.secondary_clicked() {
        state.selected_table = None;
    }
    if resp.double_clicked() {
        actions.push(ExplorerAction::EditRoutine(rref.clone()));
    }
    resp.context_menu(|ui| {
        if ui.button(tr("Execute…")).clicked() {
            actions.push(ExplorerAction::ExecuteRoutine(rref.clone()));
            ui.close();
        }
        let edit = if r.kind == RoutineKind::Procedure { tr("Edit Procedure…") } else { tr("Edit Function…") };
        if ui.button(edit).clicked() {
            actions.push(ExplorerAction::EditRoutine(rref.clone()));
            ui.close();
        }
        if ui.button(tr("Generate DDL")).clicked() {
            actions.push(ExplorerAction::RoutineDdl(rref.clone()));
            ui.close();
        }
        if ui.button(tr("Copy Name")).clicked() {
            ui.ctx().copy_text(r.name.clone());
            ui.close();
        }
        ui.separator();
        new_routine_buttons(ui, actions, rref.conn, &rref.db, Some(&rref.schema), true);
        ui.separator();
        if ui.button(RichText::new(tr("Drop…")).color(p.error)).clicked() {
            actions.push(ExplorerAction::DropRoutine(rref.clone()));
            ui.close();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn table_rows(ui: &mut Ui, state: &mut ExplorerState, actions: &mut Vec<ExplorerAction>, tref: TableRef, t: &TableInfo, depth: usize, sql: bool, copyable: bool) {
    let p = pal(ui);
    let tkey = table_key(&tref);
    let open = state.expanded.contains(&tkey);
    let icon = if t.kind == ObjectKind::Table { Ico::Table } else { Ico::View };
    let row = tree_row(
        ui,
        state,
        RowSpec {
            key: &tkey,
            depth,
            expandable: !t.columns.is_empty(),
            expanded: open,
            icon,
            label: &t.name,
            badge: None,
            detail: None,
            status: None,
            label_color: None,
        },
    );
    if row.resp.clicked() || row.resp.secondary_clicked() {
        state.selected_table = Some(tref.clone());
        actions.push(ExplorerAction::InspectObject(tref.clone(), t.kind));
    }
    if row.resp.double_clicked() {
        actions.push(ExplorerAction::OpenTable(tref.clone()));
    } else if row.toggled {
        toggle(state, &tkey, open);
    }
    row.resp.context_menu(|ui| {
        if ui.button(tr("Edit Data")).clicked() {
            actions.push(ExplorerAction::OpenTable(tref.clone()));
            ui.close();
        }
        if ui.button(tr("Copy Name")).clicked() {
            ui.ctx().copy_text(t.name.clone());
            ui.close();
        }
        // Redis and MongoDB: no SQL generation, DDL or import; MongoDB collections can be copied.
        if !sql {
            if copyable {
                ui.separator();
                if ui.button(tr("Copy Table To…")).clicked() {
                    actions.push(ExplorerAction::CopyTable(tref.clone()));
                    ui.close();
                }
            }
            return;
        }
        ui.separator();
        if ui.button(tr("Generate SELECT")).clicked() {
            actions.push(ExplorerAction::GenerateSelect(tref.clone()));
            ui.close();
        }
        if ui.button(tr("Generate INSERT")).clicked() {
            actions.push(ExplorerAction::GenerateInsert(tref.clone()));
            ui.close();
        }
        if ui.button(tr("Generate DDL")).clicked() {
            actions.push(ExplorerAction::GenerateDdl(tref.clone()));
            ui.close();
        }
        ui.separator();
        if t.kind == ObjectKind::View && ui.button(tr("Edit View…")).clicked() {
            actions.push(ExplorerAction::EditView(tref.clone()));
            ui.close();
        }
        if ui.button(tr("New Table…")).clicked() {
            actions.push(ExplorerAction::NewTable { conn: tref.conn, db: tref.db.clone(), schema: Some(tref.schema.clone()) });
            ui.close();
        }
        if ui.button(tr("New View…")).clicked() {
            actions.push(ExplorerAction::NewView { conn: tref.conn, db: tref.db.clone(), schema: Some(tref.schema.clone()) });
            ui.close();
        }
        if ui.button(tr("Copy Table To…")).clicked() {
            actions.push(ExplorerAction::CopyTable(tref.clone()));
            ui.close();
        }
        if t.kind == ObjectKind::Table {
            if ui.button(tr("Import Data from File…")).clicked() {
                actions.push(ExplorerAction::ImportData(tref.clone()));
                ui.close();
            }
            if ui.button(tr("Modify Table…")).clicked() {
                actions.push(ExplorerAction::ModifyTable(tref.clone()));
                ui.close();
            }
            if ui.button(tr("Rename…")).clicked() {
                actions.push(ExplorerAction::RenameTable(tref.clone()));
                ui.close();
            }
            if ui.button(tr("Truncate…")).clicked() {
                actions.push(ExplorerAction::TruncateTable(tref.clone()));
                ui.close();
            }
        }
        if ui.button(RichText::new(tr("Drop…")).color(p.error)).clicked() {
            actions.push(ExplorerAction::DropObject(tref.clone(), t.kind));
            ui.close();
        }
    });
    if !open {
        return;
    }
    for col in &t.columns {
        let ckey = format!("{tkey}:{}", col.name);
        let mut detail = col.data_type.to_lowercase();
        if !col.nullable {
            detail.push_str(" not null");
        }
        let row = tree_row(
            ui,
            state,
            RowSpec {
                key: &ckey,
                depth: depth + 1,
                expandable: false,
                expanded: false,
                icon: if col.primary_key { Ico::Key } else { Ico::Column },
                label: &col.name,
                badge: None,
                detail: Some((&detail, p.muted)),
                status: None,
                label_color: None,
            },
        );
        row.resp.context_menu(|ui| {
            if ui.button(tr("Copy Name")).clicked() {
                ui.ctx().copy_text(col.name.clone());
                ui.close();
            }
        });
    }
}

/// Tree key of a table (to expand or select it from outside).
pub fn table_key(t: &TableRef) -> String {
    format!("t:{}:{}:{}:{}", t.conn, t.db, t.schema, t.table)
}

/// Tree key of a database.
pub fn db_key(conn: ConnId, db: &str) -> String {
    format!("d:{conn}:{db}")
}


fn conn_of_key(key: &str) -> Option<ConnId> {
    let rest = key.split_once(':')?.1;
    rest.split(':').next()?.parse().ok()
}

fn connection_summary(c: &ConnState) -> String {
    let cfg = &c.cfg;
    match cfg.driver {
        crate::db::Driver::Sqlite => format!("SQLite: {}", cfg.file_path),
        d => format!("{}: {}@{}:{}/{}", d.label(), cfg.user, cfg.host, cfg.port, cfg.database),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ConnectionConfig;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn single_dbo_schema_shows_object_folders_directly() {
        let mut connection = ConnState::new(ConnectionConfig::new(1, Driver::MsSql));
        connection.status = ConnStatus::Connected;
        connection.databases.push(DatabaseInfo { name: "shop".into(), current: true, schemas: Some(vec![SchemaInfo { name: "dbo".into(), tables: Vec::new() }]) });
        let mut explorer = ExplorerState::default();
        for key in ["c:1", "d:1:shop"] { explorer.expanded.insert(key.into()); }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(400.0, 600.0)).build_ui_state(move |ui, state: &mut ExplorerState| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            show(ui, state, std::slice::from_ref(&connection));
        }, explorer);
        h.run_steps(3);
        assert!(h.query_by_label("Tables").is_some());
        assert!(h.query_by_label("dbo").is_none());
    }

    #[test]
    fn database_folders_start_collapsed_and_backup_targets_the_context_database() {
        let mut connection = ConnState::new(ConnectionConfig::new(1, Driver::MariaDb));
        connection.status = ConnStatus::Connected;
        connection.databases.push(DatabaseInfo { name: "shop".into(), current: true, schemas: Some(vec![SchemaInfo { name: "shop".into(), tables: vec![TableInfo { name: "orders".into(), kind: ObjectKind::Table, columns: Vec::new() }] }]) });
        let mut explorer = ExplorerState::default(); explorer.expanded.insert("c:1".into());
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(400.0, 600.0)).build_ui_state(move |ui, state: &mut (ExplorerState, Option<String>)| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            for action in show(ui, &mut state.0, std::slice::from_ref(&connection)) {
                if let ExplorerAction::Backup(1, db) = action { state.1 = Some(db); }
            }
        }, (explorer, None));
        h.run_steps(3);
        assert!(h.query_by_label("Backups").is_none());
        h.state_mut().0.expanded.insert("d:1:shop".into()); h.run_steps(2);
        assert!(h.query_by_label("Tables").is_some());
        assert!(h.query_by_label("Views").is_some());
        assert!(h.query_by_label("Backups").is_some());
        assert!(h.query_by_label("orders").is_none());
        h.get_by_label("shop").click_secondary(); h.run_steps(2);
        h.get_by_label("Back up database…").click(); h.run_steps(2);
        assert_eq!(h.state().1.as_deref(), Some("shop"));
    }

    #[test]
    fn routine_folders_list_routines_and_offer_their_actions() {
        let mut connection = ConnState::new(ConnectionConfig::new(1, Driver::Postgres));
        connection.status = ConnStatus::Connected;
        connection.databases.push(DatabaseInfo { name: "shop".into(), current: true, schemas: Some(vec![SchemaInfo { name: "public".into(), tables: Vec::new() }]) });
        let routine = |name: &str, kind| RoutineInfo { schema: "public".into(), name: name.into(), kind, signature: "a integer".into(), arguments: "a integer".into(), valid: true, ..Default::default() };
        connection.routines.insert(
            "shop".into(),
            RoutineList { items: Some(vec![routine("add_tax", RoutineKind::Function), routine("archive_orders", RoutineKind::Procedure)]), ..Default::default() },
        );
        let mut explorer = ExplorerState::default();
        for key in ["c:1", "d:1:shop", "s:1:shop:public"] {
            explorer.expanded.insert(key.into());
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(400.0, 600.0)).build_ui_state(move |ui, state: &mut (ExplorerState, Vec<String>)| {
            if std::mem::take(&mut first) { theme::install_fonts(ui.ctx()); theme::apply(ui.ctx(), true); return; }
            for action in show(ui, &mut state.0, std::slice::from_ref(&connection)) {
                let text = match action {
                    ExplorerAction::ExecuteRoutine(r) => format!("execute {}({})", r.name, r.signature),
                    ExplorerAction::EditRoutine(r) => format!("edit {}", r.name),
                    ExplorerAction::DropRoutine(r) => format!("drop {:?} {}", r.kind, r.name),
                    ExplorerAction::NewRoutine { schema, kind, .. } => format!("new {kind:?} in {schema:?}"),
                    ExplorerAction::BrowseRoutines(scope, kind) => format!("browse {kind:?} in {:?}", scope.schema),
                    _ => continue,
                };
                state.1.push(text);
            }
        }, (explorer, Vec::new()));
        h.run_steps(3);
        assert!(h.query_by_label("Procedures").is_some() && h.query_by_label("Functions").is_some());
        h.get_by_label("Procedures").click(); h.run_steps(2);
        h.get_by_label("Functions").click(); h.run_steps(2);
        assert_eq!(h.state().1, ["browse Procedure in Some(\"public\")", "browse Function in Some(\"public\")"]);
        h.state_mut().1.clear();
        assert!(h.query_by_label("add_tax").is_none(), "folders start collapsed");
        h.get_by_label("Functions").click_secondary();
        h.run_steps(2);
        h.get_by_label("New Function…").click();
        h.run_steps(2);
        h.state_mut().0.expanded.insert("g:1:shop:public:f".into());
        h.run_steps(2);
        assert!(h.query_by_label("archive_orders").is_none(), "procedures live in their own folder");
        h.get_by_label("add_tax").click_secondary();
        h.run_steps(2);
        h.get_by_label("Execute…").click();
        h.run_steps(2);
        h.get_by_label("add_tax").click_secondary();
        h.run_steps(2);
        h.get_by_label("Edit Function…").click();
        h.run_steps(2);
        assert_eq!(h.state().1, ["new Function in Some(\"public\")", "execute add_tax(a integer)", "edit add_tax"]);

        // Filtering by name finds routines too.
        h.state_mut().0.expanded.clear();
        h.state_mut().0.filter = "archive".into();
        h.run_steps(3);
        assert!(h.query_by_label("archive_orders").is_some());
    }
}
