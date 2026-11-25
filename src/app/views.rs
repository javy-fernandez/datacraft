//! "New View…" / "Edit View…" and "Generate DDL" of views: the app side of
//! `ui::view_dialog` and `db::views`.

use super::{ConnStatus, DataCraftApp};
use crate::db::views::{self, ViewDefinition, ViewSpec};
use crate::db::{Driver, ObjectKind};
use crate::i18n::trf;
use crate::ui::create_dialog;
use crate::ui::explorer::{self, TableRef};
use crate::ui::view_dialog::{ViewDialog, ViewResult};
use crate::worker::{ConnId, Request};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct ViewState {
    pub dialog: Option<ViewDialog>,
    /// "Generate DDL" of views waiting for their definition, by request id.
    ddl: HashMap<u64, TableRef>,
    next_request: u64,
}

impl ViewState {
    fn request_id(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }
}

impl DataCraftApp {
    fn send_view_definition(&mut self, t: &TableRef) -> u64 {
        let request_id = self.views.request_id();
        self.worker.send(Request::ViewDefinition {
            conn: t.conn,
            db: t.db.clone(),
            schema: t.schema.clone(),
            view: t.table.clone(),
            request_id,
        });
        request_id
    }

    pub(super) fn open_new_view(&mut self, conn: ConnId, db: String, schema: Option<String>) {
        if self.refuse_read_only(conn) {
            return;
        }
        let Some(c) = self
            .conn(conn)
            .filter(|c| c.status == ConnStatus::Connected)
        else {
            return;
        };
        let driver = c.cfg.driver;
        let schemas: Vec<String> = c
            .schemas(Some(&db))
            .unwrap_or_default()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let default = match driver {
            Driver::MySql | Driver::MariaDb => db.clone(),
            Driver::Oracle => c.cfg.user.to_uppercase(),
            _ => create_dialog::default_schema(driver).to_string(),
        };
        let schema = schema
            .or_else(|| schemas.contains(&default).then_some(default.clone()))
            .or_else(|| schemas.first().cloned())
            .unwrap_or(default);
        self.ensure_db_loaded(conn, Some(&db));
        self.views.dialog = Some(ViewDialog::new_view(conn, driver, db, schema));
    }

    pub(super) fn open_edit_view(&mut self, t: TableRef) {
        if self.refuse_read_only(t.conn) {
            return;
        }
        let Some(driver) = self
            .conn(t.conn)
            .filter(|c| c.status == ConnStatus::Connected)
            .map(|c| c.cfg.driver)
        else {
            return;
        };
        let request_id = self.send_view_definition(&t);
        self.views.dialog = Some(ViewDialog::edit_view(
            t.conn, driver, t.db, t.schema, t.table, request_id,
        ));
    }

    /// "Generate DDL" of a view: asks for its real definition. false if `t` isn't a known view.
    pub(super) fn request_view_ddl(&mut self, t: &TableRef) -> bool {
        let is_view = self
            .table_info(t.conn, Some(&t.db), &t.schema, &t.table)
            .is_some_and(|x| x.kind == ObjectKind::View);
        let connected = self
            .conn(t.conn)
            .is_some_and(|c| c.status == ConnStatus::Connected && !c.cfg.driver.is_nosql());
        if !is_view || !connected {
            return false;
        }
        let id = self.send_view_definition(t);
        self.views.ddl.insert(id, t.clone());
        true
    }

    pub(super) fn view_definition_received(
        &mut self,
        conn: ConnId,
        request_id: u64,
        result: Result<ViewDefinition, String>,
    ) {
        if let Some(d) = self
            .views
            .dialog
            .as_mut()
            .filter(|d| d.conn == conn && d.loading == Some(request_id))
        {
            d.definition_loaded(result);
            return;
        }
        let Some(t) = self.views.ddl.remove(&request_id) else {
            return;
        };
        let def = match result {
            Ok(def) => def,
            Err(e) => {
                self.error(e);
                return;
            }
        };
        let Some(driver) = self.conn(t.conn).map(|c| c.cfg.driver) else {
            return;
        };
        let spec = ViewSpec {
            schema: t.schema.clone(),
            name: t.table.clone(),
            columns: def.columns,
            body: def.body,
            algorithm: def.algorithm,
            security: def.security,
            ..Default::default()
        };
        let text = format!(
            "{};\n",
            views::create_view_sql(driver, &spec, &t.table, false)
        );
        let i = self.new_console(Some(t.conn), Some(format!("{}.sql", t.table)), text);
        self.consoles[i].db = Some(t.db);
    }

