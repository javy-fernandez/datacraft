//! Generic ODBC driver, mainly for Progress OpenEdge through Progress' own ODBC driver.
//!
//! The ODBC driver manager isn't linked into the app: it is loaded at runtime (`ffi`) from
//! unixODBC / iODBC (macOS, Linux) or `odbc32.dll` (Windows), so the app still starts where
//! there is none. Text goes through the wide (`…W`) entry points: the narrow ones use the ANSI
//! code page on Windows, and the width of `SQLWCHAR` follows the loaded manager (16 bits on
//! Windows and unixODBC, 32 on iODBC). ODBC calls block, so every call runs in
//! `spawn_blocking`, one at a time per session.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::anyhow;

use super::{
    AuthMode, Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo,
};
use crate::db::sqlite::hex_preview;
use ffi::{Api, Handle, Wide};

/// UTF-16 / UTF-32 units read per `SQLGetData` call on long text.
const TEXT_CHUNK: usize = 4096;
/// Bytes read of a binary value: enough for `hex_preview`, which shows the first 64.
const BINARY_CHUNK: usize = 256;
/// Primary keys are looked up one table at a time (`SQLPrimaryKeys`): skipped above this.
const PK_LOOKUP_LIMIT: usize = 300;
/// Schemas of the system catalogs some drivers report as plain tables or views.
const SYSTEM_SCHEMAS: &[&str] = &["SYSPROGRESS", "INFORMATION_SCHEMA", "sys", "pg_catalog"];

/// An open ODBC session.
pub struct OdbcSession {
    conn: Arc<Mutex<Conn>>,
    closed: Arc<AtomicBool>,
}

impl OdbcSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let conn_str = full_connection_string(cfg);
        let fallback = display_name(&cfg.database);
        let read_only = cfg.read_only;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        let conn =
            tokio::task::spawn_blocking(move || Conn::open(&conn_str, read_only, fallback, flag))
                .await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            closed,
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Conn) -> anyhow::Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn
                .lock()
                .map_err(|_| anyhow!("The ODBC connection became unusable"))?;
            f(&mut conn)
        })
        .await?
    }

    /// Runs ONE statement; at most `max_rows` rows per result (0 = discard rows).
    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let sql = sql.to_string();
        self.with_conn(move |c| c.run(&sql, &[], max_rows)).await
    }

    /// A SELECT with `?` parameters bound as text.
    pub async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[String],
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let (sql, parameters) = (sql.to_string(), parameters.to_vec());
        self.with_conn(move |c| c.run(&sql, &parameters, max_rows))
            .await
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        self.with_conn(|c| {
            let name = c.info_text(ffi::SQL_DATABASE_NAME).unwrap_or_default();
            Ok(if name.trim().is_empty() {
                c.fallback_name.clone()
            } else {
                name.trim().to_string()
            })
        })
        .await
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.current_database().await?])
    }

    /// Data editor changes in ONE transaction; each statement must affect exactly one row.
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |c| c.transaction(&statements, true))
            .await
    }

    /// Schemas, tables/views and columns, without system objects.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        self.with_conn(|c| c.load_schema()).await
    }

    /// Product and version for Test Connection.
    pub async fn server_version(&mut self) -> Option<String> {
        self.with_conn(|c| {
            let name = c.info_text(ffi::SQL_DBMS_NAME)?;
            let version = c.info_text(ffi::SQL_DBMS_VER).unwrap_or_default();
            Ok(format!("{} {}", name.trim(), version.trim())
                .trim()
                .to_string())
        })
        .await
        .ok()
        .filter(|v| !v.is_empty())
    }

    /// Runs the statements in ONE transaction (all or nothing); the affected rows.
    pub async fn run_transaction(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |c| c.transaction(&statements, false))
            .await
    }

    /// Manual transactions (console): from here on nothing commits by itself until
    /// `end_transaction`.
    pub async fn begin(&mut self) -> anyhow::Result<()> {
        self.with_conn(|c| {
            c.set_autocommit(false)?;
            c.manual = true;
            Ok(())
        })
        .await
    }

    /// Commits or rolls back the Manual transaction and goes back to autocommit.
    pub async fn end_transaction(&mut self, commit: bool) -> anyhow::Result<()> {
        self.with_conn(move |c| {
            // A failed commit keeps the transaction open (turning autocommit back on would
            // commit it implicitly): the user can still roll it back.
            c.end_tran(commit)?;
            c.manual = false;
            c.set_autocommit(true)
        })
        .await
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// The ODBC connection string: the DSN (`DSN=name`) or, when the database field already holds
/// a full string (`DRIVER={…};HOST=…`), that string, without a password typed into it; user and
/// password are added by `connect`.
pub fn connection_string(cfg: &ConnectionConfig) -> String {
    let source = cfg.database.trim();
    if !source.contains('=') {
        return source.to_string();
    }
    join_attributes(
        attributes(source)
            .into_iter()
            .filter(|(k, _)| !is_key(k, &["PWD", "PASSWORD"])),
    )
}

/// The data source field carries a password (`PWD=…`), which would be saved in the clear: it
/// belongs in the password field, which goes to the keychain.
pub fn has_password(cfg: &ConnectionConfig) -> bool {
    let source = cfg.database.trim();
    source.contains('=')
        && attributes(source)
            .iter()
            .any(|(k, _)| is_key(k, &["PWD", "PASSWORD"]))
}

/// `odbc:<DSN or connection string>`, as `connection_string` writes it. False when the URL isn't
/// an ODBC one.
pub fn apply_url(cfg: &mut ConnectionConfig, url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("odbc:") else {
        return false;
    };
    cfg.database = rest.trim().to_string();
    true
}

/// Connection string of Progress' OpenEdge ODBC driver: the driver's name as installed (it
/// changes with the version), the SQL broker's host and port, the database and its SQL schema.
pub const OPENEDGE_TEMPLATE: &str = "DRIVER={Progress OpenEdge 12.8 Driver};HOST=localhost;PORT=20931;DB=sports2020;DefaultSchema=PUB";

/// The string handed to `SQLDriverConnect`: the data source (a bare word is a DSN) plus the
/// dialog's user and password, which replace any `UID` / `PWD` typed into the data source.
fn full_connection_string(cfg: &ConnectionConfig) -> String {
    let source = cfg.database.trim();
    let mut attrs = if source.is_empty() {
        Vec::new()
    } else if source.contains('=') {
        attributes(source)
    } else {
        vec![("DSN".to_string(), escape_value(source))]
    };
    if cfg.auth != AuthMode::NoAuth {
        let (user, password) = (cfg.user.trim(), cfg.password.as_str());
        if !user.is_empty() {
            attrs.retain(|(k, _)| !is_key(k, &["UID"]));
            attrs.push(("UID".into(), escape_value(user)));
        }
        if !password.is_empty() {
            attrs.retain(|(k, _)| !is_key(k, &["PWD", "PASSWORD"]));
            attrs.push(("PWD".into(), escape_value(password)));
        }
    }
    join_attributes(attrs.into_iter())
}

/// Name for the connection when the driver doesn't report its database: the DSN or the
/// database attribute of the connection string.
fn display_name(source: &str) -> String {
    let source = source.trim();
    if !source.contains('=') {
        return if source.is_empty() { "ODBC" } else { source }.to_string();
    }
    let attrs = attributes(source);
    ["DSN", "DATABASE", "DB", "DBQ"]
        .iter()
        .find_map(|key| {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| unescape_value(v))
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "ODBC".to_string())
}

fn is_key(key: &str, names: &[&str]) -> bool {
    names.iter().any(|n| key.trim().eq_ignore_ascii_case(n))
}

/// `key=value` pairs of a connection string, values as written (braces included).
fn attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '=' || c == ';' {
                if c == ';' {
                    key.clear();
                    continue;
                }
                break;
            }
            key.push(c);
        }
        let key = key.trim().to_string();
        let mut value = String::new();
        // Leading spaces before a braced value.
        while chars.peek().is_some_and(|c| *c == ' ') {
            chars.next();
        }
        if chars.peek() == Some(&'{') {
            value.push(chars.next().unwrap_or('{'));
            while let Some(c) = chars.next() {
                value.push(c);
                if c == '}' {
                    if chars.peek() == Some(&'}') {
                        value.push('}');
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            // Anything up to the next `;` belongs to the value too.
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
                value.push(c);
            }
        } else {
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
                value.push(c);
            }
        }
        if !key.is_empty() {
            out.push((key, value.trim_end().to_string()));
        }
        if chars.peek().is_none() {
            break;
        }
    }
    out
}

