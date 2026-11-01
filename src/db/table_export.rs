//! Complete-table exports on private sessions, independent of the results grid.
use super::{ColumnInfo, ConnectionConfig, Driver, Engine, Session};
use crate::i18n::tr;
use anyhow::{Context, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportKind {
    Csv,
    Tsv,
    Json,
    Sql { structure: bool, data: bool },
    Print { data: bool },
}

#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub schema: String,
    pub table: String,
    pub path: PathBuf,
    pub kind: ExportKind,
    pub overwrite: bool,
}

#[derive(Default)]
pub struct ExportProgress {
    pub rows: AtomicU64,
    pub cancel: AtomicBool,
}

pub fn supported(driver: Driver) -> bool {
    matches!(driver, Driver::Sqlite | Driver::MsSql | Driver::Postgres | Driver::MySql | Driver::MariaDb)
}

pub fn supported_kind(driver: Driver, kind: ExportKind) -> bool {
    supported(driver)
        || (kind == ExportKind::Print { data: false } && !driver.is_nosql())
        || (kind == ExportKind::Sql { structure: true, data: false } && super::table_definition::supported(driver))
}

pub(super) fn cancelled(progress: &ExportProgress) -> anyhow::Result<()> {
    ensure!(!progress.cancel.load(Ordering::Relaxed), tr("Cancelled"));
    Ok(())
}

async fn cancellable<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    progress: &ExportProgress,
    handle: Option<super::CancelHandle>,
) -> anyhow::Result<T> {
    cancelled(progress)?;
    tokio::pin!(future);
    let mut timer = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = timer.tick() => if progress.cancel.load(Ordering::Relaxed) {
                if let Some(handle) = &handle {
                    let _ = tokio::time::timeout(Duration::from_secs(1), handle.cancel()).await;
                    // Blocking SQLite reads must release their connection before fixture/file cleanup.
                    let _ = tokio::time::timeout(Duration::from_secs(1), &mut future).await;
                }
                bail!(tr("Cancelled"));
            }
        }
    }
}

/// A row owns its native values; binary and text never pass through grid previews.
#[derive(Debug)]
pub(super) enum Value {
    Null,
    Text(Vec<u8>),
    Number(String),
    Bool(bool),
    Binary(Vec<u8>),
}

pub(super) enum Message {
    Row(Vec<Value>),
    Finished,
}

pub async fn export(mut cfg: ConnectionConfig, db: &str, request: ExportRequest, progress: Arc<ExportProgress>) -> anyhow::Result<u64> {
    cancelled(&progress)?;
    ensure!(supported_kind(cfg.driver, request.kind), tr("Complete table export is not available for this engine"));
    ensure!(!request.table.is_empty(), tr("Choose a table"));
    ensure!(request.kind != ExportKind::Sql { structure: false, data: false }, tr("Choose structure or data"));
    if cfg.driver == Driver::Sqlite {
        ensure!(request.schema.is_empty() || request.schema == "main", tr("Only the main SQLite database can be exported"));
        // Never replace the source database with an export, even through a symlink.
        if request.path.exists() {
            ensure!(fs::canonicalize(&request.path)? != fs::canonicalize(&cfg.file_path)?, tr("The export destination is the source database"));
        }
    } else if cfg.driver != Driver::Oracle && !db.is_empty() {
        cfg.database = db.to_owned();
    }
    ensure!(request.overwrite || !request.path.exists(), tr("The export destination already exists"));
    let mut session = cancellable(Session::connect(&cfg), &progress, None).await?;
    let handle = session.cancel_handle();
    cancellable(session.use_database(db), &progress, handle).await?;
    export_session(&mut session, cfg.driver, request, progress).await
}

async fn columns(session: &mut Session, driver: Driver, schema: &str, table: &str) -> anyhow::Result<Vec<ColumnInfo>> {
    if driver == Driver::Sqlite {
        let quoted = crate::edit::literal(driver, &Some(table.into()));
        let result = session
            .execute(&format!("SELECT name, type, \"notnull\", pk FROM pragma_table_xinfo({quoted}) WHERE hidden <> 1 ORDER BY cid"), usize::MAX)
            .await?;
        return result
            .into_iter()
            .flat_map(|r| r.rows)
            .map(|r| {
                let get = |i| r.get(i).and_then(Option::as_deref).context(tr("Incomplete SQLite column metadata"));
                Ok(ColumnInfo { name: get(0)?.into(), data_type: get(1)?.into(), nullable: get(2)? == "0" && get(3)? == "0", primary_key: get(3)? != "0" })
            })
            .collect();
    }
    session
        .introspect()
        .await?
        .into_iter()
        .find(|s| s.name == schema)
        .and_then(|s| s.tables.into_iter().find(|t| t.name == table))
        .map(|t| t.columns)
        .context(tr("The table was not found"))
}

async fn insertable_columns(session: &mut Session, driver: Driver, schema: &str, table: &str) -> anyhow::Result<Vec<String>> {
    let quoted = |name: &str| sql_literal(driver, &Value::Text(name.as_bytes().to_vec()));
    let name = quoted(&Session::quote_table(driver, schema, table))?;
    let schema = quoted(schema)?;
    let table_literal = quoted(table)?;
    let sql = match driver {
        Driver::Sqlite => format!("SELECT name FROM pragma_table_xinfo({table_literal},{schema}) WHERE hidden=0 ORDER BY cid"),
        Driver::MsSql => format!(
            "SELECT name FROM sys.columns WHERE object_id=OBJECT_ID({name}) AND is_computed=0 AND generated_always_type=0 AND system_type_id<>189 AND is_hidden=0 ORDER BY column_id"
        ),
        Driver::Postgres => {
            format!("SELECT attname FROM pg_attribute WHERE attrelid=to_regclass({name}) AND attnum>0 AND NOT attisdropped AND attgenerated='' ORDER BY attnum")
        }
        Driver::MySql | Driver::MariaDb => format!(
            "SELECT column_name FROM information_schema.columns WHERE table_schema={schema} AND table_name={table_literal} AND COALESCE(generation_expression,'')='' ORDER BY ordinal_position"
        ),
        _ => bail!(tr("Complete table export is not available for this engine")),
    };
    session
        .execute(&sql, usize::MAX)
        .await?
        .into_iter()
        .flat_map(|r| r.rows)
        .map(|row| row.into_iter().next().flatten().context(tr("Missing export column metadata")))
        .collect()
}

