//! Catalog DDL and duplicate-table previews on dedicated background sessions.

use super::*;
use crate::db::table_duplicate::{Operation, Outcome, Progress};
use std::sync::{Arc, atomic::Ordering};

#[derive(Default)]
pub(super) struct TableDuplicateState {
    dialog: Option<Dialog>,
    ddl: HashMap<u64, TableRef>,
}

struct Dialog {
    id: u64,
    table: TableRef,
    target: String,
    data: bool,
    plan: Option<crate::db::alter::Plan>,
    planned_name: String,
    pending: bool,
    progress: Arc<Progress>,
    error: Option<String>,
}

impl Dialog {
    fn operation(&self) -> Operation {
        if let Some(plan) = self.plan.as_ref().filter(|_| self.target.trim() == self.planned_name) {
            return Operation::Duplicate {
                schema: self.table.schema.clone(),
                table: self.table.table.clone(),
                target: self.target.trim().into(),
                data: self.data,
                expected: plan.statements.clone(),
            };
        }
        Operation::PreviewDuplicate { schema: self.table.schema.clone(), table: self.table.table.clone(), target: self.target.trim().into(), data: self.data }
    }

    fn show(&mut self, ctx: &egui::Context) -> Option<Operation> {
        let subtitle = format!("{} / {} / {}", self.table.db, self.table.schema, self.table.table);
        let mut operation = None;
        crate::ui::workflow_dialog::show(ctx, egui::Id::new("duplicate_table"), tr("Duplicate Table"), &subtitle, vec2(680.0, 600.0), |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::both().auto_shrink([false, false]).show(&mut body, |ui| {
                ui.add_enabled_ui(!self.pending, |ui| {
                    let name_label = ui.label(tr("New table name"));
                    if ui.add(egui::TextEdit::singleline(&mut self.target).desired_width(f32::INFINITY)).labelled_by(name_label.id).changed() {
                        self.plan = None;
                    }
                    ui.label(tr(if self.data { "Structure and Data" } else { "Structure Only" }));
                    ui.label(tr("The existing table is preserved. The destination must not exist."));
                    if let Some(plan) = self.plan.as_ref().filter(|_| self.target.trim() == self.planned_name) {
                        for warning in &plan.warnings {
                            ui.colored_label(palette(ctx.global_style().visuals.dark_mode).warn, warning);
                        }
                        let mut sql = plan.statements.iter().map(|sql| format!("{sql};\n")).collect::<String>();
                        ui.add(egui::TextEdit::multiline(&mut sql).code_editor().desired_width(f32::INFINITY).desired_rows(16).interactive(false));
                    }
                });
                if let Some(error) = &self.error {
                    ui.colored_label(palette(ctx.global_style().visuals.dark_mode).error, error);
                }
            });
            let mut footer = regions.footer_ui(root);
            footer.horizontal(|ui| {
                if self.pending {
                    ui.spinner();
                    ui.label(tr("Loading…"));
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr(if self.pending { "Cancel" } else { "Close" })).clicked() {
                        self.progress.cancel.store(true, Ordering::Relaxed);
                    }
                    let approved = self.plan.is_some() && self.target.trim() == self.planned_name;
                    let enabled = !self.pending && !self.target.trim().is_empty() && self.target.trim() != self.table.table;
                    if ui.add_enabled(enabled, egui::Button::new(tr(if approved { "Duplicate" } else { "Preview SQL" }))).clicked() {
                        operation = Some(self.operation());
                    }
                });
            });
        });
        if !self.pending && ctx.input(|input| input.key_pressed(Key::Escape)) {
            self.progress.cancel.store(true, Ordering::Relaxed);
        }
        operation
    }
}

impl DataCraftApp {
    pub(super) fn open_table_duplicate(&mut self, table: TableRef, data: bool) {
        if self.table_duplicate.dialog.as_ref().is_some_and(|dialog| dialog.pending) {
            self.error(tr("Wait for the current table operation to finish"));
            return;
        }
        if self.refuse_read_only(table.conn) {
            return;
        }
        let Some(connection) = self.conn(table.conn).filter(|connection| connection.status == ConnStatus::Connected) else {
            self.error(tr("The connection is not open"));
            return;
        };
        if !crate::db::table_definition::supported(connection.cfg.driver) {
            self.error(tr("Not available for this engine"));
            return;
        }
        if !self.table_info(table.conn, Some(&table.db), &table.schema, &table.table).is_some_and(|info| info.kind == crate::db::ObjectKind::Table) {
            self.error(tr("Select one supported table"));
            return;
        }
        if self.table_has_pending_edits(&table) {
            self.error(tr("Save or discard this table's pending edits before changing it."));
            return;
        }
        self.table_duplicate.dialog = Some(Dialog {
            id: 0,
            target: format!("{}_copy", table.table),
            table,
            data,
            plan: None,
            planned_name: String::new(),
            pending: false,
            progress: Arc::default(),
            error: None,
        });
    }

