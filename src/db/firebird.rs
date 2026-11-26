//! Firebird driver (Firebird 3+), over its wire protocol in pure Rust with rsfbclient-rust:
//! Srp/Srp256 authentication and Arc4 wire encryption, no fbclient library to install.
//!
//! The client is blocking, so the connection lives behind an `Arc<Mutex<_>>` and every call
//! runs in `spawn_blocking`, like the Oracle driver. Firebird has no autocommit of its own:
//! each console statement runs in a transaction that commits right after it, and a Manual
//! transaction keeps one open until `end_transaction`.
//!
//! The client turns NUMERIC/DECIMAL into doubles and DATE/TIME into timestamps, and rejects the
//! types Firebird 4 added. On Firebird 4+ the session asks the server (`SET BIND`) to send those
//! as text, so decimals stay exact and time zones survive; on Firebird 3 the values are
//! formatted as well as the client's types allow.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use encoding::{DecoderTrap, EncoderTrap, Encoding, RawDecoder, RawEncoder};
use rsfbclient_core::{
    Charset, Column, Dialect, FbError, FirebirdClientDbOps, FirebirdClientSqlOps, FreeStmtOp,
    SqlType, StmtType, TrDataAccessMode, TrIsolationLevel, TrLockResolution, TrOp, TrRecordVersion,
    TransactionConfiguration,
};
use rsfbclient_rust::{DbHandle, RustFbClient, RustFbClientAttachmentConfig, TrHandle};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::db::sqlite::hex_preview;
use crate::i18n::trf;

const DEFAULT_PORT: u16 = 3050;
const DEFAULT_DATABASE: &str = "employee";
const DEFAULT_USER: &str = "SYSDBA";
/// Login (TCP + authentication) time limit. The client blocks, so the abandoned thread
/// finishes on its own when the network gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DIALECT: Dialect = Dialect::D3;
/// Savepoint that makes `run_transaction` all or nothing inside a Manual transaction.
const SAVEPOINT: &str = "DATACRAFT_BATCH";

/// Prepared statement of the pure Rust client.
type Stmt = <RustFbClient as FirebirdClientSqlOps>::StmtHandle;

pub struct FbSession {
    conn: Arc<Mutex<Conn>>,
    /// The database field as configured (alias or path).
    database: String,
    /// `rdb$get_context('SYSTEM', 'ENGINE_VERSION')`, read on connecting.
    version: Option<String>,
    /// Set once the connection is known lost.
    broken: bool,
}

impl FbSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let target = Target::from_config(cfg);
        let (host, port) = (target.host.clone(), target.port);
        let database = target.database.clone();
        let task = tokio::task::spawn_blocking(move || open(&target));
        let (conn, version) = tokio::time::timeout(CONNECT_TIMEOUT, task)
            .await
            .map_err(|_| {
                anyhow!(trf(
                    "Timed out connecting to {host}:{port}",
                    &[("host", &host), ("port", &port)]
                ))
            })???;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            database,
            version,
            broken: false,
        })
    }

    /// Runs `f` with the connection on a blocking thread and marks the session as broken
    /// when the connection was lost.
    async fn with_conn<T, F>(&mut self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Conn) -> anyhow::Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let outcome = tokio::task::spawn_blocking(move || {
            let Ok(mut guard) = conn.lock() else {
                return (
                    Err(anyhow!("The Firebird connection became unusable")),
                    true,
                );
            };
            let result = f(&mut guard);
            (result, guard.lost)
        })
        .await;
        let (result, lost) = match outcome {
            Ok(r) => r,
            Err(e) => (Err(anyhow!(e.to_string())), true),
        };
        if lost {
            self.broken = true;
        }
        result
    }

    /// Runs ONE statement; at most `max_rows` rows per result (0 = discard rows).
    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let display = sql.trim().to_string();
        let sql = normalize_statement(sql);
        if sql.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(move |c| {
            let mut rs = c.run(&sql, &[], max_rows)?;
            rs.statement = display;
            Ok(vec![rs])
        })
        .await
    }

    /// A SELECT with `?` parameters bound as text.
    pub async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[String],
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let display = sql.trim().to_string();
        let sql = normalize_statement(sql);
        let parameters = parameters.to_vec();
        self.with_conn(move |c| {
            let mut rs = c.run(&sql, &parameters, max_rows)?;
            rs.statement = display;
            Ok(vec![rs])
        })
        .await
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        Ok(self.database.clone())
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.database.clone()])
    }

    /// Data editor changes in ONE transaction; each statement must affect exactly one row.
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |c| c.batch(&statements, true)).await
    }

    /// Schemas, tables/views and columns, without system objects.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let name = self.database.clone();
        self.with_conn(move |c| load_schema(c, name)).await
    }

    /// Product and version for Test Connection.
    pub async fn server_version(&mut self) -> Option<String> {
        self.version.as_ref().map(|v| format!("Firebird {v}"))
    }

    /// Runs the statements in ONE transaction (all or nothing); the affected rows.
    pub async fn run_transaction(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let statements = statements.to_vec();
        self.with_conn(move |c| c.batch(&statements, false)).await
    }

    /// Manual transactions (console): from here on nothing commits by itself until
    /// `end_transaction`.
    pub async fn begin(&mut self) -> anyhow::Result<()> {
        self.with_conn(|c| {
            if c.manual.is_none() {
                let tr = c.begin_tx()?;
                c.manual = Some(tr);
            }
            Ok(())
        })
        .await
    }

    /// Commits or rolls back the Manual transaction and goes back to autocommit.
    pub async fn end_transaction(&mut self, commit: bool) -> anyhow::Result<()> {
        self.with_conn(move |c| {
            let Some(mut tr) = c.manual.take() else {
                return Ok(());
            };
            let op = if commit { TrOp::Commit } else { TrOp::Rollback };
            let done = c.client.transaction_operation(&mut tr, op);
            // A commit the server refused (a lock conflict, a deferred check) leaves the
            // transaction open: keep it, so a Roll Back can still end it.
            if done.is_err() && !c.lost {
                c.manual = Some(tr);
            }
            c.check(done)
        })
        .await
    }

    pub fn is_closed(&self) -> bool {
        self.broken
    }
}

/// Where and how to attach, resolved from the connection settings.
struct Target {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    role: Option<String>,
    read_only: bool,
}

impl Target {
    fn from_config(cfg: &ConnectionConfig) -> Self {
        let or = |value: &str, default: &str| {
            let value = value.trim();
            if value.is_empty() {
                default.to_string()
            } else {
                value.to_string()
            }
        };
        let role = cfg.role.trim();
        Self {
            host: or(&cfg.host, "localhost"),
            port: if cfg.port == 0 {
                DEFAULT_PORT
            } else {
                cfg.port
            },
            database: or(&cfg.database, DEFAULT_DATABASE),
            user: or(&cfg.user, DEFAULT_USER),
            password: cfg.password.clone(),
            role: (!role.is_empty()).then(|| role.to_string()),
            read_only: cfg.read_only,
        }
    }
}

