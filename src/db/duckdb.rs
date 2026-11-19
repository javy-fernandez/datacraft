//! DuckDB driver: an embedded engine (a `.duckdb` file or an in-memory database) that also
//! queries Parquet, CSV, JSON and Excel files directly (`SELECT * FROM 'sales.parquet'`).
//!
//! DuckDB isn't linked into the app: its official library is downloaded on request (see
//! `install`) and loaded at runtime (`ffi`). Its API is blocking, so every call runs in
//! `spawn_blocking`, one at a time per session; a cancel interrupts from outside the lock.

mod ffi;
pub mod install;
mod values;

use std::ffi::CString;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::anyhow;

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};
use ffi::{Api, SUCCESS};

/// The loaded library and the process's database instance cache, which lets several sessions
/// (explorer, consoles, Manual transactions) open the same file: DuckDB locks a file per instance.
pub struct Lib {
    api: Api,
    cache: ffi::duckdb_instance_cache,
}

// SAFETY: DuckDB's instance cache and API are thread-safe; the cache lives as long as the process.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

static LIB: OnceLock<Mutex<Option<Arc<Lib>>>> = OnceLock::new();

/// The library, loaded once (after the driver is downloaded, without restarting the app).
fn lib() -> anyhow::Result<Arc<Lib>> {
    let mut slot = LIB.get_or_init(Default::default).lock().map_err(|_| anyhow!("DuckDB"))?;
    if let Some(lib) = &*slot {
        return Ok(Arc::clone(lib));
    }
    let Some(path) = install::installed_library() else {
        anyhow::bail!(tr("The DuckDB driver is not installed. Download it from the connection dialog."));
    };
    let api = Api::load(&path).map_err(|e| anyhow!(trf("Could not load the DuckDB driver {path}: {error}", &[("path", &path.display()), ("error", &e)])))?;
    // SAFETY: a fresh cache, kept for the life of the process.
    let cache = unsafe { (api.duckdb_create_instance_cache)() };
    let lib = Arc::new(Lib { api, cache });
    *slot = Some(Arc::clone(&lib));
    Ok(lib)
}

/// An open database and connection, closed when the last owner (session or cancel handle) drops.
pub struct Handle {
    lib: Arc<Lib>,
    db: ffi::duckdb_database,
    conn: ffi::duckdb_connection,
}

// SAFETY: a connection is used by one thread at a time (the session's lock), and
// `duckdb_interrupt` is documented as callable from another thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Interrupts the statement running on this connection.
    pub fn interrupt(&self) {
        // SAFETY: the connection is open while `self` lives.
        unsafe { (self.lib.api.duckdb_interrupt)(self.conn) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: both were opened by `connect` and are closed exactly once.
        unsafe {
            (self.lib.api.duckdb_disconnect)(&mut self.conn);
            (self.lib.api.duckdb_close)(&mut self.db);
        }
    }
}

pub struct DuckSession {
    handle: Arc<Handle>,
    lock: Arc<Mutex<()>>,
}

