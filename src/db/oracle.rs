//! Oracle driver (rust-oracle over ODPI-C; needs the Oracle Instant Client at runtime).
//! The connection has one database (the service); its schemas are the users that own objects.
//!
//! rust-oracle is blocking, so the connection lives behind an `Arc<Mutex<_>>` and every
//! call runs in `spawn_blocking`, like the SQLite driver.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use oracle::sql_type::OracleType;
use oracle::{Connection, Connector, InitParams, Row};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::db::sqlite::hex_preview;
use crate::i18n::{tr, trf};

const DEFAULT_PORT: u16 = 1521;
const DEFAULT_SERVICE: &str = "FREEPDB1";
/// Login (TCP + authentication) time limit. ODPI-C blocks, so the abandoned thread
/// finishes on its own when the network gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Owners per `IN (...)` list in the introspection query (Oracle allows 1000).
const OWNER_CHUNK: usize = 500;

pub struct OraSession {
    /// Shared so `break_execution` can cancel a running statement from another thread;
    /// `gate` serializes the session's own calls.
    conn: Arc<Connection>,
    gate: Arc<Mutex<()>>,
    /// Set after connection-level errors: the session is no longer usable.
    broken: bool,
    /// Container/database name, computed once so the explorer node stays stable.
    db_name: Option<String>,
}

impl OraSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let host = if cfg.host.trim().is_empty() { "localhost".to_string() } else { cfg.host.trim().to_string() };
        let port = if cfg.port == 0 { DEFAULT_PORT } else { cfg.port };
        let connect_string = connect_string(&host, port, &cfg.database);
        let (user, password) = (cfg.user.clone(), cfg.password.clone());

        let task = tokio::task::spawn_blocking(move || open(&user, &password, &connect_string));
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, task)
            .await
            .map_err(|_| anyhow!(trf("Timed out connecting to {host}:{port}", &[("host", &host), ("port", &port)])))??
            .map_err(ora_error)?;
        Ok(Self { conn: Arc::new(conn), gate: Arc::new(Mutex::new(())), broken: false, db_name: None })
    }

    /// Connection handle to cancel the running statement (`break_execution`).
    pub fn break_handle(&self) -> Arc<Connection> {
        Arc::clone(&self.conn)
    }

    /// Runs `f` with the connection on a blocking thread and marks the session as broken
    /// when the error means the connection was lost.
    async fn with_conn<T, F>(&mut self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, oracle::Error> + Send + 'static,
    {
        let (conn, gate) = (Arc::clone(&self.conn), Arc::clone(&self.gate));
        let outcome = tokio::task::spawn_blocking(move || {
            let Ok(_guard) = gate.lock() else {
                return Err(anyhow!(tr("The Oracle connection became unusable")));
            };
            f(&conn).map_err(ora_error)
        })
        .await;
        let result = match outcome {
            Ok(r) => r,
            Err(e) => Err(anyhow!(e.to_string())),
        };
        if let Err(e) = &result
            && (is_connection_lost(&format!("{e:#}")) || self.gate.is_poisoned())
        {
            self.broken = true;
        }
        result
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let display = sql.trim().to_string();
        let sql = normalize_statement(sql);
        if sql.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| run(conn, &sql, display, max_rows).map(|rs| vec![rs])).await
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        if let Some(name) = &self.db_name {
            return Ok(name.clone());
        }
        let name = self
            .with_conn(|conn| {
                // CON_NAME exists from 12c (multitenant); older servers only know DB_NAME.
                match conn.query_row_as::<Option<String>>("SELECT SYS_CONTEXT('USERENV','CON_NAME') FROM dual", &[]) {
                    Ok(Some(name)) => Ok(Some(name)),
                    Err(e) if is_connection_lost(&e.to_string()) => Err(e),
                    _ => conn.query_row_as::<Option<String>>("SELECT SYS_CONTEXT('USERENV','DB_NAME') FROM dual", &[]),
                }
            })
            .await?
            .ok_or_else(|| anyhow!(tr("The server did not return the current database")))?;
        self.db_name = Some(name.clone());
        Ok(name)
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.current_database().await?])
    }

    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        self.transaction(statements, true).await
    }

    /// All the statements in one transaction (e.g. an import's INSERTs).
    pub async fn run_transaction(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        self.transaction(statements, false).await
    }

    async fn transaction(&mut self, statements: &[String], single_row: bool) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        let (conn, gate) = (Arc::clone(&self.conn), Arc::clone(&self.gate));
        let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let Ok(_guard) = gate.lock() else {
                anyhow::bail!(tr("The Oracle connection became unusable"));
            };
            let result = run_in_transaction(&conn, &statements, single_row);
            if result.is_err() {
                let _ = conn.rollback();
            }
            result
        })
        .await;
        let result = match outcome {
            Ok(r) => r,
            Err(e) => Err(anyhow!(e.to_string())),
        };
        if let Err(e) = &result
            && is_connection_lost(&format!("{e:#}"))
        {
            self.broken = true;
        }
        result
    }

    pub fn is_closed(&self) -> bool {
        self.broken
    }

    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        self.with_conn(load_schemas).await
    }
}

