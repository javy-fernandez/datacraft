//! Procedures and functions: loading them for the explorer, "New / Edit Procedure…",
//! "Execute…", "Generate DDL" and dropping them. The app side of `ui::routine_dialog`,
//! `ui::routine_execute` and `db::routines`.

use super::{ConnStatus, DataCraftApp};
use crate::db::{Driver, SchemaInfo};
use crate::db::routines::{self, ArgValue, RoutineDefinition, RoutineInfo, RoutineKind, RoutineList, RoutineRef, RoutineSpec};
use crate::i18n::trf;
use crate::ui::create_dialog;
use crate::ui::drop_dialog::DropDialog;
use crate::ui::explorer;
use crate::ui::routine_dialog::{RoutineDialog, RoutineResult};
use crate::ui::routine_execute::{ExecuteDialog, ExecuteResult};
use crate::worker::{ConnId, Request};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct RoutineState {
    pub dialog: Option<RoutineDialog>,
    pub execute: Option<ExecuteDialog>,
    /// "Generate DDL" waiting for the definition, by request id.
    ddl: HashMap<u64, RoutineRef>,
    /// What was typed in "Execute…" for each routine (this session only).
    last_values: HashMap<RoutineRef, Vec<ArgValue>>,
    next_request: u64,
}

impl RoutineState {
    fn request_id(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    pub fn is_open(&self) -> bool {
        self.dialog.is_some() || self.execute.is_some()
    }
}

impl DataCraftApp {
    pub(super) fn save_routine_tab(&mut self, index: usize) {
        let Some(conn) = self.consoles.get(index).and_then(|c| c.routine.as_ref().filter(|r| c.conn == Some(r.dialog.conn)).map(|r| r.dialog.conn)) else { return };
        if self.refuse_read_only(conn) || self.routine_driver(conn).is_none() { return; }
        let Some(c) = self.consoles.get_mut(index) else { return };
        let Some(editor) = &mut c.routine else { return };
        if editor.dialog.running || editor.dialog.loading.is_some() || editor.saved.is_empty() { return; }
        let run = match editor.save_statements(&c.text) {
            Ok(run) => run,
            Err(error) => { editor.dialog.error = Some(error); return; }
        };
        if run.statements.is_empty() { return; }
        let identity = editor.identity_to_resolve(&c.text);
        editor.submitted = Some(c.text.clone());
        editor.comment.submitted = Some(editor.comment.text.clone());
        editor.dialog.running = true;
        editor.dialog.error = None;
        self.worker.send(Request::SaveRoutine { conn, db: editor.dialog.db.clone(), console: c.id, statements: run.statements, transaction: run.transaction, identity });
    }

    pub(super) fn routine_tab_saved(&mut self, console: u64, result: Result<(), String>) {
        let Some(c) = self.consoles.iter_mut().find(|c| c.id == console) else { return };
        let Some(editor) = &mut c.routine else { return };
        if !editor.dialog.running { return; }
        editor.dialog.running = false;
        match result {
            Ok(()) => {
                if let Some(submitted) = editor.comment.submitted.take() { editor.comment.saved = submitted; }
                if let Some(submitted) = editor.submitted.take() { editor.accept_saved_definition(submitted); }
                c.title.clone_from(&editor.reference.name);
                let (conn, db) = (editor.dialog.conn, editor.dialog.db.clone());
                self.load_routines(conn, &db);
            }
            Err(error) => { editor.submitted = None; editor.comment.submitted = None; editor.dialog.error = Some(error); }
        }
    }

    /// Loads (or reloads) the procedures and functions of `db` in the background.
    pub(super) fn load_routines(&mut self, conn: ConnId, db: &str) {
        let Some(c) = self.conn_mut(conn) else { return };
        if c.status != ConnStatus::Connected || !routines::supported(c.cfg.driver) {
            return;
        }
        let list = c.routines.entry(db.to_string()).or_default();
        if list.loading {
            return;
        }
        list.loading = true;
        self.worker.send(Request::LoadRoutines { conn, db: db.to_string() });
    }

