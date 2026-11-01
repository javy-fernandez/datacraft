//! Table workflows launched from the shared context menu.

use super::*;
use crate::db::{
    table_export::{ExportKind, ExportProgress, ExportRequest},
    table_privileges::{Grants, Principal},
    table_tools::{Operation, Outcome},
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

#[derive(Default)]
pub(super) struct TableActions {
    dialog: Option<Dialog>,
    pub startup: Option<crate::table_shortcut::Target>,
}

struct Dialog {
    id: u64,
    table: TableRef,
    driver: Driver,
    mode: Mode,
    pending: bool,
    progress: Arc<ExportProgress>,
    error: Option<String>,
    success: Option<String>,
}

enum Mode {
    Duplicate { target: String, data: bool, plan: Option<crate::db::alter::Plan>, planned_name: String },
    Export { kind: ExportKind, path: String },
    Grants { accounts: Vec<Principal>, selected: Option<Principal>, available: Vec<String>, old: Vec<String>, chosen: BTreeSet<String>, loaded: bool },
    Model { view: Option<crate::ui::table_model::ModelView> },
    Ddl,
    Shortcut,
}

enum Intent {
    Operation(Operation),
    SaveModel(bool),
    Export,
    Close,
}

impl Dialog {
    fn title(&self) -> &'static str {
        tr(match self.mode {
            Mode::Duplicate { .. } => "Duplicate Table",
            Mode::Export { kind: ExportKind::Print { .. }, .. } => "Print Table",
            Mode::Export { kind: ExportKind::Sql { .. }, .. } => "Dump SQL File",
            Mode::Export { .. } => "Export Table",
            Mode::Grants { .. } => "Set Privileges",
            Mode::Model { .. } => "Table Model",
            Mode::Ddl => "Generate DDL",
            Mode::Shortcut => "Create Open Table Shortcut",
        })
    }

    fn show(&mut self, ctx: &egui::Context) -> Option<Intent> {
        let title = self.title();
        let subtitle = format!("{} / {} / {}", self.table.db, self.table.schema, self.table.table);
        let mut intent = None;
        let mut preview = String::new();
        let mut primary = None;
        let pending = self.pending;
        let mut enable_primary = false;
        let size = if matches!(self.mode, Mode::Model { .. }) { vec2(1000.0, 700.0) } else { vec2(680.0, 600.0) };
        crate::ui::workflow_dialog::show(ctx, egui::Id::new("table_workflow"), title, &subtitle, size, |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::both().auto_shrink([false, false]).show(&mut body, |ui| {
                ui.add_enabled_ui(!pending, |ui| {
                    match &mut self.mode {
                        Mode::Duplicate { target, data, plan, planned_name } => {
                            ui.label(tr("New table name"));
                            if ui.add(egui::TextEdit::singleline(target).desired_width(f32::INFINITY)).changed() {
                                *plan = None;
                            }
                            ui.label(tr(if *data { "Structure and Data" } else { "Structure Only" }));
                            ui.label(tr("The existing table is preserved. The destination must not exist."));
                            if let Some(plan) = plan.as_ref().filter(|_| target.trim() == planned_name) {
                                for warning in &plan.warnings {
                                    ui.colored_label(palette(ctx.global_style().visuals.dark_mode).warn, warning);
                                }
                                preview = plan.statements.iter().map(|s| format!("{s};\n")).collect();
                                primary = Some(tr("Duplicate"));
                                enable_primary = true;
                            } else {
                                primary = Some(tr("Preview SQL"));
                                enable_primary = !target.trim().is_empty() && target.trim() != self.table.table;
                            }
                        }
                        Mode::Export { kind, path } => {
                            match kind {
                                ExportKind::Print { data } => {
                                    ui.add_enabled(crate::db::table_export::supported(self.driver), egui::Checkbox::new(data, tr("Include all table data")));
                                    ui.label(tr("The report opens in your browser. Use Print to choose a printer or PDF."));
                                }
                                ExportKind::Sql { structure, data } => {
                                    ui.label(tr(if *structure && *data {
                                        "Structure and Data"
                                    } else if *structure {
                                        "Structure Only"
                                    } else {
                                        "Data Only"
                                    }));
                                }
                                _ => {
                                    egui::ComboBox::from_id_salt("table_export_format").selected_text(format_name(*kind)).show_ui(ui, |ui| {
                                        for format in [ExportKind::Csv, ExportKind::Tsv, ExportKind::Json] {
                                            ui.selectable_value(kind, format, format_name(format));
                                        }
                                    });
                                }
                            }
                            ui.add_space(10.0);
                            ui.label(tr("Destination file"));
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(path).desired_width((ui.available_width() - 90.0).max(40.0)));
                                if ui.button(tr("Browse…")).clicked() {
                                    let ext = extension(*kind);
                                    if let Some(file) = rfd::FileDialog::new()
                                        .add_filter(format_name(*kind), &[ext])
                                        .set_file_name(format!("{}.{}", self.table.table, ext))
                                        .save_file()
                                    {
                                        *path = file.to_string_lossy().into_owned();
                                    }
                                }
                            });
                            ui.label(tr("Exports read the complete table, independently of the data editor's page and filters."));
                            primary = Some(tr(if matches!(kind, ExportKind::Print { .. }) { "Create report" } else { "Export" }));
                            enable_primary = !path.trim().is_empty();
                        }
                        Mode::Grants { accounts, selected, available, old, chosen, loaded } => {
                            ui.label(tr("These are explicit table grants, not the user's full effective permissions."));
                            let before = selected.clone();
                            let account_label = |p: &Principal| {
                                if matches!(self.driver, Driver::MySql | Driver::MariaDb) { format!("{}@{}", p.name, p.host) } else { p.name.clone() }
                            };
                            egui::ComboBox::from_id_salt("table_principal")
                                .selected_text(selected.as_ref().map(account_label).unwrap_or_else(|| tr("Select user or role").into()))
                                .show_ui(ui, |ui| {
                                    for account in accounts.iter() {
                                        ui.selectable_value(selected, Some(account.clone()), account_label(account));
                                    }
                                });
                            if *selected != before {
                                *loaded = false;
                                chosen.clear();
                                old.clear();
                                if let Some(principal) = selected.clone() {
                                    intent = Some(Intent::Operation(Operation::Grants {
                                        schema: self.table.schema.clone(),
                                        table: self.table.table.clone(),
                                        principal: Some(principal),
                                    }));
                                }
                            }
                            if *loaded {
                                for privilege in available {
                                    let mut checked = chosen.contains(privilege);
                                    if ui.checkbox(&mut checked, privilege.as_str()).changed() {
                                        if checked {
                                            chosen.insert(privilege.clone());
                                        } else {
                                            chosen.remove(privilege);
                                        }
                                    }
                                }
                                if let Some(p) = selected.as_ref() {
                                    let new: Vec<_> = chosen.iter().cloned().collect();
                                    match crate::db::table_privileges::statements(self.driver, &self.table.schema, &self.table.table, p, old, &new) {
                                        Ok(statements) => {
                                            enable_primary = !statements.is_empty();
                                            preview = statements.iter().map(|s| format!("{s};\n")).collect();
                                        }
                                        Err(error) => {
                                            ui.colored_label(palette(ctx.global_style().visuals.dark_mode).error, format!("{error:#}"));
                                        }
                                    }
                                }
                            }
                            if ui.button(tr("Reload privileges")).clicked() {
                                *loaded = false;
                                intent = Some(Intent::Operation(Operation::Grants {
                                    schema: self.table.schema.clone(),
                                    table: self.table.table.clone(),
                                    principal: selected.clone(),
                                }));
                            }
                            ui.label(tr("Some engines apply grants individually. Reload after an error to inspect the current state."));
                            primary = Some(tr("Apply"));
                        }
                        Mode::Model { view } => {
                            if let Some(view) = view {
                                ui.horizontal(|ui| {
                                    if ui.button(tr("Save model…")).clicked() {
                                        intent = Some(Intent::SaveModel(false));
                                    }
                                    if ui.button(tr("Export SVG…")).clicked() {
                                        intent = Some(Intent::SaveModel(true));
                                    }
                                });
                                view.show(ui);
                            }
                        }
                        Mode::Ddl | Mode::Shortcut => {}
                    }
                    if !preview.is_empty() {
                        ui.separator();
                        ui.label(tr("SQL preview"));
                        ui.add(egui::Label::new(sql::highlight(&preview, ctx.global_style().visuals.dark_mode, FontId::monospace(12.0))).wrap());
                    }
                });
                if let Some(error) = &self.error {
                    ui.add(egui::Label::new(RichText::new(error).color(palette(ctx.global_style().visuals.dark_mode).error)).wrap());
                }
                if let Some(success) = &self.success {
                    ui.add(egui::Label::new(success).wrap());
                }
            });
            let mut footer = regions.footer_ui(root);
            footer.horizontal(|ui| {
                if pending {
                    ui.spinner();
                    ui.label(trf("{n} rows processed", &[("n", &self.progress.rows.load(Ordering::Relaxed))]));
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr(if pending { "Cancel" } else { "Close" })).clicked() {
                        if pending {
                            self.progress.cancel.store(true, Ordering::Relaxed);
                        } else {
                            intent = Some(Intent::Close);
                        }
                    }
                    if let Some(label) = primary
                        && ui.add_enabled(!pending && enable_primary, egui::Button::new(label)).clicked()
                    {
                        intent = match &self.mode {
                            Mode::Duplicate { target, data, plan: Some(plan), planned_name } if target.trim() == planned_name => {
                                Some(Intent::Operation(Operation::Duplicate {
                                    schema: self.table.schema.clone(),
                                    table: self.table.table.clone(),
                                    target: target.trim().into(),
                                    data: *data,
                                    expected: plan.statements.clone(),
                                }))
                            }
                            Mode::Duplicate { target, data, .. } => Some(Intent::Operation(Operation::PreviewDuplicate {
                                schema: self.table.schema.clone(),
                                table: self.table.table.clone(),
                                target: target.trim().into(),
                                data: *data,
                            })),
                            Mode::Grants { selected: Some(p), old, chosen, .. } => {
                                let new: Vec<_> = chosen.iter().cloned().collect();
                                match crate::db::table_privileges::statements(self.driver, &self.table.schema, &self.table.table, p, old, &new) {
                                    Ok(expected) => Some(Intent::Operation(Operation::ApplyGrants {
                                        schema: self.table.schema.clone(),
                                        table: self.table.table.clone(),
                                        principal: p.clone(),
                                        old: old.clone(),
                                        new,
                                        expected,
                                    })),
                                    Err(error) => {
                                        self.error = Some(format!("{error:#}"));
                                        None
                                    }
                                }
                            }
                            Mode::Export { .. } => Some(Intent::Export),
                            _ => None,
                        };
                    }
                });
            });
        });
        if !pending && ctx.input(|i| i.key_pressed(Key::Escape)) {
            intent = Some(Intent::Close);
        }
        intent
    }
}

