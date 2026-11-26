//! SAP HANA driver: HANA 2 (on premise) and HANA Cloud, through `hdbconnect_async`, a pure Rust
//! implementation of HANA's SQL command network protocol (no SAP client to install).
//!
//! - TLS: the "Use TLS" driver property, port 443 or a `*.hanacloud.ondemand.com` host (HANA
//!   Cloud only accepts TLS). Certificates are checked against the webpki roots (HANA Cloud's
//!   are public); "Trust server certificate" (`tls_insecure`) skips the check for on-premise
//!   servers with their own certificates. rustls runs on the process's aws-lc provider.
//! - The Database field is the tenant (`databaseName`): the system database's port redirects
//!   to it. Empty = the database that listens on the port.
//! - Autocommit by default. A console's Manual transaction turns it off until COMMIT or
//!   ROLLBACK. hdbconnect silently reconnects and repeats a statement that failed with an I/O
//!   error while autocommit is off, which would lose the open transaction: the connection id is
//!   checked after each statement and a change ends the session with an error.
//! - Values are formatted as text here: dates ISO-like, decimals exactly, LOBs read in full
//!   (BLOBs only their first bytes, in hex).

use std::time::{Duration, Instant};

use anyhow::anyhow;
use hdbconnect_async::{
    ConnectParams, Connection, ConnectionConfiguration, HdbError, HdbReturnValue, HdbValue,
    ServerCerts,
};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::db::sqlite::hex_preview;
use crate::i18n::trf;

const DEFAULT_PORT: u16 = 30015;
/// TCP, TLS and authentication time limit.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a BLOB read to show its start (`hex_preview` shows 64 and marks the rest).
const BLOB_PREVIEW: u32 = 65;
/// Largest number of rows asked for per fetch round trip.
const MAX_FETCH_SIZE: usize = 10_000;

/// Schemas of HANA itself, never shown in the explorer (besides any owned by `SYS` or
/// `_SYS_*`).
const SYSTEM_SCHEMAS: &[&str] = &[
    "SYS",
    "UIS",
    "HANA_XS_BASE",
    "SYSTEM_DB",
    "BROKER_PO_USER",
    "BROKER_USER",
    "XSSQLCC_AUTO_USER",
    "PAL_STEM_TFIDF",
];
/// Prefixes of system schemas: `_SYS_BIC`, `_SYS_REPO`, `SAP_XS_LM`, `SAP_PA_APL`...
const SYSTEM_SCHEMA_PREFIXES: &[&str] = &["_SYS", "SAP_", "HANA_XS_", "XSSQLCC_"];

pub struct HanaSession {
    conn: Connection,
    /// Set when the connection is known lost (or replaced behind our back).
    broken: bool,
    /// A console's Manual transaction: autocommit is off.
    manual: bool,
    /// Id of the server connection; hdbconnect may reconnect silently while autocommit is off.
    connection_id: u32,
    /// Read-only data source: `SET TRANSACTION READ ONLY` again after each transaction.
    read_only: bool,
}

/// How the connection uses TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsMode {
    Off,
    /// The certificate is checked against the webpki roots and the host name.
    Verify,
    /// "Trust server certificate": encrypted, without checking the certificate.
    Insecure,
}

/// A driver property set to true (`true`, `1`, `yes`, `on`).
fn flag(cfg: &ConnectionConfig, name: &str) -> bool {
    cfg.property(name)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
}

/// HANA Cloud's hosts: `<id>.hana.<region>.hanacloud.ondemand.com`.
fn is_cloud_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    host.ends_with(".hanacloud.ondemand.com")
}

/// TLS from the driver properties; HANA Cloud (port 443 or its domain) always uses it.
fn tls_mode(cfg: &ConnectionConfig, server_host: &str, port: u16) -> TlsMode {
    let insecure = flag(cfg, "tls_insecure");
    let on = insecure || flag(cfg, "tls") || port == 443 || is_cloud_host(server_host);
    match (on, insecure) {
        (false, _) => TlsMode::Off,
        (true, true) => TlsMode::Insecure,
        (true, false) => TlsMode::Verify,
    }
}

/// Host and port to connect to: the Host field may carry a port (`host:443`, as HANA Cloud's
/// console shows the SQL endpoint). Through an SSH tunnel they are the tunnel's listener.
fn endpoint(cfg: &ConnectionConfig) -> (String, u16) {
    let port = if cfg.port == 0 {
        DEFAULT_PORT
    } else {
        cfg.port
    };
    if cfg.tunnel_host.is_some() {
        return (cfg.host.trim().to_string(), port);
    }
    let (host, port) = super::ssh::host_port(&cfg.host, port);
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    (host, port)
}

