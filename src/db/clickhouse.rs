//! ClickHouse driver on top of `nativeclick` (native TCP protocol, port 9000).
//!
//! The native protocol carries one statement per query, so the console's statement splitter
//! does the work and `execute` receives a single statement. Values arrive dynamically typed
//! and are converted to text here, like in the other drivers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures_util::TryStreamExt;
use nativeclick::{Client, ClientOptions, NativeclickError, Type, Value};
use rustls_platform_verifier::ConfigVerifierExt;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::rustls::pki_types::ServerName;

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};

/// The server's secure native port (9000 is the plain one).
pub const TLS_PORT: u16 = 9440;

pub struct ChSession {
    client: Client,
}

/// Readable message for server errors (drops nativeclick's stack trace).
fn ch_error(e: &NativeclickError) -> anyhow::Error {
    match e {
        NativeclickError::ServerException { code, name, message, .. } => anyhow!(trf(
            "Code {code} ({name}): {message}",
            &[("code", code), ("name", name), ("message", message)]
        )),
        other => anyhow!(other.to_string()),
    }
}

/// A `true`/`1`/`yes`/`on` driver property (`tls`, `tls_insecure`, as for Redis).
fn property_on(cfg: &ConnectionConfig, name: &str) -> bool {
    cfg.property(name).is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
}

pub fn uses_tls(cfg: &ConnectionConfig) -> bool {
    property_on(cfg, "tls")
}