/// Attaches and prepares the session; returns it with the server version.
fn open(target: &Target) -> anyhow::Result<(Conn, Option<String>)> {
    let mut client = RustFbClient::new(connection_charset());
    let config = RustFbClientAttachmentConfig {
        host: target.host.clone(),
        port: target.port,
        db_name: target.database.clone(),
        user: target.user.clone(),
        pass: target.password.clone(),
        role_name: target.role.clone(),
    };
    let db = client
        .attach_database(&config, DIALECT, false)
        .map_err(|e| match e {
            FbError::Io(io) => anyhow!(trf(
                "Could not connect to {host}:{port}: {error}",
                &[
                    ("host", &target.host),
                    ("port", &target.port),
                    ("error", &io)
                ]
            )),
            other => anyhow!(fb_message(&other)),
        })?;
    let mut conn = Conn {
        client,
        db,
        manual: None,
        read_only: target.read_only,
        legacy_dates: true,
        lost: false,
    };
    let version = conn
        .query("SELECT rdb$get_context('SYSTEM', 'ENGINE_VERSION') FROM rdb$database")?
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next().flatten())
        .map(|v| v.trim().to_string());
    if version.as_deref().and_then(major_version).unwrap_or(0) >= 4 {
        conn.bind_types()?;
    }
    Ok((conn, version))
}

/// Major number of an `ENGINE_VERSION` like `5.0.1`.
fn major_version(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse().ok()
}

/// What to do with a COMMIT / ROLLBACK / SET TRANSACTION typed in a statement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    /// Autocommit: every statement commits by itself, so they have nothing to do.
    Auto,
    /// Manual transaction: they end it and a new one starts.
    Manual,
    /// Inside `run_transaction` / `apply_changes`: not allowed.
    Batch,
}

struct Conn {
    client: RustFbClient,
    db: DbHandle,
    /// The open Manual transaction, if any.
    manual: Option<TrHandle>,
    read_only: bool,
    /// DATE values come as timestamps (Firebird 3, no `SET BIND`): midnight shows as a date.
    legacy_dates: bool,
    /// The connection was lost: nothing else will work.
    lost: bool,
}

impl Conn {
    /// Turns a client error into a readable one, remembering when the connection is gone.
    fn fail(&mut self, e: FbError) -> anyhow::Error {
        if is_connection_lost(&e) {
            self.lost = true;
        }
        anyhow!(fb_message(&e))
    }

    fn check<T>(&mut self, result: Result<T, FbError>) -> anyhow::Result<T> {
        result.map_err(|e| self.fail(e))
    }

    fn begin_tx(&mut self) -> anyhow::Result<TrHandle> {
        let config = transaction_config(self.read_only);
        let tr = self.client.begin_transaction(&mut self.db, config);
        self.check(tr)
    }

    fn end_tx(&mut self, tr: &mut TrHandle, commit: bool) -> anyhow::Result<()> {
        let op = if commit { TrOp::Commit } else { TrOp::Rollback };
        let done = self.client.transaction_operation(tr, op);
        self.check(done)
    }

    fn exec_immediate(&mut self, tr: &mut TrHandle, sql: &str) -> anyhow::Result<()> {
        let done = self.client.exec_immediate(&mut self.db, tr, DIALECT, sql);
        self.check(done)
    }

    /// Firebird 4+: the types the client can't read (or reads inexactly) come as text.
    fn bind_types(&mut self) -> anyhow::Result<()> {
        let mut tr = self.begin_tx()?;
        for (bind, fallback) in BINDS {
            if self.exec_immediate(&mut tr, bind).is_err() && !fallback.is_empty() {
                let _ = self.exec_immediate(&mut tr, fallback);
            }
            if self.lost {
                anyhow::bail!("The Firebird connection became unusable");
            }
        }
        if self
            .exec_immediate(&mut tr, "SET BIND OF DATE TO VARCHAR(10)")
            .is_ok()
        {
            self.legacy_dates = false;
        }
        // The client reads a TIME as a timestamp of today: as text it stays a time
        // (`08:30:00.0000`, as isql shows it).
        let _ = self.exec_immediate(&mut tr, "SET BIND OF TIME TO VARCHAR(13)");
        self.end_tx(&mut tr, true)
    }

    /// A console statement: in the Manual transaction if there is one, otherwise in a
    /// transaction of its own that commits right away (rolls back on error).
    fn run(&mut self, sql: &str, params: &[String], max_rows: usize) -> anyhow::Result<ResultSet> {
        if let Some(mut tr) = self.manual.take() {
            let result = self.statement(&mut tr, sql, params, max_rows, Control::Manual);
            self.manual = Some(tr);
            return result;
        }
        let mut tr = self.begin_tx()?;
        match self.statement(&mut tr, sql, params, max_rows, Control::Auto) {
            Ok(rs) => {
                self.end_tx(&mut tr, true)?;
                Ok(rs)
            }
            Err(e) => {
                let _ = self.end_tx(&mut tr, false);
                Err(e)
            }
        }
    }

