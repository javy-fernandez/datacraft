//! Read-only index metadata for the Database Explorer, including indexes that the table
//! editor cannot rebuild (expressions, partial indexes and ClickHouse skipping indexes).

use super::{Driver, ResultSet, Session};
use crate::edit::literal;
use crate::i18n::tr;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    /// Undecorated column names for grid navigation; expressions have no direct target.
    pub column_names: Vec<Option<String>>,
    pub unique: bool,
    pub primary_key: bool,
    pub kind: String,
    pub definition: String,
}

pub fn supported(driver: Driver) -> bool {
    matches!(
        driver,
        Driver::Postgres
            | Driver::MySql
            | Driver::MariaDb
            | Driver::MsSql
            | Driver::Oracle
            | Driver::Sqlite
            | Driver::ClickHouse
    )
}

fn catalog_query(driver: Driver, schema: &str, table: &str) -> String {
    let sch = literal(driver, &Some(schema.into()));
    let tab = literal(driver, &Some(table.into()));
    match driver {
        Driver::Postgres => format!(
            "SELECT i.relname AS name, pg_get_indexdef(i.oid, k.ord::int, false) AS column_name, \
             ix.indisunique AS is_unique, ix.indisprimary AS is_primary, am.amname AS kind, pg_get_indexdef(i.oid) AS definition, a.attname AS target_column \
             FROM pg_catalog.pg_index ix JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_catalog.pg_class t ON t.oid = ix.indrelid JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_catalog.pg_am am ON am.oid = i.relam \
             CROSS JOIN LATERAL unnest(ix.indkey::int2[]) WITH ORDINALITY k(attnum, ord) \
             LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum AND k.attnum > 0 \
             WHERE n.nspname = {sch} AND t.relname = {tab} ORDER BY i.relname, k.ord"
        ),
        // SHOW INDEX also exposes functional expressions on MySQL versions that support them,
        // without requiring an EXPRESSION catalog column on older MySQL or MariaDB servers.
        Driver::MySql | Driver::MariaDb => format!(
            "SHOW INDEX FROM {}.{}",
            driver.quote_ident(schema),
            driver.quote_ident(table)
        ),
        Driver::MsSql => format!(
            "SELECT i.name, c.name + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END \
             + CASE WHEN ic.is_included_column = 1 THEN ' (INCLUDE)' ELSE '' END AS column_name, \
             i.is_unique, i.is_primary_key AS is_primary, i.type_desc AS kind, i.filter_definition AS definition, c.name AS target_column \
             FROM sys.indexes i JOIN sys.objects t ON t.object_id = i.object_id \
             LEFT JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             LEFT JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE SCHEMA_NAME(t.schema_id) = {sch} AND t.name = {tab} AND i.index_id > 0 AND i.is_hypothetical = 0 \
             ORDER BY i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
        ),
        Driver::Oracle => format!(
            "SELECT i.index_name AS name, ic.column_name, CASE WHEN i.uniqueness = 'UNIQUE' THEN 1 ELSE 0 END AS is_unique, \
             CASE WHEN EXISTS (SELECT 1 FROM all_constraints p WHERE p.index_owner = i.owner \
             AND p.index_name = i.index_name AND p.constraint_type = 'P') THEN 1 ELSE 0 END AS is_primary, \
             i.index_type AS kind, ic.column_name AS target_column FROM all_indexes i LEFT JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
             WHERE i.table_owner = {sch} AND i.table_name = {tab} AND i.index_type <> 'LOB' ORDER BY i.index_name, ic.column_position"
        ),
        Driver::Sqlite => format!(
            "SELECT il.name, COALESCE(ii.name, '(expression)') || CASE WHEN ii.desc THEN ' DESC' ELSE '' END AS column_name, \
             il.\"unique\" AS is_unique, il.origin = 'pk' AS is_primary, 'btree' AS kind, m.sql AS definition, ii.name AS target_column \
             FROM pragma_index_list({tab}, {sch}) il JOIN pragma_index_xinfo(il.name, {sch}) ii ON ii.key = 1 \
             LEFT JOIN {}.sqlite_schema m ON m.type = 'index' AND m.name = il.name ORDER BY il.name, ii.seqno",
            driver.quote_ident(schema)
        ),
        Driver::ClickHouse => format!(
            "SELECT name, expr AS column_name, 0 AS is_unique, 0 AS is_primary, type_full AS kind, \
             concat('GRANULARITY ', toString(granularity)) AS definition, expr AS target_column FROM system.data_skipping_indices \
             WHERE database = {sch} AND table = {tab} \
             UNION ALL SELECT 'PRIMARY', primary_key, 0, 1, 'primary', '', primary_key FROM system.tables \
             WHERE database = {sch} AND name = {tab} AND primary_key != '' ORDER BY name"
        ),
        _ => String::new(),
    }
}