fn join_attributes(attrs: impl Iterator<Item = (String, String)>) -> String {
    attrs
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// A value as a connection string attribute: in braces (`}` doubled) when it holds `;`, `{`,
/// `}` or surrounding spaces.
fn escape_value(v: &str) -> String {
    if v.contains([';', '{', '}']) || v.trim() != v {
        format!("{{{}}}", v.replace('}', "}}"))
    } else {
        v.to_string()
    }
}

fn unescape_value(v: &str) -> String {
    let t = v.trim();
    match t.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        Some(inner) => inner.replace("}}", "}"),
        None => t.to_string(),
    }
}

/// ODBC SQL types fetched as bytes (shown as hex).
fn is_binary(sql_type: i16) -> bool {
    matches!(
        sql_type,
        ffi::SQL_BINARY | ffi::SQL_VARBINARY | ffi::SQL_LONGVARBINARY
    )
}

/// Column type for the explorer: `TYPE_NAME` plus length or precision where it matters.
fn type_label(name: &str, sql_type: i16, size: Option<i64>, digits: Option<i64>) -> String {
    match sql_type {
        ffi::SQL_CHAR
        | ffi::SQL_VARCHAR
        | ffi::SQL_WCHAR
        | ffi::SQL_WVARCHAR
        | ffi::SQL_BINARY
        | ffi::SQL_VARBINARY
            if !name.contains('(') =>
        {
            match size {
                Some(n) if n > 0 => format!("{name}({n})"),
                _ => name.to_string(),
            }
        }
        ffi::SQL_DECIMAL | ffi::SQL_NUMERIC if !name.contains('(') => match (size, digits) {
            (Some(p), Some(s)) if p > 0 => format!("{name}({p},{s})"),
            (Some(p), None) if p > 0 => format!("{name}({p})"),
            _ => name.to_string(),
        },
        _ => name.to_string(),
    }
}

/// The library, loaded once (retried on each connect until it loads).
fn api() -> anyhow::Result<Arc<Api>> {
    static API: OnceLock<Mutex<Option<Arc<Api>>>> = OnceLock::new();
    let mut slot = API
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow!("ODBC"))?;
    if let Some(api) = &*slot {
        return Ok(Arc::clone(api));
    }
    let api = Arc::new(Api::load()?);
    *slot = Some(Arc::clone(&api));
    Ok(api)
}

/// An open connection: environment and connection handles, freed in `Drop`.
struct Conn {
    api: Arc<Api>,
    env: Handle,
    dbc: Handle,
    connected: bool,
    /// A Manual transaction is open (autocommit off) until `end_transaction`.
    manual: bool,
    /// Name for `current_database` when the driver reports none.
    fallback_name: String,
    closed: Arc<AtomicBool>,
}

// SAFETY: ODBC handles may be used from any thread as long as calls on them don't overlap;
// the session's mutex serializes them.
unsafe impl Send for Conn {}

impl Conn {
    fn open(
        conn_str: &str,
        read_only: bool,
        fallback_name: String,
        closed: Arc<AtomicBool>,
    ) -> anyhow::Result<Conn> {
        let api = api()?;
        let env = api.alloc(ffi::SQL_HANDLE_ENV, std::ptr::null_mut())?;
        let mut conn = Conn {
            api: Arc::clone(&api),
            env,
            dbc: std::ptr::null_mut(),
            connected: false,
            manual: false,
            fallback_name,
            closed,
        };
        // SAFETY: an integer attribute passed in the pointer, as ODBC defines it.
        let rc = unsafe {
            (api.SQLSetEnvAttr)(
                env,
                ffi::SQL_ATTR_ODBC_VERSION,
                ffi::SQL_OV_ODBC3 as *mut c_void,
                0,
            )
        };
        conn.check(rc, ffi::SQL_HANDLE_ENV, env)?;
        conn.dbc = api.alloc(ffi::SQL_HANDLE_DBC, env)?;
        // Not every driver supports it: ignored on failure.
        let _ = conn.set_int_attr(ffi::SQL_ATTR_LOGIN_TIMEOUT, 15);
        let text = Wide::new(api.width, conn_str);
        let mut out = Wide::zeroed(api.width, 1024);
        let mut out_len: i16 = 0;
        // SAFETY: both buffers live through the call; lengths are in characters.
        let rc = unsafe {
            (api.SQLDriverConnectW)(
                conn.dbc,
                std::ptr::null_mut(),
                text.ptr(),
                ffi::SQL_NTS as i16,
                out.ptr_mut(),
                out.units() as i16,
                &mut out_len,
                ffi::SQL_DRIVER_NOPROMPT,
            )
        };
        drop(out); // It may hold the password.
        conn.check(rc, ffi::SQL_HANDLE_DBC, conn.dbc)?;
        conn.connected = true;
        if read_only {
            let _ = conn.set_int_attr(ffi::SQL_ATTR_ACCESS_MODE, ffi::SQL_MODE_READ_ONLY);
        }
        Ok(conn)
    }

    /// `Ok` for SQL_SUCCESS / SQL_SUCCESS_WITH_INFO; otherwise the diagnostics as an error.
    fn check(&self, rc: i16, handle_type: i16, handle: Handle) -> anyhow::Result<i16> {
        match rc {
            ffi::SQL_SUCCESS | ffi::SQL_SUCCESS_WITH_INFO => Ok(rc),
            _ => Err(self.error(rc, handle_type, handle)),
        }
    }

