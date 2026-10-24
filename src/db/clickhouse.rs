//! ClickHouse driver on top of `nativeclick` (native TCP protocol, port 9000).
//!
//! The native protocol carries one statement per query, so the console's statement splitter
//! does the work and `execute` receives a single statement. Values arrive dynamically typed
//! and are converted to text here, like in the other drivers.

use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures_util::TryStreamExt;
use nativeclick::{Client, ClientOptions, NativeclickError, Type, Value};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};

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

async fn connect_client(cfg: &ConnectionConfig) -> anyhow::Result<Client> {
    let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
    let port = if cfg.port == 0 { 9000 } else { cfg.port };
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
    let client = Client::connect((host, port), options).await.map_err(|e| match e {
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
                nullable: data_type.starts_with("Nullable("),
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
                    // Dropping the stream cancels the query: the rest isn't downloaded.
                    truncated = true;
                    break 'blocks;
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

/// Column type for DDL: nullability is part of the type in ClickHouse (`Nullable(T)`), and a
/// bare `String` is NOT NULL. The `null` modifier makes it Nullable, but the server rejects it on a
/// type that already is (also inside `LowCardinality(…)`), so it's only added to plain types.
pub fn column_type(data_type: &str, not_null: bool) -> String {
    let ty = data_type.trim();
    match ty.strip_prefix("Nullable(").and_then(|t| t.strip_suffix(')')) {
        Some(inner) if not_null => inner.to_string(),
        _ if not_null || ty.contains("Nullable(") => ty.to_string(),
        _ => format!("{ty} null"),
    }
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
        assert_eq!(column_type("String", false), "String null");
        assert_eq!(column_type("Nullable(String)", false), "Nullable(String)");
        assert_eq!(column_type(" Nullable(Int32) ", true), "Int32");
        // The server rejects NULL on a type that already is Nullable, also nested.
        assert_eq!(column_type("LowCardinality(Nullable(String))", false), "LowCardinality(Nullable(String))");
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
}