//! Explicit table grants, isolated from other table workflows.

use super::*;
use crate::db::table_privileges::{self, Edit, Grants, Principal};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub(super) struct State {
    dialog: Option<Dialog>,
    next_request: u64,
}

struct Dialog {
    id: u64,
    table: TableRef,
    driver: Driver,
    principals: Vec<Principal>,
    selected: Option<Principal>,
    available: Vec<String>,
    old: Vec<String>,
    chosen: BTreeSet<String>,
    loaded: bool,
    pending: bool,
    cancel: Arc<AtomicBool>,
    error: Option<String>,
}

impl Dialog {
    fn new(table: TableRef, driver: Driver) -> Self {
        Self {
            id: 0,
            table,
            driver,
            principals: Vec::new(),
            selected: None,
            available: Vec::new(),
            old: Vec::new(),
            chosen: BTreeSet::new(),
            loaded: false,
            pending: false,
            cancel: Arc::default(),
            error: None,
        }
    }
    fn edit(&self) -> anyhow::Result<Edit> {
        anyhow::ensure!(self.loaded && !self.pending, tr("Preview the privileges again before applying."));
        let Some(principal) = self.selected.clone() else {
            anyhow::bail!(tr("Select user or role"));
        };
        let new: Vec<_> = self.chosen.iter().cloned().collect();
        let expected = table_privileges::statements(self.driver, &self.table.schema, &self.table.table, &principal, &self.old, &new)?;
        Ok(Edit { principal, old: self.old.clone(), new, expected })
    }

    fn show(&mut self, ctx: &egui::Context) -> Option<Intent> {
        let mut intent = None;
        let subtitle = format!("{} / {} / {}", self.table.db, self.table.schema, self.table.table);
        let pending = self.pending;
        crate::ui::workflow_dialog::show(ctx, egui::Id::new("table_grants"), tr("Set Privileges"), &subtitle, vec2(680.0, 600.0), |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::both().auto_shrink([false, false]).show(&mut body, |ui| {
                ui.label(tr("These are explicit table grants, not the user's full effective permissions."));
                ui.add_enabled_ui(!pending, |ui| {
                    let account_label =
                        |p: &Principal| if matches!(self.driver, Driver::MySql | Driver::MariaDb) { format!("{}@{}", p.name, p.host) } else { p.name.clone() };
                    let before = self.selected.clone();
                    let label = ui.label(tr("Select user or role"));
                    egui::ComboBox::from_id_salt("table_principal")
                        .selected_text(self.selected.as_ref().map(account_label).unwrap_or_else(|| tr("Select user or role").into()))
                        .show_ui(ui, |ui| {
                            for principal in &self.principals {
                                ui.selectable_value(&mut self.selected, Some(principal.clone()), account_label(principal));
                            }
                        })
                        .response
                        .labelled_by(label.id);
                    if before != self.selected {
                        self.loaded = false;
                        self.old.clear();
                        self.chosen.clear();
                        intent = Some(Intent::Request(table_privileges::Request::Load(self.selected.clone())));
                    }
                    if self.loaded {
                        for privilege in &self.available {
                            let mut checked = self.chosen.contains(privilege);
                            if ui.checkbox(&mut checked, privilege).changed() {
                                if checked {
                                    self.chosen.insert(privilege.clone());
                                } else {
                                    self.chosen.remove(privilege);
                                }
                            }
                        }
                    }
                    if ui.button(tr("Reload privileges")).clicked() {
                        self.loaded = false;
                        intent = Some(Intent::Request(table_privileges::Request::Load(self.selected.clone())));
                    }
                });
                ui.label(tr("Some engines apply grants individually. Reload after an error to inspect the current state."));
                if let Ok(edit) = self.edit()
                    && !edit.expected.is_empty()
                {
                    let preview: String = edit.expected.iter().map(|sql| format!("{sql};\n")).collect();
                    ui.separator();
                    ui.label(tr("SQL preview"));
                    ui.add(egui::Label::new(sql::highlight(&preview, ctx.global_style().visuals.dark_mode, FontId::monospace(12.0))).wrap());
                }
                if let Some(error) = &self.error {
                    ui.colored_label(palette(ctx.global_style().visuals.dark_mode).error, error);
                }
            });
            let mut footer = regions.footer_ui(root);
            footer.horizontal(|ui| {
                if pending {
                    ui.spinner();
                    ui.label(tr("Loading…"));
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr(if pending { "Cancel" } else { "Close" })).clicked() {
                        if pending {
                            self.cancel.store(true, Ordering::Relaxed);
                        } else {
                            intent = Some(Intent::Close);
                        }
                    }
                    let edit = self.edit().ok().filter(|edit| !edit.expected.is_empty());
                    if ui.add_enabled(edit.is_some(), egui::Button::new(tr("Apply"))).clicked()
                        && let Some(edit) = edit
                    {
                        intent = Some(Intent::Request(table_privileges::Request::Apply(edit)));
                    }
                });
            });
        });
        if ctx.input(|input| input.key_pressed(Key::Escape)) {
            if pending {
                self.cancel.store(true, Ordering::Relaxed);
            } else {
                intent = Some(Intent::Close);
            }
        }
        intent
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Intent {
    Request(table_privileges::Request),
    Close,
}