/// TLS client: certificates checked against the system's trust store (like Snowflake's HTTPS),
/// unless `insecure`.
fn tls_connector(insecure: bool) -> anyhow::Result<TlsConnector> {
    let config = if insecure {
        let builder = ClientConfig::builder();
        let verifier = super::tls::AcceptAnyCertificate(builder.crypto_provider().clone());
        builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth()
    } else {
        ClientConfig::with_platform_verifier()?
    };
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn connect_client(cfg: &ConnectionConfig) -> anyhow::Result<Client> {
    let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
    let tls = uses_tls(cfg);
    let port = match cfg.port {
        0 if tls => TLS_PORT,
        0 => 9000,
        p => p,
    };
    let database =
        if cfg.database.trim().is_empty() { "default".to_string() } else { cfg.database.trim().to_string() };

    let options = ClientOptions {
        username: if cfg.user.trim().is_empty() { "default".into() } else { cfg.user.trim().to_string() },
        password: cfg.password.clone(),
        default_database: database,
        tcp_nodelay: true,
        quota_key: String::new(),
        connect_timeout: Some(Duration::from_secs(10)),
    };
    let connected = if tls {
        let connector = tls_connector(property_on(cfg, "tls_insecure"))?;
        // Through an SSH tunnel, the certificate is checked against the database's own host.
        let name = ServerName::try_from(cfg.tunnel_host.clone().unwrap_or_else(|| host.to_string()))
            .map_err(|_| anyhow!(trf("“{host}” is not a valid server name for TLS", &[("host", &host)])))?;
        Client::connect_tls((host, port), options, name, &connector).await
    } else {
        Client::connect((host, port), options).await
    };
    let client = connected.map_err(|e| match e {
        NativeclickError::Timeout | NativeclickError::Io(_) => anyhow!(trf("Could not connect to {host}:{port}: {error}", &[
            ("host", &host),
            ("port", &port),
            ("error", &e.to_string()),
        ])),
        other => ch_error(&other),
    })?;
    Ok(client)
}

impl ChSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        Ok(Self { client: connect_client(cfg).await? })
    }

    /// `USE` changes the session's database on the server, keeping its settings and
    /// temporary tables. Sent every time: a console's own `USE` may have moved it.
    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        let sql = format!("USE {}", super::Driver::ClickHouse.quote_ident(db));
        self.client.execute(sql.as_str()).await.map_err(|e| ch_error(&e))
    }

    /// Text rows of an internal query (introspection and the database list).
    async fn rows(&mut self, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>> {
        let results = self.execute(sql, usize::MAX).await?;
        Ok(results.into_iter().find(|rs| !rs.is_command()).map(|rs| rs.rows).unwrap_or_default())
    }

    /// Asked to the server: a console's `USE` changes it.
    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        self.rows("SELECT currentDatabase()")
            .await?
            .first()
            .and_then(|r| r.first().cloned())
            .flatten()
            .ok_or_else(|| anyhow!(tr("The server did not return the current database")))
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .rows("SELECT name FROM system.databases ORDER BY name")
            .await?
            .into_iter()
            .filter_map(|r| r.first().and_then(|c| c.clone()))
            .collect())
    }

    /// Schemas, tables/views and columns read from `system.tables`/`system.columns`.
    /// ClickHouse has no schemas: the current database is the only one shown.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        const SQL: &str = r#"
            SELECT t.name, t.engine, c.name, c.type, c.is_in_primary_key
            FROM system.tables AS t
            LEFT JOIN system.columns AS c ON c.database = t.database AND c.table = t.table
            WHERE t.database = currentDatabase()
            ORDER BY t.name, c.position
        "#;
        let mut schema = SchemaInfo { name: self.current_database().await?, tables: Vec::new() };
        for row in self.rows(SQL).await? {
            let get = |i: usize| row.get(i).and_then(|c| c.clone()).unwrap_or_default();
            let (table, engine) = (get(0), get(1));
            if schema.tables.last().is_none_or(|t| t.name != table) {
                schema.tables.push(TableInfo {
                    name: table,
                    // 'View' and 'LiveView' are virtual; the rest (MergeTree, MaterializedView…)
                    // store data and are queried like a table.
                    kind: if engine == "View" || engine == "LiveView" { ObjectKind::View } else { ObjectKind::Table },
                    columns: Vec::new(),
                });
            }
            let Some(t) = schema.tables.last_mut() else { continue };
            // Table without visible columns: the LEFT JOIN leaves an empty default.
            let column = get(2);
            if column.is_empty() {
                continue;
            }
            let data_type = get(3);
            t.columns.push(ColumnInfo {
                primary_key: get(4) == "1",
                nullable: is_nullable(&data_type),
                name: column,
                data_type,
            });
        }
        Ok(vec![schema])
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let statement = sql.trim().to_string();
        let start = Instant::now();
        let mut stream = self.client.query_raw(sql.to_string()).await.map_err(|e| ch_error(&e))?;

        let mut results: Vec<ResultSet> = Vec::new();
        let mut truncated = false;
        'blocks: while let Some(mut block) = stream.try_next().await.map_err(|e| ch_error(&e))? {
            if results.is_empty() {
                // One statement = one result set; the columns arrive with the first block,
                // even when the query returns no rows.
                results.push(ResultSet {
                    statement: statement.clone(),
                    columns: block.column_types.keys().cloned().collect(),
                    ..Default::default()
                });
            }
            let Some(rs) = results.last_mut() else { continue };
            for row in block.take_iter_rows() {
                if rs.rows.len() >= max_rows {
                    truncated = true;
                    if max_rows != 0 {
                        // Dropping the stream cancels the query: the rest isn't downloaded.
                        break 'blocks;
                    }
                    continue;
                }
                rs.rows.push(row.iter().map(|(_, type_, value)| cell(type_, value)).collect());
            }
        }
        if let Some(rs) = results.last_mut() {
            rs.elapsed = start.elapsed();
            if truncated {
                rs.truncated = true;
            }
        } else {
            // DDL/INSERT: the server answers with an end-of-stream and no blocks.
            results.push(ResultSet { statement, elapsed: start.elapsed(), ..Default::default() });
        }
        Ok(results)
    }

    /// The data editor requires "exactly one row affected" per statement. ClickHouse can't
    /// honor that for UPDATE/DELETE (asynchronous mutations without a row count), so only
    /// the INSERTs the editor generates are applied here.
    /// Everything is checked before running anything: there are no transactions to undo
    /// an INSERT already applied.
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        if !statements.iter().all(|s| s.trim_start().get(..6).is_some_and(|p| p.eq_ignore_ascii_case("INSERT"))) {
            return Err(anyhow!(tr(
                "ClickHouse applies UPDATE and DELETE as asynchronous mutations that don't report affected rows; run them from a SQL console"
            )));
        }
        for stmt in statements {
            self.client.execute(stmt.as_str()).await.map_err(|e| ch_error(&e))?;
        }
        Ok(statements.len() as u64)
    }

    /// `Session::run_transaction` path: ClickHouse has no transactions, so the statements are
    /// simply run one after another (each INSERT is atomic; no rollback exists).
    pub async fn run_statements(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        for stmt in statements {
            self.client.execute(stmt.as_str()).await.map_err(|e| ch_error(&e))?;
        }
        Ok(statements.len() as u64)
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

/// `T` of a one-argument type wrapper such as `Nullable(T)` (the name is case-insensitive).
fn unwrap_type<'a>(ty: &'a str, wrapper: &str) -> Option<&'a str> {
    let ty = ty.trim();
    let rest = ty.get(wrapper.len()..)?;
    (ty.get(..wrapper.len())?.eq_ignore_ascii_case(wrapper) && rest.starts_with('(') && rest.ends_with(')'))
        .then(|| rest[1..rest.len() - 1].trim())
}