fn decode(rs: &ResultSet, driver: Driver) -> Vec<IndexInfo> {
    let mysql = matches!(driver, Driver::MySql | Driver::MariaDb);
    let get = |row: &[super::Cell], name: &str| {
        rs.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .and_then(|i| row.get(i))
            .and_then(Option::as_deref)
            .unwrap_or("")
            .to_string()
    };
    let flag = |value: &str| matches!(value, "1" | "t" | "true" | "TRUE");
    let mut rows: Vec<_> = rs.rows.iter().collect();
    if mysql {
        rows.sort_by_key(|r| {
            (
                get(r, "Key_name"),
                get(r, "Seq_in_index").parse::<u32>().unwrap_or(0),
            )
        });
    }
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for row in rows {
        let name = get(row, if mysql { "Key_name" } else { "name" });
        if indexes.last().is_none_or(|i| i.name != name) {
            indexes.push(IndexInfo {
                unique: if mysql {
                    get(row, "Non_unique") == "0"
                } else {
                    flag(&get(row, "is_unique"))
                },
                primary_key: if mysql {
                    name == "PRIMARY"
                } else {
                    flag(&get(row, "is_primary"))
                },
                kind: get(row, if mysql { "Index_type" } else { "kind" }),
                definition: get(row, "definition"),
                name,
                columns: Vec::new(),
                column_names: Vec::new(),
            });
        }
        let mut column = get(row, if mysql { "Column_name" } else { "column_name" });
        let mut target = get(
            row,
            if mysql {
                "Column_name"
            } else {
                "target_column"
            },
        );
        if driver == Driver::ClickHouse
            && !(target.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && target
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            target.clear();
        }
        if mysql {
            if column.is_empty() {
                column = get(row, "Expression");
            }
            let prefix = get(row, "Sub_part");
            if !prefix.is_empty() {
                column.push_str(&format!("({prefix})"));
            }
            if get(row, "Collation") == "D" {
                column.push_str(" DESC");
            }
        }
        if !column.is_empty() {
            indexes
                .last_mut()
                .expect("inserted index")
                .column_names
                .push((!target.is_empty()).then_some(target));
            indexes
                .last_mut()
                .expect("inserted index")
                .columns
                .push(column);
        }
    }
    indexes
}

pub async fn load(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    table: &str,
) -> anyhow::Result<Vec<IndexInfo>> {
    anyhow::ensure!(supported(driver), tr("Not available for this engine"));
    let results = session
        .execute(&catalog_query(driver, schema, table), 100_000)
        .await?;
    let mut indexes = Vec::new();
    for rs in results {
        anyhow::ensure!(!rs.truncated, tr("Too many indexes to display"));
        indexes.extend(decode(&rs, driver));
    }
    Ok(indexes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_indexes_include_constraints_expressions_and_partial_definitions_read_only() {
        let path =
            std::env::temp_dir().join(format!("datacraft-indexes-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE \"a'b\"(a TEXT, b INTEGER, email TEXT UNIQUE, PRIMARY KEY(b,a)) WITHOUT ROWID;
            CREATE INDEX \"idx'compound\" ON \"a'b\"(email DESC,b);
            CREATE INDEX idx_expression ON \"a'b\"(lower(email)) WHERE b > 0;
            CREATE TABLE empty_table(id INTEGER PRIMARY KEY);").unwrap();
        drop(db);
        let mut cfg = super::super::ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        cfg.read_only = true;
        let mut session = Session::connect(&cfg).await.unwrap();
        let indexes = load(&mut session, Driver::Sqlite, "main", "a'b")
            .await
            .unwrap();
        assert_eq!(indexes.len(), 4);
        let primary = indexes.iter().find(|i| i.primary_key).unwrap();
        assert!(primary.unique);
        assert_eq!(primary.columns, ["b", "a"]);
        let compound = indexes.iter().find(|i| i.name == "idx'compound").unwrap();
        assert_eq!(compound.columns, ["email DESC", "b"]);
        assert_eq!(
            compound.column_names,
            [Some("email".into()), Some("b".into())]
        );
        let expression = indexes.iter().find(|i| i.name == "idx_expression").unwrap();
        assert_eq!(expression.columns, ["(expression)"]);
        assert_eq!(expression.column_names, [None]);
        assert!(expression.definition.contains("lower(email)"));
        assert!(expression.definition.contains("WHERE b > 0"));
        assert!(
            indexes
                .iter()
                .any(|i| i.unique && !i.primary_key && i.columns == ["email"])
        );
        // INTEGER PRIMARY KEY aliases the rowid; SQLite has no separate index to invent here.
        assert!(
            load(&mut session, Driver::Sqlite, "main", "empty_table")
                .await
                .unwrap()
                .is_empty()
        );
        drop(session);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mysql_indexes_keep_key_order_prefixes_and_functional_expressions() {
        let rs = ResultSet {
            columns: [
                "Key_name",
                "Seq_in_index",
                "Column_name",
                "Expression",
                "Non_unique",
                "Index_type",
                "Sub_part",
                "Collation",
            ]
            .map(String::from)
            .to_vec(),
            rows: vec![
                vec![
                    Some("idx"),
                    Some("2"),
                    None,
                    Some("lower(email)"),
                    Some("1"),
                    Some("BTREE"),
                    None,
                    Some("A"),
                ],
                vec![
                    Some("PRIMARY"),
                    Some("1"),
                    Some("id"),
                    None,
                    Some("0"),
                    Some("BTREE"),
                    None,
                    Some("A"),
                ],
                vec![
                    Some("idx"),
                    Some("1"),
                    Some("name"),
                    None,
                    Some("1"),
                    Some("BTREE"),
                    Some("20"),
                    Some("D"),
                ],
            ]
            .into_iter()
            .map(|r| r.into_iter().map(|v| v.map(String::from)).collect())
            .collect(),
            ..Default::default()
        };
        let indexes = decode(&rs, Driver::MySql);
        assert_eq!(indexes.len(), 2);
        assert!(indexes[0].primary_key && indexes[0].unique);
        assert_eq!(indexes[1].columns, ["name(20) DESC", "lower(email)"]);
        assert_eq!(indexes[1].column_names, [Some("name".into()), None]);
    }
}
