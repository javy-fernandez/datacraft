//! MySQL and MariaDB driver (mysql_async). One session per connection; `USE db` switches
//! databases. In the explorer each database has a single schema with its own name.
//!
//! Queries use the text protocol (`query_iter`): the server already sends every value as text,
//! formatted the MySQL way (dates `2026-10-16 12:30:00`, exact decimals...). Only binary values
//! (BLOB, BINARY, geometry) are turned into a hex preview and BIT into a number.
//!
//! TLS follows the data source's SSL settings (`tls::TlsSettings`). With the default, Prefer, TLS
//! is attempted first without checking the certificate, and a server without it (typical local
//! dev server) gets a plain connection.
//!
//! `list_databases` hides `information_schema` and `performance_schema` (read-only catalogs that
//! are rarely browsed) and leaves `mysql` and `sys` at the end of the list.

use std::time::{Duration, Instant};

use anyhow::anyhow;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{ClientIdentity, Conn, DriverError, OptsBuilder, SslOpts, Value};

use super::tls::{self, TlsMode};
use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::db::sqlite::hex_preview;
use crate::i18n::{tr, trf};

/// Character set number MySQL uses for binary strings.
const BINARY_CHARSET: u16 = 63;

/// System databases hidden from the list.
const HIDDEN_DATABASES: [&str; 2] = ["information_schema", "performance_schema"];
/// System databases shown after the user ones.
const SYSTEM_DATABASES: [&str; 2] = ["mysql", "sys"];

pub struct MySession {
    conn: Conn,
    /// Options the connection was opened with (to open the one that cancels its queries).
    opts: OptsBuilder,
    /// Set on I/O or protocol errors: the connection is no longer usable.
    broken: bool,
}

/// Readable message for server errors (code, SQLSTATE and message).
fn my_error(e: mysql_async::Error) -> anyhow::Error {
    match e {
        mysql_async::Error::Server(s) => anyhow!(trf(
            "Error {code} (SQLSTATE {state}): {message}",
            &[("code", &s.code), ("state", &s.state), ("message", &s.message)]
        )),
        other => anyhow!(other.to_string()),
    }
}

/// Server errors (syntax, duplicate key, permissions...) leave the connection usable, except the
/// ones the server sends right before closing it.
fn breaks_connection(e: &mysql_async::Error) -> bool {
    match e {
        // 1053 shutdown in progress, 1927 connection killed (MariaDB), 4031 idle timeout (MySQL 8).
        mysql_async::Error::Server(s) => matches!(s.code, 1053 | 1927 | 4031),
        _ => true,
    }
}

/// The connection's TLS from the data source's SSL settings; None for Disable.
fn tls_options(cfg: &ConnectionConfig) -> anyhow::Result<Option<SslOpts>> {
    let t = &cfg.tls;
    let mut ssl = SslOpts::default();
    match t.mode {
        TlsMode::Disable => return Ok(None),
        TlsMode::Prefer | TlsMode::Require => {
            ssl = ssl.with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
        }
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            if !t.ca_file.trim().is_empty() {
                // Read now: the file is checked on every connect, and a missing one says so.
                tls::certificates(&t.ca_file)?;
                let pem = std::fs::read(tls::path(&t.ca_file))?;
                ssl = ssl.with_root_certs(vec![pem.into()]).with_disable_built_in_roots(true);
            }
            if t.mode == TlsMode::VerifyCa {
                ssl = ssl.with_danger_skip_domain_validation(true);
            } else if let Some(name) = &cfg.tunnel_host {
                // Through an SSH tunnel: the certificate is for the database's host, not 127.0.0.1.
                ssl = ssl.with_danger_tls_hostname_override(Some(name.clone()));
            }
        }
    }
    match (t.cert_file.trim().is_empty(), t.key_file.trim().is_empty()) {
        (true, true) => {}
        (false, false) => {
            tls::certificates(&t.cert_file)?;
            let cert = std::fs::read(tls::path(&t.cert_file))?;
            let key = tls::private_key_pem(&t.key_file, &t.key_passphrase)?;
            ssl = ssl.with_client_identity(Some(ClientIdentity::new(cert.into(), key.into())));
        }
        (false, true) => anyhow::bail!(tr("Choose the client key of the client certificate")),
        (true, false) => anyhow::bail!(tr("Choose the client certificate of the client key")),
    }
    Ok(Some(ssl))
}