    pub(super) fn request_catalog_table_ddl(&mut self, table: &TableRef) -> bool {
        let live = self
            .conn(table.conn)
            .is_some_and(|connection| connection.status == ConnStatus::Connected && crate::db::table_definition::supported(connection.cfg.driver));
        if !live || !self.table_info(table.conn, Some(&table.db), &table.schema, &table.table).is_some_and(|info| info.kind == crate::db::ObjectKind::Table) {
            return false;
        }
        let request_id = uuid::Uuid::new_v4().as_u128() as u64;
        self.table_duplicate.ddl.insert(request_id, table.clone());
        self.worker.send(Request::TableDuplicate {
            conn: table.conn,
            db: table.db.clone(),
            request_id,
            operation: Box::new(Operation::Definition { schema: table.schema.clone(), table: table.table.clone() }),
            progress: Arc::default(),
        });
        true
    }

    fn start_table_duplicate(&mut self, operation: Operation) {
        let Some(table) = self.table_duplicate.dialog.as_ref().map(|dialog| dialog.table.clone()) else {
            return;
        };
        if self.refuse_read_only(table.conn) {
            return;
        }
        if !self.conn(table.conn).is_some_and(|connection| connection.status == ConnStatus::Connected) {
            self.error(tr("The connection is not open"));
            return;
        }
        if self.table_has_pending_edits(&table) {
            self.error(tr("Save or discard this table's pending edits before changing it."));
            return;
        }
        let Some(dialog) = self.table_duplicate.dialog.as_mut().filter(|dialog| !dialog.pending) else {
            return;
        };
        dialog.id = uuid::Uuid::new_v4().as_u128() as u64;
        dialog.pending = true;
        dialog.progress = Arc::default();
        dialog.error = None;
        self.worker.send(Request::TableDuplicate {
            conn: table.conn,
            db: table.db,
            request_id: dialog.id,
            operation: Box::new(operation),
            progress: Arc::clone(&dialog.progress),
        });
    }

    pub(super) fn table_duplicate_received(&mut self, conn: ConnId, request_id: u64, result: Result<Outcome, String>) {
        if let Some(table) = self.table_duplicate.ddl.remove(&request_id) {
            if table.conn != conn || !self.conn(conn).is_some_and(|connection| connection.status == ConnStatus::Connected) {
                return;
            }
            match result {
                Ok(Outcome::Definition(definition)) => {
                    let text = definition.ddl.iter().map(|sql| format!("{sql};\n\n")).collect();
                    let index = self.new_console(Some(conn), Some(format!("{}.sql", table.table)), text);
                    self.consoles[index].db = Some(table.db);
                }
                Err(error) => self.error(error),
                _ => {}
            }
            return;
        }
        let Some(dialog) = self.table_duplicate.dialog.as_mut().filter(|dialog| dialog.table.conn == conn && dialog.id == request_id) else {
            return;
        };
        dialog.pending = false;
        match result {
            Ok(Outcome::Plan(plan)) => {
                dialog.planned_name = dialog.target.trim().into();
                dialog.plan = Some(plan);
            }
            Ok(Outcome::Done) => {
                let table = dialog.table.clone();
                let name = dialog.target.trim().to_string();
                self.table_duplicate.dialog = None;
                if let Some(connection) = self.conn_mut(conn) {
                    connection.loading.remove(&table.db);
                }
                self.load_database(conn, table.db.clone());
                self.inspect_object(table, false);
                self.info(trf("Table “{name}” created", &[("name", &name)]));
            }
            Err(error) => {
                dialog.plan = None;
                dialog.error = Some(error);
            }
            _ => {}
        }
    }

    pub(super) fn cancel_table_duplicate(&mut self, conn: ConnId) {
        self.table_duplicate.ddl.retain(|_, table| table.conn != conn);
        if let Some(dialog) = self.table_duplicate.dialog.as_ref().filter(|dialog| dialog.table.conn == conn) {
            dialog.progress.cancel.store(true, Ordering::Relaxed);
            self.table_duplicate.dialog = None;
        }
    }