/// The type inside `Nullable(…)` / `LowCardinality(…)` (`LowCardinality(Nullable(String))` →
/// `String`).
pub fn base_type(data_type: &str) -> &str {
    let inner = unwrap_type(data_type, "LowCardinality").unwrap_or(data_type.trim());
    unwrap_type(inner, "Nullable").unwrap_or(inner)
}

/// Nullability is part of the type: `Nullable(T)`, also inside `LowCardinality(…)`.
pub fn is_nullable(data_type: &str) -> bool {
    let inner = unwrap_type(data_type, "LowCardinality").unwrap_or(data_type);
    unwrap_type(inner, "Nullable").is_some()
}

/// Column type for DDL with the requested nullability: `Nullable(T)` or `T`, keeping a
/// `LowCardinality(…)` outside. Composite types (arrays, maps, tuples, JSON…) can't be
/// Nullable and stay as written.
pub fn column_type(data_type: &str, not_null: bool) -> String {
    let ty = data_type.trim();
    let low_cardinality = unwrap_type(ty, "LowCardinality");
    let inner = low_cardinality.unwrap_or(ty);
    let base = unwrap_type(inner, "Nullable").unwrap_or(inner);
    const COMPOSITE: [&str; 9] = ["Array", "Map", "Tuple", "Nested", "Variant", "Dynamic", "JSON", "Object", "AggregateFunction"];
    let name = base.split('(').next().unwrap_or(base).trim();
    if COMPOSITE.iter().any(|c| name.eq_ignore_ascii_case(c)) || name.ends_with("AggregateFunction") {
        return ty.to_string();
    }
    let core = if not_null { base.to_string() } else { format!("Nullable({base})") };
    if low_cardinality.is_some() { format!("LowCardinality({core})") } else { core }
}