impl MySession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let port = if cfg.port == 0 { 3306 } else { cfg.port };
        let database = cfg.database.trim();

        let base = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(port)
            .user(Some(cfg.user.as_str()))
            .pass(Some(cfg.password.as_str()))
            .db_name((!database.is_empty()).then_some(database))
            // Always TCP to the configured host, never the local unix socket.
            .prefer_socket(false)
            .tcp_nodelay(true)
            // Affected rows = matched rows, so an UPDATE that writes the same value counts as 1.
            .client_found_rows(true)
            .stmt_cache_size(0);
        let (conn, opts) = match tls_options(cfg)? {
            None => (open(base.clone()).await, base),
            Some(ssl) => {
                let with_tls = base.clone().ssl_opts(ssl);
                match open(with_tls.clone()).await {
                    // Prefer: a server without TLS gets a plain connection.
                    Err(Some(mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer))) if cfg.tls.mode == TlsMode::Prefer => {
                        (open(base.clone()).await, base)
                    }
                    Err(Some(mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer))) => {
                        anyhow::bail!(trf("{host}:{port} doesn't offer TLS: choose Prefer or Disable in the SSL settings", &[("host", &host), ("port", &port)]))
                    }
                    other => (other, with_tls),
                }
            }
        };
        let name = cfg.tunnel_host.clone().unwrap_or_else(|| host.to_string());
        if let Err(Some(e)) = &conn
            && let Some(reason) = tls::explain(e, &cfg.tls, &name)
        {
            anyhow::bail!(reason);
        }
        let conn = conn.map_err(|e| match e {
            None => anyhow!(trf("Timed out connecting to {host}:{port}", &[("host", &host), ("port", &port)])),
            Some(mysql_async::Error::Io(io)) => anyhow!(trf(
                "Could not connect to {host}:{port}",
                &[("host", &host), ("port", &port)]
            ))
            .context(io.to_string()),
            Some(other) => my_error(other),
        })?;
        Ok(Self { conn, opts, broken: false })
    }

    /// "TLSv1.3 TLS_AES_256_GCM_SHA384", or None for a plain connection.
    pub async fn tls_description(&mut self) -> Option<String> {
        let rows: Vec<(String, String)> =
            self.conn.query("SHOW SESSION STATUS WHERE Variable_name IN ('Ssl_version', 'Ssl_cipher')").await.ok()?;
        let get = |name: &str| rows.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
        Some(format!("{} {}", get("Ssl_version")?, get("Ssl_cipher").unwrap_or_default()).trim().to_string())
    }

    /// Options and server thread id to `KILL QUERY` this session's running statement.
    pub fn cancel_target(&self) -> (OptsBuilder, u32) {
        (self.opts.clone().db_name(None::<String>), self.conn.id())
    }

    /// Stream full native values without exposing the driver connection.
    pub(super) async fn export_rows(
        &mut self,
        sql: &str,
        columns: &[ColumnInfo],
        driver: super::Driver,
        sender: &tokio::sync::mpsc::Sender<super::table_export::Message>,
        progress: &std::sync::Arc<super::table_export::ExportProgress>,
    ) -> anyhow::Result<()> {
        use anyhow::Context as _;
        use super::table_export::{Message, cancelled, mysql_value};
        use mysql_async::prelude::Queryable;
        let mut rows = self.conn.query_iter(sql).await?;
        let metadata = rows.columns().context(tr("Missing export column metadata"))?;
        while let Some(row) = rows.next().await? {
            cancelled(progress)?;
            let values = columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let value = row.as_ref(i).context(tr("Missing export cell"))?;
                    mysql_value(value, metadata.get(i).context(tr("Missing export column metadata"))?, c, driver)
                })
                .collect::<anyhow::Result<_>>()?;
            sender.send(Message::Row(values)).await.context(tr("Export writer stopped"))?;
        }
        Ok(())
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        if self.broken {
            anyhow::bail!(tr("The connection is not open"));
        }
        match run(&mut self.conn, sql, max_rows).await {
            Ok(results) => Ok(results),
            Err(e) => {
                if breaks_connection(&e) {
                    self.broken = true;
                    return Err(anyhow!(trf("Connection lost. {detail}", &[("detail", &e)])));
                }
                Err(my_error(e))
            }
        }
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        if let Some(db) = self.scalar_strings("SELECT DATABASE()").await?.into_iter().next() {
            return Ok(db);
        }
        // Connection without a default database: pick the first user database.
        let dbs = self.list_databases().await?;
        let db = dbs
            .iter()
            .find(|d| !is_system_database(d))
            .or_else(|| dbs.iter().find(|d| d.as_str() == "mysql"))
            .or(dbs.first())
            .cloned()
            .ok_or_else(|| anyhow!(tr("The server did not return the current database")))?;
        self.use_database(&db).await?;
        Ok(db)
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        let mut dbs: Vec<String> = self
            .scalar_strings("SHOW DATABASES")
            .await?
            .into_iter()
            .filter(|d| !HIDDEN_DATABASES.contains(&d.to_ascii_lowercase().as_str()))
            .collect();
        dbs.sort_by_key(|d| (is_system_database(d), d.to_lowercase()));
        Ok(dbs)
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        let sql = format!("USE `{}`", db.replace('`', "``"));
        self.execute(&sql, 0).await.map(|_| ())
    }

    /// First column of each row as text (NULLs skipped).
    async fn scalar_strings(&mut self, sql: &str) -> anyhow::Result<Vec<String>> {
        let rs = self.execute(sql, usize::MAX).await?;
        Ok(rs.into_iter().flat_map(|r| r.rows).filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Runs a statement and returns the affected rows, marking the session broken on I/O errors.
    async fn exec_affected(&mut self, sql: &str) -> anyhow::Result<u64> {
        let outcome = async {
            let result = self.conn.query_iter(sql).await?;
            let n = result.affected_rows();
            result.drop_result().await?;
            Ok::<_, mysql_async::Error>(n)
        }
        .await;
        outcome.map_err(|e| {
            if breaks_connection(&e) {
                self.broken = true;
            }
            my_error(e)
        })
    }

    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        if self.broken {
            anyhow::bail!(tr("The connection is not open"));
        }
        self.exec_affected("START TRANSACTION").await?;
        let mut outcome: anyhow::Result<u64> = Ok(0);
        for stmt in statements {
            match self.exec_affected(stmt).await {
                Ok(1) => {
                    if let Ok(total) = &mut outcome {
                        *total += 1;
                    }
                }
                Ok(n) => {
                    outcome = Err(super::not_single_row(n, stmt));
                    break;
                }
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        match outcome {
            Ok(total) => {
                self.exec_affected("COMMIT").await?;
                Ok(total)
            }
            Err(e) => {
                if !self.broken {
                    let _ = self.exec_affected("ROLLBACK").await;
                }
                Err(e)
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.broken
    }

    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        const SQL: &str = "
            SELECT DATABASE(), t.TABLE_NAME, t.TABLE_TYPE,
                   c.COLUMN_NAME, LOWER(c.COLUMN_TYPE), c.IS_NULLABLE, c.COLUMN_KEY
            FROM information_schema.TABLES t
            LEFT JOIN information_schema.COLUMNS c
                   ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME
            WHERE t.TABLE_SCHEMA = DATABASE()
            ORDER BY t.TABLE_NAME, c.ORDINAL_POSITION";

        let rs = self.execute(SQL, usize::MAX).await?;
        let rows = rs.into_iter().next().map(|r| r.rows).unwrap_or_default();
        let mut schema: Option<SchemaInfo> = None;
        for row in rows {
            let text = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            let s = schema.get_or_insert_with(|| SchemaInfo { name: text(0), tables: Vec::new() });
            let table = text(1);
            if s.tables.last().is_none_or(|t| t.name != table) {
                s.tables.push(TableInfo {
                    name: table,
                    // BASE TABLE, SYSTEM VERSIONED (MariaDB), VIEW, SYSTEM VIEW...
                    kind: if text(2).ends_with("VIEW") { ObjectKind::View } else { ObjectKind::Table },
                    columns: Vec::new(),
                });
            }
            let Some(t) = s.tables.last_mut() else { continue };
            // Table without visible columns: the LEFT JOIN leaves the column NULL.
            let Some(Some(column)) = row.get(3).cloned() else { continue };
            t.columns.push(ColumnInfo {
                name: column,
                data_type: text(4),
                nullable: text(5) == "YES",
                primary_key: text(6) == "PRI",
            });
        }
        let schema = match schema {
            Some(s) => s,
            // Empty database: still one schema named after it.
            None => SchemaInfo { name: self.current_database().await?, tables: Vec::new() },
        };
        Ok(vec![schema])
    }
}

fn is_system_database(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SYSTEM_DATABASES.contains(&lower.as_str()) || HIDDEN_DATABASES.contains(&lower.as_str())
}

/// Opens the connection with a 10 s timeout. `Err(None)` = timeout.
async fn open(opts: OptsBuilder) -> Result<Conn, Option<mysql_async::Error>> {
    match tokio::time::timeout(Duration::from_secs(10), Conn::new(opts)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(Some(e)),
        Err(_) => Err(None),
    }
}

async fn run(conn: &mut Conn, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>, mysql_async::Error> {
    let statement = sql.trim().to_string();
    let mut start = Instant::now();
    let mut result = conn.query_iter(sql).await?;

    let mut results: Vec<ResultSet> = Vec::new();
    // Every result set must be read to the end even when truncating; otherwise the connection is
    // left half-read. `next()` returns None at the end of each set and moves on to the next one.
    while let Some(columns) = result.columns() {
        if columns.is_empty() {
            let affected = result.affected_rows();
            result.next().await?;
            results.push(ResultSet {
                statement: statement.clone(),
                affected: Some(affected),
                elapsed: start.elapsed(),
                ..Default::default()
            });
        } else {
            let mut rs = ResultSet {
                statement: statement.clone(),
                columns: columns.iter().map(|c| c.name_str().into_owned()).collect(),
                ..Default::default()
            };
            while let Some(row) = result.next().await? {
                if rs.rows.len() >= max_rows {
                    rs.truncated = true;
                    continue;
                }
                let cells = columns
                    .iter()
                    .enumerate()
                    .map(|(i, col)| {
                        let value = row.as_ref(i).unwrap_or(&Value::NULL);
                        cell(value, col.column_type(), col.character_set())
                    })
                    .collect();
                rs.rows.push(cells);
            }
            rs.elapsed = start.elapsed();
            results.push(rs);
        }
        start = Instant::now();
    }

    // CALL ends with an OK packet without rows: hide it when the procedure returned result sets.
    if results.len() > 1 && results.iter().any(|r| !r.is_command()) {
        results.retain(|r| !r.is_command() || r.affected.is_some_and(|n| n > 0));
    }
    Ok(results)
}

/// Text of a value. With the text protocol almost everything arrives as `Bytes` already formatted.
fn cell(value: &Value, column_type: ColumnType, charset: u16) -> Cell {
    match value {
        Value::NULL => None,
        Value::Bytes(bytes) => Some(bytes_text(bytes, column_type, charset)),
        Value::Int(v) => Some(v.to_string()),
        Value::UInt(v) => Some(v.to_string()),
        Value::Float(v) => Some(v.to_string()),
        Value::Double(v) => Some(v.to_string()),
        Value::Date(y, mo, d, h, mi, s, us) => Some(date_text(*y, *mo, *d, *h, *mi, *s, *us, column_type)),
        Value::Time(neg, days, h, mi, s, us) => Some(time_text(*neg, *days, *h, *mi, *s, *us)),
    }
}

fn bytes_text(bytes: &[u8], column_type: ColumnType, charset: u16) -> String {
    match column_type {
        // BIT(n) arrives as big-endian bytes.
        ColumnType::MYSQL_TYPE_BIT if bytes.len() <= 8 => {
            bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)).to_string()
        }
        ColumnType::MYSQL_TYPE_GEOMETRY => hex_preview(bytes),
        ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_VARCHAR
        | ColumnType::MYSQL_TYPE_BLOB
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
            if charset == BINARY_CHARSET =>
        {
            hex_preview(bytes)
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn date_text(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32, column_type: ColumnType) -> String {
    let date = format!("{y:04}-{mo:02}-{d:02}");
    if column_type == ColumnType::MYSQL_TYPE_DATE {
        return date;
    }
    let mut out = format!("{date} {h:02}:{mi:02}:{s:02}");
    if us > 0 {
        out.push_str(&format!(".{us:06}"));
    }
    out
}

fn time_text(neg: bool, days: u32, h: u8, mi: u8, s: u8, us: u32) -> String {
    let hours = days * 24 + u32::from(h);
    let mut out = format!("{}{hours:02}:{mi:02}:{s:02}", if neg { "-" } else { "" });
    if us > 0 {
        out.push_str(&format!(".{us:06}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_settings_become_mysql_async_options() {
        let pki = tls::test_pki::pki();
        let mut cfg = ConnectionConfig::new(1, crate::db::Driver::MySql);
        cfg.tls.mode = TlsMode::Disable;
        assert!(tls_options(&cfg).unwrap().is_none());
        for mode in [TlsMode::Prefer, TlsMode::Require] {
            cfg.tls.mode = mode;
            let o = tls_options(&cfg).unwrap().unwrap();
            assert!(o.accept_invalid_certs() && o.skip_domain_validation(), "{mode:?}");
        }
        cfg.tls = tls::TlsSettings { mode: TlsMode::VerifyFull, ca_file: pki.ca_file.clone(), ..Default::default() };
        let o = tls_options(&cfg).unwrap().unwrap();
        assert!(!o.accept_invalid_certs() && !o.skip_domain_validation() && o.disable_built_in_roots());
        assert_eq!(o.root_certs().len(), 1);
        assert_eq!(o.tls_hostname_override(), None);
        // Through an SSH tunnel the name to check is the database's.
        cfg.tunnel_host = Some("db.internal".into());
        assert_eq!(tls_options(&cfg).unwrap().unwrap().tls_hostname_override(), Some("db.internal"));
        cfg.tls.mode = TlsMode::VerifyCa;
        assert!(tls_options(&cfg).unwrap().unwrap().skip_domain_validation());
        // A client identity, decrypted when the key has a passphrase.
        cfg.tls.cert_file = pki.client_cert.clone();
        cfg.tls.key_file = pki.client_key_encrypted.clone();
        assert!(tls_options(&cfg).is_err());
        cfg.tls.key_passphrase = "pass phrase".into();
        assert!(tls_options(&cfg).unwrap().unwrap().client_identity().is_some());
    }

    #[test]
    fn text_values() {
        let t = |b: &[u8], ty, cs| cell(&Value::Bytes(b.to_vec()), ty, cs);
        assert_eq!(t(b"12.50", ColumnType::MYSQL_TYPE_NEWDECIMAL, BINARY_CHARSET), Some("12.50".into()));
        assert_eq!(t("ñandú €".as_bytes(), ColumnType::MYSQL_TYPE_VAR_STRING, 255), Some("ñandú €".into()));
        assert_eq!(t(b"2026-10-16 12:30:00", ColumnType::MYSQL_TYPE_DATETIME, BINARY_CHARSET), Some("2026-10-16 12:30:00".into()));
        assert_eq!(t(&[0xDE, 0xAD], ColumnType::MYSQL_TYPE_BLOB, BINARY_CHARSET), Some("0xDEAD".into()));
        assert_eq!(t(&[0xDE, 0xAD], ColumnType::MYSQL_TYPE_STRING, BINARY_CHARSET), Some("0xDEAD".into()));
        assert_eq!(t(b"long text", ColumnType::MYSQL_TYPE_BLOB, 255), Some("long text".into()));
        assert_eq!(t(&[0x01, 0x02], ColumnType::MYSQL_TYPE_BIT, BINARY_CHARSET), Some("258".into()));
        assert_eq!(t(&[0x00, 0x01], ColumnType::MYSQL_TYPE_GEOMETRY, BINARY_CHARSET), Some("0x0001".into()));
        assert_eq!(cell(&Value::NULL, ColumnType::MYSQL_TYPE_LONG, BINARY_CHARSET), None);
    }

    #[test]
    fn binary_protocol_values() {
        let c = |v: Value, ty| cell(&v, ty, BINARY_CHARSET).unwrap();
        assert_eq!(c(Value::Int(-5), ColumnType::MYSQL_TYPE_LONG), "-5");
        assert_eq!(c(Value::UInt(18446744073709551615), ColumnType::MYSQL_TYPE_LONGLONG), "18446744073709551615");
        assert_eq!(c(Value::Date(2026, 10, 16, 0, 0, 0, 0), ColumnType::MYSQL_TYPE_DATE), "2026-10-16");
        assert_eq!(c(Value::Date(2026, 10, 16, 9, 5, 3, 0), ColumnType::MYSQL_TYPE_DATETIME), "2026-10-16 09:05:03");
        assert_eq!(
            c(Value::Date(2026, 10, 16, 9, 5, 3, 1500), ColumnType::MYSQL_TYPE_TIMESTAMP),
            "2026-10-16 09:05:03.001500"
        );
        assert_eq!(c(Value::Time(true, 1, 2, 3, 4, 0), ColumnType::MYSQL_TYPE_TIME), "-26:03:04");
    }

    #[test]
    fn system_databases() {
        assert!(is_system_database("mysql"));
        assert!(is_system_database("SYS"));
        assert!(is_system_database("information_schema"));
        assert!(!is_system_database("shop"));
    }
}

#[cfg(test)]
mod live_tests {
    //! Tests against a real MySQL/MariaDB. Run them with `cargo test -- --ignored` and the variables:
    //! DATACRAFT_MY_HOST (required; skipped without it), DATACRAFT_MY_PORT (3306), DATACRAFT_MY_USER (root),
    //! DATACRAFT_MY_PASSWORD, DATACRAFT_MY_DB (empty = no default database).

    use super::*;
    use crate::db::Driver;

    fn env(name: &str, default: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| default.to_string())
    }

    fn config() -> Option<ConnectionConfig> {
        let Ok(host) = std::env::var("DATACRAFT_MY_HOST") else {
            eprintln!("DATACRAFT_MY_HOST not set: skipping the MySQL test");
            return None;
        };
        let mut cfg = ConnectionConfig::new(1, Driver::MySql);
        cfg.host = host;
        cfg.port = env("DATACRAFT_MY_PORT", "3306").parse().expect("port");
        cfg.user = env("DATACRAFT_MY_USER", "root");
        cfg.password = env("DATACRAFT_MY_PASSWORD", "");
        cfg.database = env("DATACRAFT_MY_DB", "");
        Some(cfg)
    }

    async fn session() -> Option<MySession> {
        let cfg = config()?;
        Some(MySession::connect(&cfg).await.expect("connection"))
    }

    /// Fresh scratch database selected in the session.
    async fn scratch(s: &mut MySession, name: &str) {
        s.execute(&format!("DROP DATABASE IF EXISTS `{name}`"), 10).await.unwrap();
        s.execute(&format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4"), 10).await.unwrap();
        s.use_database(name).await.unwrap();
    }

    fn row(rs: &ResultSet, i: usize) -> Vec<Option<&str>> {
        rs.rows[i].iter().map(|c| c.as_deref()).collect()
    }

    #[tokio::test]
    #[ignore]
    async fn connect_list_and_current_database() {
        let Some(mut s) = session().await else { return };
        let dbs = s.list_databases().await.unwrap();
        assert!(dbs.iter().any(|d| d == "mysql"), "{dbs:?}");
        assert!(!dbs.iter().any(|d| d == "information_schema" || d == "performance_schema"));
        // System databases at the end.
        let first_system = dbs.iter().position(|d| is_system_database(d)).unwrap();
        assert!(dbs[first_system..].iter().all(|d| is_system_database(d)), "{dbs:?}");

        let current = s.current_database().await.unwrap();
        assert!(!current.is_empty());
        let rs = s.execute("SELECT DATABASE()", 10).await.unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some(current.as_str()));
        assert!(!s.is_closed());
    }

    #[tokio::test]
    #[ignore]
    async fn tls_when_the_server_supports_it() {
        let Some(mut s) = session().await else { return };
        let have_ssl = s.execute("SHOW VARIABLES LIKE 'have_ssl'", 10).await.unwrap();
        let cipher = s.execute("SHOW SESSION STATUS LIKE 'Ssl_cipher'", 10).await.unwrap();
        let have_ssl = have_ssl[0].rows.first().and_then(|r| r[1].clone()).unwrap_or_default();
        let cipher = cipher[0].rows.first().and_then(|r| r[1].clone()).unwrap_or_default();
        eprintln!("have_ssl={have_ssl} cipher={cipher}");
        assert_eq!(have_ssl == "YES", !cipher.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn bad_password_fails() {
        let Some(mut cfg) = config() else { return };
        cfg.password = "definitely-wrong-password".into();
        let err = MySession::connect(&cfg).await.err().expect("must fail");
        assert!(err.to_string().contains("1045"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn types_truncation_and_errors() {
        let Some(mut s) = session().await else { return };
        scratch(&mut s, "dc_my_types").await;
        let rs = s
            .execute(
                "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(50), price DECIMAL(10,2), \
                 born DATE, at DATETIME(3), data BLOB, flag BIT(3), ok BOOLEAN, t TIME)",
                10,
            )
            .await
            .unwrap();
        assert!(rs[0].is_command());
        let rs = s
            .execute(
                "INSERT INTO items VALUES \
                 (1, 'ñandú 😀', 12.50, '2026-10-16', '2026-10-16 12:30:45.123', X'DEADBEEF', b'101', TRUE, '-26:03:04'), \
                 (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
                 (3, 'c', 0, '1999-01-02', '1999-01-02 00:00:00', '', b'0', FALSE, '00:00:01')",
                10,
            )
            .await
            .unwrap();
        assert_eq!(rs[0].affected, Some(3));

        let rs = s.execute("SELECT * FROM items ORDER BY id", 2).await.unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].columns, ["id", "name", "price", "born", "at", "data", "flag", "ok", "t"]);
        assert_eq!(rs[0].rows.len(), 2);
        assert!(rs[0].truncated);
        assert_eq!(
            row(&rs[0], 0),
            [
                Some("1"),
                Some("ñandú 😀"),
                Some("12.50"),
                Some("2026-10-16"),
                Some("2026-10-16 12:30:45.123"),
                Some("0xDEADBEEF"),
                Some("5"),
                Some("1"),
                Some("-26:03:04")
            ]
        );
        assert!(row(&rs[0], 1)[1..].iter().all(Option::is_none));

        // The session stays usable after the truncated read.
        let rs = s.execute("SELECT COUNT(*) AS n FROM items WHERE 1 = 0", 10).await.unwrap();
        assert_eq!(row(&rs[0], 0), [Some("0")]);
        let rs = s.execute("SELECT * FROM items WHERE 1 = 0", 10).await.unwrap();
        assert_eq!(rs[0].columns.len(), 9);
        assert!(rs[0].rows.is_empty() && !rs[0].truncated);

        // SQL errors don't close the session.
        let err = s.execute("SELEC nonsense", 10).await.unwrap_err();
        assert!(err.to_string().contains("1064"), "{err}");
        let err = s.execute("INSERT INTO items (id) VALUES (1)", 10).await.unwrap_err();
        assert!(err.to_string().contains("1062"), "{err}");
        assert!(!s.is_closed());
        assert_eq!(s.execute("SELECT 1", 10).await.unwrap()[0].rows[0][0].as_deref(), Some("1"));

        let rs = s.execute("UPDATE items SET price = 0", 10).await.unwrap();
        // client_found_rows: matched rows, including row 3 that already had 0.
        assert_eq!(rs[0].affected, Some(3));

        // Procedure with two result sets.
        s.execute("CREATE PROCEDURE two() BEGIN SELECT 1 AS a; SELECT 'x' AS b, 2 AS c; END", 10)
            .await
            .unwrap();
        let rs = s.execute("CALL two()", 10).await.unwrap();
        assert_eq!(rs.len(), 2, "{rs:?}");
        assert_eq!(rs[0].columns, ["a"]);
        assert_eq!(row(&rs[1], 0), [Some("x"), Some("2")]);

        s.execute("DROP DATABASE dc_my_types", 10).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn apply_changes_commit_and_rollback() {
        let Some(mut s) = session().await else { return };
        scratch(&mut s, "dc_my_apply").await;
        s.execute("CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10)) ENGINE=InnoDB", 10).await.unwrap();
        s.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')", 10).await.unwrap();

        let n = s
            .apply_changes(&["UPDATE t SET v = 'z' WHERE id = 1".into(), "UPDATE t SET v = 'b' WHERE id = 2".into()])
            .await
            .unwrap();
        assert_eq!(n, 2);

        let err = s
            .apply_changes(&["UPDATE t SET v = 'q' WHERE id = 1".into(), "DELETE FROM t WHERE id = 99".into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");
        let err = s
            .apply_changes(&["UPDATE t SET v = 'q' WHERE id = 1".into(), "INSERT INTO t VALUES (2, 'dup')".into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1062"), "{err}");
        assert!(!s.is_closed());

        let rs = s.execute("SELECT v FROM t ORDER BY id", 10).await.unwrap();
        assert_eq!(row(&rs[0], 0), [Some("z")]);
        assert_eq!(row(&rs[0], 1), [Some("b")]);
        s.execute("DROP DATABASE dc_my_apply", 10).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn introspect_and_use_database() {
        let Some(mut s) = session().await else { return };
        scratch(&mut s, "dc_my_intro_b").await;
        scratch(&mut s, "dc_my_intro").await;
        s.execute(
            "CREATE TABLE items (id INT NOT NULL, code VARCHAR(20) NOT NULL, price DECIMAL(10,2), \
             PRIMARY KEY (id, code))",
            10,
        )
        .await
        .unwrap();
        s.execute("CREATE VIEW v_items AS SELECT id FROM items", 10).await.unwrap();

        let schemas = s.introspect().await.unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "dc_my_intro");
        let tables = &schemas[0].tables;
        assert_eq!(tables.len(), 2);
        let items = &tables[0];
        assert_eq!(items.name, "items");
        assert_eq!(items.kind, ObjectKind::Table);
        let names: Vec<_> = items.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "code", "price"]);
        // MySQL 8 omits the display width of int; MariaDB shows int(11).
        assert!(matches!(items.columns[0].data_type.as_str(), "int" | "int(11)"), "{:?}", items.columns[0]);
        assert_eq!(items.columns[1].data_type, "varchar(20)");
        assert_eq!(items.columns[2].data_type, "decimal(10,2)");
        assert!(items.columns[0].primary_key && items.columns[1].primary_key && !items.columns[2].primary_key);
        assert!(!items.columns[0].nullable && items.columns[2].nullable);
        assert_eq!(tables[1].name, "v_items");
        assert_eq!(tables[1].kind, ObjectKind::View);

        // Switch database: empty one, still one schema with its name.
        s.use_database("dc_my_intro_b").await.unwrap();
        assert_eq!(s.current_database().await.unwrap(), "dc_my_intro_b");
        let schemas = s.introspect().await.unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "dc_my_intro_b");
        assert!(schemas[0].tables.is_empty());

        assert!(s.use_database("dc_does_not_exist").await.is_err());
        assert!(!s.is_closed());

        s.execute("DROP DATABASE dc_my_intro", 10).await.unwrap();
        s.execute("DROP DATABASE dc_my_intro_b", 10).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn killed_connection_marks_closed() {
        let Some(mut s) = session().await else { return };
        let id = s.execute("SELECT CONNECTION_ID()", 10).await.unwrap()[0].rows[0][0].clone().unwrap();
        let Some(mut other) = session().await else { return };
        other.execute(&format!("KILL {id}"), 10).await.unwrap();
        assert!(s.execute("SELECT 1", 10).await.is_err());
        assert!(s.is_closed());
    }
}
