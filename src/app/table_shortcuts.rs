//! Local launchers resolve a saved connection without copying its credentials.

use super::*;
use crate::table_shortcut::Target;

impl DataCraftApp {
    pub fn start_table_shortcut(&mut self, target: Target) {
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
}