impl DuckSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let path = cfg.file_path.trim().to_string();
        let read_only = cfg.read_only;
        let id = cfg.id;
        let handle = tokio::task::spawn_blocking(move || open(&path, read_only, id)).await??;
        Ok(Self { handle: Arc::new(handle), lock: Arc::new(Mutex::new(())) })
    }

    pub fn interrupt_handle(&self) -> Arc<Handle> {
        Arc::clone(&self.handle)
    }

    async fn with_conn<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Handle) -> anyhow::Result<T> + Send + 'static,
    {
        let (handle, lock) = (Arc::clone(&self.handle), Arc::clone(&self.lock));
        tokio::task::spawn_blocking(move || {
            let _guard = lock.lock().map_err(|_| anyhow!(tr("The DuckDB connection became unusable")))?;
            f(&handle)
        })
        .await?
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let sql = sql.to_string();
        self.with_conn(move |h| run_batch(h, &sql, &[], max_rows)).await
    }

    pub async fn execute_bound(&mut self, sql: &str, parameters: &[String], max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let (sql, parameters) = (sql.to_string(), parameters.to_vec());
        self.with_conn(move |h| run_batch(h, &sql, &parameters, max_rows)).await
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        self.with_conn(|h| scalar(h, "SELECT current_database()")).await
    }

    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |h| {
            run_batch(h, "BEGIN TRANSACTION", &[], 0)?;
            let mut total = 0;
            for stmt in &statements {
                let n = match run_batch(h, stmt, &[], 0) {
                    Ok(rs) => rs.iter().filter_map(|r| r.affected).sum::<u64>(),
                    Err(e) => {
                        let _ = run_batch(h, "ROLLBACK", &[], 0);
                        return Err(e);
                    }
                };
                if n != 1 {
                    let _ = run_batch(h, "ROLLBACK", &[], 0);
                    return Err(super::not_single_row(n, stmt));
                }
                total += n;
            }
            run_batch(h, "COMMIT", &[], 0)?;
            Ok(total)
        })
        .await
    }

    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        self.with_conn(load_schema).await
    }
}

fn open(path: &str, read_only: bool, id: u64) -> anyhow::Result<Handle> {
    let lib = lib()?;
    let api = &lib.api;
    // No path: an in-memory database, shared by the connection's sessions through its name.
    let memory = path.is_empty() || path == ":memory:";
    if !memory {
        // No new database on a mistyped path; the dialog's New… leaves an empty file, which
        // DuckDB only initializes when it isn't there.
        match std::fs::metadata(path) {
            Err(_) => anyhow::bail!(trf("The file {path} does not exist", &[("path", &path)])),
            Ok(m) if m.is_file() && m.len() == 0 => std::fs::remove_file(path)?,
            Ok(_) => {}
        }
    }
    let target = if memory { format!(":memory:datacraft{id}") } else { path.to_string() };
    let c_path = CString::new(target)?;
    // SAFETY: standard DuckDB open sequence; the config is destroyed whatever happens, and the
    // database only kept when the connection opens too.
    unsafe {
        let mut config: ffi::duckdb_config = std::ptr::null_mut();
        if (api.duckdb_create_config)(&mut config) != SUCCESS {
            anyhow::bail!("DuckDB: could not create a configuration");
        }
        if read_only && !memory {
            (api.duckdb_set_config)(config, c"access_mode".as_ptr(), c"READ_ONLY".as_ptr());
        }
        let mut db: ffi::duckdb_database = std::ptr::null_mut();
        let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
        let state = (api.duckdb_get_or_create_from_cache)(lib.cache, c_path.as_ptr(), &mut db, config, &mut error);
        (api.duckdb_destroy_config)(&mut config);
        if state != SUCCESS {
            let message = api.owned_text(error);
            anyhow::bail!(trf("Could not open {path}: {error}", &[("path", &if memory { ":memory:" } else { path }), ("error", &message)]));
        }
        let mut conn: ffi::duckdb_connection = std::ptr::null_mut();
        if (api.duckdb_connect)(db, &mut conn) != SUCCESS {
            (api.duckdb_close)(&mut db);
            anyhow::bail!(tr("Could not connect to the DuckDB database"));
        }
        Ok(Handle { lib: Arc::clone(&lib), db, conn })
    }
}

/// Runs one or more statements; each SELECT keeps at most `max_rows` rows (0 discards them).
fn run_batch(h: &Handle, sql: &str, parameters: &[String], max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
    let mut results = Vec::new();
    let texts = statement_texts(sql);
    let mut index = 0;
    stream_statements(h, sql, parameters, |info| {
        let mut rs = ResultSet {
            statement: texts.get(index).cloned().unwrap_or_else(|| sql.trim().to_string()),
            columns: info.columns.clone(),
            ..Default::default()
        };
        index += 1;
        let start = Instant::now();
        let affected = info.consume(|_, rows| {
            for row in rows {
                if rs.rows.len() >= max_rows {
                    rs.truncated = true;
                    // A SELECT can stop here; anything else must run to the end.
                    return Ok(!(info.select && max_rows != 0));
                }
                rs.rows.push(row);
            }
            Ok(true)
        })?;
        // A DML's result is its count of changed rows, not a table; a command has none.
        if info.changes || info.nothing {
            rs.affected = affected;
            rs.columns.clear();
            rs.rows.clear();
            rs.truncated = false;
        }
        rs.elapsed = start.elapsed();
        results.push(rs);
        Ok(())
    })?;
    Ok(results)
}

