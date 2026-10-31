//! Data access layer: common types and dispatch to each driver.
//!
//! Every driver converts values to text (`Cell`) so the UI doesn't need to
//! know each engine's native types.

pub mod alter;
pub mod copy;
pub mod create_db;
pub mod datagrip;
pub mod backup;
pub mod clickhouse;
pub mod influx;
pub mod kusto;
pub mod mongo;
pub mod mssql;
pub mod object_info;
pub mod schema_search;
pub mod script;
pub mod subset;
pub mod mysql;
pub mod oracle;
pub mod plan;
pub mod postgres;
pub mod redis_db;
pub mod routines;
pub mod snowflake;
pub mod ssh;
pub mod tls;
pub mod sqlite;
pub mod url;
pub mod views;
pub mod users;

use serde::{Deserialize, Serialize};
use crate::i18n::trf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Driver {
    Postgres,
    Sqlite,
    MsSql,
    MySql,
    MariaDb,
    Oracle,
    Snowflake,
    ClickHouse,
    /// Redis and Valkey (same protocol).
    Redis,
    MongoDb,
    /// InfluxDB 2 (HTTP API: Flux, and InfluxQL through the v1 compatibility endpoint).
    InfluxDb,
    /// Kusto: Azure Data Explorer, Fabric Eventhouses and the Kusto emulator (REST API, KQL).
    Kusto,
}

impl Driver {
    pub const ALL: [Driver; 12] = [
        Driver::Postgres,
        Driver::MySql,
        Driver::MariaDb,
        Driver::MsSql,
        Driver::Oracle,
        Driver::Snowflake,
        Driver::ClickHouse,
        Driver::Sqlite,
        Driver::MongoDb,
        Driver::Redis,
        Driver::InfluxDb,
        Driver::Kusto,
    ];

    /// Redis, MongoDB, InfluxDB and Kusto: no SQL. Their consoles take the engine's own
    /// language, the grid is read-only and the SQL tools (DDL, import, copy, schema search…) are
    /// not offered.
    pub fn is_nosql(self) -> bool {
        matches!(self, Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto)
    }

    /// Engines whose tables can be copied from and into (Copy Table To…): not Redis keys,
    /// InfluxDB measurements nor Kusto tables.
    pub fn can_copy(self) -> bool {
        !matches!(self, Driver::Redis | Driver::InfluxDb | Driver::Kusto)
    }

    /// Engines reached over one TCP address, which an SSH tunnel can forward: not SQLite (a file)
    /// nor the cloud services (Snowflake, Kusto), which are reached over the internet.
    pub fn supports_ssh(self) -> bool {
        !matches!(self, Driver::Sqlite | Driver::Snowflake | Driver::Kusto)
    }