    fn error(&self, rc: i16, handle_type: i16, handle: Handle) -> anyhow::Error {
        let records = self.api.diagnostics(handle_type, handle);
        if records
            .iter()
            .any(|(state, _)| state == "08S01" || state == "08003")
        {
            self.closed.store(true, Ordering::Relaxed);
        }
        if records.is_empty() {
            return anyhow!("ODBC call failed (return code {rc})");
        }
        anyhow!(
            records
                .iter()
                .map(|(state, msg)| format!("{} (SQLSTATE {state})", msg.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    fn set_int_attr(&self, attr: i32, value: usize) -> anyhow::Result<()> {
        // SAFETY: an integer attribute passed in the pointer, as ODBC defines it.
        let rc = unsafe {
            (self.api.SQLSetConnectAttrW)(
                self.dbc,
                attr,
                value as *mut c_void,
                ffi::SQL_IS_UINTEGER,
            )
        };
        self.check(rc, ffi::SQL_HANDLE_DBC, self.dbc).map(|_| ())
    }

    fn set_autocommit(&self, on: bool) -> anyhow::Result<()> {
        let value = if on {
            ffi::SQL_AUTOCOMMIT_ON
        } else {
            ffi::SQL_AUTOCOMMIT_OFF
        };
        self.set_int_attr(ffi::SQL_ATTR_AUTOCOMMIT, value)
    }

    fn end_tran(&self, commit: bool) -> anyhow::Result<()> {
        let completion = if commit {
            ffi::SQL_COMMIT
        } else {
            ffi::SQL_ROLLBACK
        };
        // SAFETY: the connection handle is open.
        let rc = unsafe { (self.api.SQLEndTran)(ffi::SQL_HANDLE_DBC, self.dbc, completion) };
        self.check(rc, ffi::SQL_HANDLE_DBC, self.dbc).map(|_| ())
    }

    /// A text `SQLGetInfo` value.
    fn info_text(&self, info: u16) -> anyhow::Result<String> {
        let mut buf = Wide::zeroed(self.api.width, 512);
        let mut len: i16 = 0;
        // SAFETY: the buffer lives through the call; its length is in bytes.
        let rc = unsafe {
            (self.api.SQLGetInfoW)(self.dbc, info, buf.ptr_mut(), buf.bytes() as i16, &mut len)
        };
        self.check(rc, ffi::SQL_HANDLE_DBC, self.dbc)?;
        let units = (len.max(0) as usize / self.api.width.bytes()).min(buf.units() - 1);
        Ok(buf.decode(units))
    }

    fn statement(&self) -> anyhow::Result<Stmt<'_>> {
        let h = self
            .api
            .alloc(ffi::SQL_HANDLE_STMT, self.dbc)
            .map_err(|_| self.error(ffi::SQL_ERROR, ffi::SQL_HANDLE_DBC, self.dbc))?;
        Ok(Stmt { conn: self, h })
    }

    /// Executes ONE statement and reads its results.
    fn run(
        &self,
        sql: &str,
        parameters: &[String],
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let start = Instant::now();
        let stmt = self.statement()?;
        let api = &self.api;
        let text = Wide::new(api.width, sql);
        let rc = if parameters.is_empty() {
            // SAFETY: NUL-terminated text that lives through the call.
            unsafe { (api.SQLExecDirectW)(stmt.h, text.ptr(), ffi::SQL_NTS) }
        } else {
            // SAFETY: as above.
            let rc = unsafe { (api.SQLPrepareW)(stmt.h, text.ptr(), ffi::SQL_NTS) };
            stmt.check(rc)?;
            // The bound buffers must outlive SQLExecute and the reading of its results.
            let values: Vec<Wide> = parameters.iter().map(|p| Wide::new(api.width, p)).collect();
            let mut lengths: Vec<isize> = values
                .iter()
                .map(|v| ((v.units() - 1) * api.width.bytes()) as isize)
                .collect();
            for (i, (value, len)) in values.iter().zip(lengths.iter_mut()).enumerate() {
                // SAFETY: value and length stay alive and unmoved until after SQLExecute.
                let rc = unsafe {
                    (api.SQLBindParameter)(
                        stmt.h,
                        (i + 1) as u16,
                        ffi::SQL_PARAM_INPUT,
                        ffi::SQL_C_WCHAR,
                        ffi::SQL_VARCHAR,
                        (value.units() - 1).max(1),
                        0,
                        value.ptr() as *mut c_void,
                        value.bytes() as isize,
                        len,
                    )
                };
                stmt.check(rc)?;
            }
            // SAFETY: the statement is prepared and its parameters bound.
            let rc = unsafe { (api.SQLExecute)(stmt.h) };
            let rc = stmt.check_no_data(rc)?;
            // `values` and `lengths` stay alive while the results are read.
            return self.collect(&stmt, sql, rc, max_rows, start);
        };
        let rc = stmt.check_no_data(rc)?;
        self.collect(&stmt, sql, rc, max_rows, start)
    }

    /// The result sets of an executed statement (`rc` is SQLExecDirect's / SQLExecute's).
    fn collect(
        &self,
        stmt: &Stmt,
        sql: &str,
        rc: i16,
        max_rows: usize,
        mut start: Instant,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let api = &self.api;
        let statement = sql.trim().to_string();
        let mut results = Vec::new();
        if rc == ffi::SQL_NO_DATA {
            // A searched UPDATE / DELETE that touched no rows.
            results.push(ResultSet {
                statement: statement.clone(),
                affected: Some(0),
                elapsed: start.elapsed(),
                ..Default::default()
            });
            // SAFETY: the statement handle is valid.
            let rc = unsafe { (api.SQLMoreResults)(stmt.h) };
            if rc != ffi::SQL_SUCCESS && rc != ffi::SQL_SUCCESS_WITH_INFO {
                return Ok(results);
            }
            start = Instant::now();
        }
        loop {
            let mut rs = ResultSet {
                statement: statement.clone(),
                ..Default::default()
            };
            let mut ncols: i16 = 0;
            // SAFETY: the statement handle is valid.
            stmt.check(unsafe { (api.SQLNumResultCols)(stmt.h, &mut ncols) })?;
            if ncols > 0 {
                let columns = (1..=ncols as u16)
                    .map(|i| stmt.describe(i))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let types: Vec<i16> = columns.iter().map(|(_, t)| *t).collect();
                rs.columns = columns.into_iter().map(|(n, _)| n).collect();
                loop {
                    if rs.rows.len() >= max_rows {
                        rs.truncated = stmt.fetch()?;
                        break;
                    }
                    if !stmt.fetch()? {
                        break;
                    }
                    rs.rows.push(stmt.row(&types)?);
                }
            } else {
                let mut count: isize = -1;
                // SAFETY: the statement handle is valid.
                let rc = unsafe { (api.SQLRowCount)(stmt.h, &mut count) };
                if rc == ffi::SQL_SUCCESS || rc == ffi::SQL_SUCCESS_WITH_INFO {
                    rs.affected = u64::try_from(count).ok();
                }
            }
            rs.elapsed = start.elapsed();
            results.push(rs);
            start = Instant::now();
            // SAFETY: the statement handle is valid.
            let rc = unsafe { (api.SQLMoreResults)(stmt.h) };
            match rc {
                ffi::SQL_NO_DATA => break,
                _ => {
                    stmt.check(rc)?;
                }
            }
        }
        Ok(results)
    }

    /// The statements in one transaction; `single_row`: each must affect exactly one row. Inside
    /// a Manual transaction they just run in it, which the user commits or rolls back.
    fn transaction(&mut self, statements: &[String], single_row: bool) -> anyhow::Result<u64> {
        let run_all = |c: &Conn| -> anyhow::Result<u64> {
            let mut total = 0;
            for stmt in statements {
                let n = c
                    .run(stmt, &[], 0)?
                    .iter()
                    .filter_map(|r| r.affected)
                    .sum::<u64>();
                if single_row && n != 1 {
                    return Err(super::not_single_row(n, stmt));
                }
                total += n;
            }
            Ok(total)
        };
        if self.manual {
            return run_all(self);
        }
        self.set_autocommit(false)?;
        let result = run_all(self).and_then(|n| self.end_tran(true).map(|_| n));
        if result.is_err() {
            let _ = self.end_tran(false);
        }
        let restored = self.set_autocommit(true);
        let n = result?;
        restored?;
        Ok(n)
    }

    /// Tables and views with their columns through the catalog functions.
    fn load_schema(&self) -> anyhow::Result<Vec<SchemaInfo>> {
        let api = Arc::clone(&self.api);
        let width = api.width;
        let wide = |s: &str| Wide::new(width, s);

        // SQLTables: 2 TABLE_SCHEM, 3 TABLE_NAME, 4 TABLE_TYPE.
        let stmt = self.statement()?;
        let types = wide("TABLE,VIEW");
        // SAFETY: NULL catalog / schema / table (all) and a NUL-terminated type list.
        let rc = unsafe {
            (api.SQLTablesW)(
                stmt.h,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                types.ptr(),
                ffi::SQL_NTS as i16,
            )
        };
        stmt.check(rc)?;
        let mut tables: Vec<(String, TableInfo)> = Vec::new();
        for row in stmt.all_rows()? {
            let text = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            let schema = text(1);
            if SYSTEM_SCHEMAS
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&schema))
            {
                continue;
            }
            let kind = match text(3).to_ascii_uppercase().as_str() {
                "TABLE" => ObjectKind::Table,
                "VIEW" => ObjectKind::View,
                _ => continue,
            };
            tables.push((
                schema,
                TableInfo {
                    name: text(2),
                    kind,
                    columns: Vec::new(),
                },
            ));
        }
        drop(stmt);

        // SQLColumns for every table at once: 2 TABLE_SCHEM, 3 TABLE_NAME, 4 COLUMN_NAME,
        // 5 DATA_TYPE, 6 TYPE_NAME, 7 COLUMN_SIZE, 9 DECIMAL_DIGITS, 11 NULLABLE,
        // 17 ORDINAL_POSITION.
        let index: HashMap<(String, String), usize> = tables
            .iter()
            .enumerate()
            .map(|(i, (s, t))| ((s.clone(), t.name.clone()), i))
            .collect();
        let mut columns: Vec<Vec<(i64, ColumnInfo)>> = vec![Vec::new(); tables.len()];
        let stmt = self.statement()?;
        let all = wide("%");
        // SAFETY: NULL catalog and schema; NUL-terminated `%` patterns.
        let rc = unsafe {
            (api.SQLColumnsW)(
                stmt.h,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                all.ptr(),
                ffi::SQL_NTS as i16,
                all.ptr(),
                ffi::SQL_NTS as i16,
            )
        };
        stmt.check(rc)?;
        for row in stmt.all_rows()? {
            let text = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            let int = |i: usize| {
                row.get(i)
                    .cloned()
                    .flatten()
                    .and_then(|v| v.trim().parse::<i64>().ok())
            };
            let Some(&t) = index.get(&(text(1), text(2))) else {
                continue;
            };
            let sql_type = int(4).and_then(|v| i16::try_from(v).ok()).unwrap_or(0);
            let position = int(16).unwrap_or(columns[t].len() as i64 + 1);
            columns[t].push((
                position,
                ColumnInfo {
                    name: text(3),
                    data_type: type_label(&text(5), sql_type, int(6), int(8)),
                    nullable: int(10) != Some(0),
                    primary_key: false,
                },
            ));
        }
        drop(stmt);
        for ((_, table), mut cols) in tables.iter_mut().zip(columns) {
            cols.sort_by_key(|(pos, _)| *pos);
            table.columns = cols.into_iter().map(|(_, c)| c).collect();
        }

        // Primary keys, one call per table: only while that stays cheap.
        let base_tables = tables
            .iter()
            .filter(|(_, t)| t.kind == ObjectKind::Table)
            .count();
        if base_tables <= PK_LOOKUP_LIMIT {
            for (schema, table) in tables
                .iter_mut()
                .filter(|(_, t)| t.kind == ObjectKind::Table)
            {
                // Not every driver implements it: the explorer just goes without the key.
                let Ok(keys) = self.primary_keys(schema, &table.name) else {
                    continue;
                };
                for col in &mut table.columns {
                    col.primary_key = keys.contains(&col.name);
                }
            }
        }

        let mut schemas: Vec<SchemaInfo> = Vec::new();
        for (schema, table) in tables {
            match schemas.iter_mut().find(|s| s.name == schema) {
                Some(s) => s.tables.push(table),
                None => schemas.push(SchemaInfo {
                    name: schema,
                    tables: vec![table],
                }),
            }
        }
        for s in &mut schemas {
            s.tables.sort_by(|a, b| a.name.cmp(&b.name));
        }
        schemas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(schemas)
    }

    /// Column names of a table's primary key (`SQLPrimaryKeys`, 4 COLUMN_NAME).
    fn primary_keys(&self, schema: &str, table: &str) -> anyhow::Result<Vec<String>> {
        let width = self.api.width;
        let stmt = self.statement()?;
        let schema_w = Wide::new(width, schema);
        let table_w = Wide::new(width, table);
        let schema_ptr = if schema.is_empty() {
            std::ptr::null()
        } else {
            schema_w.ptr()
        };
        // SAFETY: NUL-terminated names that live through the call; NULL for no schema.
        let rc = unsafe {
            (self.api.SQLPrimaryKeysW)(
                stmt.h,
                std::ptr::null(),
                0,
                schema_ptr,
                if schema.is_empty() {
                    0
                } else {
                    ffi::SQL_NTS as i16
                },
                table_w.ptr(),
                ffi::SQL_NTS as i16,
            )
        };
        stmt.check(rc)?;
        Ok(stmt
            .all_rows()?
            .into_iter()
            .filter_map(|r| r.get(3).cloned().flatten())
            .collect())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // SAFETY: each handle was allocated by `open` and is freed exactly once, children first.
        unsafe {
            if self.connected {
                if self.manual {
                    let _ = (self.api.SQLEndTran)(ffi::SQL_HANDLE_DBC, self.dbc, ffi::SQL_ROLLBACK);
                }
                (self.api.SQLDisconnect)(self.dbc);
            }
            if !self.dbc.is_null() {
                (self.api.SQLFreeHandle)(ffi::SQL_HANDLE_DBC, self.dbc);
            }
            if !self.env.is_null() {
                (self.api.SQLFreeHandle)(ffi::SQL_HANDLE_ENV, self.env);
            }
        }
    }
}

/// A statement handle, freed when dropped.
struct Stmt<'a> {
    conn: &'a Conn,
    h: Handle,
}