    /// Rows of an internal query, all of them.
    fn query(&mut self, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>> {
        Ok(self.run(sql, &[], usize::MAX)?.rows)
    }

    /// The statements in one transaction (all or nothing); with `single_row`, each must affect
    /// exactly one row. Inside a Manual transaction they run under a savepoint and stay
    /// uncommitted like the rest of it.
    fn batch(&mut self, statements: &[String], single_row: bool) -> anyhow::Result<u64> {
        if let Some(mut tr) = self.manual.take() {
            let result = self.batch_in_savepoint(&mut tr, statements, single_row);
            self.manual = Some(tr);
            return result;
        }
        let mut tr = self.begin_tx()?;
        match self.batch_in(&mut tr, statements, single_row) {
            Ok(total) => {
                self.end_tx(&mut tr, true)?;
                Ok(total)
            }
            Err(e) => {
                let _ = self.end_tx(&mut tr, false);
                Err(e)
            }
        }
    }

    fn batch_in_savepoint(
        &mut self,
        tr: &mut TrHandle,
        statements: &[String],
        single_row: bool,
    ) -> anyhow::Result<u64> {
        self.exec_immediate(tr, &format!("SAVEPOINT {SAVEPOINT}"))?;
        match self.batch_in(tr, statements, single_row) {
            Ok(total) => {
                self.exec_immediate(tr, &format!("RELEASE SAVEPOINT {SAVEPOINT} ONLY"))?;
                Ok(total)
            }
            Err(e) => {
                let _ = self.exec_immediate(tr, &format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"));
                let _ = self.exec_immediate(tr, &format!("RELEASE SAVEPOINT {SAVEPOINT} ONLY"));
                Err(e)
            }
        }
    }

    fn batch_in(
        &mut self,
        tr: &mut TrHandle,
        statements: &[String],
        single_row: bool,
    ) -> anyhow::Result<u64> {
        let mut total = 0;
        for stmt in statements {
            let sql = normalize_statement(stmt);
            if sql.is_empty() {
                continue;
            }
            let n = self
                .statement(tr, &sql, &[], 0, Control::Batch)?
                .affected
                .unwrap_or(0);
            if single_row && n != 1 {
                return Err(super::not_single_row(n, stmt));
            }
            total += n;
        }
        Ok(total)
    }

    /// Prepares and runs one statement in `tr`.
    fn statement(
        &mut self,
        tr: &mut TrHandle,
        sql: &str,
        params: &[String],
        max_rows: usize,
        control: Control,
    ) -> anyhow::Result<ResultSet> {
        let start = Instant::now();
        let mut rs = ResultSet {
            statement: sql.to_string(),
            ..Default::default()
        };
        let prepared = self
            .client
            .prepare_statement(&mut self.db, tr, DIALECT, sql);
        let (kind, mut stmt) = match prepared {
            Ok(p) => p,
            // Session statements of Firebird 4+ (SET BIND, SET TIME ZONE, ALTER SESSION…) have
            // a statement type the client doesn't know; they return nothing anyway.
            Err(FbError::Other(m)) if m.contains("StmtType") && params.is_empty() => {
                self.exec_immediate(tr, sql)?;
                rs.elapsed = start.elapsed();
                return Ok(rs);
            }
            Err(e) => return Err(self.fail(e)),
        };
        if matches!(
            kind,
            StmtType::Commit | StmtType::Rollback | StmtType::StartTrans
        ) {
            let _ = self.client.free_statement(&mut stmt, FreeStmtOp::Drop);
            self.transaction_statement(tr, kind, control)?;
            rs.elapsed = start.elapsed();
            return Ok(rs);
        }
        let outcome = self.run_prepared(tr, &mut stmt, kind, sql, params, max_rows, &mut rs);
        let _ = self.client.free_statement(&mut stmt, FreeStmtOp::Drop);
        outcome?;
        rs.elapsed = start.elapsed();
        if rs.columns.is_empty()
            && matches!(kind, StmtType::Select | StmtType::SelectForUpd)
            && control != Control::Batch
        {
            rs.columns = self.probe_columns(tr, sql, params);
        }
        Ok(rs)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_prepared(
        &mut self,
        tr: &mut TrHandle,
        stmt: &mut Stmt,
        kind: StmtType,
        sql: &str,
        params: &[String],
        max_rows: usize,
        rs: &mut ResultSet,
    ) -> anyhow::Result<()> {
        let values: Vec<SqlType> = params.iter().cloned().map(SqlType::Text).collect();
        match kind {
            StmtType::Select | StmtType::SelectForUpd => {
                let done = self.client.execute(&mut self.db, tr, stmt, values);
                self.check(done)?;
                loop {
                    let fetched = self.client.fetch(&mut self.db, tr, stmt);
                    let Some(row) = self.check(fetched)? else {
                        break;
                    };
                    if rs.columns.is_empty() {
                        rs.columns = row.iter().map(|c| c.name.to_string()).collect();
                    }
                    if rs.rows.len() >= max_rows {
                        rs.truncated = true;
                        if max_rows != 0 {
                            break;
                        }
                        continue;
                    }
                    rs.rows.push(self.cells(row));
                }
            }
            // EXECUTE PROCEDURE with output parameters and DML ... RETURNING: one row.
            StmtType::ExecProcedure if self.returns_values(tr, sql) => {
                let done = self.client.execute2(&mut self.db, tr, stmt, values);
                let row = self.check(done)?;
                rs.columns = row.iter().map(|c| c.name.to_string()).collect();
                if max_rows == 0 {
                    rs.truncated = true;
                } else {
                    rs.rows.push(self.cells(row));
                }
            }
            StmtType::Insert | StmtType::Update | StmtType::Delete => {
                let done = self.client.execute(&mut self.db, tr, stmt, values);
                rs.affected = Some(self.check(done)? as u64);
            }
            _ => {
                let done = self.client.execute(&mut self.db, tr, stmt, values);
                self.check(done)?;
            }
        }
        Ok(())
    }

    fn cells(&self, row: Vec<Column>) -> Vec<Cell> {
        row.into_iter()
            .map(|c| cell(c.value, self.legacy_dates))
            .collect()
    }

    /// COMMIT / ROLLBACK typed in the console end the Manual transaction (and a new one
    /// starts, as the console is still in Manual mode).
    fn transaction_statement(
        &mut self,
        tr: &mut TrHandle,
        kind: StmtType,
        control: Control,
    ) -> anyhow::Result<()> {
        if kind == StmtType::StartTrans {
            anyhow::bail!(
                "SET TRANSACTION is not supported here: use the console's Manual transaction mode"
            );
        }
        match control {
            Control::Auto => Ok(()),
            Control::Manual => {
                self.end_tx(tr, kind == StmtType::Commit)?;
                *tr = self.begin_tx()?;
                Ok(())
            }
            Control::Batch => {
                anyhow::bail!("COMMIT and ROLLBACK can't run inside this transaction")
            }
        }
    }

    /// Whether an "execute procedure" statement returns a row: the client can't tell (the
    /// statement's description is private), and asking for a row that doesn't come breaks the
    /// protocol, so it is worked out from the text and the procedure's parameters.
    fn returns_values(&mut self, tr: &mut TrHandle, sql: &str) -> bool {
        let words = leading_words(sql, 2);
        match words.first().map(String::as_str) {
            // A DML reported as "execute procedure" is one with RETURNING.
            Some("INSERT" | "UPDATE" | "DELETE" | "MERGE") => true,
            Some("EXECUTE") if words.get(1).map(String::as_str) == Some("PROCEDURE") => {
                let Some((package, name)) = procedure_name(sql) else {
                    return false;
                };
                let query = output_parameters_query(package.as_deref(), &name);
                self.statement(tr, &query, &[], 1, Control::Batch)
                    .ok()
                    .and_then(|rs| rs.rows.into_iter().next())
                    .and_then(|row| row.into_iter().next().flatten())
                    .and_then(|n| n.trim().parse::<u64>().ok())
                    .is_some_and(|n| n > 0)
            }
            // EXECUTE BLOCK with output parameters is reported as a SELECT.
            _ => false,
        }
    }

    /// Column names of a SELECT that returned no rows (the client only names them with the
    /// rows): the same query as a derived table joined so it gives one row of NULLs.
    fn probe_columns(&mut self, tr: &mut TrHandle, sql: &str, params: &[String]) -> Vec<String> {
        if !matches!(
            leading_words(sql, 1).first().map(String::as_str),
            Some("SELECT" | "WITH")
        ) {
            return Vec::new();
        }
        self.statement(tr, &probe_query(sql), params, 1, Control::Batch)
            .map(|rs| rs.columns)
            .unwrap_or_default()
    }
}

/// `SET BIND` statements for Firebird 4+, each with a fallback when the server refuses it.
const BINDS: [(&str, &str); 5] = [
    (
        "SET BIND OF TIMESTAMP WITH TIME ZONE TO VARCHAR(80)",
        "SET BIND OF TIMESTAMP WITH TIME ZONE TO LEGACY",
    ),
    (
        "SET BIND OF TIME WITH TIME ZONE TO VARCHAR(80)",
        "SET BIND OF TIME WITH TIME ZONE TO LEGACY",
    ),
    (
        "SET BIND OF DECFLOAT TO VARCHAR(64)",
        "SET BIND OF DECFLOAT TO LEGACY",
    ),
    // Exact decimals instead of doubles (INT128-backed ones too).
    ("SET BIND OF NUMERIC TO VARCHAR(64)", ""),
    ("SET BIND OF DECIMAL TO VARCHAR(64)", ""),
];

fn transaction_config(read_only: bool) -> TransactionConfiguration {
    TransactionConfiguration {
        data_access: if read_only {
            TrDataAccessMode::ReadOnly
        } else {
            TrDataAccessMode::ReadWrite
        },
        isolation: TrIsolationLevel::ReadCommited(TrRecordVersion::RecordVersion),
        // A statement blocked by another transaction's lock gives up after a minute: there is
        // no cancel, and an abandoned one would otherwise wait forever.
        lock_resolution: TrLockResolution::Wait(Some(60)),
    }
}

/// The text of a client error: the server's message without the client's prefixes.
fn fb_message(e: &FbError) -> String {
    match e {
        FbError::Sql { msg, .. } => msg.trim().to_string(),
        FbError::Io(io) => io.to_string(),
        FbError::Other(msg) => msg.trim().to_string(),
    }
}

/// Errors after which the attachment can't be used any more.
fn is_connection_lost(e: &FbError) -> bool {
    const CODES: [i32; 7] = [
        335544324, // invalid database handle
        335544721, // unable to complete network request
        335544726, // error reading data from the connection
        335544727, // error writing data to the connection
        335544741, // connection lost to database
        335544856, // connection shutdown
        335545055, // attachment shut down (killed)
    ];
    match e {
        FbError::Io(_) => true,
        FbError::Sql { code, .. } => CODES.contains(code),
        FbError::Other(msg) => msg.contains("Connection closed by the server"),
    }
}

/// UTF8, except that text which isn't valid UTF-8 (CHAR/VARCHAR CHARACTER SET OCTETS, such as
/// `GEN_UUID()` values) shows as hex instead of failing the whole query.
struct Utf8OrHex;

static UTF8_OR_HEX: Utf8OrHex = Utf8OrHex;

impl Encoding for Utf8OrHex {
    fn name(&self) -> &'static str {
        "utf-8"
    }

    fn raw_encoder(&self) -> Box<dyn RawEncoder> {
        encoding::all::UTF_8.raw_encoder()
    }

    fn raw_decoder(&self) -> Box<dyn RawDecoder> {
        encoding::all::UTF_8.raw_decoder()
    }

    fn encode(&self, input: &str, _trap: EncoderTrap) -> Result<Vec<u8>, Cow<'static, str>> {
        Ok(input.as_bytes().to_vec())
    }

    fn decode(&self, input: &[u8], _trap: DecoderTrap) -> Result<String, Cow<'static, str>> {
        Ok(decode_text(input))
    }
}

fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => hex_preview(bytes),
    }
}

fn connection_charset() -> Charset {
    Charset {
        on_firebird: "UTF8",
        on_rust: Some(&UTF8_OR_HEX),
    }
}

/// A value as the grid shows it.
fn cell(value: SqlType, legacy_dates: bool) -> Cell {
    Some(match value {
        SqlType::Null => return None,
        SqlType::Text(text) => text,
        SqlType::Integer(n) => n.to_string(),
        SqlType::Int128(n) => n.to_string(),
        SqlType::Floating(f) => format_float(f),
        SqlType::Timestamp(ts) => format_timestamp(ts, legacy_dates),
        SqlType::Binary(bytes) => hex_preview(&bytes),
        SqlType::Boolean(b) => b.to_string(),
    })
}

/// Doubles in their shortest exact form. FLOAT columns come widened to double, so a value that
/// is exactly a single-precision float is written as one (`3.14`, not `3.140000104904175`).
fn format_float(f: f64) -> String {
    let single = f as f32;
    if f.is_finite() && f64::from(single) == f {
        single.to_string()
    } else {
        f.to_string()
    }
}

/// Day zero of Firebird dates: TIME values come as timestamps on it.
fn time_epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1858, 11, 17).expect("valid date")
}

/// `2024-01-31 13:45:00[.1234]`; a TIME as just its time and, with `legacy_dates` (DATE values
/// come as timestamps), a midnight as just its date.
fn format_timestamp(ts: NaiveDateTime, legacy_dates: bool) -> String {
    let time = format_time(ts.time());
    if ts.date() == time_epoch() {
        return time;
    }
    let date = ts.date().format("%Y-%m-%d").to_string();
    if legacy_dates && ts.time() == NaiveTime::MIN {
        return date;
    }
    format!("{date} {time}")
}

/// `13:45:00`, with the fraction (Firebird keeps tenths of a millisecond) when there is one.
fn format_time(time: NaiveTime) -> String {
    let mut text = time.format("%H:%M:%S").to_string();
    let fraction = time.nanosecond() % 1_000_000_000 / 100_000;
    if fraction != 0 {
        let digits = format!("{fraction:04}");
        text.push('.');
        text.push_str(digits.trim_end_matches('0'));
    }
    text
}

/// Drops the trailing `;` (and whitespace) the console leaves on a statement; Firebird refuses
/// it, and a PSQL block ends with its last `END`.
fn normalize_statement(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string()
}

/// The first `n` keywords of a statement, upper-case, skipping whitespace and comments.
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
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '$')))
            .map_or(rest.len(), |(i, _)| i);
        if len == 0 {
            break;
        }
        words.push(rest[..len].to_ascii_uppercase());
        rest = &rest[len..];
    }
    words
}