fn extension(kind: ExportKind) -> &'static str {
    match kind {
        ExportKind::Csv => "csv",
        ExportKind::Tsv => "tsv",
        ExportKind::Json => "json",
        ExportKind::Sql { .. } => "sql",
        ExportKind::Print { .. } => "html",
    }
}
fn format_name(kind: ExportKind) -> &'static str {
    match kind {
        ExportKind::Csv => "CSV",
        ExportKind::Tsv => "TSV",
        ExportKind::Json => "JSON",
        ExportKind::Sql { .. } => "SQL",
        ExportKind::Print { .. } => "HTML",
    }
}

impl DataCraftApp {
    pub fn start_table_shortcut(&mut self, target: crate::table_shortcut::Target) {
        let conn = target.connection;
        let Some(c) = self.conn(conn) else {
            self.error(tr("The shortcut's saved connection was not found"));
            return;
        };
        let connected = c.status == ConnStatus::Connected;
        self.table_actions.startup = Some(target);
        if connected {
            self.open_startup_table(conn);
        } else {
            self.connect(conn);
        }
    }

    pub(super) fn handle_table_action(&mut self, ctx: &egui::Context, action: ExplorerAction) {
        let (table, mode, start) = match action {
            ExplorerAction::RefreshObject(t) => {
                self.refresh_table_scope(&t);
                return;
            }
            ExplorerAction::ObjectInformation(t) => {
                let kind = self
                    .conn(t.conn)
                    .and_then(|c| c.schemas(Some(&t.db)))
                    .and_then(|s| s.iter().find(|s| s.name == t.schema))
                    .and_then(|s| s.tables.iter().find(|o| o.name == t.table))
                    .map_or(crate::db::ObjectKind::Table, |t| t.kind);
                self.handle_explorer(ctx, vec![ExplorerAction::InspectObject(t, kind)]);
                self.object_browser.active = true;
                return;
            }
            ExplorerAction::DuplicateTable(t, data) => {
                let target = format!("{}_copy", t.table);
                (t, Mode::Duplicate { target, data, plan: None, planned_name: String::new() }, None)
            }
            ExplorerAction::TablePrivileges(t) => {
                let op = Operation::Grants { schema: t.schema.clone(), table: t.table.clone(), principal: None };
                (
                    t,
                    Mode::Grants { accounts: Vec::new(), selected: None, available: Vec::new(), old: Vec::new(), chosen: BTreeSet::new(), loaded: false },
                    Some(op),
                )
            }
            ExplorerAction::ExportTable(t) => (t, Mode::Export { kind: ExportKind::Csv, path: String::new() }, None),
            ExplorerAction::DumpTable(t, structure, data) => (t, Mode::Export { kind: ExportKind::Sql { structure, data }, path: String::new() }, None),
            ExplorerAction::PrintTable(t) => (t, Mode::Export { kind: ExportKind::Print { data: false }, path: String::new() }, None),
            ExplorerAction::TableModel(tables) => {
                let Some(t) = tables.first().cloned() else {
                    return;
                };
                if tables.iter().any(|other| other.conn != t.conn || other.db != t.db) {
                    self.error(tr("Select tables in one database"));
                    return;
                }
                let op = Operation::Model { tables: tables.iter().map(|t| (t.schema.clone(), t.table.clone())).collect() };
                (t, Mode::Model { view: None }, Some(op))
            }
            ExplorerAction::TableShortcut(t) => (t, Mode::Shortcut, None),
            ExplorerAction::GenerateDdl(t) => {
                let op = Operation::Definition { schema: t.schema.clone(), table: t.table.clone() };
                (t, Mode::Ddl, Some(op))
            }
            _ => return,
        };
        if self.table_actions.dialog.as_ref().is_some_and(|d| d.pending) {
            self.error(tr("Wait for the current table operation to finish"));
            return;
        }
        let Some(c) = self.conn(table.conn) else {
            return;
        };
        let driver = c.cfg.driver;
        if !matches!(mode, Mode::Shortcut) && c.status != ConnStatus::Connected {
            self.error(tr("The connection is not open"));
            return;
        }
        if matches!(mode, Mode::Duplicate { .. } | Mode::Grants { .. }) && c.cfg.read_only {
            self.error(tr("Read-only connection"));
            return;
        }
        if matches!(&mode, Mode::Export { kind, .. } if !crate::db::table_export::supported_kind(driver, *kind)) {
            self.error(tr("Complete table export is not available for this engine"));
            return;
        }
        if matches!(mode, Mode::Grants { .. }) && !crate::db::table_privileges::supported(driver) {
            self.error(tr("Not available for this engine"));
            return;
        }
        if matches!(mode, Mode::Shortcut) && std::env::var_os("DATACRAFT_DEMO").is_some() {
            self.error(tr("Save this connection before creating a shortcut"));
            return;
        }
        self.table_actions.dialog = Some(Dialog { id: 0, table, driver, mode, pending: false, progress: Arc::default(), error: None, success: None });
        if let Some(operation) = start {
            self.start_table_operation(operation);
        } else if matches!(self.table_actions.dialog.as_ref().map(|d| &d.mode), Some(Mode::Shortcut)) {
            self.create_table_shortcut();
        }
    }