    pub fn label(self) -> &'static str {
        match self {
            Driver::Postgres => "PostgreSQL",
            Driver::Sqlite => "SQLite",
            Driver::MsSql => "SQL Server",
            Driver::MySql => "MySQL",
            Driver::MariaDb => "MariaDB",
            Driver::Oracle => "Oracle",
            Driver::Snowflake => "Snowflake",
            Driver::ClickHouse => "ClickHouse",
            Driver::Redis => "Redis / Valkey",
            Driver::MongoDb => "MongoDB",
            Driver::InfluxDb => "InfluxDB",
            Driver::Kusto => "Kusto / Azure Data Explorer",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Driver::Postgres => 5432,
            Driver::MsSql => 1433,
            Driver::MySql | Driver::MariaDb => 3306,
            Driver::Oracle => 1521,
            Driver::Snowflake => 443,
            Driver::ClickHouse => 9000,
            Driver::Redis => 6379,
            Driver::MongoDb => 27017,
            Driver::InfluxDb => 8086,
            Driver::Kusto => 443,
            Driver::Sqlite => 0,
        }
    }

    /// Engines where each database has a single schema with its own name (SQLite: "main"),
    /// shown flattened in the explorer.
    pub fn flat_schema(self) -> bool {
        matches!(self, Driver::Sqlite | Driver::MySql | Driver::MariaDb | Driver::ClickHouse | Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto)
    }

    /// Engines whose server has several databases that can be created and dropped from the app.
    pub fn has_databases(self) -> bool {
        !matches!(self, Driver::Sqlite | Driver::Oracle)
    }

    /// Identifier quoted for the dialect.
    pub fn quote_ident(self, name: &str) -> String {
        match self {
            Driver::MsSql => format!("[{}]", name.replace(']', "]]")),
            Driver::MySql | Driver::MariaDb => format!("`{}`", name.replace('`', "``")),
            // ClickHouse also quotes with backticks, but escapes with a backslash (so a backslash
            // in the name is escaped too).
            Driver::ClickHouse => format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`")),
            // InfluxQL identifiers and Flux strings: double quotes, backslash escapes.
            Driver::InfluxDb => format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\"")),
            // Kusto: `['name']`, with the escapes of a KQL string.
            Driver::Kusto => format!("['{}']", name.replace('\\', "\\\\").replace('\'', "\\'")),
            Driver::Postgres | Driver::Sqlite | Driver::Oracle | Driver::Snowflake | Driver::Redis | Driver::MongoDb => {
                format!("\"{}\"", name.replace('"', "\"\""))
            }
        }
    }
}

/// Persisted connection settings. The password is NOT stored here: it goes to the
/// macOS Keychain (see `config.rs`) and is filled into `password` in memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub id: u64,
    pub name: String,
    pub driver: Driver,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(skip)]
    pub password: String,
    /// File path for SQLite.
    pub file_path: String,
    /// SQL Server: accept self-signed certificates.
    pub trust_cert: bool,
    /// DataGrip's "Read-only": the data editor can't save and consoles only run queries.
    pub read_only: bool,
    /// Snowflake: warehouse, role and schema for the session (empty = the user's defaults).
    pub warehouse: String,
    pub role: String,
    pub schema: String,
    /// DataGrip's "Authentication" (Snowflake).
    pub auth: AuthMode,
    /// DataGrip's "Authenticator" field: the JDBC `authenticator` (`snowflake_jwt`, `oauth`…).
    pub authenticator: String,
    /// Advanced tab: user-defined driver properties (`private_key_file`…), in table order. The
    /// values of secret ones (see `is_secret_property`) are kept in the keychain, not in the file.
    pub properties: Vec<(String, String)>,
    /// "Connection type: URL only": the URL replaces host, port and database in the dialog.
    pub url_only: bool,
    /// URL parameters without a field of their own (`?sslmode=require`, `;encrypt=true`…), kept so
    /// the URL round-trips through the dialog.
    pub url_extra: String,
    /// Before the Advanced tab the key file had its own field: read so `migrate` can move it
    /// into `properties`, never written back.
    #[serde(skip_serializing)]
    pub private_key_file: String,
    /// SSH tab: the tunnel through a bastion host. Its password and passphrase go to the keychain.
    pub ssh: ssh::SshSettings,
    /// SSL part of the SSH/SSL tab (PostgreSQL, MySQL, MariaDB). The key passphrase goes to the
    /// keychain.
    pub tls: tls::TlsSettings,
    /// Set by the tunnel on the config it connects with: the database's own host, which TLS
    /// checks the certificate against while the connection goes to `127.0.0.1`.
    #[serde(skip)]
    pub tunnel_host: Option<String>,
    /// Imported (from DataGrip) without its password: it is asked for on the first connect.
    pub ask_password: bool,
}

/// Driver properties that hold secrets (`token`, `private_key_file_pwd`, `password`…): their
/// values go to the keychain like the password, never to `connections.json`.
pub fn is_secret_property(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    ["password", "pwd", "secret", "token", "passcode", "private_key_base64"].iter().any(|s| n.contains(s))
}

/// DataGrip's "Authentication" options for Snowflake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AuthMode {
    /// User and password.
    #[default]
    UserPassword,
    /// The `authenticator` field, user and password; key pairs go through driver properties.
    Authenticator,
    /// Neither user nor password: everything comes from the driver properties.
    NoAuth,
    /// The Azure CLI's signed-in account (`az login`), for Kusto.
    AzureCli,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self::new(0, Driver::Postgres)
    }
}

impl ConnectionConfig {
    pub fn new(id: u64, driver: Driver) -> Self {
        Self {
            id,
            name: format!("{} {}", driver.label(), id),
            driver,
            // Kusto: the cluster, typed (`<cluster>.<region>.kusto.windows.net`).
            host: if driver == Driver::Kusto { String::new() } else { "localhost".into() },
            port: driver.default_port(),
            database: match driver {
                Driver::Postgres => "postgres".into(),
                Driver::MsSql => "master".into(),
                // Oracle: service name of the pluggable database.
                Driver::Oracle => "FREEPDB1".into(),
                Driver::MongoDb => "test".into(),
                Driver::Redis => "0".into(),
                Driver::ClickHouse => "default".into(),
                // InfluxDB: the bucket; Kusto: the database. Empty = the first one.
                Driver::Sqlite | Driver::MySql | Driver::MariaDb | Driver::Snowflake | Driver::InfluxDb | Driver::Kusto => String::new(),
            },
            user: match driver {
                Driver::Postgres => "postgres".into(),
                Driver::MsSql => "sa".into(),
                Driver::MySql | Driver::MariaDb => "root".into(),
                Driver::Oracle => "system".into(),
                Driver::ClickHouse => "default".into(),
                // InfluxDB: the organization; empty = the token's only one. Kusto: an
                // application's client ID.
                Driver::Sqlite | Driver::Snowflake | Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto => String::new(),
            },
            password: String::new(),
            file_path: String::new(),
            trust_cert: true,
            read_only: false,
            warehouse: String::new(),
            role: String::new(),
            schema: String::new(),
            // Kusto signs in with the Azure CLI's account unless told otherwise.
            auth: if driver == Driver::Kusto { AuthMode::AzureCli } else { AuthMode::default() },
            authenticator: String::new(),
            properties: Vec::new(),
            url_only: false,
            url_extra: String::new(),
            private_key_file: String::new(),
            ssh: ssh::SshSettings::default(),
            tls: tls::TlsSettings::default(),
            tunnel_host: None,
            ask_password: false,
        }
    }

    /// Switches the engine: fields still at the old engine's defaults take the new one's, and an
    /// authentication the new engine doesn't offer goes back to user and password.
    pub fn change_driver(&mut self, new: Driver) {
        let cfg = self;
        let old = cfg.driver;
        if old == new {
            return;
        }
        let old_def = Self::new(cfg.id, old);
        let new_def = Self::new(cfg.id, new);
        // Kusto starts without a host (a cluster address, not localhost).
        if cfg.host == old_def.host {
            cfg.host = new_def.host;
        }
        if cfg.port == old_def.port {
            cfg.port = new_def.port;
        }
        if cfg.database == old_def.database {
            cfg.database = new_def.database;
        }
        if cfg.user == old_def.user {
            cfg.user = new_def.user;
        }
        if cfg.name == old_def.name || cfg.name == old.label() {
            cfg.name = new.label().to_string();
        }
        // Authenticator is Snowflake's only, the Azure CLI Kusto's only; InfluxDB always takes a
        // token, and Kusto starts with the Azure CLI's account.
        if (new != Driver::Snowflake && cfg.auth == AuthMode::Authenticator)
            || (new != Driver::Kusto && cfg.auth == AuthMode::AzureCli)
            || new == Driver::InfluxDb
        {
            cfg.auth = AuthMode::UserPassword;
        }
        if new == Driver::Kusto && cfg.auth == AuthMode::UserPassword && cfg.user.trim().is_empty() {
            cfg.auth = AuthMode::AzureCli;
        }
        cfg.driver = new;
    }

    /// Value of a driver property (case-insensitive name), when set and not blank.
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties.iter().find(|(k, v)| k.trim().eq_ignore_ascii_case(name) && !v.trim().is_empty()).map(|(_, v)| v.trim())
    }

    /// Sets a driver property, replacing one with the same name.
    pub fn set_property(&mut self, name: &str, value: &str) {
        match self.properties.iter_mut().find(|(k, _)| k.trim().eq_ignore_ascii_case(name)) {
            Some((_, v)) => *v = value.to_string(),
            None => self.properties.push((name.to_string(), value.to_string())),
        }
    }

    /// Driver properties whose values are secrets, with their values (for the keychain).
    pub fn secret_properties(&self) -> Vec<(String, String)> {
        self.properties.iter().filter(|(k, v)| is_secret_property(k) && !v.is_empty()).cloned().collect()
    }

    /// The config as written to `connections.json`: no password, and secret property values blank
    /// (their rows stay, so the Advanced table keeps its order).
    pub fn without_secrets(&self) -> ConnectionConfig {
        let mut c = self.clone();
        c.password.clear();
        for (k, v) in &mut c.properties {
            if is_secret_property(k) {
                v.clear();
            }
        }
        c
    }

    /// Upgrades a config saved by an older version: the key file of the old "Key pair"
    /// authentication becomes the `private_key_file` property of "Authenticator".
    pub fn migrate(&mut self) {
        let key = std::mem::take(&mut self.private_key_file);
        if !key.trim().is_empty() && self.property("private_key_file").is_none() {
            self.set_property("private_key_file", key.trim());
            self.auth = AuthMode::Authenticator;
        }
    }
}

/// Already formatted cell value. `None` = NULL.
pub type Cell = Option<String>;

/// Result of ONE statement.
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    /// Statement that produced it (for the title of the results tab).
    pub statement: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Affected rows (DML) if the driver knows them.
    pub affected: Option<u64>,
    /// true if rows were cut off on reaching `max_rows`.
    pub truncated: bool,
    pub elapsed: Duration,
}

impl ResultSet {
    /// Statements without columns (INSERT, CREATE...).
    pub fn is_command(&self) -> bool {
        self.columns.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub kind: ObjectKind,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone)]
pub struct SchemaInfo {
    pub name: String,
    pub tables: Vec<TableInfo>,
}

/// Server database. `schemas` is `None` until it is loaded (when expanded).
#[derive(Debug, Clone)]
pub struct DatabaseInfo {
    pub name: String,
    /// Default database of the connection.
    pub current: bool,
    pub schemas: Option<Vec<SchemaInfo>>,
}

/// Standard error when a change does not affect exactly one row.
pub(crate) fn not_single_row(n: u64, stmt: &str) -> anyhow::Error {
    anyhow::anyhow!(crate::i18n::trf(
        "The statement affected {n} rows (expected 1); nothing was saved:\n{stmt}",
        &[("n", &n), ("stmt", &stmt)]
    ))
}

/// Cancels the query a session is running, from outside that session.
#[derive(Clone)]
pub enum CancelHandle {
    Postgres(tokio_postgres::CancelToken, postgres::PgTls),
    Sqlite(std::sync::Arc<rusqlite::InterruptHandle>),
    /// MySQL / MariaDB: `KILL QUERY <thread id>` from a second connection.
    MySql(Box<mysql_async::OptsBuilder>, u32),
    /// Oracle: `break_execution` on the same connection.
    Oracle(std::sync::Arc<::oracle::Connection>),
    /// Snowflake: `abort-request` for the running request id.
    Snowflake(snowflake::SfCancel),
}

impl CancelHandle {
    pub async fn cancel(&self) -> anyhow::Result<()> {
        match self {
            CancelHandle::Postgres(t, tls) => t.cancel_query(tls.clone()).await.map_err(anyhow::Error::from),
            CancelHandle::Sqlite(h) => {
                h.interrupt();
                Ok(())
            }
            CancelHandle::Oracle(conn) => {
                let conn = std::sync::Arc::clone(conn);
                tokio::task::spawn_blocking(move || conn.break_execution()).await??;
                Ok(())
            }
            CancelHandle::MySql(opts, id) => {
                use mysql_async::prelude::Queryable;
                let mut conn = mysql_async::Conn::new((**opts).clone()).await?;
                conn.query_drop(format!("KILL QUERY {id}")).await?;
                conn.disconnect().await?;
                Ok(())
            }
            CancelHandle::Snowflake(h) => h.cancel().await,
        }
    }
}

/// Open session: the engine's, and the SSH tunnel it goes through, kept open while it is.
/// Derefs to the engine.
pub struct Session {
    engine: Engine,
    _tunnel: Option<Arc<ssh::Tunnel>>,
}

impl std::ops::Deref for Session {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.engine
    }
}

impl std::ops::DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

impl Session {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Session> {
        let tunnel = if ssh::tunnels(cfg) { Some(ssh::shared(cfg).await?) } else { None };
        let engine = Self::open(&through(cfg, tunnel.as_deref()), cfg.read_only).await?;
        Ok(Session { engine, _tunnel: tunnel })
    }

    /// Test Connection: the server's product and version. Through SSH, a tunnel of its own (so
    /// the bastion's key and sign-in are checked again), named in the result.
    pub async fn test(cfg: &ConnectionConfig) -> anyhow::Result<Option<String>> {
        let tunnel = if ssh::tunnels(cfg) { Some(ssh::private(cfg).await?) } else { None };
        let mut engine = Self::open(&through(cfg, tunnel.as_ref()), cfg.read_only).await?;
        let version = engine.server_version(cfg.driver).await;
        let mut parts: Vec<String> = version.into_iter().collect();
        // The TLS version and cipher, as Test Connection names them in DataGrip.
        if let Some(tls) = engine.tls_description().await {
            parts.push(tls);
        }
        if tunnel.is_some() {
            parts.push(trf("through SSH {bastion}", &[("bastion", &cfg.ssh.describe())]));
        }
        Ok((!parts.is_empty()).then(|| parts.join(" · ")))
    }

    async fn open(cfg: &ConnectionConfig, read_only: bool) -> anyhow::Result<Engine> {
        // "No auth": the engine's defaults (trust/peer auth…). Snowflake reads its properties.
        let no_auth;
        let cfg = if cfg.auth == AuthMode::NoAuth && cfg.driver != Driver::Snowflake {
            no_auth = ConnectionConfig { user: String::new(), password: String::new(), ..cfg.clone() };
            &no_auth
        } else {
            cfg
        };
        let mut engine = match cfg.driver {
            Driver::Postgres => Engine::Postgres(postgres::PgSession::connect(cfg).await?),
            Driver::Sqlite => Engine::Sqlite(sqlite::SqliteSession::connect(cfg).await?),
            Driver::MsSql => Engine::MsSql(Box::new(mssql::MsSession::connect(cfg).await?)),
            Driver::MySql | Driver::MariaDb => Engine::MySql(Box::new(mysql::MySession::connect(cfg).await?)),
            Driver::Oracle => Engine::Oracle(Box::new(oracle::OraSession::connect(cfg).await?)),
            Driver::Snowflake => Engine::Snowflake(Box::new(snowflake::SfSession::connect(cfg).await?)),
            Driver::ClickHouse => Engine::ClickHouse(Box::new(clickhouse::ChSession::connect(cfg).await?)),
            Driver::Redis => Engine::Redis(Box::new(redis_db::RedisSession::connect(cfg).await?)),
            Driver::MongoDb => Engine::MongoDb(Box::new(mongo::MongoSession::connect(cfg).await?)),
            Driver::InfluxDb => Engine::InfluxDb(Box::new(influx::InfluxSession::connect(cfg).await?)),
            Driver::Kusto => Engine::Kusto(Box::new(kusto::KustoSession::connect(cfg).await?)),
        };
        // Read-only data sources are also enforced by the server where the engine allows it
        // (SQLite opens the file read-only; SQL Server and Oracle rely on the app's checks).
        if read_only {
            let set = match cfg.driver {
                Driver::Postgres => Some("SET default_transaction_read_only = on"),
                Driver::MySql | Driver::MariaDb => Some("SET SESSION TRANSACTION READ ONLY"),
                _ => None,
            };
            if let Some(set) = set {
                engine.execute(set, 0).await?;
            }
        }
        Ok(engine)
    }

    /// Qualified name quoted for the dialect, e.g. `"public"."users"` or `[dbo].[users]`.
    pub fn quote_table(driver: Driver, schema: &str, table: &str) -> String {
        match driver {
            Driver::Postgres | Driver::Snowflake => format!("\"{}\".\"{}\"", schema.replace('"', "\"\""), table.replace('"', "\"\"")),
            // No qualified names: a collection, a key pattern or a measurement, as is.
            Driver::Redis | Driver::MongoDb | Driver::InfluxDb => table.to_string(),
            // Kusto: the table of the request's database.
            Driver::Kusto => Driver::Kusto.quote_ident(table),
            Driver::Sqlite => format!("\"{}\"", table.replace('"', "\"\"")),
            Driver::MsSql => format!("[{}].[{}]", schema.replace(']', "]]"), table.replace(']', "]]")),
            // MySQL: the schema is the database.
            Driver::MySql | Driver::MariaDb | Driver::Oracle => format!("{}.{}", driver.quote_ident(schema), driver.quote_ident(table)),
            // ClickHouse: the schema is the database.
            Driver::ClickHouse => format!("{}.{}", driver.quote_ident(schema), driver.quote_ident(table)),
        }
    }
}

/// The config the driver connects with: through a tunnel, its listener on `127.0.0.1`, and the
/// database's host kept for TLS (`tunnel_host`).
fn through<'a>(cfg: &'a ConnectionConfig, tunnel: Option<&ssh::Tunnel>) -> std::borrow::Cow<'a, ConnectionConfig> {
    match tunnel {
        None => std::borrow::Cow::Borrowed(cfg),
        Some(t) => {
            let mut c = cfg.clone();
            // A scheme typed in the host field (InfluxDB's `https://…`) stays.
            let host = cfg.host.trim();
            c.host = match host.split_once("://") {
                Some((scheme, _)) => format!("{scheme}://127.0.0.1"),
                None => "127.0.0.1".into(),
            };
            c.tunnel_host = Some(ssh::bare_host(host).to_string());
            c.port = t.port;
            std::borrow::Cow::Owned(c)
        }
    }
}