async fn export_session(session: &mut Session, driver: Driver, request: ExportRequest, progress: Arc<ExportProgress>) -> anyhow::Result<u64> {
    if !supported(driver) && matches!(request.kind, ExportKind::Print { data: false } | ExportKind::Sql { structure: true, data: false }) {
        return export_transaction(session, driver, request, progress).await;
    }
    // SQL Server falls back to READ COMMITTED when snapshot isolation is disabled.
    let begin = snapshot_sql(driver)?;
    let handle = session.cancel_handle();
    cancellable(session.execute(begin, 0), &progress, handle).await?;
    let result = export_transaction(session, driver, request, progress).await;
    // No source changes were made. Dropping a failed connection also releases its snapshot.
    if result.is_ok() {
        let _ = session.execute("ROLLBACK", 0).await;
    }
    result
}

fn snapshot_sql(driver: Driver) -> anyhow::Result<&'static str> {
    Ok(match driver {
        Driver::Sqlite => "BEGIN",
        Driver::Postgres => {
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL bytea_output='hex'; SET LOCAL DateStyle='ISO, YMD'; SET LOCAL extra_float_digits=3"
        }
        Driver::MsSql => {
            "IF EXISTS (SELECT 1 FROM sys.databases WHERE database_id=DB_ID() AND snapshot_isolation_state=1) BEGIN SET TRANSACTION ISOLATION LEVEL SNAPSHOT; END ELSE BEGIN SET TRANSACTION ISOLATION LEVEL READ COMMITTED; END; BEGIN TRANSACTION"
        }
        Driver::MySql | Driver::MariaDb => {
            "SET SESSION time_zone='+00:00'; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"
        }
        _ => bail!(tr("Complete table export is not available for this engine")),
    })
}

fn data_only_restore_guard(driver: Driver, self_referencing: bool) -> anyhow::Result<()> {
    ensure!(
        driver == Driver::Sqlite || !self_referencing,
        tr("Data-only SQL export cannot safely restore self-referencing foreign keys for this engine. Choose Structure and Data.")
    );
    Ok(())
}

fn data_only_self_reference_sql(driver: Driver, schema: &str, table: &str) -> anyhow::Result<Option<String>> {
    let quoted = |value: &str| sql_literal(driver, &Value::Text(value.as_bytes().to_vec()));
    let name = quoted(&Session::quote_table(driver, schema, table))?;
    Ok(Some(match driver {
        Driver::Sqlite => return Ok(None),
        Driver::MsSql => format!("SELECT TOP(1) 1 FROM sys.foreign_keys WHERE parent_object_id=OBJECT_ID({name}) AND referenced_object_id=parent_object_id"),
        Driver::Postgres => format!("SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass({name}) AND contype='f' AND confrelid=conrelid LIMIT 1"),
        Driver::MySql | Driver::MariaDb => format!(
            "SELECT 1 FROM information_schema.KEY_COLUMN_USAGE WHERE CAST(TABLE_SCHEMA AS BINARY)=CAST({} AS BINARY) AND CAST(TABLE_NAME AS BINARY)=CAST({} AS BINARY) AND CAST(REFERENCED_TABLE_SCHEMA AS BINARY)=CAST(TABLE_SCHEMA AS BINARY) AND CAST(REFERENCED_TABLE_NAME AS BINARY)=CAST(TABLE_NAME AS BINARY) LIMIT 1",
            quoted(schema)?,
            quoted(table)?
        ),
        _ => bail!(tr("Complete table export is not available for this engine")),
    }))
}

async fn export_transaction(session: &mut Session, driver: Driver, request: ExportRequest, progress: Arc<ExportProgress>) -> anyhow::Result<u64> {
    cancelled(&progress)?;
    let handle = session.cancel_handle();
    let metadata = async {
        let all_columns = columns(session, driver, &request.schema, &request.table).await?;
        ensure!(!all_columns.is_empty(), tr("The table has no visible columns"));
        let mut ddl = Vec::new();
        let mut selected = all_columns.clone();
        let mut before = Vec::new();
        let mut after = Vec::new();
        if let ExportKind::Sql { structure, data } = request.kind {
            let insertable = if structure {
                let definition = super::table_definition::definition(session, driver, &request.schema, &request.table).await?;
                if data {
                    (ddl, after) = super::table_definition::dump_ddl(driver, &request.schema, &request.table, &definition.ddl)?;
                } else {
                    ddl = definition.ddl;
                }
                definition.columns
            } else {
                if let Some(query) = data_only_self_reference_sql(driver, &request.schema, &request.table)? {
                    let result = session.execute(&query, 1).await?.into_iter().find(|r| !r.columns.is_empty()).context(tr("Missing export column metadata"))?;
                    data_only_restore_guard(driver, !result.rows.is_empty())?;
                }
                insertable_columns(session, driver, &request.schema, &request.table).await?
            };
            selected.retain(|c| insertable.contains(&c.name));
            if data {
                let mut options = super::table_definition::data_insert_options(session, driver, &request.schema, &request.table).await?;
                before = options.before;
                options.after.append(&mut after);
                after = options.after;
            }
            if data && driver == Driver::Sqlite {
                let sequence = session.execute("SELECT name FROM sqlite_schema WHERE name='sqlite_sequence'", 1).await?;
                if sequence.iter().any(|r| !r.rows.is_empty()) {
                    let table_literal = crate::edit::literal(driver, &Some(request.table.clone()));
                    let sequences = session.execute(&format!("SELECT seq FROM sqlite_sequence WHERE name={table_literal}"), 1).await?;
                    if let Some(Some(seq)) = sequences.first().and_then(|r| r.rows.first()).and_then(|r| r.first()) {
                        ensure!(seq.parse::<i64>().is_ok(), tr("Invalid SQLite sequence"));
                        after.push(format!(
                            "DELETE FROM sqlite_sequence WHERE name={table_literal};\nINSERT INTO sqlite_sequence(name,seq) VALUES({table_literal},{seq});"
                        ));
                    }
                }
            }
        }
        Ok((all_columns, selected, ddl, before, after))
    };
    let (all_columns, selected, ddl, before, after) = cancellable(metadata, &progress, handle).await?;
    let include_data = !matches!(request.kind, ExportKind::Sql { data: false, .. } | ExportKind::Print { data: false });
    let query = if include_data { select_sql(driver, &request.schema, &request.table, &selected)? } else { String::new() };
    let (sender, receiver) = mpsc::channel(64);
    let writer_progress = Arc::clone(&progress);
    let writer_request = request.clone();
    let writer_columns = selected.clone();
    let writer = tokio::task::spawn_blocking(move || {
        write_output(driver, &writer_request, &writer_columns, &all_columns, &ddl, &before, &after, receiver, &writer_progress)
    });
    let handle = session.cancel_handle();
    let result = if include_data { cancellable(stream(session, driver, query, &selected, &sender, &progress), &progress, handle).await } else { Ok(()) };
    if result.is_ok() {
        let _ = sender.send(Message::Finished).await;
    }
    drop(sender);
    let written = writer.await?;
    match (result, written) {
        (Ok(()), written) => written,
        (Err(source), Ok(_)) => Err(source),
        // A failed writer closes the bounded channel; retain its actionable cause as well.
        (Err(source), Err(writer)) => Err(source.context(writer)),
    }
}