/// Opens the connection; if the Instant Client can't be loaded from the default locations,
/// tries the usual places where people unpack it before giving up.
fn open(user: &str, password: &str, connect_string: &str) -> Result<Connection, oracle::Error> {
    let connect = || -> Result<Connection, oracle::Error> {
        // Auto-commit stays off: `run` commits each console statement itself and
        // `apply_changes` commits (or rolls back) its whole transaction.
        let conn = Connector::new(user, password, connect_string).connect()?;
        let _ = conn.set_module("DataCraft");
        // Dates and numbers as text in the same format the grid shows, so edited values,
        // cloned rows and imports can be written back as plain literals.
        conn.execute(
            "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' \
             NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
             NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM' \
             NLS_NUMERIC_CHARACTERS = '.,'",
            &[],
        )?;
        Ok(conn)
    };
    match connect() {
        Err(e) if client_load_error_code(&e.to_string()).is_some() && !InitParams::is_initialized() => {
            for dir in client_dir_candidates() {
                let loaded = InitParams::new().oracle_client_lib_dir(dir.as_os_str()).and_then(|p| p.init());
                if loaded.is_ok() {
                    return connect();
                }
            }
            Err(e)
        }
        other => other,
    }
}

/// EZConnect string `//host:port/service`. IPv6 addresses go in brackets.
fn connect_string(host: &str, port: u16, service: &str) -> String {
    let host = host.trim();
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    let service = service.trim();
    let service = if service.is_empty() { DEFAULT_SERVICE } else { service };
    format!("//{host}:{port}/{service}")
}

/// Readable message for an Oracle error: the `ORA-…`/`DPI-…` text without the crate's
/// "OCI Error:" prefix, or the Instant Client installation help when the library is missing.
fn ora_error(e: oracle::Error) -> anyhow::Error {
    let raw = match e.db_error() {
        Some(db) => db.message().to_string(),
        None => e.to_string(),
    };
    match client_load_error_code(&raw) {
        Some(code) => anyhow!(client_missing_message(code, &raw)),
        None => anyhow!(raw),
    }
}

/// ODPI-C codes for "the Oracle Client library could not be loaded / is not usable".
fn client_load_error_code(message: &str) -> Option<&'static str> {
    ["DPI-1047", "DPI-1072", "DPI-1050"].into_iter().find(|code| message.contains(code))
}

fn client_missing_message(code: &str, detail: &str) -> String {
    let hint = if cfg!(target_os = "macos") {
        tr("Download the Instant Client Basic package for macOS from oracle.com, unpack it and link libclntsh.dylib into ~/lib (or /usr/local/lib), or leave the instantclient folder in your home or Downloads folder.")
    } else if cfg!(windows) {
        tr("Download the Instant Client Basic package for Windows (64-bit) from oracle.com, unpack it and add its folder to PATH (or copy its DLLs next to DataCraft.exe). It also needs the Visual C++ Redistributable.")
    } else {
        tr("Install the Oracle Instant Client Basic package and make libclntsh.so findable (ldconfig or LD_LIBRARY_PATH).")
    };
    let mut msg = trf(
        "Oracle connections need the Oracle Instant Client, which could not be loaded ({code}).\n{hint}",
        &[("code", &code), ("hint", &hint)],
    );
    msg.push_str(&trf("\nDetail: {detail}", &[("detail", &detail)]));
    msg
}

/// Errors after which the session can't be used any more (lost network, killed session...).
fn is_connection_lost(message: &str) -> bool {
    const CODES: [&str; 17] = [
        "ORA-00028", // session killed
        "ORA-01012", // not logged on
        "ORA-02396", // idle time exceeded
        "ORA-03113", // end-of-file on communication channel
        "ORA-03114", // not connected
        "ORA-03135", // connection lost contact
        "ORA-12152", "ORA-12537", "ORA-12547", "ORA-12570", "ORA-12571", "ORA-12583",
        "DPI-1010", // not connected
        "DPI-1080", // connection was closed by ORA-%d
        "DPI-1067", // call timeout
        "DPI-1040", // LOB was already closed
        "DPI-1001", // out of memory
    ];
    CODES.iter().any(|c| message.contains(c))
}

/// Library file of the Oracle Client on this platform.
fn client_lib_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libclntsh.dylib"
    } else if cfg!(windows) {
        "oci.dll"
    } else {
        "libclntsh.so"
    }
}

fn has_client_lib(dir: &Path) -> bool {
    dir.join(client_lib_name()).is_file()
}