fn connect_params(cfg: &ConnectionConfig) -> anyhow::Result<ConnectParams> {
    let (host, port) = endpoint(cfg);
    // The server's real name: the tunnel's destination when tunnelled.
    let server_host = cfg.tunnel_host.clone().unwrap_or_else(|| host.clone());
    let mut builder = ConnectParams::builder();
    builder
        .hostname(&host)
        .port(port)
        .dbuser(cfg.user.trim())
        .password(&cfg.password);
    let database = cfg.database.trim();
    if !database.is_empty() {
        builder.dbname(database);
    }
    match tls_mode(cfg, &server_host, port) {
        TlsMode::Off => {}
        TlsMode::Insecure => {
            builder.tls_without_server_verification();
        }
        TlsMode::Verify => {
            // hdbconnect checks the certificate against the host it connects to, the tunnel's
            // 127.0.0.1, and has no way to name the real server.
            if cfg.tunnel_host.is_some() {
                anyhow::bail!(
                    "The SAP HANA driver can't check the server's certificate through an SSH \
                     tunnel (it would be checked against 127.0.0.1). Turn on \"Trust server \
                     certificate\": the tunnel itself is encrypted and authenticated."
                );
            }
            builder.tls_with(ServerCerts::RootCertificates);
        }
    }
    builder.build().map_err(hana_error)
}

/// The error and its causes, with the server's code like DataGrip shows it (`[259] invalid
/// table name: ...`).
fn hana_error(e: HdbError) -> anyhow::Error {
    if let Some(server) = e.server_error() {
        return anyhow!("[{}] {}", server.code(), server.text());
    }
    if let HdbError::ErrorAfterReconnect { second, .. } = &e
        && let Some(server) = second.server_error()
    {
        return anyhow!("[{}] {}", server.code(), server.text());
    }
    let mut text = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    anyhow!(text)
}

/// Whether the error means the connection is gone (and not just a failed statement).
fn is_connection_error(e: &HdbError) -> bool {
    matches!(
        e,
        HdbError::Io { .. }
            | HdbError::ConnectionBroken { .. }
            | HdbError::ErrorAfterReconnect { .. }
            | HdbError::SessionClosingTransactionError
            | HdbError::TlsProtocol { .. }
    )
}

/// The statement as sent: HANA rejects a trailing `;` (also after a `DO BEGIN ... END`).
fn clean_statement(sql: &str) -> &str {
    sql.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

/// HANA's date/time text (`2024-01-31T13:45:00.1230000`) the way the grid shows it:
/// `2024-01-31 13:45:00.123`, without a fraction of only zeros.
fn format_datetime(text: &str) -> String {
    let text = text.replacen('T', " ", 1);
    match text.split_once('.') {
        Some((whole, fraction)) => {
            let fraction = fraction.trim_end_matches('0');
            if fraction.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{fraction}")
            }
        }
        None => text,
    }
}