impl DataCraftApp {
    fn table_grants_driver(&self, table: &TableRef) -> Result<Driver, &'static str> {
        let connection = self.conn(table.conn).filter(|connection| connection.status == ConnStatus::Connected).ok_or(tr("The connection is not open"))?;
        if connection.cfg.read_only {
            return Err(tr("Read-only connection"));
        }
        let driver = connection.cfg.driver;
        if !table_privileges::supported(driver) {
            return Err(tr("Table grants are not available for this engine"));
        }
        if !self.table_info(table.conn, Some(&table.db), &table.schema, &table.table).is_some_and(|info| info.kind == crate::db::ObjectKind::Table) {
            return Err(tr("Select one supported table"));
        }
        Ok(driver)
    }

    pub(super) fn open_table_grants(&mut self, table: TableRef) {
        let driver = match self.table_grants_driver(&table) {
            Ok(driver) => driver,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        if self.table_has_pending_edits(&table) {
            self.error(tr("Save or discard this table's pending edits before changing it."));
            return;
        }
        self.table_grants.dialog = Some(Dialog::new(table, driver));
        self.start_table_grants(table_privileges::Request::Load(None));
    }

    fn start_table_grants(&mut self, request: table_privileges::Request) {
        let Some(dialog) = self.table_grants.dialog.as_ref().filter(|dialog| !dialog.pending) else {
            return;
        };
        let table = dialog.table.clone();
        if let Err(error) = self.table_grants_driver(&table) {
            self.table_grants.dialog = None;
            self.error(error);
            return;
        }
        if matches!(request, table_privileges::Request::Apply(_)) && self.table_has_pending_edits(&table) {
            self.error(tr("Save or discard this table's pending edits before changing it."));
            return;
        }
        self.table_grants.next_request += 1;
        let Some(dialog) = self.table_grants.dialog.as_mut() else {
            return;
        };
        dialog.id = self.table_grants.next_request;
        dialog.pending = true;
        dialog.error = None;
        dialog.cancel = Arc::default();
        self.worker.send(Request::TableGrants {
            conn: table.conn,
            db: table.db,
            schema: table.schema,
            table: table.table,
            request_id: dialog.id,
            request,
            cancel: dialog.cancel.clone(),
        });
    }

    pub(super) fn table_grants_received(&mut self, conn: ConnId, id: u64, result: Result<Grants, String>) {
        let Some(table) = self
            .table_grants
            .dialog
            .as_ref()
            .filter(|dialog| dialog.table.conn == conn && dialog.id == id && dialog.pending)
            .map(|dialog| dialog.table.clone())
        else {
            return;
        };
        if let Err(error) = self.table_grants_driver(&table) {
            self.table_grants.dialog = None;
            self.error(error);
            return;
        }
        let Some(dialog) = self.table_grants.dialog.as_mut() else {
            return;
        };
        dialog.pending = false;
        match result {
            Ok(grants) => {
                dialog.principals = grants.principals;
                dialog.available = grants.privileges;
                dialog.old = grants.granted.clone();
                dialog.chosen = grants.granted.into_iter().collect();
                dialog.loaded = dialog.selected.is_some();
            }
            Err(error) => {
                dialog.loaded = false;
                dialog.error = Some(error);
            }
        }
    }

    pub(super) fn table_grants_dialog(&mut self, ctx: &egui::Context) {
        let Some(table) = self.table_grants.dialog.as_ref().map(|dialog| dialog.table.clone()) else {
            return;
        };
        if let Err(error) = self.table_grants_driver(&table) {
            self.table_grants.dialog = None;
            self.error(error);
            return;
        }
        match self.table_grants.dialog.as_mut().and_then(|dialog| dialog.show(ctx)) {
            Some(Intent::Request(request)) => self.start_table_grants(request),
            Some(Intent::Close) => self.table_grants.dialog = None,
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::{NodeT, Queryable};

    fn snapshot() -> Grants {
        Grants {
            principals: vec![Principal { name: "reader".into(), host: "".into() }],
            privileges: vec!["SELECT".into(), "UPDATE".into()],
            granted: vec!["SELECT".into()],
        }
    }

    #[test]
    fn grants_dialog_requires_connected_writable_server_table() {
        for (driver, connected, read_only, accepted) in [
            (Driver::Postgres, true, false, true),
            (Driver::Postgres, false, false, false),
            (Driver::MySql, true, true, false),
            (Driver::Sqlite, true, false, false),
            (Driver::MongoDb, true, false, false),
        ] {
            let (mut app, table) = super::super::tests::table_menu_app(driver, connected);
            app.conns[0].cfg.read_only = read_only;
            app.open_table_grants(table);
            assert_eq!(app.table_grants.dialog.is_some(), accepted, "{driver:?}");
        }
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        app.conns[0].databases[0].schemas.as_mut().unwrap()[0].tables[0].kind = crate::db::ObjectKind::View;
        app.open_table_grants(table);
        assert!(app.table_grants.dialog.is_none());
    }

    #[test]
    fn grants_reply_ids_prevent_stale_data_and_errors_invalidate_apply() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        let mut dialog = Dialog::new(table, Driver::Postgres);
        dialog.id = 4;
        dialog.pending = true;
        dialog.selected = Some(Principal { name: "reader".into(), host: "".into() });
        app.table_grants.dialog = Some(dialog);
        app.table_grants_received(7, 3, Ok(snapshot()));
        assert!(app.table_grants.dialog.as_ref().unwrap().pending);
        app.table_grants_received(7, 4, Ok(snapshot()));
        let dialog = app.table_grants.dialog.as_mut().unwrap();
        assert!(dialog.loaded && !dialog.pending);
        dialog.chosen.insert("UPDATE".into());
        assert_eq!(dialog.edit().unwrap().expected, ["GRANT UPDATE ON TABLE \"main\".\"notes\" TO \"reader\""]);
        dialog.id = 5;
        dialog.pending = true;
        app.table_grants_received(7, 5, Err("catalog permission denied".into()));
        let dialog = app.table_grants.dialog.as_ref().unwrap();
        assert!(!dialog.loaded && !dialog.pending);
        assert!(dialog.edit().is_err());
    }

    #[test]
    fn opening_another_grants_dialog_cancels_and_invalidates_the_first_request() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        app.open_table_grants(table.clone());
        let dialog = app.table_grants.dialog.as_ref().unwrap();
        let (cancel, id) = (dialog.cancel.clone(), dialog.id);
        app.open_table_grants(table);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(app.table_grants.dialog.as_ref().unwrap().id > id);
    }

    #[test]
    fn grants_checkboxes_show_the_exact_sql_that_apply_submits() {
        let (_, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        let mut dialog = Dialog::new(table, Driver::Postgres);
        let grants = snapshot();
        dialog.selected = grants.principals.first().cloned();
        dialog.principals = grants.principals;
        dialog.available = grants.privileges;
        dialog.old = grants.granted.clone();
        dialog.chosen = grants.granted.into_iter().collect();
        dialog.loaded = true;
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(800.0, 700.0)).build_ui_state(
            move |ui, (dialog, intent): &mut (Dialog, Option<Intent>)| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                if let Some(action) = dialog.show(ui.ctx()) {
                    *intent = Some(action);
                }
            },
            (dialog, None),
        );
        harness.run_steps(2);
        assert!(harness.get_by_label("Apply").accesskit_node().is_disabled());
        harness.get_by_label("UPDATE").click();
        harness.run_steps(2);
        assert!(harness.query_by_label("SQL preview").is_some());
        harness.get_by_label("Apply").click();
        harness.run_steps(2);
        let Some(Intent::Request(table_privileges::Request::Apply(edit))) = &harness.state().1 else {
            panic!("expected an apply request");
        };
        assert_eq!(edit.old, ["SELECT"]);
        assert_eq!(edit.new, ["SELECT", "UPDATE"]);
        assert_eq!(edit.expected, ["GRANT UPDATE ON TABLE \"main\".\"notes\" TO \"reader\""]);
    }

    #[test]
    fn grants_pending_dialog_allows_cancellation_but_not_apply() {
        let (_, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        let mut dialog = Dialog::new(table, Driver::Postgres);
        dialog.pending = true;
        let cancel = dialog.cancel.clone();
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(800.0, 700.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx());
            },
            dialog,
        );
        harness.run_steps(2);
        assert!(harness.get_by_label("Apply").accesskit_node().is_disabled());
        harness.get_by_label("Cancel").click();
        harness.run_steps(2);
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn grants_principal_selection_discards_the_previous_snapshot_before_loading() {
        let (_, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        let mut dialog = Dialog::new(table, Driver::Postgres);
        dialog.selected = Some(Principal { name: "reader".into(), host: "".into() });
        dialog.principals = vec![dialog.selected.clone().unwrap(), Principal { name: "writer".into(), host: "".into() }];
        dialog.old = vec!["SELECT".into()];
        dialog.chosen = ["SELECT".into(), "UPDATE".into()].into();
        dialog.loaded = true;
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(800.0, 700.0)).build_ui_state(
            move |ui, (dialog, intent): &mut (Dialog, Option<Intent>)| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                if let Some(action) = dialog.show(ui.ctx()) {
                    *intent = Some(action);
                }
            },
            (dialog, None),
        );
        harness.run_steps(2);
        harness.get_by_role_and_label(egui::accesskit::Role::ComboBox, "Select user or role").click();
        harness.run_steps(2);
        harness.get_by_label("writer").click();
        harness.run_steps(2);
        let (dialog, intent) = harness.state();
        assert!(!dialog.loaded && dialog.old.is_empty() && dialog.chosen.is_empty());
        assert!(matches!(intent, Some(Intent::Request(table_privileges::Request::Load(Some(principal)))) if principal.name == "writer"));
        assert!(harness.get_by_label("Apply").accesskit_node().is_disabled());
    }

    #[test]
    fn grants_write_flow_preserves_pending_table_edits() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        app.open_table(&table);
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
        view.edit.as_mut().unwrap().state.set_value(&view.rs, editing::RowRef::Existing(0), 0, Some("2".into()));
        app.consoles[index].results.push(view);
        app.open_table_grants(table);
        assert!(app.table_grants.dialog.is_none());
        assert!(app.consoles[index].has_pending_edits());
    }
}