/// Directories where the Instant Client is usually unpacked (`instantclient_23_3`...),
/// plus `ORACLE_HOME`; only those that actually contain the library.
fn client_dir_candidates() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut found: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("ORACLE_HOME").map(PathBuf::from) {
        found.push(home.join("lib"));
        found.push(home);
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from) {
        roots.push(home.join("Downloads"));
        roots.push(home.join("lib"));
        roots.push(home);
    }
    if cfg!(windows) {
        roots.extend(["C:\\oracle", "C:\\", "C:\\Program Files\\Oracle"].map(PathBuf::from));
    } else {
        roots.extend(["/opt/oracle", "/usr/local/oracle", "/usr/local/lib", "/opt"].map(PathBuf::from));
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().starts_with("instantclient"))
            })
            .collect();
        // Newest version first (instantclient_23_x before instantclient_19_x).
        dirs.sort();
        dirs.reverse();
        found.extend(dirs);
    }
    found.retain(|d| has_client_lib(d));
    found.dedup();
    found
}

/// Kind of first keyword-level token, skipping whitespace and comments.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = sql;
    while words.len() < n {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, after)| after);
            continue;
        }
        if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, after)| after);
            continue;
        }
        let len = rest
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '$' | '#')))
            .map_or(rest.len(), |(i, _)| i);
        if len == 0 {
            break;
        }
        words.push(rest[..len].to_ascii_uppercase());
        rest = &rest[len..];
    }
    words
}

/// PL/SQL blocks and stored-code DDL: their final `;` is part of the syntax.
fn is_plsql(sql: &str) -> bool {
    let words = leading_words(sql, 6);
    let mut it = words.iter().map(String::as_str).peekable();
    match it.next() {
        Some("BEGIN" | "DECLARE") => true,
        Some("CREATE") => {
            if it.peek() == Some(&"OR") {
                it.next();
                if it.next() != Some("REPLACE") {
                    return false;
                }
            }
            if matches!(it.peek(), Some(&("EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING"))) {
                it.next();
            }
            matches!(it.next(), Some("PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY"))
        }
        _ => false,
    }
}

/// Adapts a statement from the console splitter to OCI: drops SQL*Plus `/` lines, removes
/// the trailing `;` of SQL statements and makes sure PL/SQL blocks keep (or get) theirs.
fn normalize_statement(sql: &str) -> String {
    let mut lines: Vec<&str> = sql.lines().collect();
    while lines.first().is_some_and(|l| l.trim().is_empty() || l.trim() == "/") {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| matches!(l.trim(), "" | "/" | "/;")) {
        lines.pop();
    }
    let text = lines.join("\n");
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if is_plsql(text) {
        if text.ends_with(';') { text.to_string() } else { format!("{text};") }
    } else {
        text.trim_end_matches(|c: char| c == ';' || c.is_whitespace()).to_string()
    }
}