/// A value as text (`None` = NULL). LOBs are read from the server here.
async fn value_text(value: HdbValue<'static>) -> anyhow::Result<Cell> {
    Ok(Some(match value {
        HdbValue::NULL => return Ok(None),
        HdbValue::TINYINT(v) => v.to_string(),
        HdbValue::SMALLINT(v) => v.to_string(),
        HdbValue::INT(v) => v.to_string(),
        HdbValue::BIGINT(v) => v.to_string(),
        HdbValue::DECIMAL(v) => v.to_plain_string(),
        HdbValue::REAL(v) => v.to_string(),
        HdbValue::DOUBLE(v) => v.to_string(),
        HdbValue::BOOLEAN(v) => v.to_string(),
        HdbValue::STRING(s) => s,
        HdbValue::STR(s) => s.to_string(),
        HdbValue::DBSTRING(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        HdbValue::BINARY(bytes) | HdbValue::GEOMETRY(bytes) | HdbValue::POINT(bytes) => {
            hex_preview(&bytes)
        }
        HdbValue::LONGDATE(d) => format_datetime(&d.to_string()),
        HdbValue::SECONDDATE(d) => format_datetime(&d.to_string()),
        HdbValue::DAYDATE(d) => d.to_string(),
        HdbValue::SECONDTIME(t) => t.to_string(),
        HdbValue::ASYNC_CLOB(clob) => clob.into_string().await.map_err(hana_error)?,
        HdbValue::ASYNC_NCLOB(nclob) => nclob.into_string().await.map_err(hana_error)?,
        HdbValue::ASYNC_BLOB(mut blob) => {
            let total = blob.total_byte_length();
            let bytes = if total > u64::from(BLOB_PREVIEW) {
                blob.read_slice(0, BLOB_PREVIEW).await.map_err(hana_error)?
            } else {
                blob.into_bytes().await.map_err(hana_error)?
            };
            hex_preview(&bytes)
        }
        HdbValue::ARRAY(values) => {
            let mut items = Vec::with_capacity(values.len());
            for v in values {
                items.push(
                    Box::pin(value_text(v))
                        .await?
                        .unwrap_or_else(|| "NULL".into()),
                );
            }
            format!("[{}]", items.join(", "))
        }
        HdbValue::ASYNC_LOBSTREAM(_) => String::new(),
    }))
}

/// Readable column type from the catalog: `NVARCHAR(50)`, `DECIMAL(10,2)`, `DECIMAL` (floating),
/// `INTEGER`...
fn type_name(data_type: &str, length: Option<i64>, scale: Option<i64>) -> String {
    let name = data_type.trim().to_ascii_uppercase();
    match name.as_str() {
        "VARCHAR" | "NVARCHAR" | "CHAR" | "NCHAR" | "ALPHANUM" | "SHORTTEXT" | "VARBINARY"
        | "BINARY" => match length {
            Some(n) if n > 0 => format!("{name}({n})"),
            _ => name,
        },
        "DECIMAL" => match (length, scale) {
            (Some(p), Some(s)) if p > 0 => format!("DECIMAL({p},{s})"),
            _ => name,
        },
        "FLOAT" => match length {
            Some(n) if n > 0 && n != 53 => format!("FLOAT({n})"),
            _ => name,
        },
        _ => name,
    }
}

fn is_system_schema(name: &str) -> bool {
    SYSTEM_SCHEMAS.contains(&name) || SYSTEM_SCHEMA_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// SQL condition on `SCHEMA_NAME` leaving out the system schemas (`is_system_schema`).
fn user_schema_filter() -> String {
    let mut parts = vec![format!(
        "SCHEMA_NAME NOT IN ({})",
        SYSTEM_SCHEMAS
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )];
    for prefix in SYSTEM_SCHEMA_PREFIXES {
        parts.push(format!(
            "SCHEMA_NAME NOT LIKE '{}%' ESCAPE '\\'",
            prefix.replace('_', "\\_")
        ));
    }
    parts.join(" AND ")
}

/// The user schemas the login can see.
fn schemas_sql() -> String {
    format!(
        "SELECT SCHEMA_NAME FROM SYS.SCHEMAS \
         WHERE HAS_PRIVILEGES = 'TRUE' AND SCHEMA_OWNER <> 'SYS' \
         AND SCHEMA_OWNER NOT LIKE '\\_SYS%' ESCAPE '\\' AND {} \
         ORDER BY SCHEMA_NAME",
        user_schema_filter()
    )
}

/// Tables (`T`) and views (`V`) of the user schemas, without system tables or table types.
fn objects_sql() -> String {
    let filter = user_schema_filter();
    format!(
        "SELECT SCHEMA_NAME, TABLE_NAME, 'T' FROM SYS.TABLES \
         WHERE IS_SYSTEM_TABLE = 'FALSE' AND IS_USER_DEFINED_TYPE = 'FALSE' AND {filter} \
         UNION ALL \
         SELECT SCHEMA_NAME, VIEW_NAME, 'V' FROM SYS.VIEWS WHERE {filter}"
    )
}

/// Columns of the tables and views in order, with their type, length, scale and nullability.
fn columns_sql() -> String {
    let filter = user_schema_filter();
    format!(
        "SELECT SCHEMA_NAME, TABLE_NAME, COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, \
         IS_NULLABLE, POSITION FROM SYS.TABLE_COLUMNS WHERE {filter} \
         UNION ALL \
         SELECT SCHEMA_NAME, VIEW_NAME, COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, \
         IS_NULLABLE, POSITION FROM SYS.VIEW_COLUMNS WHERE {filter} \
         ORDER BY 1, 2, 8"
    )
}

/// Primary key columns.
fn primary_keys_sql() -> String {
    format!(
        "SELECT SCHEMA_NAME, TABLE_NAME, COLUMN_NAME FROM SYS.CONSTRAINTS \
         WHERE IS_PRIMARY_KEY = 'TRUE' AND {}",
        user_schema_filter()
    )
}

/// `SAP HANA 2.00.076` from `2.00.076.00.1705400033`.
fn version_label(version: &str) -> String {
    let parts: Vec<&str> = version.trim().split('.').collect();
    let short = if parts.len() >= 3 {
        parts[..3].join(".")
    } else {
        version.trim().to_string()
    };
    format!("SAP HANA {short}")
}

/// Builds the explorer tree from the rows of the catalog queries.
fn build_schemas(
    schemas: Vec<String>,
    objects: &[Vec<Cell>],
    columns: &[Vec<Cell>],
    keys: &[Vec<Cell>],
) -> Vec<SchemaInfo> {
    use std::collections::{BTreeMap, HashSet};
    let text = |row: &[Cell], i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let number = |row: &[Cell], i: usize| {
        row.get(i)
            .and_then(|c| c.as_deref())
            .and_then(|s| s.trim().parse::<i64>().ok())
    };
    let pk: HashSet<(String, String, String)> = keys
        .iter()
        .map(|r| (text(r, 0), text(r, 1), text(r, 2)))
        .collect();
    let mut tree: BTreeMap<String, BTreeMap<String, TableInfo>> = schemas
        .into_iter()
        .filter(|s| !is_system_schema(s))
        .map(|s| (s, BTreeMap::new()))
        .collect();
    for row in objects {
        let (schema, name) = (text(row, 0), text(row, 1));
        let kind = if text(row, 2) == "V" {
            ObjectKind::View
        } else {
            ObjectKind::Table
        };
        if let Some(tables) = tree.get_mut(&schema) {
            tables.insert(
                name.clone(),
                TableInfo {
                    name,
                    kind,
                    columns: Vec::new(),
                },
            );
        }
    }
    for row in columns {
        let (schema, table, column) = (text(row, 0), text(row, 1), text(row, 2));
        let Some(info) = tree.get_mut(&schema).and_then(|t| t.get_mut(&table)) else {
            continue;
        };
        let primary_key = pk.contains(&(schema, table, column.clone()));
        info.columns.push(ColumnInfo {
            name: column,
            data_type: type_name(&text(row, 3), number(row, 4), number(row, 5)),
            nullable: !text(row, 6).eq_ignore_ascii_case("FALSE"),
            primary_key,
        });
    }
    tree.into_iter()
        .map(|(name, tables)| SchemaInfo {
            name,
            tables: tables.into_values().collect(),
        })
        .collect()
}

impl HanaSession {
    // The protocol's futures are large: the public methods box them so the dispatching
    // futures in `db` (and the worker's) stay small enough for the test threads' stacks.

    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        Box::pin(Self::open(cfg)).await
    }

    async fn open(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let params = connect_params(cfg)?;
        let config = ConnectionConfiguration::default().with_auto_commit(true);
        let conn = tokio::time::timeout(
            CONNECT_TIMEOUT,
            Connection::with_configuration(params, &config),
        )
        .await
        .map_err(|_| {
            let (host, port) = endpoint(cfg);
            anyhow!(trf(
                "Timed out connecting to {host}:{port}",
                &[("host", &host), ("port", &port)]
            ))
        })?
        .map_err(hana_error)?;
        conn.set_application("DataCraft").await;
        let connection_id = conn.id().await;
        let mut session = Self {
            conn,
            broken: false,
            manual: false,
            connection_id,
            read_only: cfg.read_only,
        };
        session.apply_read_only().await?;
        Ok(session)
    }

    /// Read-only data sources: the server refuses changes too.
    async fn apply_read_only(&mut self) -> anyhow::Result<()> {
        if self.read_only {
            self.conn
                .exec("SET TRANSACTION READ ONLY")
                .await
                .map_err(|e| self.failed(e))?;
        }
        Ok(())
    }

    /// Converts the error, marking the session broken when the connection is gone.
    fn failed(&mut self, e: HdbError) -> anyhow::Error {
        if is_connection_error(&e) {
            self.broken = true;
        }
        hana_error(e)
    }

    /// While autocommit is off, hdbconnect reconnects and repeats a statement that hit an I/O
    /// error: the transaction is gone then, so the session must not go on as if it were open.
    async fn check_connection(&mut self) -> anyhow::Result<()> {
        if self.conn.is_broken().await {
            self.broken = true;
            anyhow::bail!("The connection to SAP HANA was lost");
        }
        if self.conn.id().await != self.connection_id {
            self.broken = true;
            anyhow::bail!(
                "The connection to SAP HANA was lost and reopened: the open transaction was \
                 rolled back"
            );
        }
        Ok(())
    }

    /// Runs ONE statement (with `?` parameters bound as text when given).
    async fn run(
        &mut self,
        sql: &str,
        parameters: Option<&[String]>,
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        Box::pin(self.run_unboxed(sql, parameters, max_rows)).await
    }

    async fn run_unboxed(
        &mut self,
        sql: &str,
        parameters: Option<&[String]>,
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let statement = clean_statement(sql);
        if statement.is_empty() {
            return Ok(Vec::new());
        }
        self.conn
            .set_fetch_size(max_rows.saturating_add(1).clamp(1, MAX_FETCH_SIZE) as u32)
            .await;
        let start = Instant::now();
        let response = match parameters {
            None => self.conn.statement(statement).await,
            Some(parameters) => self.run_prepared(statement, parameters).await,
        };
        let outcome = match response {
            Ok(response) => self.collect(sql.trim(), response, max_rows, start).await,
            Err(e) => Err(self.failed(e)),
        };
        if outcome.is_err() && self.conn.is_broken().await {
            self.broken = true;
        }
        if !self.conn.is_auto_commit().await {
            self.check_connection().await?;
        }
        outcome
    }

    async fn run_prepared(
        &mut self,
        statement: &str,
        parameters: &[String],
    ) -> Result<hdbconnect_async::HdbResponse, HdbError> {
        let mut prepared = self.conn.prepare(statement).await?;
        let descriptors = prepared.parameter_descriptors();
        let values: Vec<HdbValue<'static>> = descriptors
            .iter_in()
            .zip(parameters)
            .map(|(d, text)| {
                d.parse_value(text)
                    .unwrap_or_else(|_| HdbValue::STRING(text.clone()))
            })
            .collect();
        prepared.execute_row(values).await
    }

    /// The results of a response: result sets (a query, a procedure's), affected rows and
    /// output parameters.
    async fn collect(
        &mut self,
        display: &str,
        response: hdbconnect_async::HdbResponse,
        max_rows: usize,
        start: Instant,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let mut results = Vec::new();
        for value in response {
            let mut rs = ResultSet {
                statement: display.to_string(),
                ..Default::default()
            };
            match value {
                HdbReturnValue::ResultSet(mut set) => {
                    rs.columns = set
                        .metadata()
                        .iter()
                        .map(|f| f.displayname().to_string())
                        .collect();
                    while let Some(mut row) = set.next_row().await.map_err(|e| self.failed(e))? {
                        if rs.rows.len() >= max_rows {
                            rs.truncated = true;
                            break;
                        }
                        let mut cells = Vec::with_capacity(rs.columns.len());
                        while let Some(v) = row.next_value() {
                            cells.push(value_text(v).await?);
                        }
                        rs.rows.push(cells);
                    }
                }
                HdbReturnValue::AffectedRows(counts) => {
                    rs.affected = Some(counts.iter().map(|&n| n as u64).sum());
                }
                HdbReturnValue::OutputParameters(output) => {
                    let (descriptors, values) = output.into_descriptors_and_values();
                    rs.columns = descriptors
                        .iter()
                        .enumerate()
                        .map(|(i, d)| {
                            d.name()
                                .map_or_else(|| format!("{}", i + 1), str::to_string)
                        })
                        .collect();
                    let mut cells = Vec::with_capacity(values.len());
                    for v in values {
                        cells.push(value_text(v).await?);
                    }
                    if max_rows == 0 {
                        rs.truncated = true;
                    } else {
                        rs.rows.push(cells);
                    }
                }
                // DDL and the like: an empty result is added below.
                HdbReturnValue::Success => continue,
            }
            rs.elapsed = start.elapsed();
            results.push(rs);
        }
        if results.is_empty() {
            results.push(ResultSet {
                statement: display.to_string(),
                elapsed: start.elapsed(),
                ..Default::default()
            });
        }
        Ok(results)
    }

    /// Runs ONE statement; at most `max_rows` rows per result (0 = discard rows).
    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        self.run(sql, None, max_rows).await
    }

    /// A SELECT with `?` parameters bound as text.
    pub async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[String],
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        self.run(sql, Some(parameters), max_rows).await
    }

    /// The rows of a catalog query.
    async fn rows(&mut self, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>> {
        Ok(self
            .run(sql, None, usize::MAX)
            .await?
            .into_iter()
            .next()
            .map(|rs| rs.rows)
            .unwrap_or_default())
    }

    /// The first value of a query.
    async fn scalar(&mut self, sql: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .rows(sql)
            .await?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .flatten())
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        self.scalar("SELECT DATABASE_NAME FROM SYS.M_DATABASE")
            .await?
            .ok_or_else(|| anyhow!("SAP HANA did not return the database name"))
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.current_database().await?])
    }

    /// Data editor changes in ONE transaction; each statement must affect exactly one row.
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        Box::pin(self.transaction(statements, true)).await
    }

    /// Schemas, tables/views and columns, without system objects.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let schemas = self
            .rows(&schemas_sql())
            .await?
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect();
        let objects = self.rows(&objects_sql()).await?;
        let columns = self.rows(&columns_sql()).await?;
        let keys = self.rows(&primary_keys_sql()).await?;
        Ok(build_schemas(schemas, &objects, &columns, &keys))
    }

    /// Product and version for Test Connection.
    pub async fn server_version(&mut self) -> Option<String> {
        self.scalar("SELECT VERSION FROM SYS.M_DATABASE")
            .await
            .ok()
            .flatten()
            .map(|v| version_label(&v))
    }

    /// Runs the statements in ONE transaction (all or nothing); the affected rows.
    pub async fn run_transaction(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        Box::pin(self.transaction(statements, false)).await
    }

    async fn transaction(
        &mut self,
        statements: &[String],
        single_row: bool,
    ) -> anyhow::Result<u64> {
        // Inside the console's Manual transaction: a savepoint, so a failure undoes only these
        // statements and nothing of the user's open work is committed here.
        let manual = self.manual;
        if manual {
            self.run("SAVEPOINT DATACRAFT_TX", None, 0).await?;
        } else {
            self.conn.set_auto_commit(false).await;
            self.connection_id = self.conn.id().await;
        }
        let mut total = 0;
        let mut result = Ok(());
        for stmt in statements {
            match self.run(stmt, None, 0).await {
                Ok(rs) => {
                    let n: u64 = rs.iter().filter_map(|r| r.affected).sum();
                    if single_row && n != 1 {
                        result = Err(super::not_single_row(n, stmt));
                        break;
                    }
                    total += n;
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if manual {
            if !self.broken {
                let undo = if result.is_ok() {
                    "RELEASE SAVEPOINT DATACRAFT_TX"
                } else {
                    "ROLLBACK TO SAVEPOINT DATACRAFT_TX"
                };
                let _ = self.run(undo, None, 0).await;
            }
            return result.map(|()| total);
        }
        if result.is_ok()
            && let Err(e) = self.conn.commit().await
        {
            result = Err(self.failed(e));
        }
        if result.is_err() && !self.broken {
            let _ = self.conn.rollback().await;
        }
        if !self.manual && !self.broken {
            self.conn.set_auto_commit(true).await;
            self.apply_read_only().await?;
        }
        result.map(|()| total)
    }

    /// Manual transactions (console): from here on nothing commits by itself until
    /// `end_transaction`.
    pub async fn begin(&mut self) -> anyhow::Result<()> {
        if !self.manual {
            self.conn.set_auto_commit(false).await;
            self.connection_id = self.conn.id().await;
            self.manual = true;
        }
        Ok(())
    }

    /// Commits or rolls back the Manual transaction and goes back to autocommit.
    pub async fn end_transaction(&mut self, commit: bool) -> anyhow::Result<()> {
        Box::pin(self.finish_transaction(commit)).await
    }

    async fn finish_transaction(&mut self, commit: bool) -> anyhow::Result<()> {
        let result = if commit {
            match self.conn.commit().await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let e = self.failed(e);
                    let _ = self.conn.rollback().await;
                    Err(e)
                }
            }
        } else {
            self.conn.rollback().await.map_err(|e| self.failed(e))
        };
        let checked = self.check_connection().await;
        self.manual = false;
        if !self.broken {
            self.conn.set_auto_commit(true).await;
            self.apply_read_only().await?;
        }
        result.and(checked)
    }

    pub fn is_closed(&self) -> bool {
        self.broken
    }
}