impl Stmt<'_> {
    fn check(&self, rc: i16) -> anyhow::Result<i16> {
        self.conn.check(rc, ffi::SQL_HANDLE_STMT, self.h)
    }

    /// Like `check`, but SQL_NO_DATA (nothing affected) is a success too.
    fn check_no_data(&self, rc: i16) -> anyhow::Result<i16> {
        if rc == ffi::SQL_NO_DATA {
            Ok(rc)
        } else {
            self.check(rc)
        }
    }

    /// Name and SQL type of a result column.
    fn describe(&self, col: u16) -> anyhow::Result<(String, i16)> {
        let api = &self.conn.api;
        let mut size = 256usize;
        loop {
            let mut name = Wide::zeroed(api.width, size);
            let (mut len, mut sql_type, mut digits, mut nullable) = (0i16, 0i16, 0i16, 0i16);
            let mut col_size: usize = 0;
            // SAFETY: every out-pointer is a live local; the name buffer's length is in characters.
            let rc = unsafe {
                (api.SQLDescribeColW)(
                    self.h,
                    col,
                    name.ptr_mut(),
                    name.units() as i16,
                    &mut len,
                    &mut sql_type,
                    &mut col_size,
                    &mut digits,
                    &mut nullable,
                )
            };
            self.check(rc)?;
            let len = len.max(0) as usize;
            if len >= size && size < 8192 {
                size = len + 1;
                continue;
            }
            return Ok((name.decode(len.min(size - 1)), sql_type));
        }
    }

    /// Moves to the next row; false at the end.
    fn fetch(&self) -> anyhow::Result<bool> {
        // SAFETY: the statement handle is valid.
        let rc = unsafe { (self.conn.api.SQLFetch)(self.h) };
        if rc == ffi::SQL_NO_DATA {
            return Ok(false);
        }
        self.check(rc)?;
        Ok(true)
    }

    fn row(&self, types: &[i16]) -> anyhow::Result<Vec<Cell>> {
        types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if is_binary(*t) {
                    self.binary((i + 1) as u16)
                } else {
                    self.text((i + 1) as u16)
                }
            })
            .collect()
    }

    /// Every remaining row, as text (catalog functions).
    fn all_rows(&self) -> anyhow::Result<Vec<Vec<Cell>>> {
        let mut ncols: i16 = 0;
        // SAFETY: the statement handle is valid.
        self.check(unsafe { (self.conn.api.SQLNumResultCols)(self.h, &mut ncols) })?;
        let mut rows = Vec::new();
        while self.fetch()? {
            rows.push(
                (1..=ncols.max(0) as u16)
                    .map(|c| self.text(c))
                    .collect::<anyhow::Result<_>>()?,
            );
        }
        Ok(rows)
    }

    /// A column of the current row as text, read in chunks (`SQL_C_WCHAR`).
    fn text(&self, col: u16) -> anyhow::Result<Cell> {
        let api = &self.conn.api;
        let unit = api.width.bytes();
        let mut buf = Wide::zeroed(api.width, TEXT_CHUNK);
        let mut acc = Wide::zeroed(api.width, 0);
        // Room for data in each chunk: the driver always ends it with a NUL.
        let room = TEXT_CHUNK - 1;
        loop {
            let mut ind: isize = 0;
            // SAFETY: the buffer lives through the call; its length is in bytes.
            let rc = unsafe {
                (api.SQLGetData)(
                    self.h,
                    col,
                    ffi::SQL_C_WCHAR,
                    buf.ptr_mut(),
                    buf.bytes() as isize,
                    &mut ind,
                )
            };
            if rc == ffi::SQL_NO_DATA {
                break;
            }
            self.check(rc)?;
            if ind == ffi::SQL_NULL_DATA {
                return Ok(None);
            }
            let whole = ind != ffi::SQL_NO_TOTAL && ind >= 0 && (ind as usize / unit) <= room;
            let units = if whole { ind as usize / unit } else { room };
            acc.extend_from(&buf, units);
            if rc == ffi::SQL_SUCCESS || whole {
                break;
            }
        }
        Ok(Some(acc.decode(acc.units())))
    }

    /// A binary column of the current row as hex (its first bytes).
    fn binary(&self, col: u16) -> anyhow::Result<Cell> {
        let mut buf = vec![0u8; BINARY_CHUNK];
        let mut ind: isize = 0;
        // SAFETY: the buffer lives through the call; its length is in bytes.
        let rc = unsafe {
            (self.conn.api.SQLGetData)(
                self.h,
                col,
                ffi::SQL_C_BINARY,
                buf.as_mut_ptr().cast(),
                buf.len() as isize,
                &mut ind,
            )
        };
        if rc == ffi::SQL_NO_DATA {
            return Ok(Some(hex_preview(&[])));
        }
        self.check(rc)?;
        if ind == ffi::SQL_NULL_DATA {
            return Ok(None);
        }
        let n = if ind == ffi::SQL_NO_TOTAL || ind < 0 {
            buf.len()
        } else {
            (ind as usize).min(buf.len())
        };
        Ok(Some(hex_preview(&buf[..n])))
    }
}