fn select_sql(driver: Driver, schema: &str, table: &str, columns: &[ColumnInfo]) -> anyhow::Result<String> {
    ensure!(!columns.is_empty(), tr("The table has no insertable columns"));
    let mut projection = Vec::new();
    for c in columns {
        let ident = driver.quote_ident(&c.name);
        let base = c.data_type.split('(').next().unwrap_or("").trim().to_ascii_lowercase();
        // Unsupported SQL Server UDT/variant/spatial values must not become placeholder text.
        if driver == Driver::MsSql {
            ensure!(
                !matches!(base.as_str(), "sql_variant" | "geometry" | "geography" | "hierarchyid"),
                crate::i18n::trf("Export of SQL Server {type} columns is not supported", &[("type", &c.data_type)])
            );
        }
        projection.push(match (driver, base.as_str()) {
            // Tiberius decodes MONEY as f64, losing the low digits on large amounts.
            (Driver::MsSql, "money" | "smallmoney") => {
                format!("CAST({ident} AS decimal(19,4)) AS {ident}")
            }
            // The server formats dates at their original precision using ISO 8601.
            (Driver::MsSql, "date" | "time" | "datetime" | "smalldatetime" | "datetime2" | "datetimeoffset") => {
                format!("CONVERT(nvarchar(max),{ident},126) AS {ident}")
            }
            (Driver::Postgres, "money") => format!("CAST({ident} AS numeric) AS {ident}"),
            _ => ident,
        });
    }
    Ok(format!("SELECT {} FROM {}", projection.join(", "), Session::quote_table(driver, schema, table)))
}

fn text(bytes: &[u8]) -> anyhow::Result<&str> {
    std::str::from_utf8(bytes).context(tr("Text is not valid UTF-8; use a SQL dump to preserve its bytes"))
}

fn binary_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(hex, "{b:02X}");
    }
    hex
}

fn unhex(s: &str) -> anyhow::Result<Vec<u8>> {
    ensure!(s.len().is_multiple_of(2) && s.is_ascii(), tr("Invalid binary value"));
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |b: u8| (b as char).to_digit(16).context(tr("Invalid hexadecimal digit"));
            Ok(((digit(pair[0])? << 4) | digit(pair[1])?) as u8)
        })
        .collect()
}

fn numeric_type(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    matches!(
        t.split('(').next().unwrap_or("").trim(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "int2"
            | "int4"
            | "int8"
            | "real"
            | "float"
            | "double"
            | "double precision"
            | "numeric"
            | "decimal"
            | "money"
            | "smallmoney"
            | "year"
    ) || t.ends_with(" unsigned")
}

pub(super) fn from_text(value: Option<&str>, column: &ColumnInfo, driver: Driver) -> anyhow::Result<Value> {
    let Some(value) = value else {
        return Ok(Value::Null);
    };
    if column.data_type.eq_ignore_ascii_case("bytea") && driver == Driver::Postgres {
        return Ok(Value::Binary(unhex(value.strip_prefix("\\x").context(tr("Expected hexadecimal bytea"))?)?));
    }
    if matches!(column.data_type.as_str(), "bool" | "boolean") {
        return Ok(Value::Bool(matches!(value, "t" | "true" | "1")));
    }
    if numeric_type(&column.data_type) {
        // MySQL YEAR(0) and ZEROFILL can produce leading zeroes, illegal in JSON numbers.
        let (sign, unsigned) = value.strip_prefix('-').map_or(("", value), |v| ("-", v));
        let digits = unsigned.bytes().take_while(u8::is_ascii_digit).count();
        let leading = unsigned.bytes().take(digits.saturating_sub(1)).take_while(|b| *b == b'0').count();
        return Ok(Value::Number(format!("{sign}{}", &unsigned[leading..])));
    }
    Ok(Value::Text(value.as_bytes().to_vec()))
}

async fn stream(
    session: &mut Session,
    driver: Driver,
    sql: String,
    columns: &[ColumnInfo],
    sender: &mpsc::Sender<Message>,
    progress: &Arc<ExportProgress>,
) -> anyhow::Result<()> {
    match &mut **session {
        Engine::Sqlite(s) => s.export_rows(sql, sender, progress).await,
        Engine::MsSql(s) => s.export_rows(&sql, sender, progress).await,
        Engine::Postgres(s) => s.export_rows(&sql, columns, sender, progress).await,
        Engine::MySql(s) => s.export_rows(&sql, columns, driver, sender, progress).await,
        _ => bail!(tr("Complete table export is not available for this engine")),
    }
}

pub(super) fn mssql_value(data: &tiberius::ColumnData<'static>) -> anyhow::Result<Value> {
    use tiberius::ColumnData as D;
    macro_rules! number {
        ($v:expr) => {
            $v.map(|v| Value::Number(v.to_string())).unwrap_or(Value::Null)
        };
    }
    Ok(match data {
        D::U8(v) => number!(v),
        D::I16(v) => number!(v),
        D::I32(v) => number!(v),
        D::I64(v) => number!(v),
        D::F32(v) => number!(v),
        D::F64(v) => number!(v),
        D::Bit(v) => v.map(Value::Bool).unwrap_or(Value::Null),
        D::String(v) => v.as_ref().map(|v| Value::Text(v.as_bytes().to_vec())).unwrap_or(Value::Null),
        D::Binary(v) => v.as_ref().map(|v| Value::Binary(v.to_vec())).unwrap_or(Value::Null),
        D::Numeric(v) => v.map(|v| Value::Number(super::mssql::numeric_text(v))).unwrap_or(Value::Null),
        D::Guid(v) => v.map(|v| Value::Text(v.to_string().into_bytes())).unwrap_or(Value::Null),
        D::Xml(v) => v.as_ref().map(|v| Value::Text(v.to_string().into_bytes())).unwrap_or(Value::Null),
        D::DateTime(_) | D::SmallDateTime(_) | D::DateTime2(_) | D::Date(_) | D::Time(_) | D::DateTimeOffset(_) => {
            bail!(tr("Expected ISO text projection for SQL Server temporal value"))
        }
    })
}

pub(super) fn mysql_value(value: &mysql_async::Value, metadata: &mysql_async::Column, column: &ColumnInfo, driver: Driver) -> anyhow::Result<Value> {
    use mysql_async::{Value as V, consts::ColumnType as T};
    Ok(match value {
        V::NULL => Value::Null,
        V::Bytes(b) if metadata.column_type() == T::MYSQL_TYPE_BIT => {
            ensure!(b.len() <= 8, tr("Unsupported MySQL BIT width"));
            Value::Number(b.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b)).to_string())
        }
        V::Bytes(b)
            if metadata.character_set() == 63
                && matches!(
                    metadata.column_type(),
                    T::MYSQL_TYPE_STRING
                        | T::MYSQL_TYPE_VAR_STRING
                        | T::MYSQL_TYPE_VARCHAR
                        | T::MYSQL_TYPE_BLOB
                        | T::MYSQL_TYPE_TINY_BLOB
                        | T::MYSQL_TYPE_MEDIUM_BLOB
                        | T::MYSQL_TYPE_LONG_BLOB
                ) =>
        {
            Value::Binary(b.clone())
        }
        V::Bytes(_) if metadata.column_type() == T::MYSQL_TYPE_GEOMETRY => {
            bail!(tr("Export of MySQL geometry is not supported"))
        }
        V::Bytes(b) => from_text(Some(text(b)?), column, driver)?,
        V::Int(n) => Value::Number(n.to_string()),
        V::UInt(n) => Value::Number(n.to_string()),
        V::Float(n) => Value::Number(n.to_string()),
        V::Double(n) => Value::Number(n.to_string()),
        // The text protocol used by query_iter returns date/time columns as Bytes.
        V::Date(..) | V::Time(..) => bail!(tr("Unexpected binary-protocol date in table export")),
    })
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn display(value: &Value) -> anyhow::Result<String> {
    Ok(match value {
        Value::Null => String::new(),
        Value::Text(t) => text(t)?.into(),
        Value::Number(n) => n.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Binary(b) => format!("0x{}", binary_hex(b)),
    })
}