#[cfg(test)]
mod tests {
    //! Pure helpers, plus tests against a real SAP HANA when DATACRAFT_HANA_HOST is set (they
    //! return early otherwise): DATACRAFT_HANA_PORT (30015), DATACRAFT_HANA_DATABASE (tenant,
    //! optional), DATACRAFT_HANA_USER (SYSTEM), DATACRAFT_HANA_PASSWORD, DATACRAFT_HANA_TLS
    //! (`true`, or `insecure` to skip the certificate check).

    use super::*;
    use crate::db::Driver;

    fn cfg(host: &str, port: u16) -> ConnectionConfig {
        ConnectionConfig {
            driver: Driver::Hana,
            host: host.into(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn statements_lose_trailing_semicolons() {
        assert_eq!(
            clean_statement("SELECT 1 FROM DUMMY;"),
            "SELECT 1 FROM DUMMY"
        );
        assert_eq!(
            clean_statement("  SELECT ';' FROM DUMMY ;; \n"),
            "SELECT ';' FROM DUMMY"
        );
        assert_eq!(
            clean_statement("DO BEGIN SELECT 1 FROM DUMMY; END;"),
            "DO BEGIN SELECT 1 FROM DUMMY; END"
        );
        assert_eq!(clean_statement(" ; "), "");
    }

    #[test]
    fn dates_and_times_are_iso_like() {
        assert_eq!(
            format_datetime("2024-01-31T13:45:00.1230000"),
            "2024-01-31 13:45:00.123"
        );
        assert_eq!(
            format_datetime("2024-01-31T13:45:00.0000000"),
            "2024-01-31 13:45:00"
        );
        assert_eq!(
            format_datetime("2024-01-31T13:45:00.0000001"),
            "2024-01-31 13:45:00.0000001"
        );
        assert_eq!(
            format_datetime("2024-01-31T13:45:07"),
            "2024-01-31 13:45:07"
        );
    }

    #[tokio::test]
    async fn values_as_text() {
        assert_eq!(value_text(HdbValue::NULL).await.unwrap(), None);
        assert_eq!(
            value_text(HdbValue::BOOLEAN(true))
                .await
                .unwrap()
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            value_text(HdbValue::BIGINT(-42)).await.unwrap().as_deref(),
            Some("-42")
        );
        assert_eq!(
            value_text(HdbValue::BINARY(vec![0xCA, 0xFE]))
                .await
                .unwrap()
                .as_deref(),
            Some("0xCAFE")
        );
        assert_eq!(
            value_text(HdbValue::STRING("ñ".into()))
                .await
                .unwrap()
                .as_deref(),
            Some("ñ")
        );
        let array = HdbValue::ARRAY(vec![HdbValue::INT(1), HdbValue::NULL]);
        assert_eq!(
            value_text(array).await.unwrap().as_deref(),
            Some("[1, NULL]")
        );
    }

    #[test]
    fn column_types_are_readable() {
        assert_eq!(type_name("NVARCHAR", Some(50), None), "NVARCHAR(50)");
        assert_eq!(type_name("DECIMAL", Some(10), Some(2)), "DECIMAL(10,2)");
        assert_eq!(type_name("DECIMAL", Some(34), None), "DECIMAL");
        assert_eq!(type_name("INTEGER", Some(10), Some(0)), "INTEGER");
        assert_eq!(type_name("VARBINARY", Some(16), None), "VARBINARY(16)");
        assert_eq!(type_name("TIMESTAMP", Some(27), Some(7)), "TIMESTAMP");
        assert_eq!(type_name("NCLOB", None, None), "NCLOB");
        assert_eq!(type_name("FLOAT", Some(53), None), "FLOAT");
        assert_eq!(type_name("FLOAT", Some(24), None), "FLOAT(24)");
    }

    #[test]
    fn system_schemas_are_left_out() {
        for s in [
            "SYS",
            "_SYS_BIC",
            "_SYS_REPO",
            "SAP_XS_LM",
            "UIS",
            "HANA_XS_BASE",
        ] {
            assert!(is_system_schema(s), "{s}");
        }
        for s in ["SYSTEM", "SALES", "MY_SAP", "SAPHANADB", "DBADMIN"] {
            assert!(!is_system_schema(s), "{s}");
        }
        let filter = user_schema_filter();
        assert!(filter.contains("SCHEMA_NAME NOT IN ('SYS', 'UIS'"));
        assert!(filter.contains("SCHEMA_NAME NOT LIKE '\\_SYS%' ESCAPE '\\'"));
        assert!(filter.contains("SCHEMA_NAME NOT LIKE 'SAP\\_%' ESCAPE '\\'"));
        assert!(schemas_sql().contains("HAS_PRIVILEGES = 'TRUE'"));
        assert!(objects_sql().contains("IS_SYSTEM_TABLE = 'FALSE'"));
        assert!(columns_sql().ends_with("ORDER BY 1, 2, 8"));
    }

    #[test]
    fn tree_from_catalog_rows() {
        let s = |v: &str| Some(v.to_string());
        let objects = vec![
            vec![s("APP"), s("ORDERS"), s("T")],
            vec![s("APP"), s("V_ORDERS"), s("V")],
            vec![s("_SYS_BIC"), s("X"), s("V")],
        ];
        let columns = vec![
            vec![
                s("APP"),
                s("ORDERS"),
                s("ID"),
                s("INTEGER"),
                s("10"),
                s("0"),
                s("FALSE"),
                s("1"),
            ],
            vec![
                s("APP"),
                s("ORDERS"),
                s("TOTAL"),
                s("DECIMAL"),
                s("10"),
                s("2"),
                s("TRUE"),
                s("2"),
            ],
            vec![
                s("APP"),
                s("V_ORDERS"),
                s("ID"),
                s("INTEGER"),
                s("10"),
                s("0"),
                s("TRUE"),
                s("1"),
            ],
        ];
        let keys = vec![vec![s("APP"), s("ORDERS"), s("ID")]];
        let tree = build_schemas(
            vec!["APP".into(), "EMPTY".into(), "_SYS_BIC".into()],
            &objects,
            &columns,
            &keys,
        );
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "APP");
        assert_eq!(tree[1].name, "EMPTY");
        let orders = &tree[0].tables[0];
        assert_eq!(orders.name, "ORDERS");
        assert_eq!(orders.kind, ObjectKind::Table);
        assert!(orders.columns[0].primary_key && !orders.columns[0].nullable);
        assert_eq!(orders.columns[1].data_type, "DECIMAL(10,2)");
        assert!(orders.columns[1].nullable && !orders.columns[1].primary_key);
        assert_eq!(tree[0].tables[1].kind, ObjectKind::View);
    }

    #[test]
    fn version_labels() {
        assert_eq!(version_label("2.00.076.00.1705400033"), "SAP HANA 2.00.076");
        assert_eq!(version_label("4.00.000.00.1710841718"), "SAP HANA 4.00.000");
        assert_eq!(version_label("2.00"), "SAP HANA 2.00");
    }

    #[test]
    fn tls_from_properties_and_cloud() {
        let mut c = cfg("hana.local", 30015);
        assert_eq!(tls_mode(&c, "hana.local", 30015), TlsMode::Off);
        c.set_property("tls", "true");
        assert_eq!(tls_mode(&c, "hana.local", 30015), TlsMode::Verify);
        c.set_property("tls_insecure", "true");
        assert_eq!(tls_mode(&c, "hana.local", 30015), TlsMode::Insecure);
        let c = cfg("x", 443);
        assert_eq!(tls_mode(&c, "x", 443), TlsMode::Verify);
        let host = "abc.hana.prod-eu10.hanacloud.ondemand.com";
        assert_eq!(tls_mode(&cfg(host, 30015), host, 30015), TlsMode::Verify);
    }

    #[test]
    fn endpoint_from_the_host_field() {
        assert_eq!(
            endpoint(&cfg("hana.local", 0)),
            ("hana.local".into(), 30015)
        );
        assert_eq!(
            endpoint(&cfg("abc.hanacloud.ondemand.com:443", 30015)),
            ("abc.hanacloud.ondemand.com".into(), 443)
        );
        let mut tunnelled = cfg("127.0.0.1", 50123);
        tunnelled.tunnel_host = Some("hana.internal".into());
        assert_eq!(endpoint(&tunnelled), ("127.0.0.1".into(), 50123));
    }

    #[test]
    fn verified_tls_is_refused_through_a_tunnel() {
        let mut c = cfg("127.0.0.1", 50123);
        c.user = "SYSTEM".into();
        c.password = "x".into();
        c.tunnel_host = Some("hana.internal".into());
        c.set_property("tls", "true");
        assert!(connect_params(&c).is_err());
        c.set_property("tls_insecure", "true");
        assert!(connect_params(&c).unwrap().is_tls());
        c.tunnel_host = None;
        c.properties.clear();
        c.database = "HXE".into();
        let p = connect_params(&c).unwrap();
        assert!(!p.is_tls());
        assert_eq!(p.dbname(), Some("HXE"));
    }

    // ---- Live tests (DATACRAFT_HANA_HOST) ----

    fn live_cfg() -> Option<ConnectionConfig> {
        let host = std::env::var("DATACRAFT_HANA_HOST").ok()?;
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let mut c = cfg(
            &host,
            env("DATACRAFT_HANA_PORT")
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_PORT),
        );
        c.database = env("DATACRAFT_HANA_DATABASE").unwrap_or_default();
        c.user = env("DATACRAFT_HANA_USER").unwrap_or_else(|| "SYSTEM".into());
        c.password = env("DATACRAFT_HANA_PASSWORD").unwrap_or_default();
        match env("DATACRAFT_HANA_TLS").map(|v| v.to_ascii_lowercase()) {
            Some(v) if v == "insecure" => c.set_property("tls_insecure", "true"),
            Some(v) if v == "true" || v == "1" => c.set_property("tls", "true"),
            _ => {}
        }
        Some(c)
    }

    #[tokio::test]
    async fn live_queries_types_and_transactions() {
        let Some(c) = live_cfg() else { return };
        let mut s = HanaSession::connect(&c).await.unwrap();
        let version = s.server_version().await.unwrap();
        assert!(version.starts_with("SAP HANA "), "{version}");
        let db = s.current_database().await.unwrap();
        assert_eq!(s.list_databases().await.unwrap(), vec![db]);

        let rs = s
            .execute(
                "SELECT CAST(12.50 AS DECIMAL(10,2)) D, TO_DATE('2024-01-31') DT, \
                 TO_TIME('13:45:00') TM, TO_TIMESTAMP('2024-01-31 13:45:00.123') TS, \
                 TO_SECONDDATE('2024-01-31 13:45:00') SD, TO_BOOLEAN('TRUE') B, \
                 TO_NCLOB('long text') NC, TO_BLOB(TO_BINARY('CAFE')) BL, \
                 CAST(NULL AS INTEGER) N, TO_VARBINARY('AB') VB FROM DUMMY;",
                100,
            )
            .await
            .unwrap();
        let row = &rs[0].rows[0];
        let got: Vec<Option<&str>> = row.iter().map(|c| c.as_deref()).collect();
        assert_eq!(
            got,
            vec![
                Some("12.50"),
                Some("2024-01-31"),
                Some("13:45:00"),
                Some("2024-01-31 13:45:00.123"),
                Some("2024-01-31 13:45:00"),
                Some("true"),
                Some("long text"),
                Some("0xCAFE"),
                None,
                Some("0xAB"),
            ]
        );

        // Truncation and max_rows = 0.
        let many = "SELECT * FROM SYS.OBJECTS";
        let rs = s.execute(many, 3).await.unwrap();
        assert_eq!(rs[0].rows.len(), 3);
        assert!(rs[0].truncated);
        let rs = s.execute(many, 0).await.unwrap();
        assert!(rs[0].rows.is_empty() && rs[0].truncated);

        // Bound parameters.
        let rs = s
            .execute_bound(
                "SELECT ? + 1 AS A, ? AS B FROM DUMMY",
                &["41".into(), "x".into()],
                10,
            )
            .await
            .unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("42"));

        // DDL, DML, procedures and anonymous blocks.
        let _ = s.execute("DROP TABLE DC_HANA_T", 0).await;
        s.execute(
            "CREATE COLUMN TABLE DC_HANA_T (ID INT PRIMARY KEY, NAME NVARCHAR(20))",
            0,
        )
        .await
        .unwrap();
        let rs = s
            .execute("INSERT INTO DC_HANA_T VALUES (1, 'a')", 0)
            .await
            .unwrap();
        assert_eq!(rs[0].affected, Some(1));
        let rs = s
            .execute("DO BEGIN SELECT COUNT(*) AS C FROM DC_HANA_T; END;", 10)
            .await
            .unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("1"));
        let _ = s.execute("DROP PROCEDURE DC_HANA_P", 0).await;
        s.execute(
            "CREATE PROCEDURE DC_HANA_P AS BEGIN SELECT ID, NAME FROM DC_HANA_T; END",
            0,
        )
        .await
        .unwrap();
        let rs = s.execute("CALL DC_HANA_P()", 10).await.unwrap();
        assert_eq!(rs[0].columns, vec!["ID", "NAME"]);
        assert_eq!(rs[0].rows[0][1].as_deref(), Some("a"));