impl Drop for Stmt<'_> {
    fn drop(&mut self) {
        // SAFETY: allocated by `Conn::statement`, freed once.
        unsafe { (self.conn.api.SQLFreeHandle)(ffi::SQL_HANDLE_STMT, self.h) };
    }
}

/// The ODBC C API, loaded at runtime from the driver manager, and wide strings sized to it.
#[allow(non_snake_case)]
mod ffi {
    use std::ffi::c_void;

    pub type Handle = *mut c_void;
    type W = *const c_void;
    type WMut = *mut c_void;

    pub const SQL_SUCCESS: i16 = 0;
    pub const SQL_SUCCESS_WITH_INFO: i16 = 1;
    pub const SQL_NO_DATA: i16 = 100;
    pub const SQL_ERROR: i16 = -1;

    pub const SQL_HANDLE_ENV: i16 = 1;
    pub const SQL_HANDLE_DBC: i16 = 2;
    pub const SQL_HANDLE_STMT: i16 = 3;

    pub const SQL_NTS: i32 = -3;
    pub const SQL_NULL_DATA: isize = -1;
    pub const SQL_NO_TOTAL: isize = -4;
    pub const SQL_IS_UINTEGER: i32 = -5;

    pub const SQL_ATTR_ODBC_VERSION: i32 = 200;
    pub const SQL_OV_ODBC3: usize = 3;
    pub const SQL_ATTR_ACCESS_MODE: i32 = 101;
    pub const SQL_MODE_READ_ONLY: usize = 1;
    pub const SQL_ATTR_AUTOCOMMIT: i32 = 102;
    pub const SQL_AUTOCOMMIT_OFF: usize = 0;
    pub const SQL_AUTOCOMMIT_ON: usize = 1;
    pub const SQL_ATTR_LOGIN_TIMEOUT: i32 = 103;
    pub const SQL_DRIVER_NOPROMPT: u16 = 0;
    pub const SQL_COMMIT: i16 = 0;
    pub const SQL_ROLLBACK: i16 = 1;