/// The text of each statement of a script, for the results' titles.
fn statement_texts(sql: &str) -> Vec<String> {
    crate::sql::split_statements(sql, super::Driver::DuckDb).into_iter().map(|s| s.text.trim().to_string()).collect()
}

/// The first value of a query's first row.
fn scalar(h: &Handle, sql: &str) -> anyhow::Result<String> {
    let rs = run_batch(h, sql, &[], 1)?;
    Ok(rs.into_iter().next().and_then(|r| r.rows.into_iter().next()).and_then(|r| r.into_iter().next()).flatten().unwrap_or_default())
}

/// A statement being read.
struct Running<'a> {
    api: &'a Api,
    result: ffi::duckdb_result,
    columns: Vec<String>,
    types: Vec<values::Ty>,
    select: bool,
    changes: bool,
    /// DDL and other commands: no rows to show.
    nothing: bool,
}

impl Running<'_> {
    /// Reads its chunks while `f` returns true; returns the changed rows of a DML.
    fn consume(&self, mut f: impl FnMut(&[values::Ty], Vec<Vec<Cell>>) -> anyhow::Result<bool>) -> anyhow::Result<Option<u64>> {
        let api = self.api;
        let mut changed: Option<u64> = None;
        loop {
            // SAFETY: the result is live; each chunk is read and destroyed before the next.
            let mut chunk = unsafe { (api.duckdb_fetch_chunk)(self.result) };
            if chunk.is_null() {
                break;
            }
            // SAFETY: the chunk's vectors match the result's columns and types.
            let rows = unsafe {
                let n = (api.duckdb_data_chunk_get_size)(chunk);
                let columns: Vec<Vec<Cell>> = self
                    .types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| values::column(api, (api.duckdb_data_chunk_get_vector)(chunk, i as u64), ty, n))
                    .collect();
                (api.duckdb_destroy_data_chunk)(&mut chunk);
                transpose(columns, n as usize)
            };
            if self.changes {
                // The single "Count" column.
                let count = rows.iter().filter_map(|r| r.first().cloned().flatten()).filter_map(|v| v.parse::<u64>().ok()).sum::<u64>();
                changed = Some(changed.unwrap_or(0) + count);
                continue;
            }
            if !f(&self.types, rows)? {
                return Ok(changed);
            }
        }
        let mut result = self.result;
        // SAFETY: an error that stopped the stream is kept on the result.
        let error = unsafe { ffi::text((api.duckdb_result_error)(&mut result)) };
        if !error.is_empty() {
            anyhow::bail!(error);
        }
        Ok(changed)
    }
}

fn transpose(columns: Vec<Vec<Cell>>, n: usize) -> Vec<Vec<Cell>> {
    let mut rows: Vec<Vec<Cell>> = (0..n).map(|_| Vec::with_capacity(columns.len())).collect();
    for column in columns {
        for (row, cell) in rows.iter_mut().zip(column) {
            row.push(cell);
        }
    }
    rows
}