fn sql_literal(driver: Driver, value: &Value) -> anyhow::Result<String> {
    Ok(match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            if driver == Driver::Postgres {
                b.to_string()
            } else {
                u8::from(*b).to_string()
            }
        }
        Value::Binary(b) => match driver {
            Driver::MsSql => format!("0x{}", binary_hex(b)),
            Driver::Postgres => format!("decode('{}','hex')", binary_hex(b)),
            _ => format!("X'{}'", binary_hex(b)),
        },
        Value::Number(n) => {
            if matches!(n.as_str(), "inf" | "-inf" | "NaN" | "Infinity" | "-Infinity") {
                if driver == Driver::Sqlite {
                    match n.as_str() {
                        "inf" | "Infinity" => "9e999".into(),
                        "-inf" | "-Infinity" => "-9e999".into(),
                        _ => bail!(tr("Cannot export NaN as SQLite numeric")),
                    }
                } else if driver == Driver::Postgres {
                    format!("'{n}'")
                } else {
                    bail!(tr("Cannot export non-finite numeric value"))
                }
            } else {
                ensure!(valid_number(n), tr("Invalid numeric export value"));
                n.clone()
            }
        }
        Value::Text(bytes) => match driver {
            // Hex avoids SQLite's SQL-literal NUL truncation and preserves invalid UTF-8 text.
            Driver::Sqlite => format!("CAST(X'{}' AS TEXT)", binary_hex(bytes)),
            // Hex avoids dependence on NO_BACKSLASH_ESCAPES and preserves embedded NUL.
            Driver::MySql | Driver::MariaDb => {
                format!("CONVERT(X'{}' USING utf8mb4)", binary_hex(bytes))
            }
            Driver::Postgres => format!("E'{}'", text(bytes)?.replace('\\', "\\\\").replace('\'', "''")),
            Driver::MsSql => {
                let value = text(bytes)?;
                if value.contains('\0') {
                    let utf16: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
                    format!("CONVERT(nvarchar(max),0x{})", binary_hex(&utf16))
                } else {
                    format!("N'{}'", value.replace('\'', "''"))
                }
            }
            _ => bail!(tr("Unsupported SQL literal dialect")),
        },
    })
}

fn valid_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let integer = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if i == integer {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let fraction = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == fraction {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == exponent {
            return false;
        }
    }
    i == b.len()
}