/// How a column's value is turned into text.
#[derive(Debug, PartialEq, Eq)]
enum CellKind {
    Text,
    Binary,
    /// Values that can't be fetched as text; shown with this label.
    Placeholder(&'static str),
}

fn cell_kind(t: &OracleType) -> CellKind {
    match t {
        OracleType::Raw(_) | OracleType::LongRaw | OracleType::BLOB => CellKind::Binary,
        OracleType::BFILE => CellKind::Placeholder("(BFILE)"),
        OracleType::RefCursor => CellKind::Placeholder("(cursor)"),
        _ => CellKind::Text,
    }
}

fn cell(row: &Row, i: usize, kind: &CellKind) -> Cell {
    let value = match kind {
        CellKind::Binary => row.get::<_, Option<Vec<u8>>>(i).map(|v| v.map(|b| hex_preview(&b))),
        CellKind::Placeholder(label) => match row.sql_values()[i].is_null() {
            Ok(true) => Ok(None),
            _ => Ok(Some((*label).to_string())),
        },
        // DATE/TIMESTAMP come as `YYYY-MM-DD HH24:MI:SS[.ff][ +hh:mm]`, CLOB as its text.
        CellKind::Text => row.get::<_, Option<String>>(i),
    };
    value.unwrap_or_else(|e| Some(format!("<{e}>")))
}

fn run(conn: &Connection, sql: &str, display: String, max_rows: usize) -> Result<ResultSet, oracle::Error> {
    let start = Instant::now();
    let fetch = if max_rows == 0 {
        1000
    } else {
        max_rows.saturating_add(1).min(1000) as u32
    };
    let mut stmt = conn.statement(sql).fetch_array_size(fetch).build()?;
    let mut rs = ResultSet { statement: display, ..Default::default() };
    if stmt.is_query() {
        let rows = stmt.query(&[])?;
        let kinds: Vec<CellKind> = rows.column_info().iter().map(|c| cell_kind(c.oracle_type())).collect();
        rs.columns = rows.column_info().iter().map(|c| c.name().to_string()).collect();
        for row in rows {
            if rs.rows.len() >= max_rows {
                rs.truncated = true;
                if max_rows != 0 {
                    break;
                }
                row?;
                continue;
            }
            let row = row?;
            rs.rows.push(kinds.iter().enumerate().map(|(i, k)| cell(&row, i, k)).collect());
        }
    } else {
        stmt.execute(&[])?;
        if stmt.is_dml() {
            rs.affected = Some(stmt.row_count()?);
        }
        // The console works in auto-commit mode.
        conn.commit()?;
        if matches!(leading_words(sql, 1).first().map(String::as_str), Some("BEGIN" | "DECLARE")) {
            let lines = dbms_output(conn, max_rows)?;
            if !lines.is_empty() {
                rs.columns = vec!["DBMS_OUTPUT".into()];
                rs.truncated = lines.len() >= max_rows;
                rs.rows = lines.into_iter().map(|line| vec![line]).collect();
            }
        }
    }
    rs.elapsed = start.elapsed();
    Ok(rs)
}

/// Lines an anonymous block wrote with `DBMS_OUTPUT` (if the session enabled it), at most
/// `max_rows`.
fn dbms_output(conn: &Connection, max_rows: usize) -> Result<Vec<Option<String>>, oracle::Error> {
    let mut stmt = conn.statement("BEGIN DBMS_OUTPUT.GET_LINE(:1, :2); END;").build()?;
    let mut lines = Vec::new();
    while lines.len() < max_rows {
        stmt.execute(&[&OracleType::Varchar2(32767), &OracleType::Int64])?;
        if stmt.bind_value::<_, i64>(2)? != 0 {
            break;
        }
        lines.push(stmt.bind_value::<_, Option<String>>(1)?);
    }
    Ok(lines)
}

/// Runs the statements without committing in between; with `single_row`, each must affect
/// exactly one row. Commits at the end (the caller rolls back on error).
fn run_in_transaction(conn: &Connection, statements: &[String], single_row: bool) -> anyhow::Result<u64> {
    let mut total = 0;
    for stmt in statements {
        let sql = normalize_statement(stmt);
        let n = conn.execute(&sql, &[]).map_err(ora_error)?.row_count().map_err(ora_error)?;
        if single_row && n != 1 {
            return Err(super::not_single_row(n, stmt));
        }
        total += n;
    }
    conn.commit().map_err(ora_error)?;
    Ok(total)
}

/// Schemas Oracle installs and maintains itself, for servers older than 12c
/// (no `ALL_USERS.ORACLE_MAINTAINED`).
pub(super) const SYSTEM_SCHEMAS: &[&str] = &[
    "ANONYMOUS", "APPQOSSYS", "AUDSYS", "CTXSYS", "DBSFWUSER", "DBSNMP", "DIP", "DVF", "DVSYS", "EXFSYS",
    "GGSYS", "GSMADMIN_INTERNAL", "GSMCATUSER", "GSMUSER", "LBACSYS", "MDDATA", "MDSYS", "MGMT_VIEW", "OJVMSYS",
    "OLAPSYS", "ORACLE_OCM", "ORDDATA", "ORDPLUGINS", "ORDSYS", "OUTLN", "OWBSYS", "OWBSYS_AUDIT",
    "REMOTE_SCHEDULER_AGENT", "SI_INFORMTN_SCHEMA", "SPATIAL_CSW_ADMIN_USR", "SPATIAL_WFS_ADMIN_USR", "SYS",
    "SYSBACKUP", "SYSDG", "SYSKM", "SYSMAN", "SYSRAC", "SYSTEM", "TSMSYS", "WMSYS", "XDB", "XS$NULL",
];

/// Owners of visible tables/views, without Oracle's own schemas.
fn owners_sql(oracle_maintained_column: bool) -> String {
    let filter = if oracle_maintained_column {
        "owner NOT IN (SELECT username FROM all_users WHERE oracle_maintained = 'Y')".to_string()
    } else {
        format!("owner NOT IN ({}) AND owner NOT LIKE 'APEX\\_%' ESCAPE '\\' AND owner NOT LIKE 'FLOWS\\_%' ESCAPE '\\'", in_list(SYSTEM_SCHEMAS))
    };
    format!("SELECT owner FROM (SELECT owner FROM all_tables UNION SELECT owner FROM all_views) WHERE {filter} ORDER BY owner")
}

/// `'A', 'B'` with the quotes escaped.
fn in_list<S: AsRef<str>>(names: &[S]) -> String {
    names.iter().map(|n| format!("'{}'", n.as_ref().replace('\'', "''"))).collect::<Vec<_>>().join(", ")
}

fn objects_sql(owners: &[String]) -> String {
    let list = in_list(owners);
    format!(
        "SELECT o.owner, o.object_name, o.kind, c.column_name, c.data_type, c.data_length, c.char_length,
                c.char_used, c.data_precision, c.data_scale, c.nullable,
                CASE WHEN pk.column_name IS NULL THEN 'N' ELSE 'Y' END
         FROM (SELECT owner, table_name AS object_name, 'T' AS kind FROM all_tables
               WHERE owner IN ({list}) AND nested = 'NO' AND secondary = 'N' AND dropped = 'NO'
                 AND (iot_type IS NULL OR iot_type = 'IOT')
               UNION ALL
               SELECT owner, view_name, 'V' FROM all_views WHERE owner IN ({list})) o
         LEFT JOIN all_tab_columns c ON c.owner = o.owner AND c.table_name = o.object_name
         LEFT JOIN (SELECT cc.owner, cc.table_name, cc.column_name
                    FROM all_constraints k
                    JOIN all_cons_columns cc ON cc.owner = k.owner AND cc.constraint_name = k.constraint_name
                    WHERE k.constraint_type = 'P' AND k.owner IN ({list})) pk
                ON pk.owner = c.owner AND pk.table_name = c.table_name AND pk.column_name = c.column_name
         ORDER BY o.owner, o.object_name, c.column_id"
    )
}

/// Column type as written in DDL, lowercased: `varchar2(255)`, `number(10,2)`, `date`...
fn type_label(
    data_type: &str,
    data_length: Option<i64>,
    char_length: Option<i64>,
    char_used: Option<&str>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let t = data_type.to_ascii_lowercase();
    match t.as_str() {
        "varchar2" | "char" => match (char_length, char_used) {
            (Some(n), Some("C")) => format!("{t}({n} char)"),
            (Some(n), _) if n > 0 => format!("{t}({n})"),
            _ => match data_length {
                Some(n) => format!("{t}({n})"),
                None => t,
            },
        },
        "nvarchar2" | "nchar" => match char_length.or(data_length) {
            Some(n) => format!("{t}({n})"),
            None => t,
        },
        "raw" => match data_length {
            Some(n) => format!("{t}({n})"),
            None => t,
        },
        "number" => match (precision, scale) {
            (None, None) => t,
            (None, Some(0)) => "integer".to_string(),
            (None, Some(s)) => format!("number(*,{s})"),
            (Some(p), Some(0) | None) => format!("number({p})"),
            (Some(p), Some(s)) => format!("number({p},{s})"),
        },
        "float" => match precision {
            Some(p) => format!("float({p})"),
            None => t,
        },
        _ => t,
    }
}

fn load_schemas(conn: &Connection) -> Result<Vec<SchemaInfo>, oracle::Error> {
    let user: String = conn.query_row_as("SELECT USER FROM dual", &[])?;

    let mut owners: Vec<String> = match conn.query_as::<String>(&owners_sql(true), &[]) {
        Ok(rows) => rows.collect::<Result<_, _>>()?,
        // ORA-00904: invalid identifier (ORACLE_MAINTAINED doesn't exist before 12c).
        Err(e) if e.to_string().contains("ORA-00904") => {
            conn.query_as::<String>(&owners_sql(false), &[])?.collect::<Result<_, _>>()?
        }
        Err(e) => return Err(e),
    };
    owners.retain(|o| *o != user);
    owners.insert(0, user);

    let mut tables: HashMap<String, Vec<TableInfo>> = HashMap::new();
    for chunk in owners.chunks(OWNER_CHUNK) {
        let sql = objects_sql(chunk);
        let mut stmt = conn.statement(&sql).fetch_array_size(1000).build()?;
        for row in stmt.query(&[])? {
            let row = row?;
            let text = |i: usize| row.get::<_, Option<String>>(i).ok().flatten();
            let int = |i: usize| row.get::<_, Option<i64>>(i).ok().flatten();
            let (Some(owner), Some(name)) = (text(0), text(1)) else { continue };
            let list = tables.entry(owner).or_default();
            if list.last().is_none_or(|t| t.name != name) {
                list.push(TableInfo {
                    name,
                    kind: if text(2).as_deref() == Some("V") { ObjectKind::View } else { ObjectKind::Table },
                    columns: Vec::new(),
                });
            }
            let Some(t) = list.last_mut() else { continue };
            // Object without visible columns: the LEFT JOIN leaves the column NULL.
            let Some(column) = text(3) else { continue };
            t.columns.push(ColumnInfo {
                name: column,
                data_type: type_label(
                    &text(4).unwrap_or_default(),
                    int(5),
                    int(6),
                    text(7).as_deref(),
                    int(8),
                    int(9),
                ),
                nullable: text(10).as_deref() != Some("N"),
                primary_key: text(11).as_deref() == Some("Y"),
            });
        }
    }

    Ok(owners
        .into_iter()
        .map(|name| SchemaInfo { tables: tables.remove(&name).unwrap_or_default(), name })
        .collect())
}

#[cfg(test)]
mod tests {
    //! Pure helpers, plus tests against a real Oracle. Run those with `cargo test -- --ignored`
    //! and the variables DATACRAFT_ORA_HOST (required; skipped without it), DATACRAFT_ORA_PORT (1521),
    //! DATACRAFT_ORA_SERVICE (FREEPDB1), DATACRAFT_ORA_USER (system), DATACRAFT_ORA_PASSWORD.
    //! They need the Oracle Instant Client.

