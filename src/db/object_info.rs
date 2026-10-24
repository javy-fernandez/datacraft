//! On-demand catalog information. Browsing never runs a full-table count.

use super::{Driver, ResultSet, Session};
use crate::edit::literal;
use crate::i18n::tr;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectInfo {
    pub rows: Option<u64>,
    pub rows_exact: bool,
    pub data_bytes: Option<u64>,
    pub index_bytes: Option<u64>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub engine: Option<String>,
    pub comment: Option<String>,
    pub collation: Option<String>,
    pub row_format: Option<String>,
    pub average_row_bytes: Option<u64>,
    pub max_data_bytes: Option<u64>,
    pub checked: Option<String>,
    pub auto_increment: Option<u64>,
    pub free_bytes: Option<u64>,
    pub create_options: Option<String>,
}

fn catalog_query(driver: Driver, schema: &str, table: &str) -> String {
    catalog_scope_query(driver, Some(schema), Some(table))
}

fn catalog_scope_query(driver: Driver, schema: Option<&str>, table: Option<&str>) -> String {
    let (base, schema_field, table_field, default_schema) = match driver {
        Driver::MySql | Driver::MariaDb => (
            "SELECT TABLE_SCHEMA AS schema_name, TABLE_NAME AS object_name, TABLE_ROWS AS row_count, DATA_LENGTH AS data_bytes, INDEX_LENGTH AS index_bytes, \
             CREATE_TIME AS created, UPDATE_TIME AS modified, ENGINE AS engine, TABLE_COMMENT AS object_comment, TABLE_COLLATION AS object_collation, \
             ROW_FORMAT AS row_format, AVG_ROW_LENGTH AS average_row_bytes, MAX_DATA_LENGTH AS max_data_bytes, CHECK_TIME AS checked, \
             AUTO_INCREMENT AS auto_increment_value, DATA_FREE AS free_bytes, CREATE_OPTIONS AS create_options FROM information_schema.TABLES WHERE 1=1".to_string(),
            "TABLE_SCHEMA", "TABLE_NAME", " AND TABLE_SCHEMA = DATABASE()"),
        Driver::Postgres => (
            "SELECT n.nspname AS schema_name, c.relname AS object_name, CASE WHEN c.relkind IN ('r','m') AND c.reltuples >= 0 THEN c.reltuples::bigint END AS row_count, \
             CASE WHEN c.relkind IN ('r','m') THEN pg_catalog.pg_table_size(c.oid) END AS data_bytes, \
             CASE WHEN c.relkind IN ('r','m') THEN pg_catalog.pg_indexes_size(c.oid) END AS index_bytes, pg_catalog.obj_description(c.oid, 'pg_class') AS object_comment \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p','v','m','f')".to_string(),
            "n.nspname", "c.relname", " AND n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%'"),
        Driver::MsSql => (
            "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name AS object_name, (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0,1)) AS row_count, \
             o.create_date AS created, o.modify_date AS modified, CAST(ep.value AS nvarchar(4000)) AS object_comment \
             FROM sys.objects o LEFT JOIN sys.extended_properties ep ON ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.class = 1 AND ep.name = N'MS_Description' \
             WHERE o.type IN ('U','V') AND o.is_ms_shipped = 0".to_string(), "SCHEMA_NAME(o.schema_id)", "o.name", ""),
        Driver::Oracle => {
            let excluded = super::oracle::SYSTEM_SCHEMAS.iter().map(|name| literal(driver, &Some((*name).into()))).collect::<Vec<_>>().join(",");
            (format!("SELECT o.owner AS schema_name, o.object_name AS object_name, t.num_rows AS row_count, o.created AS created, o.last_ddl_time AS modified, c.comments AS object_comment \
             FROM all_objects o LEFT JOIN all_tables t ON t.owner = o.owner AND t.table_name = o.object_name \
             LEFT JOIN all_tab_comments c ON c.owner = o.owner AND c.table_name = o.object_name WHERE o.object_type IN ('TABLE','VIEW') AND o.owner NOT IN ({excluded})"),
            "o.owner", "o.object_name", "")
        },
        Driver::Redis | Driver::MongoDb => (String::new(), "", "", ""),
        Driver::Snowflake => (
            "SELECT TABLE_SCHEMA AS schema_name, TABLE_NAME AS object_name, ROW_COUNT AS row_count, BYTES AS data_bytes, CREATED AS created, LAST_ALTERED AS modified, COMMENT AS object_comment \
             FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA <> 'INFORMATION_SCHEMA'".to_string(), "TABLE_SCHEMA", "TABLE_NAME", ""
        ),
        // ClickHouse keeps sizes and comments in system.tables.
        Driver::ClickHouse => (
            "SELECT database AS schema_name, name AS object_name, total_rows AS row_count, total_bytes AS data_bytes, \
             metadata_modification_time AS modified, engine AS engine, comment AS object_comment \
             FROM system.tables WHERE database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema')".to_string(),
            "database", "name", " AND database = currentDatabase()",
        ),
        Driver::Sqlite => {
            let schema = schema.unwrap_or("main");
            (format!("SELECT {} AS schema_name, name AS object_name, NULL AS row_count FROM {}.sqlite_schema WHERE type IN ('table','view')",
                literal(driver, &Some(schema.to_string())), driver.quote_ident(schema)), "", "name", "")
        }
    };
    let schema_filter = if schema_field.is_empty() {
        String::new()
    } else {
        schema
            .map(|s| format!(" AND {schema_field} = {}", literal(driver, &Some(s.into()))))
            .unwrap_or_else(|| default_schema.to_string())
    };
    let table_filter = table
        .map(|t| format!(" AND {table_field} = {}", literal(driver, &Some(t.into()))))
        .unwrap_or_default();
    format!("{base}{schema_filter}{table_filter}")
}