struct Output {
    temporary: PathBuf,
}
impl Drop for Output {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

#[allow(clippy::too_many_arguments)] // The writer owns the output; no database work happens on its blocking thread.
fn write_output(
    driver: Driver,
    request: &ExportRequest,
    columns: &[ColumnInfo],
    all_columns: &[ColumnInfo],
    ddl: &[String],
    before: &[String],
    after: &[String],
    mut receiver: mpsc::Receiver<Message>,
    progress: &ExportProgress,
) -> anyhow::Result<u64> {
    cancelled(progress)?;
    let parent = request.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let temporary = parent.join(format!(".datacraft-export-{}.tmp", uuid::Uuid::new_v4()));
    let file = OpenOptions::new().write(true).create_new(true).open(&temporary).context(tr("Could not create the export file"))?;
    let output = Output { temporary };
    let mut writer = BufWriter::new(file);
    match request.kind {
        ExportKind::Csv | ExportKind::Tsv => {
            let separator = if request.kind == ExportKind::Csv { ',' } else { '\t' };
            write_delimited(&mut writer, &columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), separator)?;
        }
        ExportKind::Json => writer.write_all(b"[\n")?,
        ExportKind::Sql { .. } => {
            if driver == Driver::Sqlite {
                writeln!(writer, "BEGIN TRANSACTION;\nPRAGMA defer_foreign_keys=ON;")?;
            }
            if matches!(driver, Driver::MySql | Driver::MariaDb) {
                // TIMESTAMP values were read in UTC; retain their instants across time zones/DST.
                writeln!(writer, "SET @datacraft_saved_time_zone=@@SESSION.time_zone;\nSET SESSION time_zone='+00:00';")?;
            }
            for statement in ddl.iter().chain(before) {
                writeln!(writer, "{};", statement.trim_end().trim_end_matches(';'))?;
            }
        }
        ExportKind::Print { data } => {
            writeln!(
                writer,
                "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>body{{font-family:sans-serif}}table{{border-collapse:collapse;margin:1em 0}}th,td{{border:1px solid #aaa;padding:4px;white-space:pre-wrap;overflow-wrap:anywhere}}thead{{display:table-header-group}}@media print{{tr{{break-inside:avoid}}}}</style></head><body><h1>{}</h1><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
                html(&format!("{}.{}", request.schema, request.table)),
                html(&format!("{}.{}", request.schema, request.table)),
                html(tr("Column")),
                html(tr("Type")),
                html(tr("Nullable")),
                html(tr("Primary key"))
            )?;
            for c in all_columns {
                writeln!(writer, "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", html(&c.name), html(&c.data_type), c.nullable, c.primary_key)?;
            }
            writer.write_all(b"</tbody></table>\n")?;
            if data {
                writer.write_all(b"<table><thead><tr>")?;
                for c in columns {
                    write!(writer, "<th>{}</th>", html(&c.name))?;
                }
                writer.write_all(b"</tr></thead><tbody>\n")?;
            }
        }
    }
    let names = columns.iter().map(|c| driver.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let name = Session::quote_table(driver, &request.schema, &request.table);
    let mut count = 0u64;
    let mut finished = false;
    while let Some(message) = receiver.blocking_recv() {
        cancelled(progress)?;
        let Message::Row(values) = message else {
            finished = true;
            break;
        };
        ensure!(values.len() == columns.len(), tr("Export row does not match column metadata"));
        match request.kind {
            ExportKind::Csv | ExportKind::Tsv => write_delimited(
                &mut writer,
                &values.iter().map(display).collect::<anyhow::Result<Vec<_>>>()?,
                if request.kind == ExportKind::Csv { ',' } else { '\t' },
            )?,
            ExportKind::Json => {
                if count > 0 {
                    writer.write_all(b",\n")?;
                }
                writer.write_all(b"{")?;
                for (i, (column, value)) in columns.iter().zip(&values).enumerate() {
                    if i > 0 {
                        writer.write_all(b",")?;
                    }
                    serde_json::to_writer(&mut writer, &column.name)?;
                    writer.write_all(b":")?;
                    match value {
                        Value::Null => writer.write_all(b"null")?,
                        Value::Bool(b) => write!(writer, "{b}")?,
                        Value::Number(n) if valid_number(n) => writer.write_all(n.as_bytes())?,
                        _ => serde_json::to_writer(&mut writer, &display(value)?)?,
                    }
                }
                writer.write_all(b"}")?;
            }
            ExportKind::Sql { .. } => {
                let values = values.iter().map(|v| sql_literal(driver, v)).collect::<anyhow::Result<Vec<_>>>()?;
                let overriding = if driver == Driver::Postgres { " OVERRIDING SYSTEM VALUE" } else { "" };
                writeln!(writer, "INSERT INTO {name} ({names}){overriding} VALUES ({});", values.join(", "))?;
            }
            ExportKind::Print { .. } => {
                writer.write_all(b"<tr>")?;
                for value in &values {
                    write!(writer, "<td>{}</td>", if matches!(value, Value::Null) { "<i>NULL</i>".into() } else { html(&display(value)?) })?;
                }
                writer.write_all(b"</tr>\n")?;
            }
        }
        count += 1;
        progress.rows.store(count, Ordering::Relaxed);
    }
    ensure!(finished, tr("The export did not finish"));
    cancelled(progress)?;
    match request.kind {
        ExportKind::Json => writer.write_all(b"\n]\n")?,
        ExportKind::Sql { .. } => {
            for statement in after {
                writeln!(writer, "{};", statement.trim_end_matches(';'))?;
            }
            if driver == Driver::Sqlite {
                writeln!(writer, "COMMIT;")?;
            }
            if matches!(driver, Driver::MySql | Driver::MariaDb) {
                writeln!(writer, "SET SESSION time_zone=@datacraft_saved_time_zone;")?;
            }
        }
        ExportKind::Print { data } => {
            if data {
                writer.write_all(b"</tbody></table>")?;
            }
            writer.write_all(b"</body></html>\n")?;
        }
        _ => {}
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    cancelled(progress)?;
    if request.overwrite {
        fs::rename(&output.temporary, &request.path)?;
    } else {
        // A same-directory hard link publishes atomically without a clobber race.
        fs::hard_link(&output.temporary, &request.path).context(tr("Could not publish the export; the destination may already exist"))?;
    }
    Ok(count)
}

fn write_delimited(writer: &mut impl Write, fields: &[String], separator: char) -> anyhow::Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            write!(writer, "{separator}")?;
        }
        if field.contains([separator, '"', '\r', '\n']) {
            write!(writer, "\"{}\"", field.replace('"', "\"\""))?;
        } else {
            writer.write_all(field.as_bytes())?;
        }
    }
    writer.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Driver;
    use super::*;
    use std::{fs, sync::atomic::Ordering};

    #[test]
    fn non_sqlite_self_references_require_structure_and_data() {
        for driver in [Driver::MsSql, Driver::Postgres, Driver::MySql, Driver::MariaDb] {
            assert!(data_only_restore_guard(driver, false).is_ok());
            assert!(data_only_restore_guard(driver, true).is_err(), "{driver:?} must reject a data-only dump with self references");
        }
        assert!(data_only_restore_guard(Driver::Sqlite, true).is_ok());
    }

    #[test]
    fn data_only_self_reference_queries_match_the_selected_relation() {
        assert!(data_only_self_reference_sql(Driver::Sqlite, "main", "node").unwrap().is_none());
        assert_eq!(
            data_only_self_reference_sql(Driver::MsSql, "dbo", "node").unwrap().unwrap(),
            "SELECT TOP(1) 1 FROM sys.foreign_keys WHERE parent_object_id=OBJECT_ID(N'[dbo].[node]') AND referenced_object_id=parent_object_id"
        );
        assert_eq!(
            data_only_self_reference_sql(Driver::Postgres, "public", "node").unwrap().unwrap(),
            "SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass(E'\"public\".\"node\"') AND contype='f' AND confrelid=conrelid LIMIT 1"
        );
        for driver in [Driver::MySql, Driver::MariaDb] {
            let query = data_only_self_reference_sql(driver, "db", "node").unwrap().unwrap();
            assert!(query.contains("CAST(TABLE_SCHEMA AS BINARY)=CAST(CONVERT(X'6462' USING utf8mb4) AS BINARY) AND CAST(TABLE_NAME AS BINARY)=CAST(CONVERT(X'6E6F6465' USING utf8mb4) AS BINARY)"));
            assert!(query.contains(
                "CAST(REFERENCED_TABLE_SCHEMA AS BINARY)=CAST(TABLE_SCHEMA AS BINARY) AND CAST(REFERENCED_TABLE_NAME AS BINARY)=CAST(TABLE_NAME AS BINARY)"
            ));
        }
    }

    #[test]
    fn metadata_only_dump_support_does_not_enable_unsupported_data_exports() {
        let structure = ExportKind::Sql { structure: true, data: false };
        assert!(supported_kind(Driver::Snowflake, structure));
        for driver in Driver::ALL {
            let native_data = matches!(driver, Driver::Sqlite | Driver::MsSql | Driver::Postgres | Driver::MySql | Driver::MariaDb);
            let native_definition = matches!(driver, Driver::Sqlite | Driver::MsSql | Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::Snowflake);
            assert_eq!(supported_kind(driver, structure), native_definition, "{driver:?}");
            assert_eq!(supported_kind(driver, ExportKind::Print { data: false }), !driver.is_nosql(), "{driver:?}");
            for kind in [
                ExportKind::Csv,
                ExportKind::Tsv,
                ExportKind::Json,
                ExportKind::Sql { structure: true, data: true },
                ExportKind::Sql { structure: false, data: true },
                ExportKind::Print { data: true },
            ] {
                assert_eq!(supported_kind(driver, kind), native_data, "{driver:?} {kind:?}");
            }
        }
    }

    #[test]
    fn sql_server_money_uses_exact_decimal_instead_of_driver_float() {
        let columns = vec![ColumnInfo { name: "cash]value".into(), data_type: "money".into(), nullable: true, primary_key: false }];
        assert_eq!(
            select_sql(Driver::MsSql, "d]bo", "order", &columns).unwrap(),
            "SELECT CAST([cash]]value] AS decimal(19,4)) AS [cash]]value] FROM [d]]bo].[order]"
        );
    }

    #[test]
    fn sql_server_native_values_preserve_binary_and_decimal() {
        use tiberius::ColumnData as D;
        let binary = D::Binary(Some(std::borrow::Cow::Owned(vec![0xFF; 200])));
        assert_eq!(sql_literal(Driver::MsSql, &mssql_value(&binary).unwrap()).unwrap(), format!("0x{}", "FF".repeat(200)));
        let decimal = D::Numeric(Some(tiberius::numeric::Numeric::new_with_scale(12345678901234567890123456789012345678, 4)));
        assert_eq!(sql_literal(Driver::MsSql, &mssql_value(&decimal).unwrap()).unwrap(), "1234567890123456789012345678901234.5678");
    }

    #[test]
    fn sql_server_datetimes_use_server_iso_text_with_original_fraction_precision() {
        let cols = vec![ColumnInfo { name: "stamp".into(), data_type: "datetime2".into(), nullable: true, primary_key: false }];
        assert_eq!(select_sql(Driver::MsSql, "dbo", "log", &cols).unwrap(), "SELECT CONVERT(nvarchar(max),[stamp],126) AS [stamp] FROM [dbo].[log]");
    }

    #[tokio::test]
    async fn cancel_stops_a_pending_driver_without_a_native_cancel_handle() {
        let progress = Arc::new(ExportProgress::default());
        let cancel = Arc::clone(&progress);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel.cancel.store(true, Ordering::Relaxed);
        });
        let result = tokio::time::timeout(Duration::from_millis(500), cancellable(std::future::pending::<anyhow::Result<()>>(), &progress, None)).await;
        assert!(result.is_ok(), "Cancellation must drop a pending driver future");
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn mysql_zero_year_and_zerofill_numbers_remain_valid_json_numbers() {
        let f = Fixture::new();
        let request = f.request(ExportKind::Json);
        let cols = vec![
            ColumnInfo { name: "year".into(), data_type: "year".into(), nullable: false, primary_key: false },
            ColumnInfo { name: "padded".into(), data_type: "int unsigned".into(), nullable: false, primary_key: false },
        ];
        let (tx, rx) = mpsc::channel(2);
        tx.try_send(Message::Row(vec![from_text(Some("0000"), &cols[0], Driver::MySql).unwrap(), from_text(Some("00012"), &cols[1], Driver::MySql).unwrap()]))
            .unwrap();
        tx.try_send(Message::Finished).unwrap();
        write_output(Driver::MySql, &request, &cols, &cols, &[], &[], &[], rx, &ExportProgress::default()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(request.path).unwrap()).unwrap();
        assert_eq!(json[0]["year"], 0);
        assert_eq!(json[0]["padded"], 12);
    }

    struct Fixture {
        dir: PathBuf,
        cfg: ConnectionConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("dc-export-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            let path = dir.join("source.db");
            let mut conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE \"odd <table>\"(id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT, bytes BLOB, amount REAL, derived TEXT GENERATED ALWAYS AS (note || '!') STORED); CREATE INDEX sample_index ON \"odd <table>\"(note);").unwrap();
            let tx = conn.transaction().unwrap();
            for id in 1..=1_025 {
                tx.execute(
                    "INSERT INTO \"odd <table>\"(id,note,bytes,amount) VALUES(?1,?2,?3,?4)",
                    rusqlite::params![
                        id,
                        if id == 1 {
                            Some(format!("<script>&\"'\0{}", "long,".repeat(2_000)))
                        } else if id == 2 {
                            None
                        } else {
                            Some(format!("row\t{id}\n"))
                        },
                        (0..200).map(|i| i as u8).collect::<Vec<_>>(),
                        1.25
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            conn.execute("UPDATE sqlite_sequence SET seq=9000", []).unwrap();
            let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            cfg.read_only = true;
            Self { dir, cfg }
        }
        fn request(&self, kind: ExportKind) -> ExportRequest {
            ExportRequest { schema: "main".into(), table: "odd <table>".into(), path: self.dir.join("output"), kind, overwrite: false }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    #[tokio::test]
    async fn json_exports_all_rows_and_complete_binary_and_text() {
        let f = Fixture::new();
        let request = f.request(ExportKind::Json);
        let progress = Arc::new(ExportProgress::default());
        assert_eq!(export(f.cfg.clone(), "main", request.clone(), progress.clone()).await.unwrap(), 1_025);
        let rows: serde_json::Value = serde_json::from_slice(&fs::read(request.path).unwrap()).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1_025);
        assert_eq!(rows[0]["id"], 1);
        assert!(rows[0]["note"].as_str().unwrap().contains('\0'));
        assert_eq!(rows[0]["note"].as_str().unwrap().len(), 10_012);
        assert_eq!(rows[0]["bytes"].as_str().unwrap().len(), 402);
        assert_eq!(rows[1]["note"], serde_json::Value::Null);
        assert_eq!(progress.rows.load(Ordering::Relaxed), 1_025);
    }

    #[tokio::test]
    async fn csv_and_tsv_escape_their_own_delimiters_and_embedded_line_breaks() {
        let f = Fixture::new();
        for (kind, separator) in [(ExportKind::Csv, ','), (ExportKind::Tsv, '\t')] {
            let mut request = f.request(kind);
            request.overwrite = true;
            export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap();
            let content = fs::read_to_string(request.path).unwrap();
            let parsed = crate::ui::import_dialog::parse_delimited(&content, separator);
            assert_eq!(parsed.len(), 1_026);
            assert_eq!(parsed[0], ["id", "note", "bytes", "amount", "derived"]);
            assert_eq!(parsed[1][1], format!("<script>&\"'\0{}", "long,".repeat(2_000)));
            assert_eq!(parsed[3][1], "row\t3\n");
            assert_eq!(parsed[1][2].len(), 402);
        }
    }

    #[tokio::test]
    async fn refuses_to_replace_source_database_and_invalid_dump_selection() {
        let f = Fixture::new();
        let mut request = f.request(ExportKind::Csv);
        request.path = PathBuf::from(&f.cfg.file_path);
        request.overwrite = true;
        assert!(export(f.cfg.clone(), "main", request, Arc::default()).await.is_err());
        let request = f.request(ExportKind::Sql { structure: false, data: false });
        assert!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.is_err());
        assert!(!request.path.exists());
        let conn = rusqlite::Connection::open(&f.cfg.file_path).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM \"odd <table>\"", [], |r| r.get::<_, i64>(0)).unwrap(), 1_025);
    }

    #[tokio::test]
    async fn sql_dump_restores_dynamic_values_generated_columns_and_indexes() {
        let f = Fixture::new();
        let request = f.request(ExportKind::Sql { structure: true, data: true });
        export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap();
        let dump = fs::read_to_string(request.path).unwrap();
        let restored = rusqlite::Connection::open_in_memory().unwrap();
        restored.execute_batch(&dump).unwrap();
        let source = rusqlite::Connection::open(&f.cfg.file_path).unwrap();
        let read = |db: &rusqlite::Connection| {
            db.prepare("SELECT id,note,bytes,amount,derived FROM \"odd <table>\" ORDER BY id")
                .unwrap()
                .query_map([], |row| (0..5).map(|i| row.get::<_, rusqlite::types::Value>(i)).collect::<Result<Vec<_>, _>>())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(read(&restored), read(&source));
        assert_eq!(restored.query_row("SELECT count(*) FROM sqlite_schema WHERE name='sample_index'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        restored.execute("INSERT INTO \"odd <table>\"(note) VALUES('after restore')", []).unwrap();
        assert_eq!(restored.last_insert_rowid(), 9_001);
    }

    #[tokio::test]
    async fn data_only_dump_allows_source_triggers_and_omits_generated_columns() {
        let f = Fixture::new();
        let source = rusqlite::Connection::open(&f.cfg.file_path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE audit(id INTEGER); CREATE TRIGGER sample_trigger AFTER INSERT ON \"odd <table>\" BEGIN INSERT INTO audit VALUES(NEW.id); END;",
            )
            .unwrap();
        let mut request = f.request(ExportKind::Sql { structure: false, data: true });
        assert_eq!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap(), 1_025);
        let dump = fs::read_to_string(&request.path).unwrap();
        assert!(!dump.contains("CREATE TABLE"));
        let restored = rusqlite::Connection::open_in_memory().unwrap();
        restored.execute_batch("CREATE TABLE \"odd <table>\"(id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT, bytes BLOB, amount REAL, derived TEXT GENERATED ALWAYS AS (note || '!') STORED)").unwrap();
        restored.execute_batch(&dump).unwrap();
        let read = |db: &rusqlite::Connection| {
            db.prepare("SELECT id,note,bytes,amount,derived FROM \"odd <table>\" ORDER BY id")
                .unwrap()
                .query_map([], |row| (0..5).map(|i| row.get::<_, rusqlite::types::Value>(i)).collect::<Result<Vec<_>, _>>())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(read(&restored), read(&source));
        assert_eq!(source.query_row("SELECT count(*) FROM audit", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        request.kind = ExportKind::Sql { structure: true, data: true };
        request.overwrite = true;
        assert!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.is_err());
        assert_eq!(fs::read_to_string(&request.path).unwrap(), dump);
    }

    #[tokio::test]
    async fn sqlite_dump_defers_self_references_without_disabling_foreign_keys() {
        let f = Fixture::new();
        let source = rusqlite::Connection::open(&f.cfg.file_path).unwrap();
        source
            .execute_batch("CREATE TABLE lineage(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES lineage(id)); INSERT INTO lineage VALUES(1,2),(2,NULL);")
            .unwrap();
        for structure in [true, false] {
            let mut request = f.request(ExportKind::Sql { structure, data: true });
            request.table = "lineage".into();
            request.overwrite = true;
            export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap();
            let dump = fs::read_to_string(request.path).unwrap();
            assert!(dump.find("VALUES (1, 2)").unwrap() < dump.find("VALUES (2, NULL)").unwrap());
            let target = rusqlite::Connection::open_in_memory().unwrap();
            target.execute_batch("PRAGMA foreign_keys=ON").unwrap();
            if !structure {
                target.execute_batch("CREATE TABLE lineage(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES lineage(id))").unwrap();
            }
            target.execute_batch(&dump).unwrap();
            assert_eq!(target.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
            assert_eq!(target.query_row("SELECT count(*) FROM lineage c JOIN lineage p ON c.parent_id=p.id", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
            assert_eq!(target.query_row("PRAGMA defer_foreign_keys", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        }
    }

    #[test]
    fn review_sql_server_exports_avoid_serializable_range_locks() {
        let sql = snapshot_sql(Driver::MsSql).unwrap();
        assert!(sql.contains("snapshot_isolation_state=1"));
        assert!(sql.contains("SET TRANSACTION ISOLATION LEVEL SNAPSHOT"));
        assert!(sql.contains("SET TRANSACTION ISOLATION LEVEL READ COMMITTED"));
        assert!(!sql.contains("SERIALIZABLE"));
        assert!(sql.ends_with("BEGIN TRANSACTION"));
    }

    #[test]
    fn mysql_source_session_uses_utc_before_snapshot() {
        for driver in [Driver::MySql, Driver::MariaDb] {
            let setup = snapshot_sql(driver).unwrap();
            assert!(setup.find("SET SESSION time_zone='+00:00'").expect("UTC source timezone") < setup.find("START TRANSACTION").unwrap());
        }
    }

    #[test]
    fn mysql_data_dump_saves_sets_and_restores_target_time_zone() {
        let f = Fixture::new();
        let columns = vec![ColumnInfo { name: "stamp".into(), data_type: "timestamp".into(), nullable: false, primary_key: false }];
        for driver in [Driver::MySql, Driver::MariaDb] {
            for structure in [false, true] {
                let mut request = f.request(ExportKind::Sql { structure, data: true });
                request.overwrite = true;
                let (tx, rx) = mpsc::channel(2);
                tx.try_send(Message::Row(vec![Value::Text(b"2026-10-25 00:30:00".to_vec())])).unwrap();
                tx.try_send(Message::Finished).unwrap();
                write_output(driver, &request, &columns, &columns, &[], &[], &[], rx, &ExportProgress::default()).unwrap();
                let dump = fs::read_to_string(&request.path).unwrap();
                let save = dump.find("SET @datacraft_saved_time_zone=@@SESSION.time_zone;").expect("save restore target timezone");
                let utc = dump.find("SET SESSION time_zone='+00:00';").expect("UTC restore timezone");
                let insert = dump.find("INSERT INTO").unwrap();
                let restore = dump.find("SET SESSION time_zone=@datacraft_saved_time_zone;").expect("restore original timezone");
                assert!(save < utc && utc < insert && insert < restore);
            }
        }
    }

    #[tokio::test]
    async fn sqlite_dump_preserves_invalid_utf8_and_dynamic_storage_classes() {
        let f = Fixture::new();
        let source = rusqlite::Connection::open(&f.cfg.file_path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE edge(value); INSERT INTO edge VALUES(CAST(X'FF00FE' AS TEXT)),(X'00FF'),(9e999),(-9e999),(1.0),(NULL),(-9223372036854775808);",
            )
            .unwrap();
        let mut request = f.request(ExportKind::Sql { structure: true, data: true });
        request.table = "edge".into();
        assert_eq!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap(), 7);
        let restored = rusqlite::Connection::open_in_memory().unwrap();
        restored.execute_batch(&fs::read_to_string(&request.path).unwrap()).unwrap();
        let read = |db: &rusqlite::Connection| {
            db.prepare("SELECT typeof(value),hex(value) FROM edge")
                .unwrap()
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(read(&source), read(&restored));
        let original = fs::read(&request.path).unwrap();
        request.kind = ExportKind::Json;
        request.overwrite = true;
        // Exceed the bounded channel so a writer failure also wakes a blocked source reader.
        source.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO edge SELECT NULL FROM n").unwrap();
        let error = export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap_err();
        assert!(format!("{error:#}").contains("UTF-8"));
        assert_eq!(fs::read(request.path).unwrap(), original);
    }

    #[tokio::test]
    async fn print_escapes_schema_columns_and_optional_data() {
        let f = Fixture::new();
        for data in [false, true] {
            let mut request = f.request(ExportKind::Print { data });
            request.overwrite = data;
            let count = export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap();
            assert_eq!(count, if data { 1_025 } else { 0 });
            let html = fs::read_to_string(request.path).unwrap();
            assert!(html.contains("main.odd &lt;table&gt;"));
            assert!(html.contains("INTEGER"));
            assert!(html.contains("<tr><td>id</td><td>INTEGER</td><td>false</td><td>true</td></tr>"));
            assert!(!html.contains("<script>"));
            assert_eq!(html.contains("&lt;script&gt;"), data);
        }
    }

    #[tokio::test]
    async fn failure_cancel_and_no_overwrite_preserve_destination_without_partial_files() {
        let f = Fixture::new();
        let mut request = f.request(ExportKind::Csv);
        fs::write(&request.path, b"keep original").unwrap();
        assert!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.is_err());
        request.overwrite = true;
        request.table = "missing".into();
        assert!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.is_err());
        request.table = "odd <table>".into();
        let progress = Arc::new(ExportProgress::default());
        progress.cancel.store(true, Ordering::Relaxed);
        assert!(export(f.cfg.clone(), "main", request.clone(), progress).await.is_err());
        assert_eq!(fs::read(&request.path).unwrap(), b"keep original");
        assert_eq!(fs::read_dir(&f.dir).unwrap().count(), 2);
        assert_eq!(export(f.cfg.clone(), "main", request.clone(), Arc::default()).await.unwrap(), 1_025);
        assert!(fs::read_to_string(request.path).unwrap().starts_with("id,note,bytes,amount,derived\n"));
    }

    #[tokio::test]
    async fn cancellation_after_rows_were_written_discards_temporary_output() {
        let f = Fixture::new();
        let mut request = f.request(ExportKind::Json);
        fs::write(&request.path, b"old output").unwrap();
        request.overwrite = true;
        let progress = Arc::new(ExportProgress::default());
        let cancel = progress.clone();
        let watcher = std::thread::spawn(move || {
            while cancel.rows.load(Ordering::Relaxed) < 10 && !cancel.cancel.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            cancel.cancel.store(true, Ordering::Relaxed);
        });
        let result = export(f.cfg.clone(), "main", request.clone(), progress.clone()).await;
        progress.cancel.store(true, Ordering::Relaxed);
        watcher.join().unwrap();
        assert!(result.is_err());
        assert!(progress.rows.load(Ordering::Relaxed) >= 10);
        assert_eq!(fs::read(&request.path).unwrap(), b"old output");
        assert_eq!(fs::read_dir(&f.dir).unwrap().count(), 2);
    }
}