    use super::*;
    use crate::db::Driver;
    use oracle::sql_type::Timestamp;

    #[test]
    fn strips_trailing_semicolon_from_sql() {
        assert_eq!(normalize_statement("SELECT 1 FROM dual;"), "SELECT 1 FROM dual");
        assert_eq!(normalize_statement("  insert into t values (1) ;  \n"), "insert into t values (1)");
        assert_eq!(normalize_statement("SELECT ';' FROM dual"), "SELECT ';' FROM dual");
        assert_eq!(normalize_statement("CREATE TABLE t (id NUMBER);;"), "CREATE TABLE t (id NUMBER)");
        assert_eq!(normalize_statement("CREATE OR REPLACE VIEW v AS SELECT 1 x FROM dual;"), "CREATE OR REPLACE VIEW v AS SELECT 1 x FROM dual");
        assert_eq!(normalize_statement("-- comment\nSELECT 1 FROM dual;"), "-- comment\nSELECT 1 FROM dual");
    }

    #[test]
    fn keeps_semicolon_on_plsql() {
        assert_eq!(normalize_statement("BEGIN NULL; END;"), "BEGIN NULL; END;");
        assert_eq!(normalize_statement("begin null; end"), "begin null; end;");
        assert_eq!(normalize_statement("DECLARE x NUMBER; BEGIN x := 1; END;\n/\n"), "DECLARE x NUMBER; BEGIN x := 1; END;");
        assert_eq!(
            normalize_statement("CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;\n/"),
            "CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;"
        );
        assert_eq!(normalize_statement("create or replace editionable function f return number is begin return 1; end;"), "create or replace editionable function f return number is begin return 1; end;");
        assert_eq!(normalize_statement("/* c */ CREATE PACKAGE BODY pk AS END pk;"), "/* c */ CREATE PACKAGE BODY pk AS END pk;");
        assert_eq!(normalize_statement("CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN NULL; END;"), "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN NULL; END;");
        assert_eq!(normalize_statement("CREATE TYPE pt AS OBJECT (x NUMBER);"), "CREATE TYPE pt AS OBJECT (x NUMBER);");
    }