/// The driver session of an engine.
pub enum Engine {
    Postgres(postgres::PgSession),
    Sqlite(sqlite::SqliteSession),
    MsSql(Box<mssql::MsSession>),
    /// MySQL and MariaDB.
    MySql(Box<mysql::MySession>),
    Oracle(Box<oracle::OraSession>),
    Snowflake(Box<snowflake::SfSession>),
    ClickHouse(Box<clickhouse::ChSession>),
    Redis(Box<redis_db::RedisSession>),
    MongoDb(Box<mongo::MongoSession>),
    InfluxDb(Box<influx::InfluxSession>),
    Kusto(Box<kusto::KustoSession>),
}

impl Engine {
    /// Runs ONE statement (or a T-SQL batch) and returns one or more result sets.
    /// Each result set keeps at most `max_rows` rows.
    /// For SQL engines, zero discards rows without stopping execution.
    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        match self {
            Engine::Postgres(s) => s.execute(sql, max_rows).await,
            Engine::Sqlite(s) => s.execute(sql, max_rows).await,
            Engine::MsSql(s) => s.execute(sql, max_rows).await,
            Engine::MySql(s) => s.execute(sql, max_rows).await,
            Engine::Oracle(s) => s.execute(sql, max_rows).await,
            Engine::Snowflake(s) => s.execute(sql, max_rows).await,
            Engine::ClickHouse(s) => s.execute(sql, max_rows).await,
            Engine::Redis(s) => s.execute(sql, max_rows).await,
            Engine::MongoDb(s) => s.execute(sql, max_rows).await,
            Engine::InfluxDb(s) => s.execute(sql, max_rows).await,
            Engine::Kusto(s) => s.execute(sql, max_rows).await,
        }
    }

    /// Database the session is connected to when opened.
    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        match self {
            Engine::Postgres(s) => s.current_database().await,
            Engine::Sqlite(_) => Ok("main".into()),
            Engine::MsSql(s) => s.current_database().await,
            Engine::MySql(s) => s.current_database().await,
            Engine::Oracle(s) => s.current_database().await,
            Engine::Snowflake(s) => s.current_database().await,
            Engine::ClickHouse(s) => s.current_database().await,
            Engine::Redis(s) => s.current_database().await,
            Engine::MongoDb(s) => s.current_database().await,
            Engine::InfluxDb(s) => s.current_database().await,
            Engine::Kusto(s) => s.current_database().await,
        }
    }

    /// Databases visible on the server (SQLite: just "main").
    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        match self {
            Engine::Postgres(s) => s.list_databases().await,
            Engine::Sqlite(_) => Ok(vec!["main".into()]),
            Engine::MsSql(s) => s.list_databases().await,
            Engine::MySql(s) => s.list_databases().await,
            Engine::Oracle(s) => s.list_databases().await,
            Engine::Snowflake(s) => s.list_databases().await,
            Engine::ClickHouse(s) => s.list_databases().await,
            Engine::Redis(s) => s.list_databases().await,
            Engine::MongoDb(s) => s.list_databases().await,
            Engine::InfluxDb(s) => s.list_databases().await,
            Engine::Kusto(s) => s.list_databases().await,
        }
    }

    /// Schema of `db`. On PostgreSQL the session must already be connected to that database.
    pub async fn introspect_database(&mut self, db: &str) -> anyhow::Result<Vec<SchemaInfo>> {
        self.use_database(db).await?;
        self.introspect().await
    }

    /// SQL Server and MySQL: `USE db`. A no-op on the other engines (each session is bound to one database).
    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        match self {
            Engine::MsSql(s) if !db.is_empty() => s.use_database(db).await,
            Engine::MySql(s) if !db.is_empty() => s.use_database(db).await,
            Engine::Snowflake(s) if !db.is_empty() => s.use_database(db).await,
            Engine::ClickHouse(s) if !db.is_empty() => s.use_database(db).await,
            Engine::Redis(s) if !db.is_empty() => s.use_database(db).await,
            Engine::MongoDb(s) if !db.is_empty() => s.use_database(db).await,
            Engine::InfluxDb(s) if !db.is_empty() => s.use_database(db).await,
            Engine::Kusto(s) if !db.is_empty() => s.use_database(db).await,
            _ => Ok(()),
        }
    }

    /// Applies data editor changes in ONE transaction. Each statement must affect exactly
    /// one row; if any fails or affects 0 or several, everything is rolled back (MongoDB on a
    /// standalone server has no transactions: see `MongoSession::apply_changes`).
    /// Returns the total number of modified rows.
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        match self {
            Engine::Postgres(s) => s.apply_changes(statements).await,
            Engine::Sqlite(s) => s.apply_changes(statements).await,
            Engine::MsSql(s) => s.apply_changes(statements).await,
            Engine::MySql(s) => s.apply_changes(statements).await,
            Engine::Oracle(s) => s.apply_changes(statements).await,
            Engine::Snowflake(s) => s.apply_changes(statements).await,
            // ClickHouse: only the editor's INSERTs (see `ChSession::apply_changes`).
            Engine::ClickHouse(s) => s.apply_changes(statements).await,
            // Their commands (not SQL) come from `redis_db::change_commands` / `mongo::change_command`.
            Engine::Redis(s) => s.apply_changes(statements).await,
            Engine::MongoDb(s) => s.apply_changes(statements).await,
            // Points are written with line protocol, which the data editor doesn't produce, and
            // Kusto ingests rows rather than editing them.
            Engine::InfluxDb(_) | Engine::Kusto(_) => anyhow::bail!(crate::i18n::tr("Editing is not available for this engine")),
        }
    }

    /// The connection's TLS version and cipher (PostgreSQL, MySQL, MariaDB), None when plain.
    pub async fn tls_description(&mut self) -> Option<String> {
        match self {
            Engine::Postgres(s) => s.tls_description().await,
            Engine::MySql(s) => s.tls_description().await,
            _ => None,
        }
    }

    /// Native way to cancel this session's running query, if the engine has one. Without it
    /// the worker abandons the query and closes the session.
    pub fn cancel_handle(&self) -> Option<CancelHandle> {
        match self {
            Engine::Postgres(s) => {
                let (token, tls) = s.cancel_token();
                Some(CancelHandle::Postgres(token, tls))
            }
            Engine::Sqlite(s) => Some(CancelHandle::Sqlite(s.interrupt_handle())),
            Engine::MySql(s) => {
                let (opts, id) = s.cancel_target();
                Some(CancelHandle::MySql(Box::new(opts), id))
            }
            Engine::Oracle(s) => Some(CancelHandle::Oracle(s.break_handle())),
            Engine::Snowflake(s) => Some(CancelHandle::Snowflake(s.cancel_handle())),
            _ => None,
        }
    }

    /// Runs the statements in ONE transaction (all or nothing), without the one-row rule of
    /// `apply_changes`. Returns the affected rows when the engine reports them.
    pub async fn run_transaction(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        if matches!(self, Engine::Redis(_) | Engine::MongoDb(_) | Engine::InfluxDb(_) | Engine::Kusto(_)) {
            anyhow::bail!(crate::i18n::tr("Editing is not available for this engine"));
        }
        if let Engine::ClickHouse(s) = self {
            // No transactions in ClickHouse: run the statements directly (each INSERT is
            // atomic on its own; there is no rollback to offer).
            return s.run_statements(statements).await;
        }
        if let Engine::Oracle(s) = self {
            return s.run_transaction(statements).await;
        }
        let (begin, commit, rollback) = match self {
            Engine::MsSql(_) => ("BEGIN TRANSACTION", "COMMIT TRANSACTION", "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"),
            Engine::MySql(_) => ("START TRANSACTION", "COMMIT", "ROLLBACK"),
            _ => ("BEGIN", "COMMIT", "ROLLBACK"),
        };
        self.execute(begin, 0).await?;
        let mut total = 0;
        for stmt in statements {
            match self.execute(stmt, 0).await {
                Ok(rs) => total += rs.iter().filter_map(|r| r.affected).sum::<u64>(),
                Err(e) => {
                    let _ = self.execute(rollback, 0).await;
                    return Err(e);
                }
            }
        }
        if let Err(e) = self.execute(commit, 0).await {
            let _ = self.execute(rollback, 0).await;
            return Err(e);
        }
        Ok(total)
    }

    /// true if the connection has been lost (socket closed, protocol error...).
    pub fn is_closed(&self) -> bool {
        match self {
            Engine::Postgres(s) => s.is_closed(),
            Engine::Sqlite(_) => false,
            Engine::MsSql(s) => s.is_closed(),
            Engine::MySql(s) => s.is_closed(),
            Engine::Oracle(s) => s.is_closed(),
            Engine::Snowflake(s) => s.is_closed(),
            Engine::ClickHouse(s) => s.is_closed(),
            Engine::Redis(s) => s.is_closed(),
            Engine::MongoDb(s) => s.is_closed(),
            Engine::InfluxDb(_) | Engine::Kusto(_) => false,
        }
    }

    /// Schemas, tables/views and columns (no system objects).
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        match self {
            Engine::Postgres(s) => s.introspect().await,
            Engine::Sqlite(s) => s.introspect().await,
            Engine::MsSql(s) => s.introspect().await,
            Engine::MySql(s) => s.introspect().await,
            Engine::Oracle(s) => s.introspect().await,
            Engine::Snowflake(s) => s.introspect().await,
            Engine::ClickHouse(s) => s.introspect().await,
            Engine::Redis(s) => s.introspect().await,
            Engine::MongoDb(s) => s.introspect().await,
            Engine::InfluxDb(s) => s.introspect().await,
            Engine::Kusto(s) => s.introspect().await,
        }
    }

    /// Product and version for the dialog's Test Connection ("Snowflake 10.37.102",
    /// "PostgreSQL 16.4"…), or None when the server won't say.
    pub async fn server_version(&mut self, driver: Driver) -> Option<String> {
        match self {
            Engine::Redis(s) => return s.server_version().await,
            Engine::MongoDb(s) => return s.server_version().await,
            Engine::InfluxDb(s) => return s.server_version().await,
            Engine::Kusto(s) => return s.server_version().await,
            _ => {}
        }
        let sql = match driver {
            Driver::Postgres => "SHOW server_version",
            Driver::MySql | Driver::MariaDb => "SELECT VERSION()",
            Driver::MsSql => "SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(128))",
            Driver::Oracle => "SELECT version FROM product_component_version WHERE product LIKE 'Oracle%'",
            Driver::Sqlite => "SELECT sqlite_version()",
            Driver::Snowflake => "SELECT CURRENT_VERSION()",
            Driver::ClickHouse => "SELECT version()",
            Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto => return None,
        };
        let rs = self.execute(sql, 1).await.ok()?;
        let v = rs.into_iter().next()?.rows.into_iter().next()?.into_iter().next()??;
        // MariaDB reports itself in its version string ("11.4.2-MariaDB-…").
        let v = v.split(['-', ' ']).next().unwrap_or(&v).to_string();
        let product = match driver {
            Driver::MsSql => "Microsoft SQL Server",
            Driver::Postgres => "PostgreSQL",
            Driver::Sqlite => "SQLite",
            d => d.label(),
        };
        Some(format!("{product} {v}"))
    }
}