/// Prepares and starts each statement of `sql` in turn, handing it to `f` to read.
fn stream_statements(h: &Handle, sql: &str, parameters: &[String], mut f: impl FnMut(&Running) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let api = &h.lib.api;
    let c_sql = CString::new(sql).map_err(|_| anyhow!(tr("The statement contains a NUL character")))?;
    // SAFETY: the DuckDB call sequence extract → prepare → pending → execute → fetch, each
    // object destroyed in reverse order on every path.
    unsafe {
        let mut extracted: ffi::duckdb_extracted_statements = std::ptr::null_mut();
        let n = (api.duckdb_extract_statements)(h.conn, c_sql.as_ptr(), &mut extracted);
        let out = (|| {
            if n == 0 {
                let error = ffi::text((api.duckdb_extract_statements_error)(extracted));
                if !error.is_empty() {
                    anyhow::bail!(error);
                }
                return Ok(());
            }
            for i in 0..n {
                let mut prepared: ffi::duckdb_prepared_statement = std::ptr::null_mut();
                let state = (api.duckdb_prepare_extracted_statement)(h.conn, extracted, i, &mut prepared);
                let run = (|| {
                    if state != SUCCESS {
                        anyhow::bail!(ffi::text((api.duckdb_prepare_error)(prepared)));
                    }
                    if !parameters.is_empty() {
                        for (j, value) in parameters.iter().enumerate().take((api.duckdb_nparams)(prepared) as usize) {
                            let v = CString::new(value.as_str())?;
                            if (api.duckdb_bind_varchar)(prepared, j as u64 + 1, v.as_ptr()) != SUCCESS {
                                anyhow::bail!(tr("Could not bind a parameter"));
                            }
                        }
                    }
                    let kind = (api.duckdb_prepared_statement_type)(prepared);
                    let mut pending: ffi::duckdb_pending_result = std::ptr::null_mut();
                    let started = (api.duckdb_pending_prepared_streaming)(prepared, &mut pending);
                    let run = (|| {
                        if started != SUCCESS {
                            anyhow::bail!(ffi::text((api.duckdb_pending_error)(pending)));
                        }
                        let mut result = ffi::duckdb_result::default();
                        if (api.duckdb_execute_pending)(pending, &mut result) != SUCCESS {
                            let error = ffi::text((api.duckdb_result_error)(&mut result));
                            (api.duckdb_destroy_result)(&mut result);
                            anyhow::bail!(error);
                        }
                        let ncols = (api.duckdb_column_count)(&mut result);
                        let columns = (0..ncols).map(|c| ffi::text((api.duckdb_column_name)(&mut result, c))).collect();
                        let types = (0..ncols)
                            .map(|c| {
                                let mut ty = (api.duckdb_column_logical_type)(&mut result, c);
                                let resolved = values::Ty::resolve(api, ty);
                                (api.duckdb_destroy_logical_type)(&mut ty);
                                resolved
                            })
                            .collect();
                        let returns = (api.duckdb_result_return_type)(result);
                        let changes = returns == ffi::RESULT_CHANGED_ROWS
                            || matches!(kind, ffi::STATEMENT_INSERT | ffi::STATEMENT_UPDATE | ffi::STATEMENT_DELETE) && ncols == 1;
                        let nothing = returns == ffi::RESULT_NOTHING;
                        let running = Running { api, result, columns, types, select: kind == ffi::STATEMENT_SELECT, changes, nothing };
                        let out = f(&running);
                        (api.duckdb_destroy_result)(&mut result);
                        out
                    })();
                    (api.duckdb_destroy_pending)(&mut pending);
                    run
                })();
                (api.duckdb_destroy_prepare)(&mut prepared);
                run?;
            }
            Ok(())
        })();
        (api.duckdb_destroy_extracted)(&mut extracted);
        out
    }
}