    #[test]
    fn slash_lines() {
        assert_eq!(normalize_statement("/"), "");
        assert_eq!(normalize_statement("\n/\n"), "");
        assert_eq!(normalize_statement("\n/\nSELECT 1 FROM dual;"), "SELECT 1 FROM dual");
        assert_eq!(normalize_statement("SELECT 4/2 FROM dual\n/"), "SELECT 4/2 FROM dual");
        assert_eq!(normalize_statement("   "), "");
    }

    #[test]
    fn plsql_detection() {
        assert!(is_plsql("  -- x\n  BEGIN NULL; END;"));
        assert!(is_plsql("DECLARE\n  n NUMBER;\nBEGIN NULL; END;"));
        assert!(is_plsql("CREATE OR REPLACE NONEDITIONABLE PACKAGE p AS END;"));
        assert!(is_plsql("CREATE OR REPLACE TYPE BODY t AS END;"));
        assert!(!is_plsql("CREATE TABLE begin_t (id NUMBER)"));
        assert!(!is_plsql("CREATE OR REPLACE VIEW v AS SELECT 1 FROM dual"));
        assert!(!is_plsql("CREATE OR FOO PROCEDURE p"));
        assert!(!is_plsql("SELECT 'BEGIN' FROM dual"));
        assert!(!is_plsql("beginning"));
        assert_eq!(leading_words("/* a */ -- b\n create  or replace(", 4), ["CREATE", "OR", "REPLACE"]);
    }

    #[test]
    fn connect_strings() {
        assert_eq!(connect_string("db.local", 1521, "FREEPDB1"), "//db.local:1521/FREEPDB1");
        assert_eq!(connect_string(" 10.0.0.5 ", 1522, " orcl.example.com "), "//10.0.0.5:1522/orcl.example.com");
        assert_eq!(connect_string("localhost", 1521, ""), "//localhost:1521/FREEPDB1");
        assert_eq!(connect_string("::1", 1521, "XE"), "//[::1]:1521/XE");
        assert_eq!(connect_string("[::1]", 1521, "XE"), "//[::1]:1521/XE");
    }

    #[test]
    fn error_classification() {
        assert!(is_connection_lost("ORA-03113: end-of-file on communication channel"));
        assert!(is_connection_lost("DPI-1010: not connected"));
        assert!(is_connection_lost("DPI-1080: connection was closed by ORA-3113"));
        assert!(is_connection_lost("ORA-03135: connection lost contact"));
        assert!(!is_connection_lost("ORA-00942: table or view does not exist"));
        assert!(!is_connection_lost("ORA-00001: unique constraint (X.PK) violated"));

        let missing = "DPI-1047: Cannot locate a 64-bit Oracle Client library: \"dlopen(libclntsh.dylib, 0x0001): tried: ...\"";
        assert_eq!(client_load_error_code(missing), Some("DPI-1047"));
        assert_eq!(client_load_error_code("ORA-01017: invalid username/password"), None);
        let msg = client_missing_message("DPI-1047", missing);
        assert!(msg.contains("Instant Client") && msg.contains("DPI-1047") && msg.contains("dlopen"), "{msg}");
    }