    pub(super) fn view_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.views.dialog.as_ref() else {
            return;
        };
        let schemas: Vec<String> = self
            .conn(d.conn)
            .and_then(|c| c.schemas(Some(&d.db)))
            .unwrap_or_default()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let Some(d) = self.views.dialog.as_mut() else {
            return;
        };
        match d.show(ctx, &schemas) {
            Some(ViewResult::Run(run)) => {
                d.running = true;
                d.error = None;
                let (conn, db) = (d.conn, Some(d.db.clone()));
                self.worker.send(Request::RunDdl {
                    conn,
                    db,
                    statements: run.statements,
                    close_db: None,
                    transaction: run.transaction,
                });
            }
            Some(ViewResult::Cancel) => self.views.dialog = None,
            None => {}
        }
    }

    /// Reply to the dialog's statements. false if they weren't the dialog's.
    pub(super) fn view_ddl_done(&mut self, conn: ConnId, result: &Result<(), String>) -> bool {
        let Some(d) = self
            .views
            .dialog
            .as_mut()
            .filter(|d| d.conn == conn && d.running)
        else {
            return false;
        };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e.clone());
            return true;
        }
        let Some(d) = self.views.dialog.take() else {
            return true;
        };
        let name = d.spec.name.trim().to_string();
        let db = d.db.clone();
        let schema = match d.driver {
            Driver::Sqlite => "main".to_string(),
            Driver::MySql | Driver::MariaDb => db.clone(),
            _ => d.spec.schema.trim().to_string(),
        };
        let tref = TableRef {
            conn,
            db: db.clone(),
            schema: schema.clone(),
            table: name.clone(),
        };
        self.explorer.expanded.insert(format!("c:{conn}"));
        self.explorer.expanded.insert(explorer::db_key(conn, &db));
        self.explorer
            .expanded
            .insert(format!("s:{conn}:{db}:{schema}"));
        self.explorer
            .expanded
            .remove(&format!("g:{conn}:{db}:{schema}:v:closed"));
        self.explorer.selected = Some(explorer::table_key(&tref));
        self.explorer.selected_table = Some(tref);
        // As after "New Table": a load already in progress may miss the change, so ask again.
        if let Some(c) = self.conn_mut(conn) {
            c.loading.remove(&db);
        }
        self.load_database(conn, db);
        if d.is_edit() {
            self.info(trf("View “{name}” saved", &[("name", &name)]));
        } else {
            self.info(trf("View “{name}” created", &[("name", &name)]));
        }
        true
    }

    /// Demo mode: `DATACRAFT_DEMO_VIEW=new` opens "New View" over the first table,
    /// `edit` opens "Edit View" on the first view.
    pub(super) fn demo_view(&mut self, conn: ConnId) {
        let mode = std::env::var("DATACRAFT_DEMO_VIEW").unwrap_or_default();
        let Some(c) = self.conn(conn) else { return };
        let Some(db) = c.default_db().map(str::to_string) else {
            return;
        };
        let schemas = c.schemas(None).unwrap_or_default();
        let find = |kind: ObjectKind| {
            schemas.iter().find_map(|s| {
                s.tables
                    .iter()
                    .find(|t| t.kind == kind)
                    .map(|t| (s.name.clone(), t.name.clone()))
            })
        };
        match mode.as_str() {
            "new" => {
                let Some((schema, table)) = find(ObjectKind::Table) else {
                    return;
                };
                self.open_new_view(conn, db, Some(schema));
                if let Some(d) = self.views.dialog.as_mut() {
                    d.spec.name = format!("{table}_view");
                    d.spec.body = format!("select *\nfrom {table}\nwhere 1 = 1");
                }
            }
            "edit" => {
                if let Some((schema, table)) = find(ObjectKind::View) {
                    self.open_edit_view(TableRef {
                        conn,
                        db,
                        schema,
                        table,
                    });
                }
            }
            _ => {}
        }
    }
}