fn decode(rs: &ResultSet) -> Option<ObjectInfo> {
    decode_row(rs, 0)
}

fn decode_row(rs: &ResultSet, index: usize) -> Option<ObjectInfo> {
    let row = rs.rows.get(index)?;
    let get = |name: &str| {
        let i = rs
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        row.get(i)?.as_ref().filter(|s| !s.is_empty()).cloned()
    };
    let number = |name: &str| get(name).and_then(|v| v.parse().ok());
    Some(ObjectInfo {
        rows: number("row_count"),
        rows_exact: false,
        data_bytes: number("data_bytes"),
        index_bytes: number("index_bytes"),
        created: get("created"),
        modified: get("modified"),
        engine: get("engine"),
        comment: get("object_comment"),
        collation: get("object_collation"),
        row_format: get("row_format"),
        average_row_bytes: number("average_row_bytes"),
        max_data_bytes: number("max_data_bytes"),
        checked: get("checked"),
        auto_increment: number("auto_increment_value"),
        free_bytes: number("free_bytes"),
        create_options: get("create_options"),
    })
}

impl Session {
    pub async fn object_list_info(
        &mut self,
        driver: Driver,
        schema: Option<&str>,
    ) -> anyhow::Result<Vec<ObjectInfoRow>> {
        if driver.is_nosql() {
            anyhow::bail!(tr("Object information is not available"));
        }
        let results = self
            .execute(&catalog_scope_query(driver, schema, None), 100_000)
            .await?;
        let mut out = Vec::new();
        for rs in results {
            if rs.truncated {
                anyhow::bail!(tr(
                    "Too many objects; select a schema to load its statistics"
                ));
            }
            let schema_index = rs
                .columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case("schema_name"));
            let name_index = rs
                .columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case("object_name"));
            if let (Some(si), Some(ni)) = (schema_index, name_index) {
                for (i, row) in rs.rows.iter().enumerate() {
                    if let (Some(Some(schema)), Some(Some(name)), Some(info)) =
                        (row.get(si), row.get(ni), decode_row(&rs, i))
                    {
                        out.push(ObjectInfoRow {
                            schema: schema.clone(),
                            name: name.clone(),
                            info,
                        });
                    }
                }
            }
        }
        Ok(out)
    }
    pub async fn object_info(
        &mut self,
        driver: Driver,
        schema: &str,
        table: &str,
        count_rows: bool,
    ) -> anyhow::Result<ObjectInfo> {
        if driver.is_nosql() {
            anyhow::bail!(tr("Object information is not available"));
        }
        let results = self
            .execute(&catalog_query(driver, schema, table), 1)
            .await?;
        let mut info = results
            .iter()
            .find_map(decode)
            .ok_or_else(|| anyhow::anyhow!(tr("Object information is not available")))?;
        if count_rows {
            // Explicit user action: an exact count may scan a large table or execute a view.
            info.rows = Some(self.table_row_count(driver, schema, table, "").await?);
            info.rows_exact = true;
        }
        Ok(info)
    }

    pub async fn table_row_count(&mut self, driver: Driver, schema: &str, table: &str, filter: &str) -> anyhow::Result<u64> {
        let qualified = format!("{}.{}", driver.quote_ident(schema), driver.quote_ident(table));
        let count = if driver == Driver::MsSql { "COUNT_BIG(*)" } else { "COUNT(*)" };
        let mut sql = format!("SELECT {count} AS row_count FROM {qualified}");
        if !filter.trim().is_empty() { sql.push_str(&format!(" WHERE {filter}")); }
        if crate::sql::split_statements(&sql, driver).len() != 1 { anyhow::bail!(tr("Filter text must be a single SQL expression")); }
        let results = self.execute(&sql, 1).await?;
        results.iter().find_map(decode).and_then(|info| info.rows).ok_or_else(|| anyhow::anyhow!(tr("Exact row count unavailable")))
    }
}

