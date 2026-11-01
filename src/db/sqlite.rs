//! SQLite driver on top of `rusqlite`.
//!
//! `rusqlite::Connection` is not `Sync` and its API is blocking, so the
//! connection lives behind an `Arc<Mutex<_>>` and all work runs in
//! `spawn_blocking` so the tokio runtime isn't blocked.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, anyhow};
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, Connection, OpenFlags};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};

/// BLOB bytes shown as hexadecimal before truncating.
const BLOB_PREVIEW: usize = 64;

pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    /// Interrupts the running statement (`sqlite3_interrupt`) without taking the lock.
    interrupt: Arc<rusqlite::InterruptHandle>,
}

impl SqliteSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let path = cfg.file_path.trim().to_string();
        let read_only = cfg.read_only;
        if path.is_empty() {
            anyhow::bail!(tr("No SQLite database file was specified"));
        }
        let conn = tokio::task::spawn_blocking(move || {
            // No SQLITE_OPEN_CREATE: a mistyped path must not create an empty database.
            if !std::path::Path::new(&path).exists() {
                anyhow::bail!(trf("The file {path} does not exist", &[("path", &path)]));
            }
            let mode = if read_only { OpenFlags::SQLITE_OPEN_READ_ONLY } else { OpenFlags::SQLITE_OPEN_READ_WRITE };
            let flags = mode | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            Connection::open_with_flags(&path, flags).with_context(|| trf("Could not open {path}", &[("path", &path)]))
        })
        .await??;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(Self { conn: Arc::new(Mutex::new(conn)), interrupt })
    }

    pub fn interrupt_handle(&self) -> Arc<rusqlite::InterruptHandle> {
        Arc::clone(&self.interrupt)
    }

    /// Runs `f` with the connection on a blocking thread.
    async fn with_conn<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| anyhow!(tr("The SQLite connection became unusable")))?;
            f(&conn)
        })
        .await?
    }

    /// Stream full native values without exposing the driver connection.
    pub(super) async fn export_rows(
        &mut self,
        sql: String,
        sender: &tokio::sync::mpsc::Sender<super::table_export::Message>,
        progress: &Arc<super::table_export::ExportProgress>,
    ) -> anyhow::Result<()> {
        use super::table_export::{Message, Value, cancelled};
        let sender = sender.clone();
        let progress = Arc::clone(progress);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let n = stmt.column_count();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                cancelled(&progress)?;
                let values = (0..n)
                    .map(|i| {
                        use rusqlite::types::ValueRef;
                        Ok(match row.get_ref(i)? {
                            ValueRef::Null => Value::Null,
                            ValueRef::Integer(n) => Value::Number(n.to_string()),
                            ValueRef::Real(n) => Value::Number(format!("{n:?}")),
                            ValueRef::Text(t) => Value::Text(t.to_vec()),
                            ValueRef::Blob(b) => Value::Binary(b.to_vec()),
                        })
                    })
                    .collect::<anyhow::Result<_>>()?;
                sender.blocking_send(Message::Row(values)).context(tr("Export writer stopped"))?;
            }
            Ok(())
        })
        .await
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let sql = sql.to_string();
        self.with_conn(move |conn| run_batch(conn, &sql, max_rows)).await
    }

    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |conn| {
            // Without commit, the transaction's Drop issues a ROLLBACK.
            let tx = conn.unchecked_transaction()?;
            let mut total = 0;
            for stmt in &statements {
                let n = tx.execute(stmt, [])? as u64;
                if n != 1 {
                    return Err(super::not_single_row(n, stmt));
                }
                total += n;
            }
            tx.commit()?;
            Ok(total)
        })
        .await
    }

    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        self.with_conn(load_schema).await
    }
}

fn run_batch(conn: &Connection, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
    let mut results = Vec::new();
    let mut batch = Batch::new(conn, sql);
    loop {
        let start = Instant::now();
        let Some(mut stmt) = batch.next()? else { break };
        let mut rs = ResultSet {
            statement: stmt.expanded_sql().unwrap_or_else(|| sql.trim().to_string()).trim().to_string(),
            ..Default::default()
        };
        let ncols = stmt.column_count();
        if ncols > 0 {
            rs.columns = stmt.column_names().into_iter().map(str::to_string).collect();
            let mut rows = stmt.raw_query();
            while let Some(row) = rows.next()? {
                if rs.rows.len() >= max_rows {
                    rs.truncated = true;
                    if max_rows != 0 {
                        break;
                    }
                    continue;
                }
                let cells = (0..ncols).map(|i| row.get_ref(i).map(value_to_cell)).collect::<Result<_, _>>()?;
                rs.rows.push(cells);
            }
        } else {
            stmt.raw_execute()?;
            // changes() keeps the value of the last DML, so it would lie after a DDL.
            if is_dml(&rs.statement) {
                rs.affected = Some(conn.changes());
            }
        }
        rs.elapsed = start.elapsed();
        results.push(rs);
    }
    Ok(results)
}