    #[test]
    fn value_formatting() {
        assert_eq!(cell_kind(&OracleType::Raw(16)), CellKind::Binary);
        assert_eq!(cell_kind(&OracleType::BLOB), CellKind::Binary);
        assert_eq!(cell_kind(&OracleType::LongRaw), CellKind::Binary);
        assert_eq!(cell_kind(&OracleType::CLOB), CellKind::Text);
        assert_eq!(cell_kind(&OracleType::Date), CellKind::Text);
        assert_eq!(cell_kind(&OracleType::Number(10, 2)), CellKind::Text);
        assert_eq!(cell_kind(&OracleType::BFILE), CellKind::Placeholder("(BFILE)"));
        assert_eq!(hex_preview(&[0xCA, 0xFE]), "0xCAFE");
        // DATE and TIMESTAMP values are shown with the crate's ISO-like format.
        let ts = Timestamp::new(2026, 10, 16, 10, 20, 30, 500_000_000).unwrap();
        assert_eq!(ts.and_prec(0).unwrap().to_string(), "2026-10-16 10:20:30");
        assert_eq!(ts.and_prec(3).unwrap().to_string(), "2026-10-16 10:20:30.500");
        assert_eq!(ts.and_prec(0).unwrap().and_tz_hm_offset(2, 0).unwrap().to_string(), "2026-10-16 10:20:30 +02:00");
    }

    #[test]
    fn type_labels() {
        assert_eq!(type_label("VARCHAR2", Some(255), Some(255), Some("B"), None, None), "varchar2(255)");
        assert_eq!(type_label("VARCHAR2", Some(400), Some(100), Some("C"), None, None), "varchar2(100 char)");
        assert_eq!(type_label("NVARCHAR2", Some(100), Some(50), Some("C"), None, None), "nvarchar2(50)");
        assert_eq!(type_label("NUMBER", Some(22), None, None, Some(10), Some(2)), "number(10,2)");
        assert_eq!(type_label("NUMBER", Some(22), None, None, Some(10), Some(0)), "number(10)");
        assert_eq!(type_label("NUMBER", Some(22), None, None, None, Some(0)), "integer");
        assert_eq!(type_label("NUMBER", Some(22), None, None, None, None), "number");
        assert_eq!(type_label("RAW", Some(16), None, None, None, None), "raw(16)");
        assert_eq!(type_label("FLOAT", Some(22), None, None, Some(126), None), "float(126)");
        assert_eq!(type_label("DATE", Some(7), None, None, None, None), "date");
        assert_eq!(type_label("TIMESTAMP(6) WITH TIME ZONE", Some(13), None, None, None, Some(6)), "timestamp(6) with time zone");
        assert_eq!(type_label("CLOB", Some(4000), None, None, None, None), "clob");
    }

    #[test]
    fn introspection_sql() {
        assert_eq!(in_list(&["A", "O'B"]), "'A', 'O''B'");
        assert!(owners_sql(true).contains("oracle_maintained = 'Y'"));
        let legacy = owners_sql(false);
        assert!(legacy.contains("'SYS'") && legacy.contains("'XDB'") && !legacy.contains("oracle_maintained"));
        let sql = objects_sql(&["HR".into(), "SCOTT".into()]);
        assert!(sql.contains("owner IN ('HR', 'SCOTT')") && sql.contains("constraint_type = 'P'"));
    }

    /// Without the Instant Client, connecting must return the installation help (not panic).
    #[tokio::test]
    async fn missing_instant_client_is_reported() {
        if !client_dir_candidates().is_empty() || default_client_present() {
            eprintln!("Oracle Instant Client present: skipping the missing-client test");
            return;
        }
        let mut cfg = ConnectionConfig::new(1, Driver::Oracle);
        cfg.host = "127.0.0.1".into();
        cfg.port = 1;
        let Err(err) = OraSession::connect(&cfg).await else { panic!("connected without a server") };
        let msg = err.to_string();
        if client_load_error_code(&msg).is_none() {
            // The library loaded after all (found on the default search path): the error is a network one.
            eprintln!("Instant Client found on the default path: {msg}");
            return;
        }
        eprintln!("{msg}");
        assert!(msg.contains("Instant Client"), "{msg}");
    }

    fn default_client_present() -> bool {
        let dirs: Vec<PathBuf> = ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH", "PATH"]
            .iter()
            .filter_map(std::env::var_os)
            .flat_map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
            .chain(std::env::var_os("HOME").map(|h| PathBuf::from(h).join("lib")))
            .chain(["/usr/local/lib", "/usr/lib"].map(PathBuf::from))
            .collect();
        dirs.iter().any(|d| has_client_lib(d))
    }

    async fn session() -> Option<OraSession> {
        let Ok(host) = std::env::var("DATACRAFT_ORA_HOST") else {
            eprintln!("DATACRAFT_ORA_HOST not set: skipping the Oracle test");
            return None;
        };
        let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        let mut cfg = ConnectionConfig::new(1, Driver::Oracle);
        cfg.host = host;
        cfg.port = env("DATACRAFT_ORA_PORT", "1521").parse().expect("port");
        cfg.database = env("DATACRAFT_ORA_SERVICE", "FREEPDB1");
        cfg.user = env("DATACRAFT_ORA_USER", "system");
        cfg.password = env("DATACRAFT_ORA_PASSWORD", "");
        Some(OraSession::connect(&cfg).await.expect("connection"))
    }