    fn start_table_operation(&mut self, operation: Operation) {
        let Some(d) = self.table_actions.dialog.as_mut() else {
            return;
        };
        d.id = uuid::Uuid::new_v4().as_u128() as u64;
        d.progress = Arc::default();
        d.pending = true;
        d.error = None;
        d.success = None;
        self.worker.send(Request::TableOperation {
            conn: d.table.conn,
            db: d.table.db.clone(),
            request_id: d.id,
            operation: Box::new(operation),
            progress: Arc::clone(&d.progress),
        });
    }

    pub(super) fn table_operation_received(&mut self, request_id: u64, result: Result<Outcome, String>) {
        let Some(d) = self.table_actions.dialog.as_mut().filter(|d| d.id == request_id) else {
            return;
        };
        d.pending = false;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Mode::Grants { loaded, .. } = &mut d.mode {
                    *loaded = false;
                }
                if let Mode::Duplicate { plan, .. } = &mut d.mode {
                    *plan = None;
                }
                d.error = Some(error);
                return;
            }
        };
        match outcome {
            Outcome::Plan(plan) => {
                if let Mode::Duplicate { plan: slot, target, planned_name, .. } = &mut d.mode {
                    *planned_name = target.trim().into();
                    *slot = Some(plan);
                }
            }
            Outcome::Grants(Grants { principals, privileges, granted }) => {
                if let Mode::Grants { accounts, available, old, chosen, loaded, selected } = &mut d.mode {
                    *accounts = principals;
                    *available = privileges;
                    *old = granted.clone();
                    *chosen = granted.into_iter().collect();
                    *loaded = selected.is_some();
                }
            }
            Outcome::Model(model) => {
                if let Mode::Model { view } = &mut d.mode {
                    *view = Some(crate::ui::table_model::ModelView::new(model));
                }
            }
            Outcome::Definition(definition) => {
                let t = d.table.clone();
                let text = definition.ddl.iter().map(|sql| format!("{sql};\n\n")).collect();
                self.table_actions.dialog = None;
                let i = self.new_console(Some(t.conn), Some(format!("{}.sql", t.table)), text);
                self.consoles[i].db = Some(t.db);
            }
            Outcome::Exported { path, rows, print } => {
                d.success = Some(trf("Exported {n} rows to {path}", &[("n", &rows.to_string()), ("path", &path.to_string_lossy())]));
                if print {
                    self.worker.send(Request::OpenTableReport(path));
                }
            }
            Outcome::Done => {
                let t = d.table.clone();
                let name = if let Mode::Duplicate { target, .. } = &d.mode { target.trim().to_owned() } else { t.table.clone() };
                self.table_actions.dialog = None;
                self.refresh_table_scope(&t);
                self.info(trf("Table “{name}” created", &[("name", &name)]));
            }
        }
    }

    fn refresh_table_scope(&mut self, table: &TableRef) {
        if let Some(c) = self.conn_mut(table.conn) {
            c.loading.remove(&table.db);
        }
        self.load_database(table.conn, table.db.clone());
        self.inspect_object(table.clone(), false);
    }

    pub(super) fn show_table_actions(&mut self, ctx: &egui::Context) {
        let intent = self.table_actions.dialog.as_mut().and_then(|d| d.show(ctx));
        match intent {
            Some(Intent::Operation(operation)) => self.start_table_operation(operation),
            Some(Intent::Export) => {
                let Some(d) = self.table_actions.dialog.as_ref() else {
                    return;
                };
                let Mode::Export { kind, path } = &d.mode else {
                    return;
                };
                let path = PathBuf::from(path.trim());
                if matches!(kind, ExportKind::Print { .. })
                    && !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
                {
                    if let Some(d) = self.table_actions.dialog.as_mut() {
                        d.error = Some(tr("Choose an HTML file for the print report").into());
                    }
                    return;
                }
                let overwrite = path.exists();
                if overwrite
                    && rfd::MessageDialog::new()
                        .set_title(tr("Replace file?"))
                        .set_description(tr("The destination already exists. Replace it only after the export succeeds?"))
                        .set_buttons(rfd::MessageButtons::YesNo)
                        .show()
                        != rfd::MessageDialogResult::Yes
                {
                    return;
                }
                let request = ExportRequest { schema: d.table.schema.clone(), table: d.table.table.clone(), path, kind: *kind, overwrite };
                self.start_table_operation(Operation::Export(request));
            }
            Some(Intent::SaveModel(svg)) => {
                let Some(d) = self.table_actions.dialog.as_ref() else {
                    return;
                };
                let Mode::Model { view: Some(view) } = &d.mode else {
                    return;
                };
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter(if svg { "SVG" } else { "JSON" }, &[if svg { "svg" } else { "json" }])
                    .set_file_name(if svg { "table-model.svg" } else { "table-model.json" })
                    .save_file()
                {
                    let content = if svg { Ok(view.svg()) } else { serde_json::to_string_pretty(&view.model) };
                    match content {
                        Ok(contents) => self.worker.send(Request::SaveTableArtifact { path, contents }),
                        Err(error) => self.error(error.to_string()),
                    }
                }
            }
            Some(Intent::Close) => self.table_actions.dialog = None,
            None => {}
        }
    }

    fn create_table_shortcut(&mut self) {
        let Some(d) = self.table_actions.dialog.as_ref() else {
            return;
        };
        let ext = crate::table_shortcut::extension();
        if let Some(path) = rfd::FileDialog::new().add_filter(tr("Table shortcut"), &[ext]).set_file_name(format!("{}.{}", d.table.table, ext)).save_file() {
            self.worker.send(Request::CreateTableShortcut { path, target: crate::table_shortcut::Target::from(&d.table) });
        }
        self.table_actions.dialog = None;
    }

    pub(super) fn open_startup_table(&mut self, conn: ConnId) {
        let Some(target) = self.table_actions.startup.take_if(|t| t.connection == conn) else {
            return;
        };
        self.open_table(&TableRef { conn, db: target.database, schema: target.schema, table: target.table });
    }

    pub(super) fn table_has_pending_edits(&self, t: &TableRef) -> bool {
        let default_db = self.conn(t.conn).and_then(|c| c.default_db());
        self.consoles.iter().any(|c| {
            c.conn == Some(t.conn)
                && c.db.as_deref().or(default_db) == Some(t.db.as_str())
                && c.table.as_ref().is_some_and(|table| table.schema == t.schema && table.table == t.table)
                && c.has_pending_edits()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_and_table() -> (DataCraftApp, TableRef) {
        let mut app = super::super::tests::tabs_app();
        let mut connection = ConnState::new(ConnectionConfig::new(7, Driver::Sqlite));
        connection.status = ConnStatus::Connected;
        connection.databases = vec![DatabaseInfo {
            name: "main".into(),
            current: true,
            schemas: Some(vec![SchemaInfo {
                name: "main".into(),
                tables: vec![TableInfo { name: "notes".into(), kind: crate::db::ObjectKind::Table, columns: vec![] }],
            }]),
        }];
        app.conns.push(connection);
        (app, TableRef { conn: 7, db: "main".into(), schema: "main".into(), table: "notes".into() })
    }

    #[test]
    fn review_cached_ddl_is_available_offline_and_for_other_engines() {
        for driver in [Driver::Oracle, Driver::ClickHouse, Driver::Kusto, Driver::Sqlite] {
            for status in [ConnStatus::Connected, ConnStatus::Disconnected] {
                if driver == Driver::Sqlite && status == ConnStatus::Connected {
                    continue;
                }
                let (mut app, table) = app_and_table();
                app.conns[0].cfg.driver = driver;
                app.conns[0].status = status;
                app.conns[0].databases[0].schemas.as_mut().unwrap()[0].tables[0].columns =
                    vec![ColumnInfo { name: "id".into(), data_type: "INTEGER".into(), primary_key: true, nullable: false }];
                app.handle_explorer(&egui::Context::default(), vec![ExplorerAction::GenerateDdl(table)]);
                assert!(app.table_actions.dialog.is_none(), "{driver:?}: local DDL should not start a catalog request");
                assert!(
                    app.consoles.iter().any(|c| c.text.contains("create table") && c.text.contains("primary key") && c.text.contains("id")),
                    "{driver:?}: cached DDL was lost"
                );
            }
        }
    }

    #[test]
    fn new_table_actions_open_real_workflows_and_reject_read_only_writes() {
        let (mut app, table) = app_and_table();
        let ctx = egui::Context::default();
        app.handle_table_action(&ctx, ExplorerAction::DuplicateTable(table.clone(), true));
        assert!(matches!(&app.table_actions.dialog.as_ref().unwrap().mode, Mode::Duplicate { data: true, target, .. } if target == "notes_copy"));
        for (action, expected) in [
            (ExplorerAction::ExportTable(table.clone()), "Export Table"),
            (ExplorerAction::DumpTable(table.clone(), true, true), "Dump SQL File"),
            (ExplorerAction::PrintTable(table.clone()), "Print Table"),
        ] {
            app.handle_table_action(&ctx, action);
            assert_eq!(app.table_actions.dialog.as_ref().unwrap().title(), tr(expected));
        }
        app.table_actions.dialog = None;
        app.conns[0].cfg.read_only = true;
        app.handle_table_action(&ctx, ExplorerAction::DuplicateTable(table.clone(), false));
        assert!(app.table_actions.dialog.is_none());
        app.handle_table_action(&ctx, ExplorerAction::ExportTable(table.clone()));
        assert!(app.table_actions.dialog.is_some());
        app.table_actions.dialog = None;
        app.conns[0].status = ConnStatus::Disconnected;
        app.handle_table_action(&ctx, ExplorerAction::ExportTable(table));
        assert!(app.table_actions.dialog.is_none());
    }

    #[test]
    fn late_table_results_cannot_replace_a_new_workflow() {
        let (mut app, t) = app_and_table();
        app.handle_table_action(&egui::Context::default(), ExplorerAction::DuplicateTable(t, true));
        app.table_actions.dialog.as_mut().unwrap().id = 12;
        app.table_operation_received(11, Ok(Outcome::Done));
        assert_eq!(app.table_actions.dialog.as_ref().unwrap().id, 12);
        app.table_operation_received(11, Err("old error".into()));
        assert!(app.table_actions.dialog.as_ref().unwrap().error.is_none());
    }

    #[test]
    fn table_writes_preserve_unsaved_data_editor_changes() {
        let (mut app, t) = app_and_table();
        app.open_table(&t);
        let index = app.active;
        let mut view = ResultView::new(crate::db::ResultSet { columns: vec!["id".into()], rows: vec![vec![Some("1".into())]], ..Default::default() }, 901);
        view.set_edit_target(Ok(editing::EditTarget {
            schema: "main".into(),
            table: "notes".into(),
            col_names: vec!["id".into()],
            key_cols: vec![0],
            binary_cols: vec![false],
            has_pk: true,
            insert_only: false,
        }));
        view.edit.as_mut().unwrap().state.set_value(&view.rs, RowRef::Existing(0), 0, Some("2".into()));
        app.consoles[index].results.push(view);
        for kind in [DropKind::Empty, DropKind::Truncate, DropKind::Rename, DropKind::Table] {
            app.open_object_dialog(t.clone(), kind);
            assert!(app.drop.is_none());
            assert!(app.consoles[index].has_pending_edits());
        }
        app.open_modify_table(t);
        assert!(app.modify.is_none());
    }

    #[test]
    fn workflow_footer_remains_visible_in_small_windows() {
        use egui_kittest::kittest::Queryable;
        let (mut app, table) = app_and_table();
        app.handle_table_action(&egui::Context::default(), ExplorerAction::ExportTable(table));
        app.table_actions.dialog.as_mut().unwrap().error = Some("Very long error\n".repeat(80));
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(600.0, 370.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                app.show_table_actions(ui.ctx());
            },
            app,
        );
        h.run_steps(3);
        assert!(h.get_by_label("Close").rect().bottom() < 370.0);
        h.get_by_label("Close").click();
        h.run_steps(2);
        assert!(h.state().table_actions.dialog.is_none());
    }

    #[test]
    #[ignore = "Requires a GPU; renders full-window table workflows with a disposable SQLite database"]
    fn render_table_context_menu_workflows() {
        use egui_kittest::kittest::Queryable;
        let directory = std::env::var_os("DATACRAFT_CAPTURE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/table-action-snapshots"));
        std::fs::create_dir_all(&directory).unwrap();
        let path = std::env::temp_dir().join(format!("datacraft-ui-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE categories(id INTEGER PRIMARY KEY,name TEXT); INSERT INTO categories VALUES(1,'Sample category'); CREATE TABLE notes(id INTEGER PRIMARY KEY,category_id INTEGER REFERENCES categories(id),value TEXT,computed TEXT GENERATED ALWAYS AS ('Note '||id) VIRTUAL);").unwrap();
        for id in 1..=1200 {
            db.execute("INSERT INTO notes(id,category_id,value) VALUES(?1,1,'Synthetic QA record')", [id]).unwrap();
        }
        drop(db);
        let (mut app, table) = app_and_table();
        app.consoles.clear();
        app.active = 0;
        app.conns[0].cfg.file_path = path.to_string_lossy().into_owned();
        app.conns[0].cfg.name = "Sample SQLite".into();
        app.conns[0].status = ConnStatus::Disconnected;
        app.conns[0].databases.clear();
        app.connect(7);
        app.object_browser.open(crate::ui::object_browser::ObjectScope {
            conn: 7,
            db: "main".into(),
            schema: Some("main".into()),
            kind: Some(crate::db::ObjectKind::Table),
        });
        app.object_browser.layout = crate::ui::object_browser::ObjectLayout::Grid;
        let mut first = true;
        let mut frame = eframe::Frame::_new_kittest();
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), true);
                    return;
                }
                eframe::App::logic(app, ui.ctx(), &mut frame);
                let bounds = ui.ctx().content_rect();
                let mut root = ui.new_child(egui::UiBuilder::new().max_rect(bounds));
                root.set_clip_rect(bounds);
                eframe::App::ui(app, &mut root, &mut frame);
            },
            app,
        );
        fn wait(h: &mut egui_kittest::Harness<'_, DataCraftApp>, ready: impl Fn(&DataCraftApp) -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                h.run_steps(2);
                if ready(h.state()) {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "workflow did not complete");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        wait(&mut h, |a| a.conns[0].status == ConnStatus::Connected && !a.object_browser.catalog_loading);
        h.get_by_label("notes").click_secondary();
        h.run_steps(3);
        h.render().unwrap().save(directory.join("objects-context-menu.png")).unwrap();
        h.key_press(Key::Escape);
        h.run_steps(2);
        h.state_mut().handle_table_action(&egui::Context::default(), ExplorerAction::DuplicateTable(table.clone(), true));
        h.run_steps(2);
        h.get_by_label("Preview SQL").click();
        wait(&mut h, |a| a.table_actions.dialog.as_ref().is_some_and(|d| !d.pending));
        assert!(h.state().table_actions.dialog.as_ref().unwrap().error.is_none());
        h.render().unwrap().save(directory.join("duplicate-table.png")).unwrap();
        h.get_by_label("Duplicate").click();
        wait(&mut h, |a| a.table_actions.dialog.is_none());
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM notes_copy", [], |r| r.get::<_, i64>(0)).unwrap(), 1200);
        drop(db);
        h.state_mut().handle_table_action(&egui::Context::default(), ExplorerAction::ExportTable(table.clone()));
        if let Mode::Export { kind, path } = &mut h.state_mut().table_actions.dialog.as_mut().unwrap().mode {
            *kind = ExportKind::Json;
            *path = directory.join("notes-export.json").to_string_lossy().into_owned();
        }
        // Give the export a fresh destination so a repeat capture cannot open an overwrite dialog.
        let exported = directory.join(format!("notes-export-{}.json", uuid::Uuid::new_v4()));
        if let Mode::Export { path, .. } = &mut h.state_mut().table_actions.dialog.as_mut().unwrap().mode {
            *path = exported.to_string_lossy().into_owned();
        }
        h.run_steps(2);
        h.get_by_label("Export").click();
        wait(&mut h, |a| a.table_actions.dialog.as_ref().is_some_and(|d| !d.pending));
        assert!(h.state().table_actions.dialog.as_ref().unwrap().error.is_none());
        let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(&exported).unwrap()).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1200);
        h.render().unwrap().save(directory.join("export-table.png")).unwrap();
        h.get_by_label("Close").click();
        h.run_steps(2);
        h.state_mut().handle_table_action(&egui::Context::default(), ExplorerAction::TableModel(vec![table.clone()]));
        wait(&mut h, |a| a.table_actions.dialog.as_ref().is_some_and(|d| !d.pending));
        assert!(h.state().table_actions.dialog.as_ref().unwrap().error.is_none());
        h.render().unwrap().save(directory.join("table-model.png")).unwrap();
        h.get_by_label("Close").click();
        h.run_steps(2);
        h.state_mut().handle_table_action(&egui::Context::default(), ExplorerAction::PrintTable(table));
        h.run_steps(2);
        h.render().unwrap().save(directory.join("print-table.png")).unwrap();
        h.get_by_label("Close").click();
        h.run_steps(2);
        h.state_mut().disconnect(7);
        wait(&mut h, |a| a.conns[0].status == ConnStatus::Disconnected);
        drop(h);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while let Err(error) = std::fs::remove_file(&path) {
            assert!(std::time::Instant::now() < deadline, "fixture cleanup: {error}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_file(exported).unwrap();
    }
}