    pub(super) fn table_duplicate_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.table_duplicate.dialog.as_mut() else {
            return;
        };
        let operation = dialog.show(ctx);
        if !dialog.pending && dialog.progress.cancel.load(Ordering::Relaxed) {
            self.table_duplicate.dialog = None;
            return;
        }
        if let Some(operation) = operation {
            self.start_table_duplicate(operation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_opens_only_for_one_connected_writable_supported_table() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        app.handle_explorer(&egui::Context::default(), vec![ExplorerAction::DuplicateTable(table.clone(), true)]);
        let dialog = app.table_duplicate.dialog.as_ref().unwrap();
        assert_eq!(dialog.target, "notes_copy");
        assert!(dialog.data);
        for (driver, connected, read_only) in
            [(Driver::Sqlite, false, false), (Driver::Sqlite, true, true), (Driver::Oracle, true, false), (Driver::MongoDb, true, false)]
        {
            let (mut app, table) = crate::app::tests::table_menu_app(driver, connected);
            app.conns[0].cfg.read_only = read_only;
            app.open_table_duplicate(table, true);
            assert!(app.table_duplicate.dialog.is_none());
        }
    }

    #[test]
    fn catalog_ddl_is_requested_only_when_the_live_table_is_supported() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        assert!(app.request_catalog_table_ddl(&table));
        assert_eq!(app.table_duplicate.ddl.len(), 1);
        for (driver, connected) in [(Driver::Sqlite, false), (Driver::ClickHouse, true), (Driver::MongoDb, true)] {
            let (mut app, table) = crate::app::tests::table_menu_app(driver, connected);
            assert!(!app.request_catalog_table_ddl(&table));
            assert!(app.table_duplicate.ddl.is_empty());
        }
    }

    #[test]
    fn duplicate_target_changes_require_a_new_preview() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        app.open_table_duplicate(table, true);
        let dialog = app.table_duplicate.dialog.as_mut().unwrap();
        dialog.plan = Some(crate::db::alter::Plan { statements: vec!["approved".into()], ..Default::default() });
        dialog.planned_name = dialog.target.clone();
        assert!(matches!(dialog.operation(),Operation::Duplicate{expected,..} if expected==["approved"]));
        dialog.target = "changed".into();
        assert!(matches!(dialog.operation(),Operation::PreviewDuplicate{target,..} if target=="changed"));
    }

    #[test]
    fn duplicate_preview_ignores_old_requests_and_disconnect_cancels_pending_work() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        app.open_table_duplicate(table.clone(), true);
        let dialog = app.table_duplicate.dialog.as_mut().unwrap();
        dialog.id = 41;
        dialog.pending = true;
        let progress = Arc::clone(&dialog.progress);
        for (conn, id) in [(7, 40), (8, 41)] {
            app.table_duplicate_received(conn, id, Ok(Outcome::Plan(Default::default())));
            assert!(app.table_duplicate.dialog.as_ref().unwrap().pending);
            assert!(app.table_duplicate.dialog.as_ref().unwrap().plan.is_none());
        }
        app.table_duplicate_received(7, 41, Ok(Outcome::Plan(Default::default())));
        assert!(!app.table_duplicate.dialog.as_ref().unwrap().pending);
        assert!(app.table_duplicate.dialog.as_ref().unwrap().plan.is_some());
        assert!(app.request_catalog_table_ddl(&table));
        app.disconnect(7);
        assert!(app.table_duplicate.dialog.is_none());
        assert!(app.table_duplicate.ddl.is_empty());
        assert!(progress.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn duplicate_refuses_pending_edits_and_a_new_read_only_setting() {
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        app.open_table_duplicate(table.clone(), true);
        let operation = app.table_duplicate.dialog.as_ref().unwrap().operation();
        app.conns[0].cfg.read_only = true;
        app.start_table_duplicate(operation);
        assert!(!app.table_duplicate.dialog.as_ref().unwrap().pending);
        app.table_duplicate.dialog = None;
        app.conns[0].cfg.read_only = false;
        app.open_table(&table);
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
        view.edit.as_mut().unwrap().state.set_value(&view.rs, editing::RowRef::Existing(0), 0, Some("2".into()));
        app.consoles[app.active].results.push(view);
        app.open_table_duplicate(table, true);
        assert!(app.table_duplicate.dialog.is_none());
        assert!(app.consoles[app.active].has_pending_edits());
    }

    #[test]
    fn duplicate_dialog_submits_preview_and_exposes_cancellation() {
        use egui_kittest::kittest::Queryable;
        let (mut app, table) = crate::app::tests::table_menu_app(Driver::Sqlite, true);
        app.open_table_duplicate(table, true);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(960.0, 760.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                app.table_duplicate_dialog(ui.ctx());
            },
            app,
        );
        harness.run_steps(3);
        assert!(harness.query_by_label("New table name").is_some());
        harness.get_by_label("Preview SQL").click();
        harness.run_steps(2);
        assert!(harness.state().table_duplicate.dialog.as_ref().unwrap().pending);
        harness.get_by_label("Cancel").click();
        harness.run_steps(2);
        assert!(harness.state().table_duplicate.dialog.as_ref().unwrap().progress.cancel.load(Ordering::Relaxed));
    }
}