    async fn one(s: &mut OraSession, sql: &str, max_rows: usize) -> ResultSet {
        let mut rs = s.execute(sql, max_rows).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(rs.len(), 1);
        rs.remove(0)
    }

    #[tokio::test]
    #[ignore]
    async fn databases_and_types() {
        let Some(mut s) = session().await else { return };
        let db = s.current_database().await.unwrap();
        assert_eq!(s.list_databases().await.unwrap(), std::slice::from_ref(&db));
        assert_eq!(s.current_database().await.unwrap(), db);

        let rs = one(
            &mut s,
            "SELECT 42 AS i, CAST(12.5 AS NUMBER(10,2)) AS n, 'hello' AS t, DATE '2026-10-16' AS d, \
             TIMESTAMP '2026-10-16 10:20:30.5' AS ts, HEXTORAW('CAFE') AS r, TO_CLOB('long text') AS c, \
             NULL AS z FROM dual;",
            10,
        )
        .await;
        assert_eq!(rs.columns, ["I", "N", "T", "D", "TS", "R", "C", "Z"]);
        let row: Vec<Option<&str>> = rs.rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(
            row,
            [
                Some("42"),
                Some("12.5"),
                Some("hello"),
                Some("2026-10-16 00:00:00"),
                Some("2026-10-16 10:20:30.500000000"),
                Some("0xCAFE"),
                Some("long text"),
                None
            ]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn dml_plsql_transactions_and_introspection() {
        let Some(mut s) = session().await else { return };
        let _ = s.execute("DROP TABLE dc_items PURGE", 0).await;
        let _ = s.execute("DROP VIEW dc_items_v", 0).await;
        one(&mut s, "CREATE TABLE dc_items (id NUMBER(10) PRIMARY KEY, name VARCHAR2(20) NOT NULL, price NUMBER(10,2));", 0).await;
        let rs = one(&mut s, "INSERT INTO dc_items SELECT level, 'n' || level, NULL FROM dual CONNECT BY level <= 3;", 0).await;
        assert_eq!(rs.affected, Some(3));
        let rs = one(&mut s, "SELECT * FROM dc_items ORDER BY id", 2).await;
        assert_eq!(rs.rows.len(), 2);
        assert!(rs.truncated);
        assert_eq!(rs.rows[1][2], None);
        let rs = one(&mut s, "SELECT * FROM dc_items WHERE 1 = 0", 10).await;
        assert_eq!(rs.columns.len(), 3);
        assert!(rs.rows.is_empty() && !rs.truncated);
        assert_eq!(one(&mut s, "UPDATE dc_items SET price = 1", 0).await.affected, Some(3));

        // PL/SQL keeps its semicolon and runs in auto-commit.
        let rs = one(&mut s, "BEGIN UPDATE dc_items SET price = 2 WHERE id = 1; END;\n/", 0).await;
        assert!(rs.is_command());
        let rs = one(&mut s, "SELECT price FROM dc_items WHERE id = 1 FETCH FIRST 1 ROWS ONLY", 10).await;
        assert_eq!(rs.rows[0][0].as_deref(), Some("2"));

        // apply_changes: commit, then rollback when a statement doesn't touch exactly one row.
        assert_eq!(s.apply_changes(&["UPDATE dc_items SET name = 'x' WHERE id = 1".into()]).await.unwrap(), 1);
        let err = s
            .apply_changes(&["UPDATE dc_items SET name = 'y' WHERE id = 1".into(), "DELETE FROM dc_items WHERE id = 9".into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains('0'), "{err}");
        let rs = one(&mut s, "SELECT name FROM dc_items WHERE id = 1", 10).await;
        assert_eq!(rs.rows[0][0].as_deref(), Some("x"));

        // Errors don't break the session.
        let err = s.execute("SELECT * FROM dc_missing_xyz", 10).await.unwrap_err();
        assert!(err.to_string().contains("ORA-00942"), "{err}");
        assert!(!s.is_closed());

        one(&mut s, "CREATE VIEW dc_items_v AS SELECT id FROM dc_items", 0).await;
        let schemas = s.introspect().await.unwrap();
        let me = &schemas[0];
        let items = me.tables.iter().find(|t| t.name == "DC_ITEMS").expect("table");
        assert_eq!(items.kind, ObjectKind::Table);
        let cols: Vec<_> = items.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable, c.primary_key)).collect();
        assert_eq!(
            cols,
            [("ID", "number(10)", false, true), ("NAME", "varchar2(20)", false, false), ("PRICE", "number(10,2)", true, false)]
        );
        assert_eq!(me.tables.iter().find(|t| t.name == "DC_ITEMS_V").expect("view").kind, ObjectKind::View);
        assert!(!schemas.iter().skip(1).any(|s| s.name == "SYS" || s.name == "XDB"));

        one(&mut s, "DROP VIEW dc_items_v", 0).await;
        one(&mut s, "DROP TABLE dc_items PURGE", 0).await;
    }
}
