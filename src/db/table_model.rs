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

pub async fn load(session: &mut super::Session, driver: super::Driver, db: &str, selected: &[(String, String)]) -> anyhow::Result<Model> {
    let catalog = if driver == super::Driver::Sqlite {
        let sql = "SELECT m.name,p.name,p.type,p.\"notnull\",p.pk FROM main.sqlite_schema m JOIN pragma_table_xinfo(m.name,'main') p WHERE m.type='table' AND p.hidden<>1 ORDER BY m.name,p.cid";
        let sets = session.execute(sql, 100_000).await?;
        let mut tables: std::collections::BTreeMap<String, Vec<super::ColumnInfo>> = std::collections::BTreeMap::new();
        for rs in sets {
            anyhow::ensure!(!rs.truncated, crate::i18n::tr("The table catalog is too large to model completely"));
            for row in rs.rows {
                let field = |i| row.get(i).and_then(Option::as_deref).ok_or_else(|| anyhow::anyhow!(crate::i18n::tr("Incomplete SQLite column metadata")));
                tables.entry(field(0)?.into()).or_default().push(super::ColumnInfo {
                    name: field(1)?.into(),
                    data_type: field(2)?.into(),
                    nullable: field(3)? == "0",
                    primary_key: field(4)?.parse::<u32>()? > 0,
                });
            }
        }
        vec![SchemaInfo {
            name: "main".into(),
            tables: tables.into_iter().map(|(name, columns)| super::TableInfo { name, kind: super::ObjectKind::Table, columns }).collect(),
        }]
    } else {
        session.introspect_database(db).await?
    };
    let mut keys = Vec::new();
    let schemas: std::collections::BTreeSet<_> = selected.iter().map(|(s, _)| s.as_str()).collect();
    for schema in schemas {
        let sql = if driver == super::Driver::Sqlite {
            // An omitted parent column list follows PRIMARY KEY ordinal, not table-column order.
            "SELECT m.name||':'||p.id,'main',m.name,p.\"from\",'main',p.\"table\",coalesce(nullif(p.\"to\",''),pk.name),p.seq FROM main.sqlite_schema m JOIN pragma_foreign_key_list(m.name,'main') p LEFT JOIN pragma_table_xinfo(p.\"table\",'main') pk ON pk.pk=p.seq+1 WHERE m.type='table'".into()
        } else {
            super::subset::fk_sql(driver, db, schema)
        };
        if sql.is_empty() {
            break;
        }
        let result = session.execute(&sql, 100_000).await?;
        for rs in result {
            anyhow::ensure!(!rs.truncated, crate::i18n::tr("The foreign key catalog is too large to model completely"));
            for key in super::subset::parse_fks(driver, &rs, &catalog) {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        if !matches!(driver, super::Driver::Oracle | super::Driver::Snowflake) {
            break;
        }
    }
    from_catalog(&catalog, selected, &keys)
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
