//! SQL Server driver on top of `tiberius` (TDS 7.3, TLS with rustls).

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use futures_util::TryStreamExt;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::db::sqlite::hex_preview;
use crate::i18n::{tr, trf};

pub struct MsSession {
    client: Client<Compat<TcpStream>>,
    /// Set on I/O or protocol errors: the connection is no longer usable.
    broken: bool,
}

/// Readable message for server errors (without tiberius' `Debug`).
fn ms_error(e: tiberius::error::Error) -> anyhow::Error {
    match e {
        tiberius::error::Error::Server(t) => anyhow!(trf(
            "Error {code} (level {level}, line {line}): {message}",
            &[("code", &t.code()), ("level", &t.class()), ("line", &t.line()), ("message", &t.message())]
        )),
        other => anyhow!(other.to_string()),
    }
}

impl MsSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let port = if cfg.port == 0 { 1433 } else { cfg.port };

        let mut config = Config::new();
        // Through an SSH tunnel the TCP connection goes to its listener, and TLS checks the
        // certificate against the database's own host.
        config.host(cfg.tunnel_host.as_deref().unwrap_or(host));
        config.port(port);
        if !cfg.database.trim().is_empty() {
            config.database(cfg.database.trim());
        }
        config.application_name("DataCraft");
        config.authentication(AuthMethod::sql_server(&cfg.user, &cfg.password));
        // Always encrypt when the server supports it; the certificate is only validated when it isn't trusted.
        config.encryption(EncryptionLevel::Required);
        if cfg.trust_cert {
            config.trust_cert();
        }

        let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, port)))
            .await
            .map_err(|_| anyhow!(trf("Timed out connecting to {host}:{port}", &[("host", &host), ("port", &port)])))?
            .with_context(|| trf("Could not connect to {host}:{port}", &[("host", &host), ("port", &port)]))?;
        tcp.set_nodelay(true)?;

        let client = Client::connect(config, tcp.compat_write()).await.map_err(ms_error)?;
        Ok(Self { client, broken: false })
    }

    /// Stream full native values without exposing the driver connection.
    pub(super) async fn export_rows(
        &mut self,
        sql: &str,
        sender: &tokio::sync::mpsc::Sender<super::table_export::Message>,
        progress: &std::sync::Arc<super::table_export::ExportProgress>,
    ) -> anyhow::Result<()> {
        use super::table_export::{Message, cancelled, mssql_value};
        let mut rows = self.client.simple_query(sql).await?;
        while let Some(item) = rows.try_next().await? {
            cancelled(progress)?;
            if let tiberius::QueryItem::Row(row) = item {
                let values = row.cells().map(|(_, d)| mssql_value(d)).collect::<anyhow::Result<_>>()?;
                sender.send(Message::Row(values)).await.context(tr("Export writer stopped"))?;
            }
        }
        Ok(())
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        match run(&mut self.client, sql, max_rows).await {
            Ok(results) => Ok(results),
            Err(e) => {
                // Server errors (syntax, permissions...) don't invalidate the connection.
                if !matches!(e, tiberius::error::Error::Server(_)) {
                    self.broken = true;
                }
                Err(ms_error(e))
            }
        }
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        let rows = self.scalar_strings("SELECT DB_NAME()").await?;
        rows.into_iter().next().ok_or_else(|| anyhow!(tr("The server did not return the current database")))
    }

    /// ONLINE databases the user has access to, as in DataGrip's explorer.
    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        self.scalar_strings(
            "SELECT name FROM sys.databases WHERE state_desc = 'ONLINE' AND HAS_DBACCESS(name) = 1 ORDER BY name",
        )
        .await
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        let sql = format!("USE [{}]", db.replace(']', "]]"));
        self.execute(&sql, 0).await.map(|_| ())
    }

    /// First column of each row as text.
    async fn scalar_strings(&mut self, sql: &str) -> anyhow::Result<Vec<String>> {
        let rs = self.execute(sql, usize::MAX).await?;
        Ok(rs.into_iter().flat_map(|r| r.rows).filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        self.execute("SET XACT_ABORT ON; BEGIN TRANSACTION", 0).await?;
        let mut outcome: anyhow::Result<u64> = Ok(0);
        for stmt in statements {
            let n = match self.client.execute(stmt.as_str(), &[]).await {
                Ok(r) => r.total(),
                Err(e) => {
                    if !matches!(e, tiberius::error::Error::Server(_)) {
                        self.broken = true;
                    }
                    outcome = Err(ms_error(e));
                    break;
                }
            };
            if n != 1 {
                outcome = Err(super::not_single_row(n, stmt));
                break;
            }
            if let Ok(total) = &mut outcome {
                *total += n;
            }
        }
        match outcome {
            Ok(total) => {
                self.execute("COMMIT TRANSACTION", 0).await?;
                Ok(total)
            }
            Err(e) => {
                if !self.broken {
                    let _ = self.execute("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION", 0).await;
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
            SELECT t.TABLE_SCHEMA, t.TABLE_NAME, t.TABLE_TYPE,
                   c.COLUMN_NAME, c.DATA_TYPE, c.CHARACTER_MAXIMUM_LENGTH,
                   c.NUMERIC_PRECISION, c.NUMERIC_SCALE, c.IS_NULLABLE,
                   CAST(CASE WHEN pk.COLUMN_NAME IS NULL THEN 0 ELSE 1 END AS bit) AS IS_PK
            FROM INFORMATION_SCHEMA.TABLES t
            LEFT JOIN INFORMATION_SCHEMA.COLUMNS c
                   ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME
            LEFT JOIN (
                SELECT k.TABLE_SCHEMA, k.TABLE_NAME, k.COLUMN_NAME
                FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc
                JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k
                  ON k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA
                 AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
                WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY'
            ) pk ON pk.TABLE_SCHEMA = c.TABLE_SCHEMA AND pk.TABLE_NAME = c.TABLE_NAME
                AND pk.COLUMN_NAME = c.COLUMN_NAME
            WHERE t.TABLE_SCHEMA NOT IN ('sys', 'INFORMATION_SCHEMA')
            ORDER BY t.TABLE_SCHEMA, t.TABLE_NAME, c.ORDINAL_POSITION";

        let rows = self
            .client
            .simple_query(SQL)
            .await
            .map_err(ms_error)?
            .into_first_result()
            .await
            .map_err(ms_error)?;

        let mut schemas: Vec<SchemaInfo> = Vec::new();
        for row in rows {
            let text = |i: usize| row.try_get::<&str, _>(i).ok().flatten().unwrap_or_default();
            let int = |i: usize| -> Option<i64> {
                match row.get_column_data(i).ok()? {
                    ColumnData::U8(v) => v.map(i64::from),
                    ColumnData::I16(v) => v.map(i64::from),
                    ColumnData::I32(v) => v.map(i64::from),
                    ColumnData::I64(v) => *v,
                    _ => None,
                }
            };
            let (schema, table) = (text(0), text(1));

            if schemas.last().is_none_or(|s| s.name != schema) {
                schemas.push(SchemaInfo { name: schema.to_string(), tables: Vec::new() });
            }
            let Some(s) = schemas.last_mut() else { continue };
            if s.tables.last().is_none_or(|t| t.name != table) {
                s.tables.push(TableInfo {
                    name: table.to_string(),
                    kind: if text(2) == "VIEW" { ObjectKind::View } else { ObjectKind::Table },
                    columns: Vec::new(),
                });
            }
            let Some(t) = s.tables.last_mut() else { continue };

            let Ok(Some(column)) = row.try_get::<&str, _>(3) else { continue };
            t.columns.push(ColumnInfo {
                name: column.to_string(),
                data_type: type_label(text(4), int(5), int(6), int(7)),
                nullable: text(8) == "YES",
                primary_key: row.try_get::<bool, _>(9).ok().flatten().unwrap_or(false),
            });
        }
        Ok(schemas)
    }
}

/// Type with length/precision in SSMS style: `nvarchar(50)`, `varchar(max)`, `decimal(10,2)`.
fn type_label(data_type: &str, max_len: Option<i64>, precision: Option<i64>, scale: Option<i64>) -> String {
    match data_type {
        "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary" => match max_len {
            Some(-1) => format!("{data_type}(max)"),
            Some(n) => format!("{data_type}({n})"),
            None => data_type.to_string(),
        },
        "decimal" | "numeric" => match (precision, scale) {
            (Some(p), Some(s)) => format!("{data_type}({p},{s})"),
            _ => data_type.to_string(),
        },
        _ => data_type.to_string(),
    }
}

async fn run(
    client: &mut Client<Compat<TcpStream>>,
    sql: &str,
    max_rows: usize,
) -> Result<Vec<ResultSet>, tiberius::error::Error> {
    let statement = sql.trim().to_string();
    let mut start = Instant::now();
    let mut stream = client.simple_query(sql).await?;

    let mut results: Vec<ResultSet> = Vec::new();
    // The whole stream must be consumed even when truncating; otherwise the connection is left half-read.
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(meta) => {
                if let Some(prev) = results.last_mut() {
                    prev.elapsed = start.elapsed();
                    start = Instant::now();
                }
                results.push(ResultSet {
                    statement: statement.clone(),
                    columns: meta.columns().iter().map(|c| c.name().to_string()).collect(),
                    ..Default::default()
                });
            }
            QueryItem::Row(row) => {
                let Some(rs) = results.last_mut() else { continue };
                if rs.rows.len() >= max_rows {
                    rs.truncated = true;
                    continue;
                }
                rs.rows.push(row.cells().map(|(_, data)| cell(data)).collect());
            }
        }
    }

    match results.last_mut() {
        Some(rs) => rs.elapsed = start.elapsed(),
        // DML/DDL: simple_query doesn't report affected rows.
        None => results.push(ResultSet { statement, elapsed: start.elapsed(), ..Default::default() }),
    }
    Ok(results)
}

/// Converts a tiberius value to text. `None` = NULL.
fn cell(data: &ColumnData<'static>) -> Cell {
    match data {
        ColumnData::U8(v) => v.map(|x| x.to_string()),
        ColumnData::I16(v) => v.map(|x| x.to_string()),
        ColumnData::I32(v) => v.map(|x| x.to_string()),
        ColumnData::I64(v) => v.map(|x| x.to_string()),
        ColumnData::F32(v) => v.map(|x| x.to_string()),
        ColumnData::F64(v) => v.map(|x| x.to_string()),
        ColumnData::Bit(v) => v.map(|x| x.to_string()),
        ColumnData::String(v) => v.as_ref().map(|s| s.to_string()),
        ColumnData::Guid(v) => v.map(|g| g.to_string().to_uppercase()),
        ColumnData::Binary(v) => v.as_ref().map(|b| hex_preview(b)),
        ColumnData::Numeric(v) => v.map(numeric_text),
        ColumnData::Xml(v) => v.as_ref().map(|x| x.to_string()),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            date_cell::<NaiveDateTime>(data, |d| d.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        ColumnData::Date(_) => date_cell::<NaiveDate>(data, |d| d.format("%Y-%m-%d").to_string()),
        ColumnData::Time(_) => date_cell::<NaiveTime>(data, |d| d.format("%H:%M:%S%.f").to_string()),
        ColumnData::DateTimeOffset(_) => {
            date_cell::<DateTime<FixedOffset>>(data, |d| d.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string())
        }
    }
}

/// `numeric(18,0)` must show as `42`, not `42.0` as tiberius' `Display` does.
pub(super) fn numeric_text(n: tiberius::numeric::Numeric) -> String {
    let scale = u32::from(n.scale());
    let sign = if n.value() < 0 { "-" } else { "" };
    let abs = n.value().unsigned_abs();
    if scale == 0 {
        return format!("{sign}{abs}");
    }
    let div = 10u128.pow(scale);
    format!("{sign}{}.{:0width$}", abs / div, abs % div, width = scale as usize)
}

fn date_cell<'a, T: FromSql<'a>>(data: &'a ColumnData<'static>, fmt: impl Fn(T) -> String) -> Cell {
    match T::from_sql(data) {
        Ok(v) => v.map(fmt),
        Err(e) => Some(format!("<{e}>")),
    }
}

#[cfg(test)]
mod tests {
    //! Tests against a real SQL Server (NON-destructive: only #tmp temporary tables and SELECTs).
    //! Run them with `cargo test -- --ignored` and the environment variables
    //! DATACRAFT_MSSQL_HOST, DATACRAFT_MSSQL_PORT (1433), DATACRAFT_MSSQL_USER (sa),
    //! DATACRAFT_MSSQL_PASSWORD and DATACRAFT_MSSQL_DB (master). Without HOST the test is skipped.
    use super::numeric_text;
    use tiberius::numeric::Numeric;

    #[test]
    fn numeric_formats_scale() {
        assert_eq!(numeric_text(Numeric::new_with_scale(42, 0)), "42");
        assert_eq!(numeric_text(Numeric::new_with_scale(-1234, 2)), "-12.34");
        assert_eq!(numeric_text(Numeric::new_with_scale(-5, 1)), "-0.5");
        assert_eq!(numeric_text(Numeric::new_with_scale(7, 3)), "0.007");
    }


    use super::*;
    use crate::db::Driver;

    async fn session() -> Option<MsSession> {
        let Ok(host) = std::env::var("DATACRAFT_MSSQL_HOST") else {
            eprintln!("DATACRAFT_MSSQL_HOST not set: skipping the SQL Server test");
            return None;
        };
        let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        let mut cfg = ConnectionConfig::new(1, Driver::MsSql);
        cfg.host = host;
        cfg.port = env("DATACRAFT_MSSQL_PORT", "1433").parse().expect("port");
        cfg.user = env("DATACRAFT_MSSQL_USER", "sa");
        cfg.password = env("DATACRAFT_MSSQL_PASSWORD", "");
        cfg.database = env("DATACRAFT_MSSQL_DB", "master");
        Some(MsSession::connect(&cfg).await.expect("connection"))
    }

    #[tokio::test]
    #[ignore]
    async fn databases_and_use() {
        let Some(mut s) = session().await else { return };
        let current = s.current_database().await.unwrap();
        let dbs = s.list_databases().await.unwrap();
        assert!(dbs.iter().any(|d| d.eq_ignore_ascii_case("master")), "{dbs:?}");
        s.use_database("master").await.unwrap();
        assert_eq!(s.current_database().await.unwrap(), "master");
        s.use_database(&current).await.unwrap();
        assert_eq!(s.current_database().await.unwrap(), current);
        // Transactions on a temporary table: commit and rollback leave nothing open.
        s.execute("CREATE TABLE #dc_edit(id int PRIMARY KEY, v nvarchar(10)); INSERT INTO #dc_edit VALUES (1, N'a'), (2, N'b');", 10)
            .await
            .unwrap();
        assert_eq!(s.apply_changes(&["UPDATE #dc_edit SET v = N'x' WHERE id = N'1'".into()]).await.unwrap(), 1);
        assert!(s.apply_changes(&["UPDATE #dc_edit SET v = N'y' WHERE id = N'1'".into(), "DELETE FROM #dc_edit WHERE id = N'9'".into()]).await.is_err());
        let rs = s.execute("SELECT v FROM #dc_edit WHERE id = 1; SELECT @@TRANCOUNT;", 10).await.unwrap();
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("x"));
        assert_eq!(rs[1].rows[0][0].as_deref(), Some("0"));
        s.execute("DROP TABLE #dc_edit", 0).await.unwrap();
        println!("databases: {dbs:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn mixed_types() {
        let Some(mut s) = session().await else { return };
        let rs = s
            .execute(
                "SELECT CAST(42 AS int) AS i, CAST(12.5 AS decimal(10,2)) AS n, N'hello' AS t, \
                 CAST(1 AS bit) AS b, CAST('2026-10-16' AS date) AS d, \
                 CAST('2026-10-16T10:20:30.5' AS datetime2(3)) AS dt, \
                 CAST('2026-10-16T10:20:30+02:00' AS datetimeoffset(0)) AS dto, \
                 CAST('10:20:30' AS time(0)) AS tm, \
                 CAST('6F1C1B1E-0000-4000-8000-000000000001' AS uniqueidentifier) AS u, \
                 CAST(NULL AS int) AS z, CAST(3.25 AS float) AS f, 0xCAFE AS bin, \
                 CAST(9223372036854775807 AS bigint) AS big",
                100,
            )
            .await
            .unwrap();
        assert_eq!(rs.len(), 1);
        let row: Vec<Option<&str>> = rs[0].rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(
            row,
            [
                Some("42"),
                Some("12.50"),
                Some("hello"),
                Some("true"),
                Some("2026-10-16"),
                Some("2026-10-16 10:20:30.500"),
                Some("2026-10-16 10:20:30 +02:00"),
                Some("10:20:30"),
                Some("6F1C1B1E-0000-4000-8000-000000000001"),
                None,
                Some("3.25"),
                Some("0xCAFE"),
                Some("9223372036854775807"),
            ]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn temp_table_multiple_results_and_introspection() {
        let Some(mut s) = session().await else { return };
        let rs = s
            .execute("CREATE TABLE #dc_tmp (id int PRIMARY KEY, name nvarchar(20) NULL)", 10)
            .await
            .unwrap();
        assert!(rs[0].is_command());

        let rs = s
            .execute(
                "INSERT INTO #dc_tmp VALUES (1, N'a'), (2, NULL), (3, N'c');\
                 SELECT id, name FROM #dc_tmp ORDER BY id;\
                 SELECT id FROM #dc_tmp WHERE 1 = 0;\
                 SELECT COUNT(*) AS n FROM #dc_tmp;",
                2,
            )
            .await
            .unwrap();
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0].columns, ["id", "name"]);
        assert_eq!(rs[0].rows.len(), 2);
        assert!(rs[0].truncated);
        assert_eq!(rs[0].rows[1][1], None);
        assert!(rs[1].rows.is_empty() && rs[1].columns == ["id"]);
        assert_eq!(rs[2].rows[0][0].as_deref(), Some("3"));

        // The connection stays usable after truncating and after an error.
        let err = s.execute("SELECT * FROM dbo.datacraft_missing_xyz", 10).await.unwrap_err();
        assert!(err.to_string().contains("datacraft_missing_xyz"), "{err}");
        s.execute("DROP TABLE #dc_tmp", 10).await.unwrap();

        let schemas = s.introspect().await.unwrap();
        assert!(!schemas.iter().any(|s| s.name == "sys" || s.name == "INFORMATION_SCHEMA"));
        for t in schemas.iter().flat_map(|s| &s.tables) {
            assert!(!t.name.is_empty());
        }
    }
}