        // All or nothing.
        let bad = vec![
            "INSERT INTO DC_HANA_T VALUES (2, 'b')".to_string(),
            "INSERT INTO DC_HANA_T VALUES (1, 'dup')".to_string(),
        ];
        assert!(s.run_transaction(&bad).await.is_err());
        let count = "SELECT COUNT(*) FROM DC_HANA_T";
        assert_eq!(s.scalar(count).await.unwrap().as_deref(), Some("1"));
        let n = s
            .run_transaction(&["INSERT INTO DC_HANA_T VALUES (2, 'b')".to_string()])
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(
            s.apply_changes(&["UPDATE DC_HANA_T SET NAME = 'z'".to_string()])
                .await
                .is_err()
        );
        assert_eq!(
            s.scalar("SELECT COUNT(*) FROM DC_HANA_T WHERE NAME = 'z'")
                .await
                .unwrap()
                .as_deref(),
            Some("0")
        );

        // Manual transactions.
        s.begin().await.unwrap();
        s.execute("DELETE FROM DC_HANA_T", 0).await.unwrap();
        s.end_transaction(false).await.unwrap();
        assert_eq!(s.scalar(count).await.unwrap().as_deref(), Some("2"));
        s.begin().await.unwrap();
        s.execute("DELETE FROM DC_HANA_T WHERE ID = 2", 0)
            .await
            .unwrap();
        s.end_transaction(true).await.unwrap();
        assert_eq!(s.scalar(count).await.unwrap().as_deref(), Some("1"));

        // The explorer sees the table with its key.
        let tree = s.introspect().await.unwrap();
        assert!(tree.iter().all(|schema| !is_system_schema(&schema.name)));
        let table = tree
            .iter()
            .flat_map(|schema| &schema.tables)
            .find(|t| t.name == "DC_HANA_T")
            .expect("DC_HANA_T in the tree");
        assert_eq!(table.columns[0].name, "ID");
        assert!(table.columns[0].primary_key);
        assert_eq!(table.columns[1].data_type, "NVARCHAR(20)");

        s.execute("DROP PROCEDURE DC_HANA_P", 0).await.unwrap();
        s.execute("DROP TABLE DC_HANA_T", 0).await.unwrap();
        let err = s.execute("SELECT * FROM DC_HANA_MISSING", 10).await;
        assert!(err.unwrap_err().to_string().starts_with('['));
        assert!(!s.is_closed());
    }
}
