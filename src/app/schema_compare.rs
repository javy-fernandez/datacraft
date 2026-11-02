//! Opening "Compare Schemas…", receiving its models and acting on the generated script.

use super::*;
use crate::db::schema_compare::SchemaModel;
use crate::ui::schema_compare::{CompareAction, CompareDialog};

impl DataCraftApp {
    pub(super) fn open_compare(&mut self, conn: ConnId, db: Option<String>, schema: Option<String>) {
        let Some(c) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected) else {
            self.error(tr("The connection is not open"));
            return;
        };
        if !crate::db::schema_compare::supported(c.cfg.driver) {
            self.error(tr("Not available for this engine"));
            return;
        }
        if let Some(mut previous) = self.compare.take() {
            previous.cancel();
        }
        self.compare = Some(CompareDialog::new(&self.conns, conn, db, schema));
    }

    pub(super) fn compare_received(&mut self, request_id: u64, result: Result<Box<(SchemaModel, SchemaModel)>, String>) {
        if let Some(d) = self.compare.as_mut() {
            d.finished(request_id, result);
        }
    }

    pub(super) fn show_compare(&mut self, ctx: &egui::Context) {
        if let Some(action) = self.compare.as_mut().and_then(|d| d.show(ctx, &self.conns)) {
            self.compare_action(action);
        }
    }

    fn compare_action(&mut self, action: CompareAction) {
        match action {
            CompareAction::Start { source, target, sides, progress } => {
                let request_id = self.compare.as_ref().map_or(0, |d| d.request_id);
                self.worker.send(Request::SchemaCompare { source, target, request_id, sides, progress });
            }
            CompareAction::Load(conn, db) => self.ensure_db_loaded(conn, Some(&db)),
            CompareAction::Save { schema, text } => {
                if let Some(path) = rfd::FileDialog::new().set_title(tr("Save Script")).add_filter("SQL", &["sql"]).set_file_name(format!("migrate_{schema}.sql")).save_file() {
                    match std::fs::write(&path, text) {
                        Ok(()) => self.info(trf("Script saved to {path}", &[("path", &path.display())])),
                        Err(e) => self.error(trf("Could not save the script: {error}", &[("error", &e)])),
                    }
                }
            }
            // A new console on the compared target with the script, never run from here.
            CompareAction::OpenConsole { target, text } => {
                self.compare = None;
                let read_only = self.conn(target.conn).is_some_and(|c| c.cfg.read_only);
                let i = self.new_console(Some(target.conn), Some(format!("migrate_{}.sql", target.schema)), text);
                self.consoles[i].db = Some(target.db);
                if read_only {
                    self.error(tr("The target connection is read-only: the script can be reviewed, not run."));
                } else {
                    self.info(tr("Review the migration script before running it."));
                }
            }
            CompareAction::Close => {
                if let Some(mut d) = self.compare.take() {
                    d.cancel();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema_compare::demo_models;
    use std::sync::atomic::Ordering;

    #[test]
    fn stale_replies_are_ignored_and_the_script_opens_on_the_compared_target() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Postgres, true);
        app.conns[0].cfg.read_only = true;
        app.open_compare(table.conn, Some(table.db.clone()), Some(table.schema.clone()));
        let (src, tgt) = demo_models();
        let d = app.compare.as_mut().unwrap();
        assert_eq!((d.source.conn, d.source.db.as_str(), d.source.schema.as_str()), (table.conn, "main", "main"));
        assert_eq!(d.target.conn, table.conn, "the only connection is also the target");
        // A reply when nothing was asked changes nothing.
        d.finished(d.request_id, Ok(Box::new((src.clone(), tgt.clone()))));
        assert!(d.outcome.is_none());
        let CompareAction::Start { progress: first, .. } = d.start() else { unreachable!() };
        let old = d.request_id;
        d.cancel();
        assert!(first.cancel.load(Ordering::Relaxed));
        let CompareAction::Start { progress, .. } = d.start() else { unreachable!() };
        d.finished(old, Ok(Box::new((src.clone(), tgt.clone()))));
        assert!(d.outcome.is_none() && d.running(), "an older reply is ignored");
        let current = d.request_id;
        // A reply for the first dialog's request never reaches a dialog opened after it.
        app.open_compare(table.conn, None, None);
        let d = app.compare.as_mut().unwrap();
        let CompareAction::Start { .. } = d.start() else { unreachable!() };
        assert!(d.request_id > current, "request ids are unique in the app");
        app.compare_received(current, Ok(Box::new((src.clone(), tgt.clone()))));
        let d = app.compare.as_mut().unwrap();
        assert!(d.outcome.is_none() && d.running(), "a closed dialog's reply is ignored");
        d.finished(d.request_id, Ok(Box::new((src, tgt))));
        assert!(!d.running());
        assert!(progress.cancel.load(Ordering::Relaxed), "the replaced dialog's comparison was cancelled");
        let text = d.outcome.as_ref().unwrap().text.clone();
        assert!(text.contains("create table shop_staging.payments"), "{text}");
        // Changing a picker makes the result stale: nothing leaves the dialog until compared again.
        let compared = d.target.clone();
        d.target.schema = "elsewhere".into();
        assert!(d.stale());
        d.target.schema.clone_from(&compared.schema);
        assert!(!d.stale());
        let consoles = app.consoles.len();
        app.compare_action(CompareAction::OpenConsole { target: compared, text: text.clone() });
        assert_eq!(app.consoles.len(), consoles + 1);
        let console = app.consoles.last().unwrap();
        assert_eq!((console.conn, console.db.as_deref(), console.text.as_str()), (Some(table.conn), Some("main"), text.as_str()));
        assert!(!console.running);
        assert!(app.compare.is_none());
        assert!(app.status_msg.as_ref().is_some_and(|(m, error)| *error && m.contains("read-only")));
    }

    #[test]
    fn compare_dialog_blocks_the_hidden_console_shortcut() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, true);
        let active = app.active;
        app.consoles[active].conn = Some(table.conn);
        app.consoles[active].db = Some(table.db.clone());
        app.consoles[active].text = "SELECT 1".into();
        app.object_browser.active = false;
        app.open_compare(table.conn, None, None);
        let mut h = egui_kittest::Harness::builder().build_ui_state(|ui, app: &mut DataCraftApp| app.shortcuts(ui.ctx()), app);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
        h.run_steps(2);
        assert!(!h.state().consoles[active].running, "the compare dialog must block the hidden console");
    }
}