#[derive(Clone, Debug)]
pub struct ObjectInfoRow {
    pub schema: String,
    pub name: String,
    pub info: ObjectInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_names_are_literals_and_never_trigger_full_counts() {
        for driver in Driver::ALL.into_iter().filter(|d| !d.is_nosql()) {
            let query = catalog_query(driver, "a'b", "x'; DROP TABLE users; --");
            assert!(query.contains("a''b") || driver == Driver::Sqlite);
            assert!(query.contains("x''; DROP TABLE users; --"));
            assert!(!query.contains("COUNT(*)"));
        }
        assert!(catalog_query(Driver::Sqlite, "a\"b", "t").contains("\"a\"\"b\".sqlite_schema"));
    }

    #[test]
    fn missing_and_negative_statistics_remain_unknown() {
        let rs = ResultSet {
            columns: vec!["ROW_COUNT".into(), "DATA_BYTES".into(), "CREATED".into()],
            rows: vec![vec![Some("-1".into()), Some("4096".into()), None]],
            ..Default::default()
        };
        let info = decode(&rs).unwrap();
        assert_eq!(info.rows, None);
        assert_eq!(info.data_bytes, Some(4096));
        assert!(!info.rows_exact);
        assert_eq!(info.created, None);
    }

    #[tokio::test]
    async fn sqlite_catalog_and_explicit_count_support_quoted_names_and_views() {
        let path = std::env::temp_dir().join(format!("datacraft_info_{}.db", uuid::Uuid::new_v4()));
        std::fs::File::create(&path).unwrap();
        let mut cfg = super::super::ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let mut session = Session::connect(&cfg).await.unwrap();
        session.execute("CREATE TABLE \"a'b\"(id INTEGER); INSERT INTO \"a'b\" VALUES(1),(2); CREATE VIEW v AS SELECT * FROM \"a'b\";", 10).await.unwrap();
        let info = session
            .object_info(Driver::Sqlite, "main", "a'b", false)
            .await
            .unwrap();
        let catalog = session
            .object_list_info(Driver::Sqlite, Some("main"))
            .await
            .unwrap();
        assert_eq!(catalog.len(), 2);
        assert!(
            catalog
                .iter()
                .any(|row| row.name == "a'b" && row.schema == "main")
        );
        assert!(catalog.iter().any(|row| row.name == "v"));
        assert!(catalog.iter().all(|row| row.info.rows.is_none()));
        assert_eq!(info.rows, None);
        let counted = session
            .object_info(Driver::Sqlite, "main", "a'b", true)
            .await
            .unwrap();
        assert_eq!(counted.rows, Some(2));
        assert!(counted.rows_exact);
        assert_eq!(session.table_row_count(Driver::Sqlite, "main", "a'b", "id > 1").await.unwrap(), 1);
        assert_eq!(session.table_row_count(Driver::Sqlite, "main", "a'b", "id > 5").await.unwrap(), 0);
        assert!(session.table_row_count(Driver::Sqlite, "main", "a'b", "1=1; DROP TABLE v").await.is_err());
        assert_eq!(
            session
                .object_info(Driver::Sqlite, "main", "v", true)
                .await
                .unwrap()
                .rows,
            Some(2)
        );
        assert!(
            session
                .object_info(Driver::Sqlite, "main", "missing", false)
                .await
                .is_err()
        );
        drop(session);
        std::fs::remove_file(path).unwrap();
    }
}