    pub(super) fn routines_loaded(&mut self, conn: ConnId, db: String, result: Result<Vec<RoutineInfo>, String>) {
        // A reply that arrives after a disconnect belongs to the old session.
        let Some(c) = self.conn_mut(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        if let Ok(items) = &result {
            add_routine_schemas(c.cfg.driver, c.databases.iter_mut().find(|d| d.name == db).and_then(|d| d.schemas.as_mut()), items);
        }
        let list = c.routines.entry(db).or_insert_with(RoutineList::default);
        list.loading = false;
        match result {
            Ok(items) => {
                list.items = Some(items);
                list.error = None;
            }
            Err(e) => list.error = Some(e),
        }
    }

    fn send_routine_definition(&mut self, r: &RoutineRef) -> u64 {
        let request_id = self.routines.request_id();
        self.worker.send(Request::RoutineDefinition { routine: r.clone(), request_id });
        request_id
    }

    fn routine_driver(&self, conn: ConnId) -> Option<Driver> {
        self.conn(conn).filter(|c| c.status == ConnStatus::Connected && routines::supported(c.cfg.driver)).map(|c| c.cfg.driver)
    }

    pub(super) fn open_new_routine(&mut self, conn: ConnId, db: String, schema: Option<String>, kind: RoutineKind) {
        if self.refuse_read_only(conn) {
            return;
        }
        let Some(driver) = self.routine_driver(conn) else { return };
        let Some(c) = self.conn(conn) else { return };
        let schemas: Vec<String> = c.schemas(Some(&db)).unwrap_or_default().iter().map(|s| s.name.clone()).collect();
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
        self.routines.dialog = Some(RoutineDialog::new_routine(conn, driver, db, schema, kind));
    }

    pub(super) fn open_edit_routine(&mut self, r: RoutineRef) {
        if self.refuse_read_only(r.conn) {
            return;
        }
        let Some(driver) = self.routine_driver(r.conn) else { return };
        if let Some(i) = self.consoles.iter().position(|c| c.routine.as_ref().is_some_and(|e| e.reference == r)) {
            let retry = self.consoles[i].routine.as_ref().is_some_and(|e| e.saved.is_empty() && e.dialog.loading.is_none());
            if retry {
                let request_id = self.send_routine_definition(&r);
                if let Some(editor) = &mut self.consoles[i].routine { editor.dialog.loading = Some(request_id); editor.dialog.error = None; }
            }
            self.active = i;
            self.object_browser.active = false;
            return;
        }
        let request_id = self.send_routine_definition(&r);
        let placeholder = RoutineSpec { kind: r.kind, schema: r.schema.clone(), name: r.name.clone(), ..Default::default() };
        let dialog = RoutineDialog::edit_routine(r.conn, driver, r.db.clone(), placeholder, request_id);
        let i = self.new_console(Some(r.conn), Some(r.name.clone()), String::new());
        self.consoles[i].db = Some(r.db.clone());
        self.consoles[i].routine = Some(crate::ui::routine_dialog::RoutineEditor { comment: Default::default(), reference: r, dialog, saved: String::new(), submitted: None, transaction: false, page: Default::default(), word_wrap: false, outline: true, snippets: true });
    }

    pub(super) fn open_execute_routine(&mut self, r: RoutineRef) {
        let Some(driver) = self.routine_driver(r.conn) else { return };
        let request_id = self.send_routine_definition(&r);
        let values = self.routines.last_values.get(&r).cloned().unwrap_or_default();
        self.routines.execute = Some(ExecuteDialog::new(r, driver, request_id, values));
    }

    pub(super) fn request_routine_ddl(&mut self, r: RoutineRef) {
        if self.routine_driver(r.conn).is_none() {
            return;
        }
        let id = self.send_routine_definition(&r);
        self.routines.ddl.insert(id, r);
    }

    pub(super) fn open_drop_routine(&mut self, r: RoutineRef) {
        if self.refuse_read_only(r.conn) {
            return;
        }
        let Some(driver) = self.routine_driver(r.conn) else { return };
        self.drop = Some(DropDialog::routine(driver, &r));
    }

    pub(super) fn routine_definition_received(&mut self, conn: ConnId, request_id: u64, result: Result<RoutineDefinition, String>) {
        if let Some(c) = self.consoles.iter_mut().find(|c| c.routine.as_ref().is_some_and(|r| r.dialog.conn == conn && r.dialog.loading == Some(request_id))) {
            if let Some(editor) = &mut c.routine {
                if let Ok(definition) = &result {
                    editor.comment.text.clone_from(&definition.comment);
                    editor.comment.saved.clone_from(&definition.comment);
                }
                editor.dialog.definition_loaded(result);
                if editor.dialog.error.is_none() {
                    c.text = editor.dialog.definition_text();
                    editor.transaction = editor.dialog.statements().transaction;
                    editor.saved.clone_from(&c.text);
                }
            }
            return;
        }
        if let Some(d) = self.routines.dialog.as_mut().filter(|d| d.conn == conn && d.loading == Some(request_id)) {
            d.definition_loaded(result);
            return;
        }
        if let Some(d) = self.routines.execute.as_mut().filter(|d| d.routine.conn == conn && d.loading == Some(request_id)) {
            d.definition_loaded(result);
            return;
        }
        let Some(r) = self.routines.ddl.remove(&request_id) else { return };
        match result {
            Ok(def) => {
                let i = self.new_console(Some(r.conn), Some(format!("{}.sql", r.name)), def.ddl);
                self.consoles[i].db = Some(r.db);
            }
            Err(e) => self.error(e),
        }
    }

    pub(super) fn routine_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = self.routines.dialog.as_ref() {
            let schemas: Vec<String> =
                self.conn(d.conn).and_then(|c| c.schemas(Some(&d.db))).unwrap_or_default().iter().map(|s| s.name.clone()).collect();
            if let Some(d) = self.routines.dialog.as_mut() {
                match d.show(ctx, &schemas) {
                    Some(RoutineResult::Run(run)) => {
                        d.running = true;
                        d.error = None;
                        let (conn, db) = (d.conn, Some(d.db.clone()));
                        self.worker.send(Request::RunDdl { conn, db, statements: run.statements, close_db: None, transaction: run.transaction });
                    }
                    Some(RoutineResult::Cancel) => self.routines.dialog = None,
                    None => {}
                }
            }
        }
        let Some(d) = self.routines.execute.as_mut() else { return };
        match d.show(ctx) {
            Some(ExecuteResult::Console { script, run }) => {
                let Some(d) = self.routines.execute.take() else { return };
                self.routines.last_values.insert(d.routine.clone(), d.values);
                let r = d.routine;
                let i = self.new_console(Some(r.conn), Some(format!("{}.sql", r.name)), script);
                self.consoles[i].db = Some(r.db);
                if run {
                    self.run_console(i, true);
                }
            }
            Some(ExecuteResult::Cancel) => {
                if let Some(d) = self.routines.execute.take() {
                    self.routines.last_values.insert(d.routine, d.values);
                }
            }
            None => {}
        }
    }