/// Package (if any) and name of the procedure of `EXECUTE PROCEDURE [pkg.]name …`, as stored
/// in the catalog (unquoted names upper-case).
fn procedure_name(sql: &str) -> Option<(Option<String>, String)> {
    // Past the two keywords (and any comments before them).
    let mut rest = sql;
    for _ in 0..2 {
        loop {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix("--") {
                rest = r.split_once('\n').map_or("", |(_, after)| after);
            } else if let Some(r) = rest.strip_prefix("/*") {
                rest = r.split_once("*/").map_or("", |(_, after)| after);
            } else {
                break;
            }
        }
        let len = rest
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(rest.len(), |(i, _)| i);
        rest = &rest[len..];
    }
    let (first, rest) = identifier(rest.trim_start())?;
    match rest.trim_start().strip_prefix('.') {
        Some(after) => {
            let (second, _) = identifier(after.trim_start())?;
            Some((Some(first), second))
        }
        None => Some((None, first)),
    }
}

/// A leading identifier as stored in the catalog, and what follows it.
fn identifier(text: &str) -> Option<(String, &str)> {
    if let Some(quoted) = text.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if quoted[i + 1..].starts_with('"') {
                    name.push('"');
                    chars.next();
                    continue;
                }
                return Some((name, &quoted[i + 1..]));
            }
            name.push(c);
        }
        return None;
    }
    let len = text
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '$')))
        .map_or(text.len(), |(i, _)| i);
    (len > 0).then(|| (text[..len].to_uppercase(), &text[len..]))
}

fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Counts the output parameters of a procedure.
fn output_parameters_query(package: Option<&str>, name: &str) -> String {
    let package = match package {
        Some(p) => format!("= {}", quote_literal(p)),
        None => "IS NULL".to_string(),
    };
    format!(
        "SELECT COUNT(*) FROM RDB$PROCEDURE_PARAMETERS \
         WHERE RDB$PROCEDURE_NAME = {} AND RDB$PARAMETER_TYPE = 1 AND RDB$PACKAGE_NAME {package}",
        quote_literal(name)
    )
}

/// A query that returns the columns of `sql` with exactly one row of NULLs. The newline keeps
/// a trailing `--` comment from swallowing the closing parenthesis.
fn probe_query(sql: &str) -> String {
    format!("SELECT q.* FROM RDB$DATABASE LEFT JOIN (\n{sql}\n) q ON 1 = 0")
}

/// User tables and views with their columns, in order.
const COLUMNS_QUERY: &str = "SELECT TRIM(r.RDB$RELATION_NAME), \
       CASE WHEN r.RDB$VIEW_BLR IS NULL THEN 0 ELSE 1 END, \
       TRIM(rf.RDB$FIELD_NAME), f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH, \
       f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE, f.RDB$CHARACTER_SET_ID, \
       COALESCE(rf.RDB$NULL_FLAG, f.RDB$NULL_FLAG, 0) \
     FROM RDB$RELATIONS r \
     LEFT JOIN RDB$RELATION_FIELDS rf ON rf.RDB$RELATION_NAME = r.RDB$RELATION_NAME \
     LEFT JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE \
     WHERE COALESCE(r.RDB$SYSTEM_FLAG, 0) = 0 \
     ORDER BY r.RDB$RELATION_NAME, rf.RDB$FIELD_POSITION";

/// Columns of each primary key.
const PRIMARY_KEYS_QUERY: &str = "SELECT TRIM(rc.RDB$RELATION_NAME), TRIM(s.RDB$FIELD_NAME) \
     FROM RDB$RELATION_CONSTRAINTS rc \
     JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME \
     WHERE rc.RDB$CONSTRAINT_TYPE = 'PRIMARY KEY'";

fn load_schema(c: &mut Conn, name: String) -> anyhow::Result<Vec<SchemaInfo>> {
    let primary_keys: std::collections::HashSet<(String, String)> = c
        .query(PRIMARY_KEYS_QUERY)?
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            Some((it.next()??, it.next()??))
        })
        .collect();
    let mut tables: Vec<TableInfo> = Vec::new();
    for row in c.query(COLUMNS_QUERY)? {
        let text = |i: usize| row.get(i).cloned().flatten();
        let number = |i: usize| text(i).and_then(|v| v.trim().parse::<i64>().ok());
        let Some(table) = text(0) else { continue };
        if tables.last().is_none_or(|t| t.name != table) {
            let kind = if number(1) == Some(1) {
                ObjectKind::View
            } else {
                ObjectKind::Table
            };
            tables.push(TableInfo {
                name: table.clone(),
                kind,
                columns: Vec::new(),
            });
        }
        let Some(column) = text(2) else { continue };
        let field = FieldType {
            field_type: number(3).unwrap_or(0),
            sub_type: number(4).unwrap_or(0),
            length: number(5).unwrap_or(0),
            char_length: number(6),
            precision: number(7).unwrap_or(0),
            scale: number(8).unwrap_or(0),
            charset_id: number(9),
        };
        let primary_key = primary_keys.contains(&(table.clone(), column.clone()));
        if let Some(t) = tables.last_mut() {
            t.columns.push(ColumnInfo {
                name: column,
                data_type: field.name(),
                nullable: number(10).unwrap_or(0) == 0,
                primary_key,
            });
        }
    }
    Ok(vec![SchemaInfo { name, tables }])
}

/// A column's type as stored in `RDB$FIELDS`.
#[derive(Debug, Default, Clone, Copy)]
struct FieldType {
    field_type: i64,
    sub_type: i64,
    /// In bytes.
    length: i64,
    /// In characters (CHAR / VARCHAR).
    char_length: Option<i64>,
    precision: i64,
    /// Negative for decimals.
    scale: i64,
    charset_id: Option<i64>,
}