/// Schemas (with their tables, views and columns) of the current database.
fn load_schema(h: &Handle) -> anyhow::Result<Vec<SchemaInfo>> {
    let mut schemas: Vec<SchemaInfo> = Vec::new();
    let names = run_batch(
        h,
        "SELECT schema_name FROM duckdb_schemas() WHERE database_name = current_database() AND NOT internal ORDER BY schema_name",
        &[],
        usize::MAX,
    )?;
    for row in names.into_iter().flat_map(|r| r.rows) {
        if let Some(Some(name)) = row.into_iter().next() {
            schemas.push(SchemaInfo { name, tables: Vec::new() });
        }
    }
    let columns = run_batch(
        h,
        "SELECT c.schema_name, c.table_name, v.view_name IS NOT NULL, c.column_name, c.data_type, c.is_nullable,
                coalesce(list_contains(p.cols, c.column_name), false)
         FROM duckdb_columns() c
         LEFT JOIN duckdb_views() v
           ON v.database_name = c.database_name AND v.schema_name = c.schema_name AND v.view_name = c.table_name
         LEFT JOIN (SELECT database_name, schema_name, table_name, constraint_column_names AS cols
                    FROM duckdb_constraints() WHERE constraint_type = 'PRIMARY KEY') p
           ON p.database_name = c.database_name AND p.schema_name = c.schema_name AND p.table_name = c.table_name
         WHERE c.database_name = current_database() AND NOT c.internal
         ORDER BY c.schema_name, c.table_name, c.column_index",
        &[],
        usize::MAX,
    )?;
    let text = |c: &Cell| c.clone().unwrap_or_default();
    for row in columns.into_iter().flat_map(|r| r.rows) {
        let [schema, table, view, column, data_type, nullable, pk] = <[Cell; 7]>::try_from(row).map_err(|_| anyhow!("duckdb_columns()"))?;
        let schema_name = text(&schema);
        let idx = match schemas.iter().position(|s| s.name == schema_name) {
            Some(i) => i,
            None => {
                schemas.push(SchemaInfo { name: schema_name, tables: Vec::new() });
                schemas.len() - 1
            }
        };
        let tables = &mut schemas[idx].tables;
        let table_name = text(&table);
        if tables.last().is_none_or(|t| t.name != table_name) {
            let kind = if text(&view) == "true" { ObjectKind::View } else { ObjectKind::Table };
            tables.push(TableInfo { name: table_name, kind, columns: Vec::new() });
        }
        if let Some(t) = tables.last_mut() {
            t.columns.push(ColumnInfo {
                name: text(&column),
                data_type: text(&data_type),
                nullable: text(&nullable) == "true",
                primary_key: text(&pk) == "true",
            });
        }
    }
    Ok(schemas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;

    /// Session on a fresh in-memory database, or None (test skipped) without the library:
    /// set `DATACRAFT_DUCKDB_LIB` to a `libduckdb` of version `install::VERSION`.
    async fn session() -> Option<DuckSession> {
        if !install::is_installed() {
            eprintln!("DuckDB library not available: skipped");
            return None;
        }
        let cfg = ConnectionConfig::new(rand_id(), Driver::DuckDb);
        Some(DuckSession::connect(&cfg).await.unwrap())
    }

    fn rand_id() -> u64 {
        uuid::Uuid::new_v4().as_u64_pair().0
    }

    #[tokio::test]
    async fn queries_types_dml_and_limits() {
        let Some(mut s) = session().await else { return };
        let rs = s
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR, price DECIMAL(10,2), ts TIMESTAMP, tags VARCHAR[], meta STRUCT(a INT, b VARCHAR), u UUID, d DATE, flag BOOLEAN, big HUGEINT, m MAP(VARCHAR, INT), iv INTERVAL, e ENUM('x','y'));
                 INSERT INTO t VALUES
                   (1, 'one', 12.5, TIMESTAMP '2024-01-02 03:04:05.5', ['a', 'b'], {'a': 1, 'b': 'z'}, '550e8400-e29b-41d4-a716-446655440000', DATE '2024-02-29', true, 170141183460469231731687303715884105727, MAP {'k': 1}, INTERVAL 1 DAY, 'y'),
                   (2, NULL, -0.05, NULL, [], NULL, NULL, NULL, false, -1, NULL, NULL, NULL);
                 SELECT * FROM t ORDER BY id;
                 UPDATE t SET name = 'two' WHERE id = 2;",
                10,
            )
            .await
            .unwrap();
        assert_eq!(rs.len(), 4);
        assert!(rs[0].is_command());
        assert_eq!(rs[1].affected, Some(2));
        assert!(rs[1].is_command());
        assert_eq!(rs[2].columns.len(), 13);
        let row = &rs[2].rows[0];
        let c = |v: &str| Some(v.to_string());
        assert_eq!(row[..4], [c("1"), c("one"), c("12.50"), c("2024-01-02 03:04:05.5")]);
        assert_eq!(row[4], c("['a', 'b']"));
        assert_eq!(row[5], c("{'a': 1, 'b': 'z'}"));
        assert_eq!(row[6], c("550e8400-e29b-41d4-a716-446655440000"));
        assert_eq!(row[7..10], [c("2024-02-29"), c("true"), c("170141183460469231731687303715884105727")]);
        assert_eq!(row[10], c("{'k'=1}"));
        assert_eq!(row[11..], [c("1 day"), c("y")]);
        let row2 = &rs[2].rows[1];
        assert_eq!(row2[1], None);
        assert_eq!(row2[2], c("-0.05"));
        assert_eq!(row2[4], c("[]"));
        assert_eq!(rs[3].affected, Some(1));

        // A large SELECT stops at the limit and says so.
        let rs = s.execute("SELECT * FROM range(100000)", 3).await.unwrap();
        assert_eq!(rs[0].rows.len(), 3);
        assert!(rs[0].truncated);
        let rs = s.execute("SELECT * FROM range(5)", 5).await.unwrap();
        assert!(!rs[0].truncated);

        // Errors (at prepare and while running) are reported.
        assert!(s.execute("SELECT * FROM missing_table", 10).await.is_err());
        assert!(s.execute("SELECT 'x'::INTEGER", 10).await.is_err());
    }

    #[tokio::test]
    async fn bound_parameters_edits_and_catalog() {
        let Some(mut s) = session().await else { return };
        s.execute("CREATE SCHEMA sales; CREATE TABLE sales.orders (id INTEGER PRIMARY KEY, note VARCHAR NOT NULL); CREATE VIEW v AS SELECT 1 AS one; INSERT INTO sales.orders VALUES (1, 'a'), (2, 'b')", 0)
            .await
            .unwrap();
        let rs = s.execute_bound("SELECT note FROM sales.orders WHERE id = ?", &["2".into()], 10).await.unwrap();
        assert_eq!(rs[0].rows, vec![vec![Some("b".to_string())]]);
        assert_eq!(s.apply_changes(&["UPDATE sales.orders SET note = 'x' WHERE id = 1".into()]).await.unwrap(), 1);
        // A change that matches no row rolls everything back.
        let bad = ["UPDATE sales.orders SET note = 'y' WHERE id = 1".to_string(), "UPDATE sales.orders SET note = 'z' WHERE id = 99".to_string()];
        assert!(s.apply_changes(&bad).await.is_err());
        let rs = s.execute("SELECT note FROM sales.orders ORDER BY id", 10).await.unwrap();
        assert_eq!(rs[0].rows[0], vec![Some("x".to_string())]);

        let schemas = s.introspect().await.unwrap();
        let sales = schemas.iter().find(|s| s.name == "sales").unwrap();
        let orders = &sales.tables[0];
        assert_eq!(orders.name, "orders");
        assert_eq!(orders.kind, ObjectKind::Table);
        assert!(orders.columns[0].primary_key && !orders.columns[0].nullable);
        assert_eq!(orders.columns[1].data_type, "VARCHAR");
        let main = schemas.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main.tables[0].kind, ObjectKind::View);
        assert!(!s.current_database().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sessions_of_a_connection_share_its_database_and_cancel() {
        let Some(_) = session().await else { return };
        let mut cfg = ConnectionConfig::new(rand_id(), Driver::DuckDb);
        let mut a = DuckSession::connect(&cfg).await.unwrap();
        let mut b = DuckSession::connect(&cfg).await.unwrap();
        a.execute("CREATE TABLE shared AS SELECT 42 AS v", 0).await.unwrap();
        assert_eq!(b.execute("SELECT v FROM shared", 1).await.unwrap()[0].rows[0][0].as_deref(), Some("42"));

        // A file database: created by the dialog's New… (an empty file), opened twice.
        let path = std::env::temp_dir().join(format!("datacraft-{}.duckdb", rand_id()));
        std::fs::File::create(&path).unwrap();
        cfg.file_path = path.display().to_string();
        let mut f1 = DuckSession::connect(&cfg).await.unwrap();
        let mut f2 = DuckSession::connect(&cfg).await.unwrap();
        f1.execute("CREATE TABLE x AS SELECT 1 AS v", 0).await.unwrap();
        assert_eq!(f2.execute("SELECT count(*) FROM x", 1).await.unwrap()[0].rows[0][0].as_deref(), Some("1"));

        // A cancel interrupts a long query.
        let handle = a.interrupt_handle();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            handle.interrupt();
        });
        let started = Instant::now();
        let r = a.execute("SELECT count(*) FROM range(10000000000) r1, range(10) r2", 1).await;
        canceller.await.unwrap();
        assert!(r.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        drop((f1, f2));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("duckdb.wal"));
    }

    /// The app's catalog features on DuckDB: view definitions, object information, row counts,
    /// foreign keys and the schema search.
    #[tokio::test]
    async fn catalog_features_run_on_duckdb() {
        let Some(_) = session().await else { return };
        let cfg = ConnectionConfig::new(rand_id(), Driver::DuckDb);
        let mut s = crate::db::Session::connect(&cfg).await.unwrap();
        s.execute(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, name VARCHAR); CREATE TABLE child (id INTEGER, parent_id INTEGER REFERENCES parent(id));
             INSERT INTO parent VALUES (1, 'Ada'), (2, 'Bob'); INSERT INTO child VALUES (10, 1);
             CREATE VIEW names AS SELECT name FROM parent WHERE id > 0;",
            0,
        )
        .await
        .unwrap();
        let view = s.view_definition(Driver::DuckDb, "", "main", "names").await.unwrap();
        // DuckDB keeps the view normalized: `SELECT "name" FROM parent WHERE (id > 0)`.
        assert!(view.body.starts_with("SELECT") && view.body.contains("FROM parent") && !view.body.ends_with(';'), "{}", view.body);
        let info = s.object_info(Driver::DuckDb, "main", "parent", true).await.unwrap();
        assert_eq!(info.rows, Some(2));
        assert_eq!(s.object_list_info(Driver::DuckDb, Some("main")).await.unwrap().len(), 3);
        assert_eq!(s.table_row_count(Driver::DuckDb, "main", "parent", "id > 1").await.unwrap(), 1);
        let rs = s.execute(&crate::db::subset::fk_sql(Driver::DuckDb, "", "main"), 100).await.unwrap();
        let catalog = s.introspect().await.unwrap();
        let fks = crate::db::subset::parse_fks(Driver::DuckDb, &rs[0], &catalog);
        assert_eq!(fks.len(), 1);
        assert_eq!((fks[0].table.as_str(), fks[0].columns.clone(), fks[0].ref_table.as_str(), fks[0].ref_columns.clone()), ("child", vec!["parent_id".to_string()], "parent", vec!["id".to_string()]));
    }

    #[tokio::test]
    async fn reads_parquet_files() {
        let Some(mut s) = session().await else { return };
        let path = std::env::temp_dir().join(format!("datacraft-{}.parquet", rand_id()));
        let p = path.display().to_string().replace('\'', "''");
        s.execute(&format!("COPY (SELECT range AS id, 'n' || range AS name FROM range(3)) TO '{p}' (FORMAT parquet)"), 0).await.unwrap();
        let rs = s.execute(&format!("SELECT * FROM '{p}' ORDER BY id"), 10).await.unwrap();
        assert_eq!(rs[0].columns, ["id", "name"]);
        assert_eq!(rs[0].rows[2], vec![Some("2".to_string()), Some("n2".to_string())]);
        let _ = std::fs::remove_file(path);
    }
}
