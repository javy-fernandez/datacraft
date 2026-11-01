//! Local launchers resolve a saved connection without copying its credentials.

use super::*;
use crate::table_shortcut::Target;

impl DataCraftApp {
    pub fn start_table_shortcut(&mut self, target: Target) {
        self.startup_table = None;
        let conn = target.connection;
        let Some(connection) = self.conn(conn) else {
            self.error(tr("The shortcut's saved connection was not found"));
            return;
        };
        let connected = connection.status == ConnStatus::Connected;
        self.startup_table = Some(target);
        if connected {
            self.open_startup_table(conn);
        } else {
            self.connect(conn);
        }
    }

    pub(super) fn open_startup_table(&mut self, conn: ConnId) {
        let Some(target) = self.startup_table.take_if(|t| t.connection == conn) else {
            return;
        };
        self.open_table(&TableRef { conn, db: target.database, schema: target.schema, table: target.table });
    }

    pub(super) fn cancel_startup_table(&mut self, conn: ConnId) {
        if self.startup_table.as_ref().is_some_and(|target| target.connection == conn) {
            self.startup_table = None;
        }
    }

    pub(super) fn create_table_shortcut(&mut self, table: TableRef) {
        if self.conn(table.conn).is_none() {
            return;
        }
        if std::env::var_os("DATACRAFT_DEMO").is_some() {
            self.error(tr("Save this connection before creating a shortcut"));
            return;
        }
        let extension = crate::table_shortcut::extension();
        if let Some(path) =
            rfd::FileDialog::new().add_filter(tr("Table shortcut"), &[extension]).set_file_name(format!("{}.{}", table.table, extension)).save_file()
        {
            self.worker.send(Request::TableShortcut { path, target: Target::from(&table) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_opens_the_requested_saved_connection_and_qualified_table() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, true);
        app.start_table_shortcut(Target::from(&table));
        assert!(app.startup_table.is_none());
        assert_eq!(app.consoles[app.active].conn, Some(table.conn));
        assert_eq!(app.consoles[app.active].db.as_deref(), Some(table.db.as_str()));
        assert_eq!(app.consoles[app.active].table.as_ref().unwrap().table, table.table);
        let before = app.consoles.len();
        app.start_table_shortcut(Target { connection: 999, database: "main".into(), schema: "main".into(), table: "notes".into() });
        assert!(app.startup_table.is_none());
        assert_eq!(app.consoles.len(), before);
        app.startup_table = Some(Target::from(&table));
        app.open_startup_table(999);
        assert!(app.startup_table.is_some());
    }

    #[test]
    fn startup_connection_failure_cannot_open_on_later_manual_success() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, false);
        app.conns[0].cfg.file_path = std::env::temp_dir().join(format!("dc-missing-startup-{}.db", uuid::Uuid::new_v4())).to_string_lossy().into_owned();
        app.start_table_shortcut(Target::from(&table));
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while Instant::now() < deadline && app.conn(7).is_some_and(|connection| connection.status == ConnStatus::Connecting) {
            app.handle_responses();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(matches!(app.conn(7).unwrap().status, ConnStatus::Error(_)));
        assert!(app.startup_table.is_none());
        let before = app.consoles.len();
        app.conns[0].status = ConnStatus::Connected;
        app.open_startup_table(7);
        assert_eq!(app.consoles.len(), before);
    }

    #[test]
    fn startup_password_cancel_finishes_the_launch_intent() {
        use egui_kittest::kittest::Queryable;
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, false);
        app.conns[0].cfg.ask_password = true;
        app.start_table_shortcut(Target::from(&table));
        assert!(app.ask_password.is_some());
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(900.0, 700.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                app.dialogs(ui.ctx());
            },
            app,
        );
        harness.run_steps(3);
        harness.get_by_label("Cancel").click();
        harness.run_steps(2);
        assert!(harness.state().ask_password.is_none());
        assert!(harness.state().startup_table.is_none());
        let app = harness.state_mut();
        let before = app.consoles.len();
        app.conns[0].status = ConnStatus::Connected;
        app.open_startup_table(7);
        assert_eq!(app.consoles.len(), before);
    }

    #[test]
    fn startup_cancellation_is_scoped_and_terminal_requests_clear_old_intent() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, true);
        app.startup_table = Some(Target::from(&table));
        app.disconnect(999);
        assert!(app.startup_table.is_some());
        let before = app.consoles.len();
        app.open_startup_table(7);
        assert_eq!(app.consoles.len(), before + 1);
        app.startup_table = Some(Target::from(&table));
        app.disconnect(7);
        assert!(app.startup_table.is_none());
        app.startup_table = Some(Target::from(&table));
        app.start_table_shortcut(Target { connection: 999, database: "main".into(), schema: "main".into(), table: "notes".into() });
        assert!(app.startup_table.is_none());
    }
}