impl FieldType {
    /// Readable type, as in DDL: `VARCHAR(50)`, `NUMERIC(10,2)`, `BLOB SUB_TYPE TEXT`…
    fn name(&self) -> String {
        const OCTETS: i64 = 1;
        let exact = |plain: &str, max: i64| {
            if self.scale < 0 || self.sub_type == 1 || self.sub_type == 2 {
                let kind = if self.sub_type == 2 {
                    "DECIMAL"
                } else {
                    "NUMERIC"
                };
                let precision = if self.precision > 0 {
                    self.precision
                } else {
                    max
                };
                format!("{kind}({precision},{})", -self.scale)
            } else {
                plain.to_string()
            }
        };
        let text = |kind: &str| {
            let length = self.char_length.unwrap_or(self.length);
            if self.charset_id == Some(OCTETS) {
                format!("{kind}({length}) CHARACTER SET OCTETS")
            } else {
                format!("{kind}({length})")
            }
        };
        match self.field_type {
            7 => exact("SMALLINT", 4),
            8 => exact("INTEGER", 9),
            16 => exact("BIGINT", 18),
            26 => exact("INT128", 38),
            10 => "FLOAT".into(),
            27 => "DOUBLE PRECISION".into(),
            11 => "D_FLOAT".into(),
            24 => "DECFLOAT(16)".into(),
            25 => "DECFLOAT(34)".into(),
            12 => "DATE".into(),
            13 => "TIME".into(),
            35 => "TIMESTAMP".into(),
            28 => "TIME WITH TIME ZONE".into(),
            29 => "TIMESTAMP WITH TIME ZONE".into(),
            14 => text("CHAR"),
            37 => text("VARCHAR"),
            40 => text("CSTRING"),
            23 => "BOOLEAN".into(),
            261 => match self.sub_type {
                0 => "BLOB SUB_TYPE BINARY".into(),
                1 => "BLOB SUB_TYPE TEXT".into(),
                n => format!("BLOB SUB_TYPE {n}"),
            },
            9 => "QUAD".into(),
            45 => "BLOB_ID".into(),
            other => format!("TYPE {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Pure helpers, plus tests against a real Firebird that run only with the variables
    //! DATACRAFT_FB_HOST (required; skipped without it), DATACRAFT_FB_PORT (3050),
    //! DATACRAFT_FB_DATABASE (employee), DATACRAFT_FB_USER (SYSDBA), DATACRAFT_FB_PASSWORD.

    use super::*;
    use crate::db::Driver;

    fn field(field_type: i64) -> FieldType {
        FieldType {
            field_type,
            ..Default::default()
        }
    }

    #[test]
    fn readable_type_names() {
        assert_eq!(field(8).name(), "INTEGER");
        assert_eq!(field(7).name(), "SMALLINT");
        assert_eq!(field(16).name(), "BIGINT");
        assert_eq!(field(35).name(), "TIMESTAMP");
        assert_eq!(field(12).name(), "DATE");
        assert_eq!(field(29).name(), "TIMESTAMP WITH TIME ZONE");
        assert_eq!(field(23).name(), "BOOLEAN");
        assert_eq!(field(27).name(), "DOUBLE PRECISION");
        let numeric = FieldType {
            field_type: 8,
            sub_type: 1,
            precision: 9,
            scale: -2,
            ..Default::default()
        };
        assert_eq!(numeric.name(), "NUMERIC(9,2)");
        let decimal = FieldType {
            field_type: 16,
            sub_type: 2,
            precision: 18,
            scale: -4,
            ..Default::default()
        };
        assert_eq!(decimal.name(), "DECIMAL(18,4)");
        // Dialect 1 decimals have no precision stored.
        let legacy = FieldType {
            field_type: 16,
            scale: -2,
            ..Default::default()
        };
        assert_eq!(legacy.name(), "NUMERIC(18,2)");
        let numeric0 = FieldType {
            field_type: 16,
            sub_type: 1,
            precision: 10,
            ..Default::default()
        };
        assert_eq!(numeric0.name(), "NUMERIC(10,0)");
        let varchar = FieldType {
            field_type: 37,
            length: 200,
            char_length: Some(50),
            charset_id: Some(4),
            ..Default::default()
        };
        assert_eq!(varchar.name(), "VARCHAR(50)");
        let uuid = FieldType {
            field_type: 14,
            length: 16,
            char_length: Some(16),
            charset_id: Some(1),
            ..Default::default()
        };
        assert_eq!(uuid.name(), "CHAR(16) CHARACTER SET OCTETS");
        let text = FieldType {
            field_type: 261,
            sub_type: 1,
            ..Default::default()
        };
        assert_eq!(text.name(), "BLOB SUB_TYPE TEXT");
        let binary = FieldType {
            field_type: 261,
            ..Default::default()
        };
        assert_eq!(binary.name(), "BLOB SUB_TYPE BINARY");
    }

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, micro: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_micro_opt(h, mi, s, micro)
            .unwrap()
    }

    #[test]
    fn formats_values() {
        assert_eq!(cell(SqlType::Null, true), None);
        assert_eq!(cell(SqlType::Integer(-42), true).as_deref(), Some("-42"));
        assert_eq!(
            cell(
                SqlType::Int128(170141183460469231731687303715884105727),
                true
            )
            .as_deref(),
            Some("170141183460469231731687303715884105727")
        );
        assert_eq!(cell(SqlType::Boolean(true), true).as_deref(), Some("true"));
        assert_eq!(
            cell(SqlType::Boolean(false), true).as_deref(),
            Some("false")
        );
        assert_eq!(
            cell(SqlType::Binary(vec![0xCA, 0xFE]), true).as_deref(),
            Some("0xCAFE")
        );
        assert_eq!(
            cell(SqlType::Text("héllo".into()), true).as_deref(),
            Some("héllo")
        );
        assert_eq!(format_float(12.5), "12.5");
        assert_eq!(format_float(0.1), "0.1");
        assert_eq!(format_float(f64::from(2.71f32)), "2.71");
        assert_eq!(format_float(100.0), "100");
        assert_eq!(format_float(1234567.891), "1234567.891");
    }

    #[test]
    fn formats_dates_and_times() {
        let t = ts(2024, 1, 31, 13, 45, 0, 0);
        assert_eq!(format_timestamp(t, false), "2024-01-31 13:45:00");
        assert_eq!(
            format_timestamp(ts(2024, 1, 31, 13, 45, 7, 120_000), false),
            "2024-01-31 13:45:07.12"
        );
        assert_eq!(
            format_timestamp(ts(2024, 1, 31, 13, 45, 7, 100), false),
            "2024-01-31 13:45:07.0001"
        );
        // TIME values arrive on day zero.
        assert_eq!(
            format_timestamp(ts(1858, 11, 17, 8, 30, 0, 0), false),
            "08:30:00"
        );
        // Firebird 3: DATE values arrive as midnight timestamps.
        assert_eq!(
            format_timestamp(ts(2024, 1, 31, 0, 0, 0, 0), true),
            "2024-01-31"
        );
        assert_eq!(
            format_timestamp(ts(2024, 1, 31, 0, 0, 0, 0), false),
            "2024-01-31 00:00:00"
        );
    }

    #[test]
    fn octets_text_shows_as_hex() {
        assert_eq!(decode_text("añb".as_bytes()), "añb");
        assert_eq!(decode_text(&[0xFF, 0x00, 0x10]), "0xFF0010");
        let charset = connection_charset();
        assert_eq!(charset.decode(&[0xC3u8, 0x28][..]).unwrap(), "0xC328");
        assert_eq!(charset.decode("ok".as_bytes()).unwrap(), "ok");
        assert_eq!(charset.encode("ñ").unwrap().as_ref(), "ñ".as_bytes());
    }

    #[test]
    fn normalizes_statements() {
        assert_eq!(
            normalize_statement("  SELECT 1 FROM RDB$DATABASE ;\n"),
            "SELECT 1 FROM RDB$DATABASE"
        );
        assert_eq!(
            normalize_statement("EXECUTE BLOCK AS BEGIN END;"),
            "EXECUTE BLOCK AS BEGIN END"
        );
        assert_eq!(
            normalize_statement("SELECT ';' FROM t"),
            "SELECT ';' FROM t"
        );
    }

    #[test]
    fn reads_leading_words_and_procedure_names() {
        assert_eq!(
            leading_words("/* c */ -- x\n execute  procedure p", 2),
            ["EXECUTE", "PROCEDURE"]
        );
        assert_eq!(
            procedure_name("EXECUTE PROCEDURE get_total(1, 2)"),
            Some((None, "GET_TOTAL".into()))
        );
        assert_eq!(
            procedure_name("execute procedure \"My Proc\""),
            Some((None, "My Proc".into()))
        );
        assert_eq!(
            procedure_name("EXECUTE PROCEDURE pkg.calc 1"),
            Some((Some("PKG".into()), "CALC".into()))
        );
        assert_eq!(
            output_parameters_query(None, "O'NEIL"),
            "SELECT COUNT(*) FROM RDB$PROCEDURE_PARAMETERS WHERE RDB$PROCEDURE_NAME = 'O''NEIL' \
             AND RDB$PARAMETER_TYPE = 1 AND RDB$PACKAGE_NAME IS NULL"
        );
        assert!(output_parameters_query(Some("PKG"), "P").ends_with("RDB$PACKAGE_NAME = 'PKG'"));
        assert_eq!(
            probe_query("SELECT a FROM t -- note"),
            "SELECT q.* FROM RDB$DATABASE LEFT JOIN (\nSELECT a FROM t -- note\n) q ON 1 = 0"
        );
    }

    #[test]
    fn resolves_defaults() {
        let mut cfg = ConnectionConfig::new(1, Driver::Firebird);
        cfg.host = String::new();
        cfg.port = 0;
        cfg.database = "  /data/test.fdb ".into();
        cfg.user = String::new();
        let target = Target::from_config(&cfg);
        assert_eq!(target.host, "localhost");
        assert_eq!(target.port, 3050);
        assert_eq!(target.database, "/data/test.fdb");
        assert_eq!(target.user, "SYSDBA");
        assert_eq!(major_version("5.0.1"), Some(5));
        assert_eq!(major_version("3.0.12"), Some(3));
        assert_eq!(major_version("x"), None);
    }

    // ---- Live tests (DATACRAFT_FB_HOST) ----

    async fn session() -> Option<FbSession> {
        let Ok(host) = std::env::var("DATACRAFT_FB_HOST") else {
            eprintln!("DATACRAFT_FB_HOST not set: skipping the Firebird test");
            return None;
        };
        let env =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        let mut cfg = ConnectionConfig::new(1, Driver::Firebird);
        cfg.host = host;
        cfg.port = env("DATACRAFT_FB_PORT", "3050").parse().expect("port");
        cfg.database = env("DATACRAFT_FB_DATABASE", "employee");
        cfg.user = env("DATACRAFT_FB_USER", "SYSDBA");
        cfg.password = env("DATACRAFT_FB_PASSWORD", "");
        Some(FbSession::connect(&cfg).await.expect("connection"))
    }

    async fn one(s: &mut FbSession, sql: &str, max_rows: usize) -> ResultSet {
        let mut rs = s
            .execute(sql, max_rows)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(rs.len(), 1);
        rs.remove(0)
    }

    fn texts(row: &[Cell]) -> Vec<Option<&str>> {
        row.iter().map(|c| c.as_deref()).collect()
    }

    /// A fresh table name per test, so tests can run in parallel.
    async fn fresh_table(s: &mut FbSession, name: &str) {
        let _ = s.execute(&format!("DROP VIEW {name}_V"), 0).await;
        let _ = s.execute(&format!("DROP TABLE {name}"), 0).await;
    }

    #[tokio::test]
    async fn live_connects_and_reads_types() {
        let Some(mut s) = session().await else { return };
        let version = s.server_version().await.expect("version");
        assert!(version.starts_with("Firebird "), "{version}");
        let db = s.current_database().await.unwrap();
        assert_eq!(s.list_databases().await.unwrap(), [db]);

        let rs = one(&mut s, "SELECT 1 AS ONE FROM RDB$DATABASE;", 10).await;
        assert_eq!(rs.columns, ["ONE"]);
        assert_eq!(texts(&rs.rows[0]), [Some("1")]);

        let rs = one(
            &mut s,
            "SELECT CAST(12.50 AS NUMERIC(10,2)) AS N, CAST('2024-01-31' AS DATE) AS D, \
             CAST('2024-01-31 13:45:00' AS TIMESTAMP) AS TS, CAST('08:30:00' AS TIME) AS T, \
             TRUE AS B, CAST(NULL AS INTEGER) AS Z, 'héllo' AS S, x'CAFE' AS O \
             FROM RDB$DATABASE",
            10,
        )
        .await;
        let row = texts(&rs.rows[0]);
        assert!(matches!(row[0], Some("12.50" | "12.5")), "{row:?}");
        assert_eq!(row[1], Some("2024-01-31"));
        assert_eq!(row[2], Some("2024-01-31 13:45:00"));
        assert!(
            matches!(row[3], Some("08:30:00" | "08:30:00.0000")),
            "{row:?}"
        );
        assert_eq!(row[4], Some("true"));
        assert_eq!(row[5], None);
        assert_eq!(row[6], Some("héllo"));
        assert_eq!(row[7], Some("0xCAFE"));

        // Firebird 4+ types the client can't read natively.
        if version.as_str() >= "Firebird 4" {
            let rs = one(
                &mut s,
                "SELECT CURRENT_TIMESTAMP AS NOW, CAST(1.5 AS DECFLOAT(16)) AS DF, \
                 CAST(123456789012345678901234.56 AS NUMERIC(38,2)) AS BIG FROM RDB$DATABASE",
                10,
            )
            .await;
            let row = texts(&rs.rows[0]);
            assert!(row[0].is_some());
            assert_eq!(row[1], Some("1.5"));
            assert_eq!(row[2], Some("123456789012345678901234.56"));
        }

        // An empty result keeps its columns; max_rows truncates.
        let rs = one(
            &mut s,
            "SELECT RDB$RELATION_NAME AS NAME FROM RDB$RELATIONS WHERE 1 = 0",
            10,
        )
        .await;
        assert_eq!(rs.columns, ["NAME"]);
        assert!(rs.rows.is_empty());
        let rs = one(&mut s, "SELECT RDB$RELATION_ID FROM RDB$RELATIONS", 2).await;
        assert_eq!(rs.rows.len(), 2);
        assert!(rs.truncated);
        let rs = one(&mut s, "SELECT RDB$RELATION_ID FROM RDB$RELATIONS", 0).await;
        assert!(rs.rows.is_empty());
        assert!(rs.truncated);

        let rs = s
            .execute_bound(
                "SELECT CAST(? AS INTEGER) + 1 AS N FROM RDB$DATABASE",
                &["41".into()],
                10,
            )
            .await
            .unwrap();
        assert_eq!(texts(&rs[0].rows[0]), [Some("42")]);

        let rs = one(
            &mut s,
            "EXECUTE BLOCK RETURNS (N INTEGER) AS BEGIN N = 7; SUSPEND; END",
            10,
        )
        .await;
        assert_eq!(texts(&rs.rows[0]), [Some("7")]);
        one(
            &mut s,
            "EXECUTE BLOCK AS DECLARE X INTEGER; BEGIN X = 1; END",
            10,
        )
        .await;
        assert!(!s.is_closed());
    }

    #[tokio::test]
    async fn live_tables_transactions_and_schema() {
        let Some(mut s) = session().await else { return };
        let table = "DATACRAFT_FB_TEST";
        fresh_table(&mut s, table).await;
        one(
            &mut s,
            &format!(
                "CREATE TABLE {table} (ID INTEGER NOT NULL PRIMARY KEY, NAME VARCHAR(50), \
                 PRICE NUMERIC(10,2), CREATED TIMESTAMP, DUE DATE, NOTES BLOB SUB_TYPE TEXT, \
                 DATA BLOB SUB_TYPE BINARY)"
            ),
            0,
        )
        .await;
        one(
            &mut s,
            &format!("CREATE VIEW {table}_V AS SELECT ID, NAME FROM {table}"),
            0,
        )
        .await;
        let rs = one(
            &mut s,
            &format!(
                "INSERT INTO {table} VALUES (1, 'one', 12.30, '2024-01-31 13:45:00', \
                 '2024-01-31', 'long text', x'0102')"
            ),
            0,
        )
        .await;
        assert_eq!(rs.affected, Some(1));
        one(&mut s, &format!("INSERT INTO {table} (ID) VALUES (2)"), 0).await;
        let rs = one(&mut s, &format!("SELECT * FROM {table} ORDER BY ID"), 10).await;
        assert_eq!(
            rs.columns,
            ["ID", "NAME", "PRICE", "CREATED", "DUE", "NOTES", "DATA"]
        );
        let first = texts(&rs.rows[0]);
        assert_eq!(first[1], Some("one"));
        assert!(matches!(first[2], Some("12.30" | "12.3")), "{first:?}");
        assert_eq!(first[3], Some("2024-01-31 13:45:00"));
        assert_eq!(first[4], Some("2024-01-31"));
        assert_eq!(first[5], Some("long text"));
        assert_eq!(first[6], Some("0x0102"));
        assert_eq!(texts(&rs.rows[1])[1..], [None; 6]);

        // INSERT … RETURNING gives its row.
        let rs = one(
            &mut s,
            &format!("INSERT INTO {table} (ID, NAME) VALUES (3, 'three') RETURNING ID, NAME"),
            10,
        )
        .await;
        assert_eq!(texts(&rs.rows[0]), [Some("3"), Some("three")]);

        // Manual transaction: rollback, then commit.
        s.begin().await.unwrap();
        one(&mut s, &format!("DELETE FROM {table} WHERE ID = 3"), 0).await;
        s.end_transaction(false).await.unwrap();
        let count = format!("SELECT COUNT(*) FROM {table}");
        assert_eq!(texts(&one(&mut s, &count, 1).await.rows[0]), [Some("3")]);
        s.begin().await.unwrap();
        one(&mut s, &format!("DELETE FROM {table} WHERE ID = 3"), 0).await;
        s.end_transaction(true).await.unwrap();
        assert_eq!(texts(&one(&mut s, &count, 1).await.rows[0]), [Some("2")]);

        // All or nothing.
        let err = s
            .run_transaction(&[
                format!("INSERT INTO {table} (ID) VALUES (10)"),
                format!("INSERT INTO {table} (ID) VALUES (1)"),
            ])
            .await;
        assert!(err.is_err());
        assert_eq!(texts(&one(&mut s, &count, 1).await.rows[0]), [Some("2")]);
        let n = s
            .run_transaction(&[
                format!("INSERT INTO {table} (ID) VALUES (10)"),
                format!("UPDATE {table} SET NAME = 'x'"),
            ])
            .await
            .unwrap();
        assert_eq!(n, 4);

        // apply_changes: one row per statement or nothing.
        let err = s
            .apply_changes(&[format!("UPDATE {table} SET NAME = 'y'")])
            .await;
        assert!(err.is_err());
        let n = s
            .apply_changes(&[format!("UPDATE {table} SET NAME = 'z' WHERE ID = 1")])
            .await
            .unwrap();
        assert_eq!(n, 1);
        let rs = one(
            &mut s,
            &format!("SELECT COUNT(*) FROM {table} WHERE NAME = 'y'"),
            1,
        )
        .await;
        assert_eq!(texts(&rs.rows[0]), [Some("0")]);

        let schemas = s.introspect().await.unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, s.current_database().await.unwrap());
        let t = schemas[0]
            .tables
            .iter()
            .find(|t| t.name == table)
            .expect("table");
        assert_eq!(t.kind, ObjectKind::Table);
        let cols: Vec<(&str, &str, bool, bool)> = t
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.data_type.as_str(),
                    c.nullable,
                    c.primary_key,
                )
            })
            .collect();
        assert_eq!(
            cols,
            [
                ("ID", "INTEGER", false, true),
                ("NAME", "VARCHAR(50)", true, false),
                ("PRICE", "NUMERIC(10,2)", true, false),
                ("CREATED", "TIMESTAMP", true, false),
                ("DUE", "DATE", true, false),
                ("NOTES", "BLOB SUB_TYPE TEXT", true, false),
                ("DATA", "BLOB SUB_TYPE BINARY", true, false),
            ]
        );
        let v = schemas[0]
            .tables
            .iter()
            .find(|t| t.name == format!("{table}_V"))
            .expect("view");
        assert_eq!(v.kind, ObjectKind::View);
        assert_eq!(v.columns.len(), 2);

        fresh_table(&mut s, table).await;
    }
}