    pub const SQL_DATABASE_NAME: u16 = 16;
    pub const SQL_DBMS_NAME: u16 = 17;
    pub const SQL_DBMS_VER: u16 = 18;

    pub const SQL_PARAM_INPUT: i16 = 1;
    pub const SQL_C_WCHAR: i16 = -8;
    pub const SQL_C_BINARY: i16 = -2;

    pub const SQL_CHAR: i16 = 1;
    pub const SQL_NUMERIC: i16 = 2;
    pub const SQL_DECIMAL: i16 = 3;
    pub const SQL_VARCHAR: i16 = 12;
    pub const SQL_WCHAR: i16 = -8;
    pub const SQL_WVARCHAR: i16 = -9;
    pub const SQL_BINARY: i16 = -2;
    pub const SQL_VARBINARY: i16 = -3;
    pub const SQL_LONGVARBINARY: i16 = -4;

    /// Size of `SQLWCHAR`: 16 bits on Windows and unixODBC, `wchar_t` (32 bits) on iODBC.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Width {
        Utf16,
        Utf32,
    }

    impl Width {
        pub fn bytes(self) -> usize {
            match self {
                Width::Utf16 => 2,
                Width::Utf32 => 4,
            }
        }

        /// iODBC's `SQLWCHAR` is `wchar_t`, 32 bits outside Windows.
        pub fn of_library(name: &str) -> Width {
            if cfg!(not(windows)) && name.to_ascii_lowercase().contains("iodbc") {
                Width::Utf32
            } else {
                Width::Utf16
            }
        }
    }

    /// A `SQLWCHAR` buffer of the manager's width.
    pub enum Wide {
        U16(Vec<u16>),
        U32(Vec<u32>),
    }

    impl Wide {
        /// NUL-terminated text.
        pub fn new(width: Width, s: &str) -> Wide {
            match width {
                Width::Utf16 => Wide::U16(s.encode_utf16().chain([0]).collect()),
                Width::Utf32 => Wide::U32(s.chars().map(u32::from).chain([0]).collect()),
            }
        }

        pub fn zeroed(width: Width, units: usize) -> Wide {
            match width {
                Width::Utf16 => Wide::U16(vec![0; units]),
                Width::Utf32 => Wide::U32(vec![0; units]),
            }
        }

        pub fn units(&self) -> usize {
            match self {
                Wide::U16(v) => v.len(),
                Wide::U32(v) => v.len(),
            }
        }

        pub fn bytes(&self) -> usize {
            match self {
                Wide::U16(v) => v.len() * 2,
                Wide::U32(v) => v.len() * 4,
            }
        }

        pub fn ptr(&self) -> W {
            match self {
                Wide::U16(v) => v.as_ptr().cast(),
                Wide::U32(v) => v.as_ptr().cast(),
            }
        }

        pub fn ptr_mut(&mut self) -> WMut {
            match self {
                Wide::U16(v) => v.as_mut_ptr().cast(),
                Wide::U32(v) => v.as_mut_ptr().cast(),
            }
        }

        /// Appends the first `units` of `other` (same width).
        pub fn extend_from(&mut self, other: &Wide, units: usize) {
            match (self, other) {
                (Wide::U16(a), Wide::U16(b)) => a.extend_from_slice(&b[..units.min(b.len())]),
                (Wide::U32(a), Wide::U32(b)) => a.extend_from_slice(&b[..units.min(b.len())]),
                _ => {}
            }
        }

        /// The text in the first `units` (invalid sequences replaced).
        pub fn decode(&self, units: usize) -> String {
            match self {
                Wide::U16(v) => String::from_utf16_lossy(&v[..units.min(v.len())]),
                Wide::U32(v) => v[..units.min(v.len())]
                    .iter()
                    .map(|&c| char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect(),
            }
        }
    }

    macro_rules! api {
        ($($name:ident: fn($($arg:ty),*) -> $ret:ty;)*) => {
            /// The driver manager's functions; the library stays loaded while this lives.
            pub struct Api {
                _lib: libloading::Library,
                pub width: Width,
                $(pub $name: unsafe extern "system" fn($($arg),*) -> $ret,)*
            }

            impl Api {
                fn load_from(path: &str) -> Result<Api, libloading::Error> {
                    // SAFETY: loading the system's ODBC driver manager, whose initializers have
                    // no special requirements.
                    let lib = unsafe { libloading::Library::new(path) }?;
                    // SAFETY: each symbol is the ODBC function with the declared signature
                    // (sql.h / sqlext.h / sqlucode.h; SQLLEN and SQLULEN are pointer-sized).
                    unsafe {
                        $(let $name = *lib.get::<unsafe extern "system" fn($($arg),*) -> $ret>(
                            concat!(stringify!($name), "\0").as_bytes())?;)*
                        Ok(Api { _lib: lib, width: Width::of_library(path), $($name,)* })
                    }
                }
            }
        };
    }

    api! {
        SQLAllocHandle: fn(i16, Handle, *mut Handle) -> i16;
        SQLFreeHandle: fn(i16, Handle) -> i16;
        SQLSetEnvAttr: fn(Handle, i32, *mut c_void, i32) -> i16;
        SQLSetConnectAttrW: fn(Handle, i32, *mut c_void, i32) -> i16;
        SQLDriverConnectW: fn(Handle, *mut c_void, W, i16, WMut, i16, *mut i16, u16) -> i16;
        SQLDisconnect: fn(Handle) -> i16;
        SQLGetInfoW: fn(Handle, u16, *mut c_void, i16, *mut i16) -> i16;
        SQLEndTran: fn(i16, Handle, i16) -> i16;
        SQLExecDirectW: fn(Handle, W, i32) -> i16;
        SQLPrepareW: fn(Handle, W, i32) -> i16;
        SQLExecute: fn(Handle) -> i16;
        SQLBindParameter: fn(Handle, u16, i16, i16, i16, usize, i16, *mut c_void, isize, *mut isize) -> i16;
        SQLNumResultCols: fn(Handle, *mut i16) -> i16;
        SQLDescribeColW: fn(Handle, u16, WMut, i16, *mut i16, *mut i16, *mut usize, *mut i16, *mut i16) -> i16;
        SQLFetch: fn(Handle) -> i16;
        SQLGetData: fn(Handle, u16, i16, *mut c_void, isize, *mut isize) -> i16;
        SQLRowCount: fn(Handle, *mut isize) -> i16;
        SQLMoreResults: fn(Handle) -> i16;
        SQLGetDiagRecW: fn(i16, Handle, i16, WMut, *mut i32, WMut, i16, *mut i16) -> i16;
        SQLTablesW: fn(Handle, W, i16, W, i16, W, i16, W, i16) -> i16;
        SQLColumnsW: fn(Handle, W, i16, W, i16, W, i16, W, i16) -> i16;
        SQLPrimaryKeysW: fn(Handle, W, i16, W, i16, W, i16) -> i16;
    }

    /// Where the driver manager is looked for, in order.
    pub fn candidates() -> Vec<String> {
        if let Ok(path) = std::env::var("DATACRAFT_ODBC_LIB")
            && !path.trim().is_empty()
        {
            return vec![path.trim().to_string()];
        }
        if cfg!(windows) {
            vec!["odbc32.dll".into()]
        } else if cfg!(target_os = "macos") {
            let mut out = Vec::new();
            for name in ["libodbc.2.dylib", "libiodbc.2.dylib"] {
                for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"] {
                    out.push(format!("{dir}/{name}"));
                }
                out.push(name.into());
            }
            out
        } else {
            ["libodbc.so.2", "libodbc.so", "libiodbc.so.2"]
                .map(String::from)
                .to_vec()
        }
    }

    impl Api {
        /// The first driver manager that loads.
        pub fn load() -> anyhow::Result<Api> {
            let mut errors = Vec::new();
            for path in candidates() {
                match Api::load_from(&path) {
                    Ok(api) => return Ok(api),
                    Err(e) => errors.push(format!("{path}: {e}")),
                }
            }
            let hint = if cfg!(windows) {
                "Install the database's ODBC driver"
            } else if cfg!(target_os = "macos") {
                "Install unixODBC (brew install unixodbc) or iODBC, and the database's ODBC driver"
            } else {
                "Install unixODBC (the unixodbc package) or iODBC, and the database's ODBC driver"
            };
            anyhow::bail!(
                "No ODBC driver manager could be loaded. {hint}; DATACRAFT_ODBC_LIB can point to \
                 the library.\n{}",
                errors.join("\n")
            )
        }

        pub fn alloc(&self, handle_type: i16, parent: Handle) -> anyhow::Result<Handle> {
            let mut h: Handle = std::ptr::null_mut();
            // SAFETY: `parent` is null (environment) or a live handle; `h` is a live local.
            let rc = unsafe { (self.SQLAllocHandle)(handle_type, parent, &mut h) };
            if (rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) && !h.is_null() {
                Ok(h)
            } else {
                anyhow::bail!("Could not allocate an ODBC handle (return code {rc})")
            }
        }

        /// The diagnostic records of a handle: (SQLSTATE, message).
        pub fn diagnostics(&self, handle_type: i16, handle: Handle) -> Vec<(String, String)> {
            let mut out = Vec::new();
            if handle.is_null() {
                return out;
            }
            for record in 1..=20i16 {
                let mut state = Wide::zeroed(self.width, 6);
                let mut msg = Wide::zeroed(self.width, 2048);
                let (mut native, mut len) = (0i32, 0i16);
                // SAFETY: buffers live through the call; their lengths are in characters.
                let rc = unsafe {
                    (self.SQLGetDiagRecW)(
                        handle_type,
                        handle,
                        record,
                        state.ptr_mut(),
                        &mut native,
                        msg.ptr_mut(),
                        msg.units() as i16,
                        &mut len,
                    )
                };
                if rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO {
                    break;
                }
                let len = (len.max(0) as usize).min(msg.units() - 1);
                out.push((state.decode(5), msg.decode(len)));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;
    use ffi::Width;

    fn cfg(database: &str, user: &str, password: &str) -> ConnectionConfig {
        let mut cfg = ConnectionConfig::new(1, Driver::Odbc);
        cfg.database = database.into();
        cfg.user = user.into();
        cfg.password = password.into();
        cfg.auth = AuthMode::UserPassword;
        cfg
    }

    #[test]
    fn bare_word_is_a_dsn() {
        assert_eq!(full_connection_string(&cfg("Sales", "", "")), "DSN=Sales");
        assert_eq!(
            full_connection_string(&cfg(" Sales ", "bob", "s3cret")),
            "DSN=Sales;UID=bob;PWD=s3cret"
        );
        assert_eq!(display_name("Sales"), "Sales");
    }

    #[test]
    fn credentials_are_escaped_and_replace_typed_ones() {
        let c = cfg(
            "DRIVER={Progress OpenEdge 12.8 Driver};HOST=h;PORT=1;DB=x;UID=old;PWD=old;",
            "a;b",
            "p}w{d",
        );
        assert_eq!(
            full_connection_string(&c),
            "DRIVER={Progress OpenEdge 12.8 Driver};HOST=h;PORT=1;DB=x;UID={a;b};PWD={p}}w{d}"
        );
        let mut no_auth = c.clone();
        no_auth.auth = AuthMode::NoAuth;
        assert!(full_connection_string(&no_auth).ends_with("UID=old;PWD=old"));
        assert_eq!(escape_value(" x"), "{ x}");
        assert_eq!(unescape_value("{p}}w{d}"), "p}w{d");
    }

    #[test]
    fn parses_braced_values() {
        let a = attributes("Driver={A;B}}C};Server = s ;;Pwd={x;y}");
        assert_eq!(
            a,
            vec![
                ("Driver".to_string(), "{A;B}}C}".to_string()),
                ("Server".to_string(), "s".to_string()),
                ("Pwd".to_string(), "{x;y}".to_string()),
            ]
        );
    }

    #[test]
    fn url_never_carries_the_password() {
        let c = cfg("DSN=x;PWD={a;b};Password=c;DB=y", "u", "secret");
        assert_eq!(connection_string(&c), "DSN=x;DB=y");
        assert_eq!(connection_string(&cfg("Sales", "u", "p")), "Sales");
        let mut back = ConnectionConfig::new(2, Driver::Odbc);
        assert!(apply_url(&mut back, "odbc:DSN=x;DB=y"));
        assert_eq!(back.database, "DSN=x;DB=y");
        assert!(!apply_url(&mut back, "postgres://x"));
    }

    #[test]
    fn display_name_from_the_string() {
        assert_eq!(display_name(OPENEDGE_TEMPLATE), "sports2020");
        assert_eq!(display_name("DSN={My DSN};UID=a"), "My DSN");
        assert_eq!(display_name("DRIVER=x"), "ODBC");
        assert_eq!(display_name(""), "ODBC");
    }

    #[test]
    fn type_labels_and_binary_types() {
        assert_eq!(
            type_label("varchar", ffi::SQL_VARCHAR, Some(30), None),
            "varchar(30)"
        );
        assert_eq!(
            type_label("decimal", ffi::SQL_DECIMAL, Some(10), Some(2)),
            "decimal(10,2)"
        );
        assert_eq!(type_label("integer", 4, Some(10), Some(0)), "integer");
        assert_eq!(
            type_label("char(5)", ffi::SQL_CHAR, Some(5), None),
            "char(5)"
        );
        assert!(is_binary(ffi::SQL_VARBINARY) && is_binary(ffi::SQL_LONGVARBINARY));
        assert!(!is_binary(ffi::SQL_WVARCHAR));
        assert_eq!(hex_preview(&[0xCA, 0xFE]), "0xCAFE");
    }

    #[test]
    fn wide_strings_of_both_widths() {
        for width in [Width::Utf16, Width::Utf32] {
            let w = Wide::new(width, "añ😀");
            assert_eq!(w.bytes(), w.units() * width.bytes());
            assert_eq!(w.decode(w.units() - 1), "añ😀");
            let mut acc = Wide::zeroed(width, 0);
            acc.extend_from(&w, 2);
            acc.extend_from(&w, 0);
            assert_eq!(acc.decode(acc.units()), "añ");
        }
        assert_eq!(Wide::new(Width::Utf16, "😀").units(), 3);
        assert_eq!(Wide::new(Width::Utf32, "😀").units(), 2);
        if cfg!(not(windows)) {
            assert_eq!(Width::of_library("/usr/lib/libiodbc.2.dylib"), Width::Utf32);
        }
        assert_eq!(Width::of_library("libodbc.so.2"), Width::Utf16);
    }

    /// Live tests against `DATACRAFT_ODBC_CONN` (a full connection string, e.g. SQLite's ODBC
    /// driver or psqlODBC); skipped when unset.
    fn live_config() -> Option<ConnectionConfig> {
        let conn = std::env::var("DATACRAFT_ODBC_CONN").ok()?;
        let mut cfg = ConnectionConfig::new(1, Driver::Odbc);
        cfg.database = conn;
        cfg.auth = AuthMode::NoAuth;
        Some(cfg)
    }

    async fn count(s: &mut OdbcSession, table: &str) -> String {
        let rs = s
            .execute(&format!("SELECT COUNT(*) FROM {table}"), 10)
            .await
            .unwrap();
        rs[0].rows[0][0].clone().unwrap()
    }

    #[tokio::test]
    async fn live_queries_transactions_and_catalog() {
        let Some(cfg) = live_config() else {
            return;
        };
        let mut s = OdbcSession::connect(&cfg).await.unwrap();
        let version = s.server_version().await;
        assert!(version.is_some_and(|v| !v.is_empty()));
        assert!(!s.current_database().await.unwrap().is_empty());

        let table = format!("datacraft_odbc_{}", std::process::id());
        let _ = s.execute(&format!("DROP TABLE {table}"), 0).await;
        s.execute(
            &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, name VARCHAR(20000), note VARCHAR(10))"),
            0,
        )
        .await
        .unwrap();

        // Rows, NULLs, non-ASCII and long text through a bound parameter.
        let long = "ñ".repeat(9000) + "end";
        s.execute(&format!("INSERT INTO {table} VALUES (1, 'añ😀', NULL)"), 0)
            .await
            .unwrap();
        let rs = s
            .execute_bound(
                &format!("INSERT INTO {table} (id, name, note) VALUES (?, ?, ?)"),
                &["2".into(), long.clone(), "x".into()],
                0,
            )
            .await
            .unwrap();
        assert_eq!(rs[0].affected, Some(1));
        s.execute(&format!("INSERT INTO {table} VALUES (3, 'c', 'z')"), 0)
            .await
            .unwrap();
        let rs = s
            .execute(
                &format!("SELECT id, name, note FROM {table} ORDER BY id"),
                2,
            )
            .await
            .unwrap();
        assert_eq!(rs[0].columns.len(), 3);
        assert_eq!(rs[0].rows.len(), 2);
        assert!(rs[0].truncated);
        assert_eq!(rs[0].rows[0][1].as_deref(), Some("añ😀"));
        assert_eq!(rs[0].rows[0][2], None);
        assert_eq!(rs[0].rows[1][1].as_deref(), Some(long.as_str()));
        let rs = s
            .execute_bound(
                &format!("SELECT name FROM {table} WHERE note = ?"),
                &["z".into()],
                10,
            )
            .await
            .unwrap();
        assert_eq!(rs[0].rows, vec![vec![Some("c".to_string())]]);
        let rs = s
            .execute(&format!("UPDATE {table} SET note = 'q' WHERE id = 99"), 0)
            .await
            .unwrap();
        assert_eq!(rs[0].affected, Some(0));
        assert!(
            s.execute("SELECT * FROM datacraft_no_such_table", 10)
                .await
                .is_err()
        );

        // Transactions.
        s.begin().await.unwrap();
        s.execute(&format!("DELETE FROM {table} WHERE id = 3"), 0)
            .await
            .unwrap();
        s.end_transaction(false).await.unwrap();
        assert_eq!(count(&mut s, &table).await, "3");
        s.begin().await.unwrap();
        s.execute(&format!("DELETE FROM {table} WHERE id = 3"), 0)
            .await
            .unwrap();
        s.end_transaction(true).await.unwrap();
        assert_eq!(count(&mut s, &table).await, "2");
        let n = s
            .run_transaction(&[
                format!("INSERT INTO {table} VALUES (4, 'd', NULL)"),
                format!("INSERT INTO {table} VALUES (5, 'e', NULL)"),
            ])
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(
            s.run_transaction(&[
                format!("INSERT INTO {table} VALUES (6, 'f', NULL)"),
                format!("INSERT INTO {table} VALUES (6, 'f', NULL)"),
            ])
            .await
            .is_err()
        );
        assert_eq!(count(&mut s, &table).await, "4");

        // apply_changes: exactly one row per statement, or nothing.
        assert_eq!(
            s.apply_changes(&[format!("UPDATE {table} SET note = 'one' WHERE id = 1")])
                .await
                .unwrap(),
            1
        );
        let err = s
            .apply_changes(&[
                format!("UPDATE {table} SET note = 'two' WHERE id = 1"),
                format!("UPDATE {table} SET note = 'many' WHERE id IN (4, 5)"),
            ])
            .await
            .unwrap_err();
        assert!(err.to_string().contains('2'));
        let rs = s
            .execute(&format!("SELECT note FROM {table} WHERE id = 1"), 10)
            .await
            .unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("one"));

        // Catalog.
        let schemas = s.introspect().await.unwrap();
        let t = schemas
            .iter()
            .flat_map(|s| &s.tables)
            .find(|t| t.name.eq_ignore_ascii_case(&table))
            .expect("the test table in the catalog");
        assert_eq!(t.kind, ObjectKind::Table);
        let names: Vec<String> = t.columns.iter().map(|c| c.name.to_lowercase()).collect();
        assert_eq!(names, ["id", "name", "note"]);

        s.execute(&format!("DROP TABLE {table}"), 0).await.unwrap();
        assert!(!s.is_closed());
    }
}