    /// Reply to the routine dialog's statements. false if they weren't the dialog's.
    pub(super) fn routine_ddl_done(&mut self, conn: ConnId, result: &Result<(), String>) -> bool {
        let Some(d) = self.routines.dialog.as_mut().filter(|d| d.conn == conn && d.running) else { return false };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e.clone());
            // Oracle: the routine may exist now, invalid; show it in the explorer.
            let db = d.db.clone();
            self.load_routines(conn, &db);
            return true;
        }
        let Some(d) = self.routines.dialog.take() else { return true };
        let edit = d.is_edit();
        let kind = d.kind();
        let name = routines::final_name(d.driver, &d.spec);
        let schema = match d.driver {
            Driver::MySql | Driver::MariaDb => d.db.clone(),
            _ => d.spec.schema.trim().to_string(),
        };
        self.explorer.expanded.insert(format!("c:{conn}"));
        self.explorer.expanded.insert(explorer::db_key(conn, &d.db));
        self.explorer.expanded.insert(format!("s:{conn}:{}:{schema}", d.db));
        let gk = if kind == RoutineKind::Procedure { "p" } else { "f" };
        self.explorer.expanded.insert(format!("g:{conn}:{}:{schema}:{gk}", d.db));
        if let Some(c) = self.conn_mut(conn) {
            c.routines.entry(d.db.clone()).or_default().loading = false;
        }
        self.load_routines(conn, &d.db);
        let msg = match (kind, edit) {
            (RoutineKind::Procedure, false) => "Procedure “{name}” created",
            (RoutineKind::Procedure, true) => "Procedure “{name}” saved",
            (RoutineKind::Function, false) => "Function “{name}” created",
            (RoutineKind::Function, true) => "Function “{name}” saved",
        };
        self.info(trf(msg, &[("name", &name)]));
        true
    }

    /// After "Drop…" of a routine.
    pub(super) fn routine_dropped(&mut self, r: &RoutineRef) {
        if self.explorer.selected.as_deref() == Some(explorer::routine_key(r).as_str()) {
            self.explorer.selected = None;
        }
        if let Some(c) = self.conn_mut(r.conn)
            && let Some(list) = c.routines.get_mut(&r.db)
        {
            if let Some(items) = list.items.as_mut() {
                items.retain(|i| !(i.schema == r.schema && i.name == r.name && i.kind == r.kind && i.signature == r.signature));
            }
            list.loading = false;
        }
        self.load_routines(r.conn, &r.db);
        let msg = if r.kind == RoutineKind::Procedure { "Procedure “{name}” dropped" } else { "Function “{name}” dropped" };
        self.info(trf(msg, &[("name", &r.name)]));
    }
}

