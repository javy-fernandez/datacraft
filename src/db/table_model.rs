//! A local model made only from catalog objects and declared foreign keys.

use super::{SchemaInfo, subset::ForeignKey};
use serde::{Deserialize, Serialize};

pub fn supported(driver: super::Driver) -> bool {
    !driver.is_nosql() && driver != super::Driver::ClickHouse
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub tables: Vec<ModelTable>,
    pub relations: Vec<Relation>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ModelColumn>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelColumn {
    pub name: String,
    pub data_type: String,
    pub primary_key: bool,
    pub nullable: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

pub fn from_catalog(catalog: &[SchemaInfo], selected: &[(String, String)], keys: &[ForeignKey]) -> anyhow::Result<Model> {
    use std::collections::BTreeSet;
    anyhow::ensure!(!selected.is_empty(), crate::i18n::tr("Select tables to model"));
    let available: BTreeSet<_> =
        catalog.iter().flat_map(|s| s.tables.iter().filter(|t| t.kind == super::ObjectKind::Table).map(|t| (s.name.clone(), t.name.clone()))).collect();
    let selected: BTreeSet<_> = selected.iter().cloned().collect();
    anyhow::ensure!(selected.is_subset(&available), crate::i18n::tr("A selected table was not found in the catalog"));
    let mut included = selected.clone();
    for key in keys {
        let child = (key.schema.clone(), key.table.clone());
        let parent = (key.ref_schema.clone(), key.ref_table.clone());
        if selected.contains(&child) || selected.contains(&parent) {
            included.insert(child);
            included.insert(parent);
        }
    }
    let tables: Vec<_> = catalog
        .iter()
        .flat_map(|s| {
            s.tables.iter().filter(|t| included.contains(&(s.name.clone(), t.name.clone()))).map(|t| ModelTable {
                schema: s.name.clone(),
                name: t.name.clone(),
                columns: t
                    .columns
                    .iter()
                    .map(|c| ModelColumn { name: c.name.clone(), data_type: c.data_type.clone(), primary_key: c.primary_key, nullable: c.nullable })
                    .collect(),
            })
        })
        .collect();
    let visible: BTreeSet<_> = tables.iter().map(|t| (t.schema.clone(), t.name.clone())).collect();
    let relations = keys
        .iter()
        .filter(|k| visible.contains(&(k.schema.clone(), k.table.clone())) && visible.contains(&(k.ref_schema.clone(), k.ref_table.clone())))
        .map(|k| Relation {
            schema: k.schema.clone(),
            table: k.table.clone(),
            columns: k.columns.clone(),
            ref_schema: k.ref_schema.clone(),
            ref_table: k.ref_table.clone(),
            ref_columns: k.ref_columns.clone(),
        })
        .collect();
    Ok(Model { tables, relations })
}

fn column_sql(driver: super::Driver, schema: &str, table: &str) -> String {
    use super::{Driver, table_definition::lit};
    let (sch, tab) = (lit(driver, schema), lit(driver, table));
    match driver {
        Driver::Sqlite => format!(
            "SELECT m.name,p.name,p.type,CASE p.\"notnull\" WHEN 0 THEN '1' ELSE '0' END,CASE WHEN p.pk>0 THEN '1' ELSE '0' END FROM {}.sqlite_schema m LEFT JOIN pragma_table_xinfo(m.name,{sch}) p WHERE m.type='table' AND m.name={tab} AND (p.hidden<>1 OR p.name IS NULL) ORDER BY p.cid",
            driver.quote_ident(schema)
        ),
        Driver::Postgres => format!(
            "SELECT c.relname,a.attname,format_type(a.atttypid,a.atttypmod),CASE WHEN a.attnotnull OR (ty.typtype='d' AND ty.typnotnull) THEN '0' ELSE '1' END,CASE WHEN EXISTS(SELECT 1 FROM pg_index i WHERE i.indrelid=c.oid AND i.indisprimary AND a.attnum=ANY(i.indkey)) THEN '1' ELSE '0' END FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped LEFT JOIN pg_type ty ON ty.oid=a.atttypid WHERE n.nspname={sch} AND c.relname={tab} AND c.relkind IN ('r','p','f') ORDER BY a.attnum"
        ),
        Driver::MySql | Driver::MariaDb => format!(
            "SELECT t.TABLE_NAME,c.COLUMN_NAME,LOWER(c.COLUMN_TYPE),CASE c.IS_NULLABLE WHEN 'YES' THEN '1' ELSE '0' END,CASE c.COLUMN_KEY WHEN 'PRI' THEN '1' ELSE '0' END FROM information_schema.TABLES t LEFT JOIN information_schema.COLUMNS c ON c.TABLE_SCHEMA=t.TABLE_SCHEMA AND c.TABLE_NAME=t.TABLE_NAME WHERE t.TABLE_SCHEMA={sch} AND t.TABLE_NAME={tab} AND t.TABLE_TYPE NOT LIKE '%VIEW' ORDER BY c.ORDINAL_POSITION"
        ),
        Driver::MsSql => format!(
            "SELECT t.TABLE_NAME,c.COLUMN_NAME,c.DATA_TYPE,CASE c.IS_NULLABLE WHEN 'YES' THEN '1' ELSE '0' END,CASE WHEN EXISTS(SELECT 1 FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS k JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE p ON p.CONSTRAINT_SCHEMA=k.CONSTRAINT_SCHEMA AND p.CONSTRAINT_NAME=k.CONSTRAINT_NAME AND p.TABLE_NAME=k.TABLE_NAME WHERE k.CONSTRAINT_TYPE='PRIMARY KEY' AND p.TABLE_SCHEMA=c.TABLE_SCHEMA AND p.TABLE_NAME=c.TABLE_NAME AND p.COLUMN_NAME=c.COLUMN_NAME) THEN '1' ELSE '0' END,c.CHARACTER_MAXIMUM_LENGTH,c.NUMERIC_PRECISION,c.NUMERIC_SCALE FROM INFORMATION_SCHEMA.TABLES t LEFT JOIN INFORMATION_SCHEMA.COLUMNS c ON c.TABLE_SCHEMA=t.TABLE_SCHEMA AND c.TABLE_NAME=t.TABLE_NAME WHERE t.TABLE_SCHEMA={sch} AND t.TABLE_NAME={tab} AND t.TABLE_TYPE='BASE TABLE' ORDER BY c.ORDINAL_POSITION"
        ),
        Driver::Oracle => format!(
            "SELECT t.table_name,c.column_name,c.data_type,CASE c.nullable WHEN 'Y' THEN '1' ELSE '0' END,CASE WHEN EXISTS(SELECT 1 FROM all_constraints k JOIN all_cons_columns p ON p.owner=k.owner AND p.constraint_name=k.constraint_name WHERE k.constraint_type='P' AND p.owner=c.owner AND p.table_name=c.table_name AND p.column_name=c.column_name) THEN '1' ELSE '0' END,c.data_length,c.char_length,c.data_precision,c.data_scale,c.char_used FROM all_tables t LEFT JOIN all_tab_cols c ON c.owner=t.owner AND c.table_name=t.table_name AND c.hidden_column='NO' WHERE t.owner={sch} AND t.table_name={tab} ORDER BY c.column_id"
        ),
        Driver::Snowflake => format!(
            "SELECT t.TABLE_NAME,c.COLUMN_NAME,c.DATA_TYPE,CASE c.IS_NULLABLE WHEN 'YES' THEN '1' ELSE '0' END,'0',c.CHARACTER_MAXIMUM_LENGTH,c.NUMERIC_PRECISION,c.NUMERIC_SCALE,c.DATETIME_PRECISION FROM INFORMATION_SCHEMA.TABLES t LEFT JOIN INFORMATION_SCHEMA.COLUMNS c ON c.TABLE_SCHEMA=t.TABLE_SCHEMA AND c.TABLE_NAME=t.TABLE_NAME WHERE t.TABLE_SCHEMA={sch} AND t.TABLE_NAME={tab} AND t.TABLE_TYPE NOT LIKE '%VIEW' ORDER BY c.ORDINAL_POSITION"
        ),
        _ => String::new(),
    }
}

async fn table_columns(session: &mut super::Session, driver: super::Driver, schema: &str, table: &str) -> anyhow::Result<Option<super::TableInfo>> {
    use super::{ColumnInfo, Driver, ObjectKind, TableInfo};
    let sets = session.execute(&column_sql(driver, schema, table), 100_000).await?;
    let mut found = false;
    let mut columns = Vec::new();
    for rs in sets {
        anyhow::ensure!(!rs.truncated, crate::i18n::tr("The table catalog is too large to model completely"));
        for row in rs.rows {
            found = true;
            let text = |i| row.get(i).and_then(Option::as_deref);
            let Some(name) = text(1) else { continue };
            let ty = text(2).ok_or_else(|| anyhow::anyhow!(crate::i18n::tr("Incomplete table column metadata")))?;
            let int = |i| text(i).and_then(|s| s.parse::<i64>().ok());
            let data_type = match driver {
                Driver::MsSql => super::mssql::type_label(ty, int(5), int(6), int(7)),
                Driver::Oracle => super::oracle::type_label(ty, int(5), int(6), text(9), int(7), int(8)),
                Driver::Snowflake => super::snowflake::type_label(ty, int(5), int(6), int(7), int(8)),
                _ => ty.to_string(),
            };
            columns.push(ColumnInfo { name: name.into(), data_type, nullable: text(3) == Some("1"), primary_key: text(4) == Some("1") });
        }
    }
    if found && driver == Driver::Snowflake {
        for rs in session.execute(&format!("SHOW PRIMARY KEYS IN TABLE {}", super::Session::quote_table(driver, schema, table)), 100_000).await? {
            anyhow::ensure!(!rs.truncated, crate::i18n::tr("The table catalog is too large to model completely"));
            if let Some(index) = rs.columns.iter().position(|c| c.eq_ignore_ascii_case("column_name")) {
                for column in &mut columns {
                    column.primary_key = rs.rows.iter().any(|row| row.get(index).and_then(Option::as_deref) == Some(&column.name));
                }
            }
        }
    }
    Ok(found.then(|| TableInfo { name: table.into(), kind: ObjectKind::Table, columns }))
}

fn sqlite_fk_sql(tables: &std::collections::BTreeSet<(String, String)>, direct: bool) -> String {
    let names =
        tables.iter().filter(|(schema, _)| schema == "main").map(|(_, table)| super::table_definition::lit(super::Driver::Sqlite, table)).collect::<Vec<_>>();
    let names = if names.is_empty() { "NULL".into() } else { names.join(",") };
    let join = if direct { "OR" } else { "AND" };
    // Resolve omitted parent PK columns only in the projection of visible edges.
    format!(
        "WITH keys AS (SELECT m.name AS child,p.id,p.\"from\" AS child_column,p.\"table\" AS parent,p.\"to\" AS parent_column,p.seq FROM main.sqlite_schema m JOIN pragma_foreign_key_list(m.name,'main') p WHERE m.type='table' AND (m.name IN ({names}) {join} p.\"table\" IN ({names}))) SELECT child||':'||id,'main',child,child_column,'main',parent,coalesce(nullif(parent_column,''),(SELECT name FROM pragma_table_xinfo(parent,'main') WHERE pk=seq+1)),seq FROM keys"
    )
}

pub async fn load(session: &mut super::Session, driver: super::Driver, db: &str, selected: &[(String, String)]) -> anyhow::Result<Model> {
    use std::collections::{BTreeMap, BTreeSet};
    anyhow::ensure!(supported(driver), crate::i18n::tr("Not available for this engine"));
    anyhow::ensure!(!selected.is_empty(), crate::i18n::tr("Select tables to model"));
    let selected: BTreeSet<_> = selected.iter().cloned().collect();
    let mut keys = Vec::new();
    let schemas: BTreeSet<_> = selected.iter().map(|(s, _)| s.as_str()).collect();
    for schema in schemas {
        let sql = if driver == super::Driver::Sqlite { sqlite_fk_sql(&selected, true) } else { super::subset::fk_sql(driver, db, schema) };
        if sql.is_empty() {
            break;
        }
        for rs in session.execute(&sql, 100_000).await? {
            anyhow::ensure!(!rs.truncated, crate::i18n::tr("The foreign key catalog is too large to model completely"));
            for key in super::subset::parse_fks(driver, &rs, &[]) {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        if !matches!(driver, super::Driver::Oracle | super::Driver::Snowflake) {
            break;
        }
    }
    // Only the FK inventory spans the accessible scope; columns are read for the visible one-hop graph.
    let mut included = selected.clone();
    for key in &keys {
        let child = (key.schema.clone(), key.table.clone());
        let parent = (key.ref_schema.clone(), key.ref_table.clone());
        if selected.contains(&child) || selected.contains(&parent) {
            included.insert(child);
            included.insert(parent);
        }
    }
    let mut catalog: BTreeMap<String, Vec<super::TableInfo>> = BTreeMap::new();
    for (schema, table) in &included {
        if let Some(info) = table_columns(session, driver, schema, table).await? {
            catalog.entry(schema.clone()).or_default().push(info);
        }
    }
    let catalog: Vec<_> = catalog.into_iter().map(|(name, tables)| SchemaInfo { name, tables }).collect();
    if driver == super::Driver::Sqlite {
        keys.clear();
        for rs in session.execute(&sqlite_fk_sql(&included, false), 100_000).await? {
            anyhow::ensure!(!rs.truncated, crate::i18n::tr("The foreign key catalog is too large to model completely"));
            keys.extend(super::subset::parse_fks(driver, &rs, &catalog));
        }
    }
    from_catalog(&catalog, &selected.into_iter().collect::<Vec<_>>(), &keys)
}

/// Read on a dedicated session; cancellation interrupts a native query before releasing it.
pub async fn read(mut cfg: super::ConnectionConfig, db: &str, tables: &[(String, String)], cancel: &std::sync::atomic::AtomicBool) -> anyhow::Result<Model> {
    use crate::i18n::tr;
    use std::{sync::atomic::Ordering, time::Duration};
    anyhow::ensure!(supported(cfg.driver), tr("Not available for this engine"));
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), tr("Cancelled"));
    let driver = cfg.driver;
    if !matches!(driver, super::Driver::Sqlite | super::Driver::Oracle) {
        cfg.database = db.into();
    }
    let mut session = tokio::time::timeout(Duration::from_secs(30), super::Session::connect(&cfg)).await??;
    tokio::time::timeout(Duration::from_secs(30), session.use_database(db)).await??;
    let handle = session.cancel_handle();
    let query = load(&mut session, driver, db, tables);
    tokio::pin!(query);
    let cancelled = async {
        while !cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let (result, abandoned) = tokio::select! {
        result = &mut query => (result, false),
        _ = tokio::time::sleep(Duration::from_secs(30)) => (Err(anyhow::anyhow!(tr("Table operation timed out"))), true),
        _ = cancelled => (Err(anyhow::anyhow!(tr("Cancelled"))), true),
    };
    if abandoned && let Some(handle) = handle {
        let _ = tokio::time::timeout(Duration::from_secs(3), handle.cancel()).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut query).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnInfo, ObjectKind, TableInfo};

    #[test]
    fn targeted_postgres_nullability_keeps_domain_not_null_constraints() {
        let sql = column_sql(super::super::Driver::Postgres, "s", "t");
        let select = sql.strip_prefix("SELECT ").unwrap().split(" FROM pg_class ").next().unwrap();
        let (mut depth, mut start) = (0, 0);
        let mut fields = Vec::new();
        for (i, ch) in select.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    fields.push(&select[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        fields.push(&select[start..]);
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE pg_attribute(attnum INTEGER,attnotnull INTEGER,atttypid INTEGER); CREATE TABLE pg_type(oid INTEGER,typtype TEXT,typnotnull INTEGER); INSERT INTO pg_type VALUES(1,'b',0),(2,'d',1),(3,'d',0); INSERT INTO pg_attribute VALUES(1,0,1),(2,1,1),(3,0,2),(4,0,3);").unwrap();
        let mut query = db.prepare(&format!("SELECT {} FROM pg_attribute a JOIN pg_type ty ON ty.oid=a.atttypid ORDER BY a.attnum", fields[3])).unwrap();
        let nullable = query.query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(nullable, ["1", "0", "0", "1"]);
    }

    #[tokio::test]
    async fn targeted_oracle_catalog_keeps_character_semantics_and_unbounded_number_scale() {
        let path = std::env::temp_dir().join(format!("datacraft-model-oracle-catalog-{}.db", uuid::Uuid::new_v4()));
        std::fs::File::create(&path).unwrap();
        let mut cfg = super::super::ConnectionConfig::new(1, super::super::Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let mut session = super::super::Session::connect(&cfg).await.unwrap();
        session.execute("CREATE TABLE all_tables(owner TEXT,table_name TEXT); INSERT INTO all_tables VALUES('S','T'); CREATE TABLE all_tab_cols(owner TEXT,table_name TEXT,hidden_column TEXT,column_id INTEGER,column_name TEXT,data_type TEXT,nullable TEXT,data_length INTEGER,char_length INTEGER,char_used TEXT,data_precision INTEGER,data_scale INTEGER); INSERT INTO all_tab_cols VALUES('S','T','NO',1,'title','VARCHAR2','Y',40,10,'C',NULL,NULL),('S','T','NO',2,'amount','NUMBER','N',22,NULL,NULL,NULL,2); CREATE TABLE all_constraints(owner TEXT,constraint_name TEXT,constraint_type TEXT); CREATE TABLE all_cons_columns(owner TEXT,constraint_name TEXT,table_name TEXT,column_name TEXT);", 0).await.unwrap();
        let result = table_columns(&mut session, super::super::Driver::Oracle, "S", "T").await;
        drop(session);
        std::fs::remove_file(path).unwrap();
        let columns = result.unwrap().unwrap().columns;
        assert_eq!(columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), ["varchar2(10 char)", "number(*,2)"]);
        assert!(columns[0].nullable && !columns[1].nullable);
    }

    #[tokio::test]
    async fn targeted_columns_keep_quoted_names_and_edges_between_visible_neighbors() {
        let path = std::env::temp_dir().join(format!("datacraft-model-quotes-{}.db", uuid::Uuid::new_v4()));
        std::fs::File::create(&path).unwrap();
        let mut cfg = super::super::ConnectionConfig::new(1, super::super::Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let mut session = super::super::Session::connect(&cfg).await.unwrap();
        session.execute("CREATE TABLE far(id INTEGER PRIMARY KEY); CREATE TABLE c(id INTEGER PRIMARY KEY, next INTEGER REFERENCES far); CREATE TABLE b(id INTEGER PRIMARY KEY, next INTEGER REFERENCES c); CREATE TABLE \"chosen'--\"(id INTEGER PRIMARY KEY, b INTEGER REFERENCES b, c INTEGER REFERENCES c, generated TEXT GENERATED ALWAYS AS ('visible') VIRTUAL);", 0).await.unwrap();
        let model = load(&mut session, super::super::Driver::Sqlite, "main", &[("main".into(), "chosen'--".into())]).await.unwrap();
        assert_eq!(model.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["b", "c", "chosen'--"]);
        assert_eq!(model.relations.len(), 3);
        assert!(model.relations.iter().any(|r| r.table == "b" && r.ref_table == "c"));
        assert!(model.tables.last().unwrap().columns.iter().any(|c| c.name == "generated"));
        assert!(load(&mut session, super::super::Driver::Sqlite, "main", &[("main".into(), "missing".into())]).await.is_err());
        drop(session);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_small_model_does_not_read_unrelated_column_catalogs() {
        let path = std::env::temp_dir().join(format!("datacraft-model-large-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE chosen(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES parent); CREATE TABLE parent(id INTEGER PRIMARY KEY);")
            .unwrap();
        let columns = (0..1600).map(|i| format!("c{i} INTEGER")).collect::<Vec<_>>().join(",");
        for index in 0..64 {
            db.execute_batch(&format!("CREATE TABLE unrelated_{index}({columns},FOREIGN KEY(c0) REFERENCES unrelated_0(c0))")).unwrap();
        }
        drop(db);
        let mut cfg = super::super::ConnectionConfig::new(1, super::super::Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let mut session = super::super::Session::connect(&cfg).await.unwrap();
        let selected = [("main".into(), "chosen".into())].into_iter().collect();
        let edges = session.execute(&sqlite_fk_sql(&selected, true), 100_000).await.unwrap();
        assert_eq!(edges.iter().map(|rs| rs.rows.len()).sum::<usize>(), 1);
        let result = load(&mut session, super::super::Driver::Sqlite, "main", &[("main".into(), "chosen".into())]).await;
        drop(session);
        std::fs::remove_file(path).unwrap();
        let model = result.unwrap();
        assert_eq!(model.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["chosen", "parent"]);
        assert_eq!(model.relations.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_model_keeps_generated_columns_and_resolves_composite_keys_in_declared_order() {
        let path = std::env::temp_dir().join(format!("datacraft-model-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE parent(a INTEGER,b INTEGER,computed TEXT GENERATED ALWAYS AS (a||':'||b) VIRTUAL,PRIMARY KEY(b,a)); CREATE TABLE child(x INTEGER,y INTEGER,FOREIGN KEY(x,y) REFERENCES parent); INSERT INTO parent(a,b) VALUES(1,2); INSERT INTO child VALUES(2,1);").unwrap();
        drop(db);
        let mut cfg = crate::db::ConnectionConfig::new(1, crate::db::Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        cfg.read_only = true;
        let mut session = crate::db::Session::connect(&cfg).await.unwrap();
        let model = load(&mut session, crate::db::Driver::Sqlite, "main", &[("main".into(), "child".into())]).await.unwrap();
        assert_eq!(model.relations[0].ref_columns, ["b", "a"]);
        let parent = model.tables.iter().find(|t| t.name == "parent").unwrap();
        assert!(parent.columns.iter().any(|c| c.name == "computed"));
        drop(session);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn model_preserves_declared_keys_and_includes_referenced_tables_only() {
        let catalog = vec![SchemaInfo {
            name: "s".into(),
            tables: ["orders", "customers", "unrelated"]
                .into_iter()
                .map(|name| TableInfo {
                    name: name.into(),
                    kind: ObjectKind::Table,
                    columns: vec![ColumnInfo { name: "id".into(), data_type: "integer".into(), primary_key: true, nullable: false }],
                })
                .collect(),
        }];
        let key = ForeignKey {
            schema: "s".into(),
            table: "orders".into(),
            columns: vec!["id".into()],
            ref_schema: "s".into(),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
        };
        let m = from_catalog(&catalog, &[("s".into(), "orders".into())], &[key]).unwrap();
        assert_eq!(m.tables.len(), 2);
        assert_eq!(m.relations.len(), 1);
        assert!(m.tables[0].columns[0].primary_key);
        assert!(!m.tables.iter().any(|t| t.name == "unrelated"));
        assert_eq!(m.relations[0].ref_columns, ["id"]);
        assert!(from_catalog(&catalog, &[("s".into(), "missing".into())], &[]).is_err());
        assert!(from_catalog(&catalog, &[], &[]).is_err());
    }
}