/// Converts a value to the text shown in the grid. The enum name is preferred for
/// `Enum8`/`Enum16`; `Value`'s own `Display` is SQL serialization (quotes, `makeDate(…)`),
/// only adequate for nested values.
fn cell(type_: &Type, value: &Value) -> Cell {
    Some(match (type_, value) {
        (_, Value::Null) => return None,
        (_, Value::String(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        (_, Value::Int128(v)) => v.to_string(),
        (_, Value::Int256(v)) => v.to_string(),
        (_, Value::UInt128(v)) => v.to_string(),
        (_, Value::UInt256(v)) => v.to_string(),
        (_, Value::Uuid(u)) => u.to_string(),
        (_, Value::Ipv4(ip)) => ip.to_string(),
        (_, Value::Ipv6(ip)) => ip.to_string(),
        (_, Value::Date(d)) => chrono::NaiveDate::from(*d).format("%Y-%m-%d").to_string(),
        (_, Value::Date32(d)) => chrono::NaiveDate::try_from(*d)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|_| d.0.to_string()),
        (_, Value::DateTime(dt)) => chrono::DateTime::<chrono::FixedOffset>::try_from(*dt)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S%.f").to_string())
            .unwrap_or_else(|_| dt.1.to_string()),
        (_, Value::DateTime64(dt)) => chrono::DateTime::<chrono::FixedOffset>::try_from(*dt)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S%.f").to_string())
            .unwrap_or_else(|_| dt.1.to_string()),
        (_, Value::Time(secs)) => time_text(*secs as i64, 0),
        (_, Value::Time64(scale, ticks)) => time_text(*ticks, *scale),
        (Type::Enum8(pairs), Value::Enum8(v)) => pair_name(pairs, *v),
        (Type::Enum16(pairs), Value::Enum16(v)) => pair_name(pairs, *v),
        (_, Value::Dynamic(dyn_value)) => cell(&dyn_value.type_, &dyn_value.value)?,
        // Numbers, Bool, Decimal and the composites (Array/Tuple/Map/geometry) keep
        // nativeclick's formatting.
        _ => value.to_string(),
    })
}

/// Name of an Enum value; falls back to the number when the pair list lacks it.
fn pair_name<K: PartialEq + std::fmt::Display>(pairs: &[(String, K)], value: K) -> String {
    pairs.iter().find(|(_, e)| *e == value).map(|(n, _)| n.clone()).unwrap_or_else(|| value.to_string())
}

/// `Time`/`Time64`: seconds (may exceed a day or be negative) with a 10^scale divisor.
fn time_text(ticks: i64, scale: usize) -> String {
    let div = 10u64.pow(scale as u32);
    let sign = if ticks < 0 { "-" } else { "" };
    let abs = ticks.unsigned_abs();
    let (h, m, s) = (abs / div / 3600, abs / div % 3600 / 60, abs / div % 60);
    if scale == 0 {
        return format!("{sign}{h:02}:{m:02}:{s:02}");
    }
    let frac = abs % div;
    format!("{sign}{h:02}:{m:02}:{s:02}.{frac:0width$}", width = scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_types() {
        assert_eq!(column_type("String", true), "String");
        assert_eq!(column_type("String", false), "Nullable(String)");
        assert_eq!(column_type("Nullable(String)", false), "Nullable(String)");
        assert_eq!(column_type(" Nullable(Int32) ", true), "Int32");
        // LowCardinality stays outside; composite types can't be Nullable.
        assert_eq!(column_type("LowCardinality(String)", false), "LowCardinality(Nullable(String))");
        assert_eq!(column_type("LowCardinality(Nullable(String))", true), "LowCardinality(String)");
        assert_eq!(column_type("Array(String)", false), "Array(String)");
        assert_eq!(column_type("Map(String, UInt64)", false), "Map(String, UInt64)");
        assert_eq!(column_type("Decimal(18, 2)", false), "Nullable(Decimal(18, 2))");
    }

    #[test]
    fn nullability() {
        assert!(is_nullable("Nullable(String)"));
        assert!(is_nullable("LowCardinality(Nullable(String))"));
        assert!(!is_nullable("LowCardinality(String)"));
        assert!(!is_nullable("Array(Nullable(String))"));
        assert!(!is_nullable("String"));
    }

    #[test]
    fn time_formats() {
        assert_eq!(time_text(3723, 0), "01:02:03");
        assert_eq!(time_text(90061, 0), "25:01:01"); // Time can exceed a day.
        assert_eq!(time_text(-3723, 0), "-01:02:03");
        assert_eq!(time_text(3_723_456_000_000, 9), "01:02:03.456000000");
    }

    #[test]
    fn cells() {
        assert_eq!(cell(&Type::Nothing, &Value::Null), None);
        assert_eq!(cell(&Type::String, &Value::String(b"O'Brien".to_vec())), Some("O'Brien".to_string()));
        // Int128 Display would add "::Int128" (SQL serialization); the grid wants the number.
        assert_eq!(cell(&Type::Int128, &Value::Int128(42)), Some("42".to_string()));
        let enums = Type::Enum8(vec![("x".into(), 1), ("y".into(), 2)]);
        assert_eq!(cell(&enums, &Value::Enum8(2)), Some("y".to_string()));
    }
}

#[cfg(test)]
mod live_tests {
    //! Live tests against a real ClickHouse (non-destructive: its own dc_test database,
    //! dropped at the end). Run with `cargo test -- --ignored` and DATACRAFT_CH_HOST
    //! (skipped without it), DATACRAFT_CH_PORT (9000), DATACRAFT_CH_USER (default),
    //! DATACRAFT_CH_PASSWORD, DATACRAFT_CH_DATABASE (default).

    use super::*;
    use crate::app::table_query;
    use crate::db::{Driver, Session};

    fn test_cfg() -> Option<ConnectionConfig> {
        let Ok(host) = std::env::var("DATACRAFT_CH_HOST") else {
            eprintln!("DATACRAFT_CH_HOST not set: skipping the ClickHouse test");
            return None;
        };
        let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        let mut cfg = ConnectionConfig::new(1, Driver::ClickHouse);
        cfg.host = host;
        cfg.port = env("DATACRAFT_CH_PORT", "9000").parse().expect("port");
        cfg.user = env("DATACRAFT_CH_USER", "default");
        cfg.password = env("DATACRAFT_CH_PASSWORD", "");
        cfg.database = env("DATACRAFT_CH_DATABASE", "default");
        Some(cfg)
    }

    async fn session() -> Option<ChSession> {
        Some(ChSession::connect(&test_cfg()?).await.expect("connection"))
    }

    #[tokio::test]
    #[ignore]
    async fn mixed_types_and_introspection() {
        let Some(mut s) = session().await else { return };
        s.execute("DROP DATABASE IF EXISTS dc_test", 10).await.unwrap();
        s.execute("CREATE DATABASE dc_test", 10).await.unwrap();
        s.use_database("dc_test").await.unwrap();
        assert_eq!(s.current_database().await.unwrap(), "dc_test");

        let rs = s
            .execute(
                "SELECT 42::UInt64 AS i, -1.5::Float64 AS f, 'O''Brien' AS s, NULL::Nullable(String) AS z, \
                 toDate('2026-10-18') AS d, toDateTime('2026-10-18 12:30:00', 'UTC') AS dt, \
                 CAST([1,2] AS Array(UInt8)) AS a, CAST('x' AS Enum8('x'=1,'y'=2)) AS e, true AS b",
                100,
            )
            .await
            .unwrap();
        assert_eq!(
            rs[0].columns,
            ["i", "f", "s", "z", "d", "dt", "a", "e", "b"]
        );
        let row: Vec<Option<&str>> = rs[0].rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(row[0], Some("42"));
        assert_eq!(row[1], Some("-1.5"));
        assert_eq!(row[2], Some("O'Brien"));
        assert_eq!(row[3], None);
        assert_eq!(row[4], Some("2026-10-18"));
        assert!(row[5].is_some_and(|v| v.starts_with("2026-10-18 12:30:00")), "{:?}", row[5]);
        assert_eq!(row[6], Some("[1,2]"));
        assert_eq!(row[7], Some("x"));
        assert_eq!(row[8], Some("true"));

        // DDL through the editor path + the editor's INSERT only.
        let ddl = crate::ui::create_dialog::create_table_sql(
            Driver::ClickHouse,
            "dc_test",
            "items",
            &[
                crate::ui::create_dialog::NewColumn {
                    name: "id".into(),
                    data_type: "UInt64".into(),
                    not_null: true,
                    primary_key: true,
                    default: String::new(),
                },
                crate::ui::create_dialog::NewColumn {
                    name: "name".into(),
                    data_type: "Nullable(String)".into(),
                    not_null: false,
                    primary_key: false,
                    default: String::new(),
                },
            ],
        );
        s.execute(&ddl, 10).await.unwrap();
        s.apply_changes(&[
            "INSERT INTO `dc_test`.`items` (`id`, `name`) VALUES (1, 'a')".to_string(),
            "INSERT INTO `dc_test`.`items` (`id`, `name`) VALUES (2, NULL)".to_string(),
        ])
        .await
        .unwrap();
        let err = s.apply_changes(&["DELETE FROM `dc_test`.`items` WHERE id = 1".to_string()]).await.unwrap_err();
        assert!(err.to_string().contains("mutation"), "{err}");

        let schemas = s.introspect().await.unwrap();
        assert_eq!(schemas[0].name, "dc_test");
        let items = schemas[0].tables.iter().find(|t| t.name == "items").expect("items");
        assert_eq!(items.kind, ObjectKind::Table);
        assert_eq!(items.columns[0].name, "id");
        assert!(items.columns[0].primary_key);
        assert!(!items.columns[0].nullable);
        assert!(items.columns[1].nullable);

        s.execute("CREATE VIEW `dc_test`.`v` AS SELECT id FROM `dc_test`.`items`", 10).await.unwrap();
        let schemas = s.introspect().await.unwrap();
        let view = schemas[0].tables.iter().find(|t| t.name == "v").expect("view");
        assert_eq!(view.kind, ObjectKind::View);

        assert!(s.list_databases().await.unwrap().contains(&"dc_test".to_string()));
        assert!(!schemas.iter().any(|sch| sch.name == "system"));

        // A console's own USE moves the session; use_database moves it back.
        s.execute("USE system", 10).await.unwrap();
        assert_eq!(s.current_database().await.unwrap(), "system");
        s.use_database("dc_test").await.unwrap();
        assert_eq!(s.introspect().await.unwrap()[0].name, "dc_test");

        // Truncation stops reading (the query is cancelled) and the session stays usable.
        let rs = s.execute("SELECT number FROM system.numbers", 5).await.unwrap();
        assert_eq!(rs[0].rows.len(), 5);
        assert!(rs[0].truncated);
        let rs = s.execute("SELECT 1", 5).await.unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("1"));

        // Editor values and names with backslashes round-trip.
        let v = crate::edit::literal(Driver::ClickHouse, &Some(r"C:\ it's\".into()));
        let rs = s.execute(&format!("SELECT {v}"), 5).await.unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some(r"C:\ it's\"));
        let q = crate::edit::quote_ident(Driver::ClickHouse, r"a`b\");
        let rs = s.execute(&format!("SELECT 1 AS {q}"), 5).await.unwrap();
        assert_eq!(rs[0].columns, [r"a`b\"]);

        // A mixed batch is rejected before the INSERT runs.
        s.apply_changes(&[
            "INSERT INTO `dc_test`.`items` (`id`, `name`) VALUES (3, 'c')".to_string(),
            "DELETE FROM `dc_test`.`items` WHERE id = 1".to_string(),
        ])
        .await
        .unwrap_err();
        let rs = s.execute("SELECT count() FROM `dc_test`.`items` WHERE id = 3", 5).await.unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("0"));

        let err = s.execute("SELECT * FROM missing_table_xyz", 10).await.unwrap_err();
        assert!(err.to_string().contains("missing_table_xyz"), "{err}");
        s.use_database("default").await.unwrap();
        s.execute("DROP DATABASE dc_test", 10).await.unwrap();
    }

    /// The same flow through the engine-agnostic `Session` dispatch the worker uses:
    /// connect, qualified table_query (backticks), introspect_database (use_database) and
    /// the editor's INSERT-only apply_changes.
    #[tokio::test]
    #[ignore]
    async fn session_dispatch() {
        let Some(cfg) = test_cfg() else { return };
        let mut session = Session::connect(&cfg).await.unwrap();

        session.execute("DROP DATABASE IF EXISTS dc_disp", 10).await.unwrap();
        session.execute("CREATE DATABASE dc_disp", 10).await.unwrap();
        let schemas = session.introspect_database("dc_disp").await.unwrap();
        assert_eq!(schemas[0].name, "dc_disp");

        session.execute("CREATE TABLE `dc_disp`.`t` (`a` String) ENGINE = MergeTree ORDER BY tuple()", 10).await.unwrap();
        session.apply_changes(&["INSERT INTO `dc_disp`.`t` (`a`) VALUES ('x')".to_string()]).await.unwrap();
        let q = table_query(Driver::ClickHouse, "dc_disp", "t", "", "", 100);
        assert_eq!(q, "SELECT * FROM `dc_disp`.`t` LIMIT 100");
        let rs = session.execute(&q, 100).await.unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("x"));

        assert!(session.list_databases().await.unwrap().contains(&"dc_disp".to_string()));
        session.use_database("default").await.unwrap();
        session.execute("DROP DATABASE dc_disp", 10).await.unwrap();
    }

    /// TLS on the secure port (DATACRAFT_CH_TLS_PORT; skipped without it). The certificate is
    /// checked against the system trust store unless "Trust server certificate" is on: set
    /// DATACRAFT_CH_TLS_TRUSTED=1 when the server's CA is trusted (SSL_CERT_FILE on Linux).
    #[tokio::test]
    #[ignore]
    async fn tls() {
        let (Some(mut cfg), Ok(port)) = (test_cfg(), std::env::var("DATACRAFT_CH_TLS_PORT")) else { return };
        cfg.port = port.parse().expect("port");
        cfg.set_property("tls", "true");
        let trusted = std::env::var("DATACRAFT_CH_TLS_TRUSTED").is_ok_and(|v| v == "1");
        match ChSession::connect(&cfg).await {
            Ok(mut s) => {
                assert!(trusted, "an untrusted certificate was accepted");
                assert_eq!(s.rows("SELECT 1").await.unwrap()[0][0].as_deref(), Some("1"));
            }
            Err(e) => {
                assert!(!trusted, "{e}");
                assert!(e.to_string().contains("certificate"), "{e}");
            }
        }
        cfg.set_property("tls_insecure", "true");
        let mut s = ChSession::connect(&cfg).await.expect("TLS without the certificate check");
        assert_eq!(s.rows("SELECT 1").await.unwrap()[0][0].as_deref(), Some("1"));
        // The plain protocol doesn't talk to the secure port.
        cfg.set_property("tls", "");
        assert!(ChSession::connect(&cfg).await.is_err());
    }

    /// Find in Schema: NULLs in Nullable columns don't fail the search, LIKE wildcards in the
    /// value are literal, and arrays are searched as text.
    #[tokio::test]
    #[ignore]
    async fn schema_search_nulls_and_wildcards() {
        use crate::db::schema_search::{SearchRequest, search};
        let Some(cfg) = test_cfg() else { return };
        let mut s = ChSession::connect(&cfg).await.unwrap();
        for sql in [
            "DROP DATABASE IF EXISTS dc_search",
            "CREATE DATABASE dc_search",
            "CREATE TABLE dc_search.t (id UInt8, n Nullable(String), lc LowCardinality(Nullable(String)), a Array(String)) ENGINE = MergeTree ORDER BY id",
            r"INSERT INTO dc_search.t VALUES (1, NULL, NULL, []), (2, 'P_Q', NULL, []), (3, 'pxq', 'x', ['50%']), (4, 'a\\b', NULL, ['50x'])",
        ] {
            s.execute(sql, 10).await.unwrap();
        }
        let find = |value: &str| {
            let request = SearchRequest { db: "dc_search".into(), schema: None, value: value.into(), exact: false, case_sensitive: false };
            search(cfg.clone(), request, Arc::new(tokio::sync::Notify::new()))
        };
        for (value, ids) in [("p_q", ["2"]), ("50%", ["3"]), (r"a\b", ["4"])] {
            let report = find(value).await.unwrap();
            assert!(report.errors.is_empty(), "{value}: {:?}", report.errors);
            let found: Vec<String> = report.matches.iter().flat_map(|m| m.result.rows.iter().map(|r| r[0].clone().unwrap_or_default())).collect();
            assert_eq!(found, ids, "{value}");
        }
        s.execute("DROP DATABASE dc_search", 10).await.unwrap();
    }

    /// Copy Table To… into ClickHouse and back out, with SQLite on the other side: target types
    /// that keep the values, NULLs, TRUNCATE for "delete the target's rows first", and ClickHouse
    /// types (Nullable, unsigned…) read back.
    #[tokio::test]
    #[ignore]
    async fn copy_in_and_out() {
        use crate::db::copy::{CopyRequest, Mask, copy};
        let Some(cfg) = test_cfg() else { return };
        let dir = std::env::temp_dir().join(format!("datacraft-ch-copy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.db");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "create table src (id integer primary key, price double precision, amount numeric(10,2), ok boolean, born date, seen timestamp, note text);
                 insert into src values (1, 0.30000000000000004, 12.34, 1, '1960-01-01', '2026-10-23 12:34:56.789', 'a_b');
                 insert into src values (2, null, null, null, null, null, null);",
            )
            .unwrap();
        let mut sqlite = ConnectionConfig::new(2, Driver::Sqlite);
        sqlite.file_path = path.display().to_string();
        let col = |name: &str, ty: &str, pk: bool| ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk };
        let columns = vec![
            col("id", "integer", true),
            col("price", "double precision", false),
            col("amount", "numeric(10,2)", false),
            col("ok", "boolean", false),
            col("born", "date", false),
            col("seen", "timestamp", false),
            col("note", "text", false),
        ];
        let request = |source: (&str, &str), columns: Vec<ColumnInfo>, target: (&str, &str), create: bool, clear: bool| CopyRequest {
            source_db: source.0.into(),
            source_schema: source.0.into(),
            source_table: source.1.into(),
            target_names: columns.iter().map(|c| c.name.clone()).collect(),
            masks: vec![Mask::Keep; columns.len()],
            columns,
            filter: String::new(),
            percent: 100,
            target_db: target.0.into(),
            target_schema: target.0.into(),
            target_table: target.1.into(),
            create,
            clear,
            mask_key: "k".into(),
            parents: false,
            children: false,
            mask_related: false,
        };
        let cancel = || Arc::new(tokio::sync::Notify::new());
        let mut s = ChSession::connect(&cfg).await.unwrap();
        s.execute("DROP DATABASE IF EXISTS dc_copy", 10).await.unwrap();
        s.execute("CREATE DATABASE dc_copy", 10).await.unwrap();

        // Into ClickHouse, creating the table.
        copy(sqlite.clone(), cfg.clone(), request(("main", "src"), columns.clone(), ("dc_copy", "t"), true, false), cancel()).await.unwrap();
        let types: Vec<String> = s
            .rows("SELECT name, type FROM system.columns WHERE database = 'dc_copy' AND table = 't' ORDER BY position")
            .await
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r[0].as_deref().unwrap(), r[1].as_deref().unwrap()))
            .collect();
        assert_eq!(
            types,
            [
                "id Int32",
                "price Nullable(Float64)",
                "amount Nullable(Decimal(10, 2))",
                "ok Nullable(Bool)",
                "born Nullable(Date32)",
                "seen Nullable(DateTime64(6))",
                "note Nullable(String)"
            ]
        );
        let rows = s.rows("SELECT * FROM dc_copy.t ORDER BY id").await.unwrap();
        let first: Vec<Cell> = ["1", "0.30000000000000004", "12.34", "true", "1960-01-01", "2026-10-23 12:34:56.789", "a_b"].map(|v| Some(v.to_string())).into();
        assert_eq!(rows[0], first);
        assert!(rows[1][1..].iter().all(Option::is_none), "{:?}", rows[1]);

        // Again into the existing table, deleting its rows first (TRUNCATE).
        copy(sqlite.clone(), cfg.clone(), request(("main", "src"), columns, ("dc_copy", "t"), false, true), cancel()).await.unwrap();
        assert_eq!(s.rows("SELECT count() FROM dc_copy.t").await.unwrap()[0][0].as_deref(), Some("2"));

        // Out of ClickHouse into a new SQLite table, with the columns as introspected.
        s.use_database("dc_copy").await.unwrap();
        let ch_columns = s.introspect().await.unwrap()[0].tables.iter().find(|t| t.name == "t").unwrap().columns.clone();
        copy(cfg.clone(), sqlite.clone(), request(("dc_copy", "t"), ch_columns, ("main", "back"), true, false), cancel()).await.unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        let mut info = db.prepare("select name, type, \"notnull\" from pragma_table_info('back')").unwrap();
        let back: Vec<String> = info
            .query_map([], |r| Ok(format!("{} {} {}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(back, ["id INTEGER 1", "price REAL 0", "amount NUMERIC 0", "ok BOOLEAN 0", "born DATE 0", "seen DATETIME 0", "note TEXT 0"]);
        let price: f64 = db.query_row("select price from back where id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(price, 0.30000000000000004);
        let nulls: i64 = db.query_row("select count(*) from back where id = 2 and price is null and note is null", [], |r| r.get(0)).unwrap();
        assert_eq!(nulls, 1);

        s.use_database("default").await.unwrap();
        s.execute("DROP DATABASE dc_copy", 10).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}