/// The schema introspection only lists schemas with tables or views: schemas that only hold
/// routines are added so their routines show in the explorer.
fn add_routine_schemas(driver: Driver, schemas: Option<&mut Vec<SchemaInfo>>, routines: &[RoutineInfo]) {
    let Some(schemas) = schemas else { return };
    if driver.flat_schema() {
        return;
    }
    for r in routines {
        if schemas.iter().any(|s| s.name == r.schema) {
            continue;
        }
        let at = schemas.iter().position(|s| s.name > r.schema).unwrap_or(schemas.len());
        // Oracle lists the connected user's schema first.
        let at = if driver == Driver::Oracle { at.max(1).min(schemas.len()) } else { at };
        schemas.insert(at, SchemaInfo { name: r.schema.clone(), tables: Vec::new() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine_editor_harness() -> egui_kittest::Harness<'static, DataCraftApp> {
        let mut app = crate::app::tests::tabs_app();
        let mut connection = crate::app::ConnState::new(crate::db::ConnectionConfig::new(7, Driver::MsSql));
        connection.status = ConnStatus::Connected;
        app.conns.push(connection);
        app.open_edit_routine(RoutineRef { conn: 7, db: "shop".into(), schema: "dbo".into(), name: "archive_orders".into(), kind: RoutineKind::Procedure, signature: String::new() });
        let request = app.consoles[app.active].routine.as_ref().unwrap().dialog.loading.unwrap();
        app.routine_definition_received(7, request, Ok(RoutineDefinition { comment: String::new(), ddl: "CREATE PROCEDURE dbo.archive_orders\n    @before datetime\nAS\nBEGIN\n    SET NOCOUNT ON;\n    SELECT id, created_at\n    FROM dbo.orders\n    WHERE created_at < @before;\nEND".into(), spec: None }));
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_ui_state(move |ui, app: &mut DataCraftApp| {
            if std::mem::take(&mut first) { crate::ui::theme::install_fonts(ui.ctx()); crate::ui::theme::apply(ui.ctx(), true); return; }
            app.central(ui);
        }, app);
        h.run_steps(4);
        h
    }

    #[test]
    fn parsed_routine_tabs_edit_one_definition_and_regenerate_safety_checks() {
        for driver in [Driver::MySql, Driver::Oracle] {
            let mut app = crate::app::tests::tabs_app();
            let mut connection = crate::app::ConnState::new(crate::db::ConnectionConfig::new(7, driver));
            connection.status = ConnStatus::Connected;
            app.conns.push(connection);
            let schema = if driver == Driver::Oracle { "SHOP" } else { "shop" };
            let name = if driver == Driver::Oracle { "TOTAL" } else { "total" };
            let reference = RoutineRef { conn: 7, db: "shop".into(), schema: schema.into(), name: name.into(), kind: RoutineKind::Function, signature: String::new() };
            app.open_edit_routine(reference);
            let active = app.active;
            let request = app.consoles[active].routine.as_ref().unwrap().dialog.loading.unwrap();
            let mut spec = RoutineSpec::new(driver, RoutineKind::Function, schema.into());
            spec.name = name.into();
            spec.body = if driver == Driver::Oracle { "begin return 1; end;" } else { "return 1" }.into();
            spec.mark_original("");
            app.routine_definition_received(7, request, Ok(RoutineDefinition { comment: String::new(), ddl: String::new(), spec: Some(spec) }));
            let text = &app.consoles[active].text;
            assert_eq!(text.matches("return 1").count(), 1, "the editable text must contain only the real definition");
            let changed = text.replace("return 1", "return 2");
            let editor = app.consoles[active].routine.as_ref().unwrap();
            let plan = editor.save_statements(&changed).unwrap();
            assert_eq!(plan.statements.iter().filter(|s| s.contains("return 2")).count(), 2, "both scratch and final definitions must use the edit");
            assert!(!plan.statements.iter().any(|s| s.contains("return 1")));
        }
    }

    #[test]
    fn cleared_inline_comments_are_not_restored_by_later_definition_saves() {
        let mut h = routine_editor_harness();
        let active = h.state().active;
        let editor = h.state_mut().consoles[active].routine.as_mut().unwrap();
        editor.dialog.driver = Driver::MariaDb;
        editor.saved = "CREATE PROCEDURE shop.p() COMMENT 'old comment' SELECT 1".into();
        editor.reference.name = "p".into();
        editor.reference.schema = "shop".into();
        let plan = editor.save_statements("CREATE PROCEDURE shop.p() COMMENT 'old comment' SELECT 2").unwrap();
        assert!(!plan.statements.iter().any(|s| s.contains("old comment")));
    }

    #[test]
    fn reopening_an_unloaded_routine_retries_its_definition() {
        let mut h = routine_editor_harness();
        let app = h.state_mut();
        let active = app.active;
        let count = app.consoles.len();
        let editor = app.consoles[active].routine.as_mut().unwrap();
        editor.saved.clear();
        editor.dialog.loading = None;
        editor.dialog.error = Some("failed load".into());
        let reference = editor.reference.clone();
        app.open_edit_routine(reference);
        assert_eq!(app.consoles.len(), count);
        assert!(app.consoles[active].routine.as_ref().unwrap().dialog.loading.is_some());
        assert!(app.consoles[active].routine.as_ref().unwrap().dialog.error.is_none());
    }

    #[test]
    fn routine_tabs_cannot_be_rebound_to_another_connection() {
        let mut h = routine_editor_harness();
        let app = h.state_mut();
        let active = app.active;
        let mut other = crate::app::ConnState::new(crate::db::ConnectionConfig::new(8, Driver::MsSql));
        other.status = ConnStatus::Connected;
        app.conns.push(other);
        app.set_active_connection(8);
        assert_eq!(app.consoles[active].conn, Some(7));
        assert_eq!(app.consoles[active].db.as_deref(), Some("shop"));
        // A mismatched restored state must never send a save to either server.
        app.consoles[active].conn = Some(8);
        app.consoles[active].routine.as_mut().unwrap().comment.text = "pending".into();
        app.save_routine_tab(active);
        assert!(!app.consoles[active].routine.as_ref().unwrap().dialog.running);
    }

    #[test]
    fn routine_comment_is_editable_and_marks_the_tab_dirty() {
        use egui_kittest::kittest::Queryable;
        let mut h = routine_editor_harness();
        h.get_all_by_label("Comment").min_by(|a, b| a.rect().top().total_cmp(&b.rect().top())).unwrap().click(); h.run_steps(2);
        assert!(h.query_by_label("Routine comment").is_some(), "the comment tab must contain an editable field");
        let active = h.state().active;
        let definition = h.state().consoles[active].text.clone();
        h.get_by_label("Routine comment").click();
        h.get_by_label("Routine comment").type_text("Archive orders — operator's note\nSecond line");
        h.run_steps(3);
        let console = &h.state().consoles[active];
        assert_eq!(console.text, definition);
        assert!(console.has_pending_edits());
        let editor = console.routine.as_ref().unwrap();
        assert_eq!(editor.comment.text, "Archive orders — operator's note\nSecond line");
        let plan = editor.save_statements(&console.text).unwrap();
        assert_eq!(plan.statements.len(), 1, "a comment-only edit must not recompile the routine");
        assert!(plan.transaction);
        assert!(plan.statements[0].contains("MS_Description"));
        let plan = editor.save_statements(&format!("{definition}\n-- changed definition")).unwrap();
        assert_eq!(plan.statements.len(), 2);
        assert!(plan.transaction, "the definition and metadata must commit together on SQL Server");
        assert!(editor.save_statements("").is_err());
        h.get_by_label("Definition").click(); h.run_steps(2);
        assert_eq!(h.state().consoles[active].text, definition);
    }

    #[test]
    fn routine_editor_views_preserve_unsaved_definition() {
        use egui_kittest::kittest::Queryable;
        let mut h = routine_editor_harness();
        let active = h.state().active;
        h.state_mut().consoles[active].text.push_str("\n-- keep this unsaved edit");
        let text = h.state().consoles[active].text.clone();
        h.get_by_label("Word Wrap").click(); h.run_steps(2);
        assert!(h.state().consoles[active].routine.as_ref().unwrap().word_wrap);
        for label in ["SQL Preview", "Comment", "Definition"] {
            h.get_by_label(label).click(); h.run_steps(2);
            assert_eq!(h.state().consoles[active].text, text);
            assert!(h.state().consoles[active].has_pending_edits());
        }
    }

    #[test]
    #[ignore = "Requires a GPU; renders the tabbed routine editor using fixture SQL"]
    fn render_tabbed_routine_editor() {
        use egui_kittest::kittest::Queryable;
        let mut h = routine_editor_harness();
        assert!(h.query_by_label("Save").is_some());
        assert!(h.query_by_label("Execute…").is_some());
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/routine-snapshots");
        std::fs::create_dir_all(&directory).unwrap();
        h.render().unwrap().save(directory.join("tabbed-editor.png")).unwrap();
        h.get_all_by_label("Comment").min_by(|a, b| a.rect().top().total_cmp(&b.rect().top())).unwrap().click(); h.run_steps(2);
        h.get_by_label("Routine comment").click();
        h.get_by_label("Routine comment").type_text("Archive orders older than the selected date.\nPreserves the audit history."); h.run_steps(3);
        h.render().unwrap().save(directory.join("tabbed-editor-comment.png")).unwrap();
        h.get_by_label("SQL Preview").click(); h.run_steps(3);
        h.render().unwrap().save(directory.join("tabbed-editor-preview.png")).unwrap();
    }

    #[test]
    fn routine_tabs_keep_edits_and_route_save_results() {
        let mut app = crate::app::tests::tabs_app();
        let mut connection = crate::app::ConnState::new(crate::db::ConnectionConfig::new(7, Driver::MsSql));
        connection.status = ConnStatus::Connected;
        app.conns.push(connection);
        let reference = |name: &str| RoutineRef { conn: 7, db: "shop".into(), schema: "dbo".into(), name: name.into(), kind: RoutineKind::Procedure, signature: String::new() };
        app.open_edit_routine(reference("first"));
        let first = app.active;
        let request = app.consoles[first].routine.as_ref().unwrap().dialog.loading.unwrap();
        app.routine_definition_received(7, request, Ok(RoutineDefinition { comment: String::new(), ddl: "CREATE PROCEDURE dbo.first AS SELECT 1".into(), spec: None }));
        app.consoles[first].text.push_str("\n-- unsaved");
        app.open_edit_routine(reference("second"));
        assert_ne!(app.active, first);
        app.open_edit_routine(reference("first"));
        assert_eq!(app.active, first);
        assert!(app.consoles[first].text.ends_with("-- unsaved"));
        let id = app.consoles[first].id;
        assert!(!app.guard(crate::app::Discard::Close(id)));
        let editor = app.consoles[first].routine.as_mut().unwrap();
        editor.dialog.running = true;
        editor.submitted = Some("submitted".into());
        editor.comment.text = "edited comment".into();
        editor.comment.submitted = Some("edited comment".into());
        app.routine_tab_saved(id, Err("save failed".into()));
        assert!(app.consoles[first].has_pending_edits());
        assert_eq!(app.consoles[first].routine.as_ref().unwrap().dialog.error.as_deref(), Some("save failed"));
        assert_eq!(app.consoles[first].routine.as_ref().unwrap().comment.saved, "");
        let editor = app.consoles[first].routine.as_mut().unwrap();
        editor.dialog.running = true;
        editor.submitted = Some("submitted".into());
        editor.comment.submitted = Some("edited comment".into());
        editor.comment.text = "newer comment".into();
        app.routine_tab_saved(id, Ok(()));
        assert_eq!(app.consoles[first].routine.as_ref().unwrap().saved, "submitted");
        assert_eq!(app.consoles[first].routine.as_ref().unwrap().comment.saved, "edited comment");
        assert!(app.consoles[first].has_pending_edits(), "a reply must not mark newer text as saved");
        let editor = app.consoles[first].routine.as_ref().unwrap();
        let persisted = crate::app::PersistedConsole {
            routine_comment: Some(editor.comment.clone()),
            routine: Some((editor.reference.clone(), editor.saved.clone(), editor.transaction)),
            title: "first".into(), text: app.consoles[first].text.clone(), conn: Some(7),
            table: None, table_view: Default::default(), db: Some("shop".into()),
        };
        app.consoles.clear();
        app.restore_tabs(vec![persisted], 0);
        assert!(app.consoles[0].text.ends_with("-- unsaved"));
        assert!(app.consoles[0].routine.is_some());
        assert_eq!(app.consoles[0].routine.as_ref().unwrap().comment.text, "newer comment");
        assert_eq!(app.consoles[0].routine.as_ref().unwrap().comment.saved, "edited comment");
        assert!(app.consoles[0].has_pending_edits());
    }

    #[test]
    fn schemas_with_only_routines_are_added() {
        let schema = |name: &str| SchemaInfo { name: name.into(), tables: Vec::new() };
        let routine = |schema: &str| RoutineInfo { schema: schema.into(), name: "f".into(), ..Default::default() };
        let mut schemas = vec![schema("app"), schema("public")];
        add_routine_schemas(Driver::Postgres, Some(&mut schemas), &[routine("api"), routine("public"), routine("zeta"), routine("api")]);
        assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["api", "app", "public", "zeta"]);
        let mut oracle = vec![schema("HR"), schema("APP")];
        add_routine_schemas(Driver::Oracle, Some(&mut oracle), &[routine("AAA")]);
        assert_eq!(oracle.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["HR", "AAA", "APP"]);
        let mut flat = vec![schema("shop")];
        add_routine_schemas(Driver::MySql, Some(&mut flat), &[routine("other")]);
        assert_eq!(flat.len(), 1);
    }
}