fn is_dml(sql: &str) -> bool {
    let first = sql.split_whitespace().next().unwrap_or("");
    ["INSERT", "UPDATE", "DELETE", "REPLACE", "WITH"].iter().any(|k| first.eq_ignore_ascii_case(k))
}

fn value_to_cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Some(hex_preview(b)),
    }
}

/// `0x` + hex of the first bytes; appends "…" if the blob is longer.
pub(crate) fn hex_preview(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(BLOB_PREVIEW)];
    let mut s = String::with_capacity(2 + shown.len() * 2 + 3);
    s.push_str("0x");
    for b in shown {
        let _ = write!(s, "{b:02X}");
    }
    if bytes.len() > BLOB_PREVIEW {
        s.push('…');
    }
    s
}

fn load_schema(conn: &Connection) -> anyhow::Result<Vec<SchemaInfo>> {
    let mut stmt = conn.prepare(
        "SELECT m.name, m.type, p.name, p.type, p.\"notnull\", p.pk
         FROM sqlite_master m
         JOIN pragma_table_info(m.name) p
         WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
         ORDER BY m.name, p.cid",
    )?;
    let mut tables: Vec<TableInfo> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        let kind: String = row.get(1)?;
        if tables.last().is_none_or(|t| t.name != table) {
            tables.push(TableInfo {
                name: table,
                kind: if kind == "view" { ObjectKind::View } else { ObjectKind::Table },
                columns: Vec::new(),
            });
        }
        let notnull: i64 = row.get(4)?;
        let pk: i64 = row.get(5)?;
        if let Some(t) = tables.last_mut() {
            t.columns.push(ColumnInfo {
                name: row.get(2)?,
                data_type: row.get(3)?,
                nullable: notnull == 0 && pk == 0,
                primary_key: pk > 0,
            });
        }
    }
    Ok(vec![SchemaInfo { name: "main".into(), tables }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;

    /// Database in a temporary file that is removed afterwards.
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("datacraft_{name}_{}.db", std::process::id()));
            let _ = std::fs::remove_file(&path);
            // The driver no longer creates files: this plays the dialog's "New…" button.
            std::fs::File::create(&path).expect("create temporary file");
            Self(path)
        }

        async fn open(&self) -> SqliteSession {
            let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
            cfg.file_path = self.0.to_string_lossy().into_owned();
            SqliteSession::connect(&cfg).await.unwrap()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn sessions_are_send() {
        fn assert_send<T: Send + 'static>() {}
        assert_send::<SqliteSession>();
        assert_send::<crate::db::postgres::PgSession>();
        assert_send::<crate::db::mssql::MsSession>();
    }

    #[tokio::test]
    async fn empty_path_is_an_error() {
        let cfg = ConnectionConfig::new(1, Driver::Sqlite);
        assert!(SqliteSession::connect(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn apply_changes_is_atomic() {
        let db = TempDb::new("apply");
        let mut s = db.open().await;
        s.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'b'),(4,'c');", 10)
            .await
            .unwrap();
        let ok = vec!["UPDATE t SET v = 'x' WHERE id = '1'".to_string(), "DELETE FROM t WHERE id = '4'".to_string()];
        assert_eq!(s.apply_changes(&ok).await.unwrap(), 2);
        // The second one affects 0 rows: the first one is rolled back too.
        let bad = vec!["UPDATE t SET v = 'y' WHERE id = '1'".to_string(), "UPDATE t SET v = 'z' WHERE id = '99'".to_string()];
        assert!(s.apply_changes(&bad).await.is_err());
        // Non-unique key (no PK): affects 2 rows and is rejected.
        assert!(s.apply_changes(&["UPDATE t SET v = 'w' WHERE v = 'b'".to_string()]).await.is_err());
        let rs = s.execute("SELECT id, v FROM t ORDER BY id", 10).await.unwrap();
        let rows: Vec<Vec<Option<String>>> = rs[0].rows.clone();
        let b = |id: &str, v: &str| vec![Some(id.to_string()), Some(v.to_string())];
        assert_eq!(rows, vec![b("1", "x"), b("2", "b"), b("3", "b")]);
    }

    #[tokio::test]
    async fn missing_file_is_not_created() {
        let path = std::env::temp_dir().join(format!("datacraft_missing_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        assert!(SqliteSession::connect(&cfg).await.is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn new_table_dialog_sql_runs() {
        use crate::ui::create_dialog::{NewColumn, create_table_sql};
        let db = TempDb::new("new_table");
        let mut s = db.open().await;
        let col = |name: &str, ty: &str, pk: bool, default: &str| NewColumn {
            name: name.into(),
            data_type: ty.into(),
            not_null: false,
            primary_key: pk,
            default: default.into(),
        };
        let cols = [col("id", "INTEGER", true, ""), col("Full Name", "TEXT", false, "'n/a'")];
        s.execute(&create_table_sql(Driver::Sqlite, "main", "order", &cols), 0).await.unwrap();
        s.execute("INSERT INTO \"order\" (id) VALUES (1)", 0).await.unwrap();
        let rs = s.execute("SELECT \"Full Name\" FROM \"order\"", 10).await.unwrap();
        assert_eq!(rs[0].rows, vec![vec![Some("n/a".to_string())]]);
        let schemas = s.introspect().await.unwrap();
        let t = schemas[0].tables.iter().find(|t| t.name == "order").unwrap();
        assert!(t.columns[0].primary_key && !t.columns[0].nullable);
    }

    #[tokio::test]
    async fn ddl_does_not_inherit_affected_rows() {
        let db = TempDb::new("ddl");
        let mut s = db.open().await;
        let rs = s.execute("CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(3); CREATE TABLE u(y);", 10).await.unwrap();
        assert_eq!(rs[1].affected, Some(3));
        assert_eq!(rs[2].affected, None);
    }

    #[tokio::test]
    async fn create_insert_and_select() {
        let db = TempDb::new("crud");
        let mut s = db.open().await;
        let rs = s.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n TEXT, r REAL, b BLOB)", 10).await.unwrap();
        assert_eq!(rs.len(), 1);
        assert!(rs[0].is_command());

        let rs = s
            .execute("INSERT INTO t (n, r, b) VALUES ('one', 1.5, x'00FF'), (NULL, NULL, NULL)", 10)
            .await
            .unwrap();
        assert_eq!(rs[0].affected, Some(2));

        let big = "AB".repeat(100);
        s.execute(&format!("INSERT INTO t (n, b) VALUES ('big', x'{big}')"), 10).await.unwrap();

        let rs = s.execute("SELECT id, n, r, b FROM t ORDER BY id", 10).await.unwrap();
        assert_eq!(rs[0].columns, ["id", "n", "r", "b"]);
        assert!(!rs[0].truncated);
        let rows = &rs[0].rows;
        assert_eq!(rows[0], [Some("1".into()), Some("one".into()), Some("1.5".into()), Some("0x00FF".into())]);
        assert_eq!(rows[1], [Some("2".into()), None, None, None]);
        let blob = rows[2][3].as_deref().unwrap();
        assert!(blob.starts_with("0xABAB") && blob.ends_with('…'));
        assert_eq!(blob.chars().count(), 2 + 64 * 2 + 1);
    }

    #[tokio::test]
    async fn multiple_statements_and_max_rows() {
        let db = TempDb::new("multi");
        let mut s = db.open().await;
        let rs = s
            .execute(
                "CREATE TABLE n (v INTEGER);
                 INSERT INTO n VALUES (1),(2),(3),(4),(5);
                 SELECT v FROM n ORDER BY v;
                 SELECT v FROM n WHERE v > 100;
                 DELETE FROM n WHERE v < 3;",
                3,
            )
            .await
            .unwrap();
        assert_eq!(rs.len(), 5);
        assert_eq!(rs[1].affected, Some(5));
        assert_eq!(rs[2].rows.len(), 3);
        assert!(rs[2].truncated);
        assert_eq!(rs[2].statement, "SELECT v FROM n ORDER BY v;");
        assert_eq!(rs[3].columns, ["v"]);
        assert!(rs[3].rows.is_empty() && !rs[3].truncated);
        assert_eq!(rs[4].affected, Some(2));

        // An error halfway through the batch is propagated.
        assert!(s.execute("SELECT 1; SELECT * FROM missing", 10).await.is_err());
    }

    #[tokio::test]
    async fn zero_row_limit_discards_successful_results() {
        let db = TempDb::new("discard");
        let mut s = db.open().await;
        let rs = s
            .execute(
                "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v + 1 FROM n WHERE v < 10000) SELECT v FROM n",
                0,
            )
            .await
            .unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].columns, ["v"]);
        assert!(rs[0].rows.is_empty() && rs[0].truncated);
    }

    #[tokio::test]
    async fn introspection_with_pk_and_views() {
        let db = TempDb::new("schema");
        let mut s = db.open().await;
        s.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL, bio TEXT);
             CREATE TABLE pair (a INT, b INT, PRIMARY KEY (a, b));
             CREATE VIEW emails AS SELECT email FROM users;",
            10,
        )
        .await
        .unwrap();
        let schemas = s.introspect().await.unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "main");
        let names: Vec<_> = schemas[0].tables.iter().map(|t| t.name.as_str()).collect();
        // sqlite_sequence (created by AUTOINCREMENT) is left out.
        assert_eq!(names, ["emails", "pair", "users"]);

        let emails = &schemas[0].tables[0];
        assert_eq!(emails.kind, ObjectKind::View);
        let pair = &schemas[0].tables[1];
        assert!(pair.columns.iter().all(|c| c.primary_key));
        let users = &schemas[0].tables[2];
        assert_eq!(users.kind, ObjectKind::Table);
        let cols: Vec<_> = users.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable, c.primary_key)).collect();
        assert_eq!(cols, [("id", "INTEGER", false, true), ("email", "TEXT", false, false), ("bio", "TEXT", true, false)]);
    }
}
