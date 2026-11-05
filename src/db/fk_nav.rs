//! Foreign-key navigation: the declared keys one table takes part in, `WHERE` text that matches
//! a key's values, and bounded counts of the rows referencing one row.

use super::{Cell, ConnectionConfig, Driver, Session, subset::ForeignKey, table_definition::lit};
use crate::i18n::tr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Counts stop here: the dialog shows "1000+" instead of scanning a huge child table.
pub const COUNT_LIMIT: u64 = 1000;
/// Each count gets this long; a slow one (e.g. an unindexed key column) does not lose the others.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn supported(driver: Driver) -> bool {
    super::table_model::supported(driver)
}

/// Keys where the table is the child (`outgoing`) or the referenced parent (`incoming`); a
/// self-reference is in both.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableKeys {
    pub outgoing: Vec<ForeignKey>,
    pub incoming: Vec<ForeignKey>,
}

impl TableKeys {
    pub fn of(keys: Vec<ForeignKey>, schema: &str, table: &str) -> Self {
        let mut out = Self::default();
        for key in keys {
            if key.schema == schema && key.table == table && !out.outgoing.contains(&key) {
                out.outgoing.push(key.clone());
            }
            if key.ref_schema == schema && key.ref_table == table && !out.incoming.contains(&key) {
                out.incoming.push(key);
            }
        }
        let order = |k: &ForeignKey| (k.schema.clone(), k.table.clone(), k.columns.clone());
        out.incoming.sort_by_key(order);
        out
    }
}

/// A navigation filter keeps its display text separate from the executable placeholders.
/// Editing the WHERE field drops the binding rather than applying stale values to new SQL.
#[derive(Clone)]
pub struct BoundFilter {
    pub display: String,
    pub sql: String,
    pub parameters: Vec<String>,
    /// Only this generated statement receives the values; other tab actions run normally.
    pub query: Option<String>,
}

pub fn bound_filter(driver: Driver, columns: &[String], values: &[Cell]) -> Option<BoundFilter> {
    bound_filter_at(driver, columns, values, 0)
}

/// OR complete tuples, rather than independent IN lists that could match unrelated rows.
/// NULL tuples cannot reference a row; duplicate tuples need only one set of bind values.
pub fn bound_filter_rows(driver: Driver, columns: &[String], rows: &[Vec<Cell>]) -> Option<BoundFilter> {
    let mut seen = std::collections::HashSet::new();
    let mut filters = Vec::new();
    let mut parameters = Vec::new();
    for row in rows {
        if columns.len() != row.len() { return None; }
        if row.iter().any(|v| v.is_none() || crate::edit::is_lossy_display(v)) || !seen.insert(row) { continue; }
        let filter = if parameters.is_empty() { bound_filter(driver, columns, row)? } else { bound_filter_at(driver, columns, row, parameters.len())? };
        parameters.extend(filter.parameters.clone());
        filters.push(filter);
    }
    if filters.len() == 1 { return filters.pop(); }
    if filters.is_empty() { return None; }
    let display = format!("({})", filters.iter().map(|f| f.display.as_str()).collect::<Vec<_>>().join(" OR "));
    // Keep expression depth logarithmic for large selections, including SQLite's limit.
    fn disjunction(filters: &[BoundFilter]) -> String {
        if filters.len() == 1 { return format!("({})", filters[0].sql); }
        let (left, right) = filters.split_at(filters.len() / 2);
        format!("({} OR {})", disjunction(left), disjunction(right))
    }
    let sql = disjunction(&filters);
    Some(BoundFilter { display, sql, parameters, query: None })
}

fn bound_filter_at(driver: Driver, columns: &[String], values: &[Cell], offset: usize) -> Option<BoundFilter> {
    let display = format!("({})", key_filter(driver, columns, values)?);
    let sql = columns.iter().enumerate().map(|(i, column)| {
        let parameter = match driver {
            Driver::Postgres => format!("${}", offset + i + 1),
            Driver::MsSql => format!("@P{}", offset + i + 1),
            Driver::Oracle => format!(":{}", offset + i + 1),
            _ => "?".into(),
        };
        format!("{} = {parameter}", driver.quote_ident(column))
    }).collect::<Vec<_>>().join(" AND ");
    Some(BoundFilter { display, sql, query: None, parameters: values.iter().map(|v| v.clone().unwrap_or_default()).collect() })
}

/// A child table and the filter selecting the rows that reference one parent row.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    pub schema: String,
    pub table: String,
    pub filter: String,
}

/// `"a" = '1' AND "b" = '2'`: the rows whose `columns` hold `values`. `None` when a value is
/// NULL (a key with a NULL column references nothing) or the lists do not pair up.
pub fn key_filter(driver: Driver, columns: &[String], values: &[Cell]) -> Option<String> {
    if columns.is_empty() || columns.len() != values.len() || values.iter().any(|v| v.is_none() || crate::edit::is_lossy_display(v)) {
        return None;
    }
    Some(columns.iter().zip(values).map(|(c, v)| format!("{} = {}", driver.quote_ident(c), crate::edit::literal(driver, v))).collect::<Vec<_>>().join(" AND "))
}

/// SQLite keys of `main.table`. `pragma_foreign_key_list` keeps the names as written after
/// REFERENCES, which SQLite matches case-insensitively: they are resolved to the declared table
/// and column names (an omitted parent column list resolves to the primary key, in order).
fn sqlite_keys_sql(table: &str) -> String {
    let t = lit(Driver::Sqlite, table);
    format!(
        "WITH keys AS (SELECT m.name AS child,p.id,p.seq,p.\"from\" AS child_column,pm.name AS parent,nullif(p.\"to\",'') AS parent_column FROM main.sqlite_schema m JOIN pragma_foreign_key_list(m.name,'main') p JOIN main.sqlite_schema pm ON pm.type='table' AND pm.name=p.\"table\" COLLATE NOCASE WHERE m.type='table' AND (m.name={t} OR pm.name={t})) SELECT child||':'||id,'main',child,coalesce((SELECT name FROM pragma_table_xinfo(child,'main') WHERE name=child_column COLLATE NOCASE),child_column),'main',parent,coalesce((SELECT name FROM pragma_table_xinfo(parent,'main') WHERE name=parent_column COLLATE NOCASE),(SELECT name FROM pragma_table_xinfo(parent,'main') WHERE pk=seq+1 AND parent_column IS NULL)),seq FROM keys"
    )
}

/// Catalog queries for the keys of `schema.table` only (the inventory readers of `subset`,
/// narrowed to this table on the server).
fn keys_sql(driver: Driver, db: &str, schema: &str, table: &str) -> Vec<String> {
    let (s, t) = (lit(driver, schema), lit(driver, table));
    let base = super::subset::fk_sql(driver, db, schema);
    match driver {
        // PostgreSQL 11+ clones a key once per partition (`conparentid` <> 0): only the declared
        // key between the partitioned tables is kept. Older servers have no such column.
        Driver::Postgres => vec![format!(
            "{base} AND coalesce((to_jsonb(con) ->> 'conparentid')::oid, 0) = 0 AND ((cn.nspname = {s} AND cl.relname = {t}) OR (pn.nspname = {s} AND pl.relname = {t}))"
        )],
        Driver::MySql | Driver::MariaDb => vec![format!(
            "{base} AND ((TABLE_SCHEMA = {s} AND TABLE_NAME = {t}) OR (REFERENCED_TABLE_SCHEMA = {s} AND REFERENCED_TABLE_NAME = {t}))"
        )],
        Driver::MsSql => vec![format!("{base} WHERE (sch.name = {s} AND t.name = {t}) OR (rsch.name = {s} AND rt.name = {t})")],
        // The inventory is per owner: references from other owners are not listed.
        Driver::Oracle => vec![format!("{base} AND (c.table_name = {t} OR (r.owner = {s} AND r.table_name = {t}))")],
        Driver::Sqlite if schema == "main" => vec![sqlite_keys_sql(table)],
        Driver::Snowflake => {
            let name = format!("{}.{}", Session::quote_table(driver, db, schema), driver.quote_ident(table));
            vec![format!("SHOW IMPORTED KEYS IN TABLE {name}"), format!("SHOW EXPORTED KEYS IN TABLE {name}")]
        }
        _ => Vec::new(),
    }
}

pub async fn load(session: &mut Session, driver: Driver, db: &str, schema: &str, table: &str) -> anyhow::Result<TableKeys> {
    anyhow::ensure!(supported(driver), tr("Not available for this engine"));
    let mut keys = Vec::new();
    for sql in keys_sql(driver, db, schema, table) {
        for mut rs in session.execute(&sql, 100_000).await? {
            anyhow::ensure!(!rs.truncated, tr("The foreign key catalog is too large to model completely"));
            if driver == Driver::Snowflake {
                same_database(&mut rs, db);
            }
            keys.extend(super::subset::parse_fks(driver, &rs, &[]));
        }
    }
    Ok(TableKeys::of(keys, schema, table))
}

/// Snowflake: keys from or to other databases would open the wrong table, so they are left out.
fn same_database(rs: &mut super::ResultSet, db: &str) {
    let at: Vec<usize> = ["fk_database_name", "pk_database_name"].iter().filter_map(|n| rs.columns.iter().position(|c| c.eq_ignore_ascii_case(n))).collect();
    rs.rows.retain(|row| at.iter().all(|&i| row.get(i).and_then(Option::as_deref) == Some(db)));
}

/// At most `COUNT_LIMIT + 1` matching rows are counted.
pub fn count_sql(driver: Driver, probe: &Probe) -> String {
    let (t, n) = (Session::quote_table(driver, &probe.schema, &probe.table), COUNT_LIMIT + 1);
    match driver {
        Driver::MsSql => format!("SELECT COUNT(*) FROM (SELECT TOP ({n}) 1 AS x FROM {t} WHERE {}) q", probe.filter),
        Driver::Oracle => format!("SELECT COUNT(*) FROM {t} WHERE ({}) AND ROWNUM <= {n}", probe.filter),
        _ => format!("SELECT COUNT(*) FROM (SELECT 1 AS x FROM {t} WHERE {} LIMIT {n}) q", probe.filter),
    }
}

/// Counts one probe at a time, each within `limit`: a probe that times out is interrupted and
/// reported, and the next one runs (unless the interrupt did not stop it).
async fn counts(session: &mut Session, driver: Driver, probes: &[Probe], cancel: &AtomicBool, limit: Duration) -> anyhow::Result<Vec<Result<u64, String>>> {
    let handle = session.cancel_handle();
    let mut out = Vec::with_capacity(probes.len());
    for probe in probes {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), tr("Cancelled"));
        let sql = count_sql(driver, probe);
        let query = session.execute(&sql, 1);
        tokio::pin!(query);
        let count = tokio::select! {
            result = &mut query => match result {
                Ok(sets) => sets.iter().find_map(|rs| rs.rows.first()?.first().cloned().flatten()?.trim().parse::<u64>().ok()).ok_or_else(|| tr("Exact row count unavailable").to_string()),
                Err(e) => Err(format!("{e:#}")),
            },
            _ = tokio::time::sleep(limit) => {
                if let Some(handle) = &handle {
                    let _ = tokio::time::timeout(Duration::from_secs(3), handle.cancel()).await;
                }
                let stopped = tokio::time::timeout(Duration::from_secs(3), &mut query).await.is_ok();
                out.push(Err(tr("Counting timed out").to_string()));
                if !stopped {
                    break;
                }
                continue;
            }
        };
        out.push(count);
    }
    out.resize_with(probes.len(), || Err(tr("Counting timed out").to_string()));
    Ok(out)
}

/// Runs `work` on a session of its own: `cancel` interrupts the native query before the session
/// is dropped.
async fn on_session<T>(mut cfg: ConnectionConfig, db: &str, cancel: &AtomicBool, work: impl AsyncFnOnce(&mut Session) -> anyhow::Result<T>) -> anyhow::Result<T> {
    anyhow::ensure!(supported(cfg.driver), tr("Not available for this engine"));
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), tr("Cancelled"));
    if !matches!(cfg.driver, Driver::Sqlite | Driver::Oracle) {
        cfg.database = db.into();
    }
    let mut session = tokio::time::timeout(Duration::from_secs(30), Session::connect(&cfg)).await??;
    tokio::time::timeout(Duration::from_secs(30), session.use_database(db)).await??;
    let handle = session.cancel_handle();
    let query = work(&mut session);
    tokio::pin!(query);
    let cancelled = async {
        while !cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let (result, abandoned) = tokio::select! {
        result = &mut query => (result, false),
        _ = cancelled => (Err(anyhow::anyhow!(tr("Cancelled"))), true),
    };
    if abandoned && let Some(handle) = handle {
        let _ = tokio::time::timeout(Duration::from_secs(3), handle.cancel()).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut query).await;
    }
    result
}

/// One count per probe, in order, each within `limit`; a failed count does not stop the others.
pub async fn count(cfg: ConnectionConfig, db: &str, probes: &[Probe], cancel: &AtomicBool, limit: Duration) -> anyhow::Result<Vec<Result<u64, String>>> {
    let driver = cfg.driver;
    on_session(cfg, db, cancel, async |s: &mut Session| counts(s, driver, probes, cancel, limit).await).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(schema: &str, table: &str, columns: &[&str], ref_table: &str, ref_columns: &[&str]) -> ForeignKey {
        let v = |c: &[&str]| c.iter().map(|s| s.to_string()).collect();
        ForeignKey { schema: schema.into(), table: table.into(), columns: v(columns), ref_schema: schema.into(), ref_table: ref_table.into(), ref_columns: v(ref_columns) }
    }

    #[test]
    fn composite_filters_quote_names_and_values_per_dialect() {
        let cols = ["order id".to_string(), "line\"no".to_string()];
        let vals = [Some("7".to_string()), Some("O'Brien".to_string())];
        assert_eq!(key_filter(Driver::Postgres, &cols, &vals).unwrap(), "\"order id\" = '7' AND \"line\"\"no\" = 'O''Brien'");
        assert_eq!(key_filter(Driver::MsSql, &cols, &vals).unwrap(), "[order id] = N'7' AND [line\"no] = N'O''Brien'");
        assert_eq!(key_filter(Driver::MySql, &cols[..1], &[Some("a\\'b".into())]).unwrap(), "`order id` = 'a\\\\''b'");
        assert_eq!(key_filter(Driver::Oracle, &cols[..1], &vals[..1]).unwrap(), "\"order id\" = '7'");
    }

    #[test]
    fn navigation_filters_keep_values_separate_from_dialect_placeholders() {
        let columns = ["order id".to_string(), "region".to_string()];
        let values = [Some("O'Brien'); DROP TABLE orders; --".into()), Some("north".into())];
        for (driver, placeholders) in [
            (Driver::Postgres, ["$1", "$2"]),
            (Driver::MsSql, ["@P1", "@P2"]),
            (Driver::Oracle, [":1", ":2"]),
            (Driver::MySql, ["?", "?"]),
            (Driver::MariaDb, ["?", "?"]),
            (Driver::Sqlite, ["?", "?"]),
            (Driver::Snowflake, ["?", "?"]),
        ] {
            let filter = bound_filter(driver, &columns, &values).unwrap();
            assert_eq!(filter.sql, format!("{} = {} AND {} = {}", driver.quote_ident(&columns[0]), placeholders[0], driver.quote_ident(&columns[1]), placeholders[1]));
            assert_eq!(filter.parameters, [values[0].clone().unwrap(), "north".into()]);
            assert!(!filter.sql.contains("DROP TABLE"));
        }
    }

    #[test]
    fn multi_row_filters_number_parameters_globally_and_preserve_pairs() {
        let columns = ["id".into(), "shop".into()];
        let rows = [vec![Some("1".into()), Some("north".into())], vec![Some("2".into()), Some("south".into())], vec![Some("1".into()), Some("north".into())], vec![None, Some("ignored".into())]];
        for (driver, last) in [(Driver::Postgres, "$4"), (Driver::MsSql, "@P4"), (Driver::Oracle, ":4"), (Driver::Sqlite, "?"), (Driver::MySql, "?"), (Driver::MariaDb, "?"), (Driver::Snowflake, "?")] {
            let filter = bound_filter_rows(driver, &columns, &rows).unwrap();
            assert_eq!(filter.parameters, ["1", "north", "2", "south"]);
            assert!(filter.sql.contains(last));
            assert_eq!(filter.sql.matches(" OR ").count(), 1);
            assert_eq!(filter.sql.matches(" AND ").count(), 2);
            assert!(!filter.sql.contains("north"));
        }
        assert!(bound_filter_rows(Driver::Sqlite, &columns, &rows[3..]).is_none());
        assert!(bound_filter_rows(Driver::Sqlite, &columns, &[vec![Some("1".into())]]).is_none());
    }

    #[test]
    fn null_or_unpaired_keys_are_not_links() {
        let cols = ["a".to_string(), "b".to_string()];
        assert!(key_filter(Driver::Sqlite, &cols, &[Some("1".into()), None]).is_none());
        assert!(key_filter(Driver::Sqlite, &cols, &[Some("1".into())]).is_none());
        assert!(key_filter(Driver::Sqlite, &[], &[]).is_none());
        assert!(key_filter(Driver::Sqlite, &cols[..1], &[Some("0x0102…".into())]).is_none());
    }

    #[test]
    fn keys_split_by_direction_and_keep_self_references_in_both() {
        let keys = vec![
            key("s", "orders", &["customer_id"], "customers", &["id"]),
            key("s", "orders", &["parent_id"], "orders", &["id"]),
            key("s", "lines", &["order_id"], "orders", &["id"]),
            key("s", "lines", &["order_id"], "orders", &["id"]),
            key("s", "other", &["x"], "customers", &["id"]),
        ];
        let k = TableKeys::of(keys, "s", "orders");
        assert_eq!(k.outgoing.iter().map(|k| k.ref_table.as_str()).collect::<Vec<_>>(), ["customers", "orders"]);
        assert_eq!(k.incoming.iter().map(|k| k.table.as_str()).collect::<Vec<_>>(), ["lines", "orders"]);
    }

    #[test]
    fn catalog_queries_are_narrowed_with_literals_and_counts_are_bounded() {
        for driver in [Driver::Postgres, Driver::MySql, Driver::MariaDb, Driver::MsSql, Driver::Oracle, Driver::Snowflake] {
            let sql = keys_sql(driver, "db", "s'x", "t'; DROP TABLE x; --").join(";");
            assert!(sql.contains("t''; DROP TABLE x; --") || driver == Driver::Snowflake, "{driver:?}: {sql}");
        }
        assert!(keys_sql(Driver::ClickHouse, "db", "s", "t").is_empty() && keys_sql(Driver::MongoDb, "db", "s", "t").is_empty());
        // Partition clones are skipped; SQLite reads the main schema only.
        assert!(keys_sql(Driver::Postgres, "db", "s", "t")[0].contains("'conparentid')::oid, 0) = 0"));
        assert!(keys_sql(Driver::Sqlite, "main", "aux", "t").is_empty() && keys_sql(Driver::Sqlite, "main", "main", "t'x")[0].contains("'t''x'"));
        let mut rs = super::super::ResultSet { columns: ["FK_DATABASE_NAME", "PK_DATABASE_NAME", "FK_NAME"].map(String::from).to_vec(), ..Default::default() };
        rs.rows = [["SHOP", "SHOP", "a"], ["OTHER", "SHOP", "b"], ["SHOP", "OTHER", "c"]].iter().map(|r| r.iter().map(|v| Some(v.to_string())).collect()).collect();
        same_database(&mut rs, "SHOP");
        assert_eq!(rs.rows.iter().map(|r| r[2].clone().unwrap()).collect::<Vec<_>>(), ["a"]);
        let probe = Probe { schema: "s".into(), table: "t".into(), filter: "\"a\" = '1'".into() };
        assert_eq!(count_sql(Driver::Postgres, &probe), "SELECT COUNT(*) FROM (SELECT 1 AS x FROM \"s\".\"t\" WHERE \"a\" = '1' LIMIT 1001) q");
        assert!(count_sql(Driver::MsSql, &probe).contains("TOP (1001)"));
        assert!(count_sql(Driver::Oracle, &probe).ends_with("ROWNUM <= 1001"));
    }

    async fn sqlite_keys(path: &std::path::Path, table: &str) -> TableKeys {
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let mut session = Session::connect(&cfg).await.unwrap();
        load(&mut session, Driver::Sqlite, "main", "main", table).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sqlite_keys_resolve_names_and_slow_counts_do_not_lose_the_others() {
        let path = std::env::temp_dir().join(format!("datacraft-fk-nav-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE parent(a INTEGER,b TEXT,PRIMARY KEY(b,a)); CREATE TABLE child(id INTEGER PRIMARY KEY,x INTEGER,y TEXT,FOREIGN KEY(X,Y) REFERENCES Parent(A,B));
             CREATE TABLE implicit(id INTEGER PRIMARY KEY,pb TEXT,pa INTEGER,FOREIGN KEY(pb,pa) REFERENCES PARENT);
             INSERT INTO parent VALUES(1,'O''x'),(2,'b'); INSERT INTO child(x,y) VALUES(1,'O''x'),(1,'O''x'),(2,'b'),(NULL,'b');",
        )
        .unwrap();
        drop(db);
        let keys = sqlite_keys(&path, "parent").await;
        assert!(keys.outgoing.is_empty());
        assert_eq!(keys.incoming.len(), 2);
        // `REFERENCES Parent(A,B)` and `FOREIGN KEY(X,Y)` resolve to the declared names.
        assert_eq!((keys.incoming[0].columns.clone(), keys.incoming[0].ref_table.as_str(), keys.incoming[0].ref_columns.clone()), (vec!["x".to_string(), "y".to_string()], "parent", vec!["a".to_string(), "b".to_string()]));
        assert_eq!(keys.incoming[1].ref_columns, ["b", "a"]);
        assert_eq!(sqlite_keys(&path, "child").await.outgoing, keys.incoming[..1]);
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let filter = key_filter(Driver::Sqlite, &keys.incoming[0].columns, &[Some("1".into()), Some("O'x".into())]).unwrap();
        let endless = "(WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT max(n) FROM r) > 0";
        let probe = |filter: &str| Probe { schema: "main".into(), table: "child".into(), filter: filter.into() };
        let probes = [probe(endless), probe(&filter), probe("missing = 1")];
        let started = std::time::Instant::now();
        let counted = count(cfg.clone(), "main", &probes, &AtomicBool::new(false), Duration::from_millis(300)).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(counted[0], Err(tr("Counting timed out").to_string()));
        assert_eq!(counted[1], Ok(2));
        assert!(counted[2].is_err());
        assert!(count(cfg, "main", &probes, &AtomicBool::new(true), PROBE_TIMEOUT).await.is_err());
        std::fs::remove_file(path).unwrap();
    }

    /// Live PostgreSQL: `cargo test -- --ignored`, with DATACRAFT_PG_HOST (required), DATACRAFT_PG_PORT,
    /// DATACRAFT_PG_USER, DATACRAFT_PG_PASSWORD and DATACRAFT_PG_DB as in `postgres::tests`.
    #[tokio::test]
    #[ignore]
    async fn postgres_keys_filters_and_counts_cross_schemas() {
        let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else {
            eprintln!("DATACRAFT_PG_HOST not set: skipping the PostgreSQL test");
            return;
        };
        let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.host = host;
        cfg.port = env("DATACRAFT_PG_PORT", "5432").parse().unwrap();
        cfg.user = env("DATACRAFT_PG_USER", "postgres");
        cfg.password = env("DATACRAFT_PG_PASSWORD", "");
        cfg.database = env("DATACRAFT_PG_DB", "postgres");
        let db = cfg.database.clone();
        let mut session = Session::connect(&cfg).await.unwrap();
        session
            .execute(
                "DROP SCHEMA IF EXISTS \"dc fk\" CASCADE; DROP SCHEMA IF EXISTS dc_fk_child CASCADE; CREATE SCHEMA \"dc fk\"; CREATE SCHEMA dc_fk_child;
                 CREATE TABLE \"dc fk\".\"Shops\"(code text, region text, name text, PRIMARY KEY(code, region));
                 CREATE TABLE dc_fk_child.orders(id int PRIMARY KEY, shop text, region text, FOREIGN KEY(shop, region) REFERENCES \"dc fk\".\"Shops\"(code, region));
                 CREATE TABLE \"dc fk\".unrelated(id int PRIMARY KEY);
                 INSERT INTO \"dc fk\".\"Shops\" VALUES('N''1', 'north', 'O''Brien'), ('N''1', 'south', 'x');
                 INSERT INTO dc_fk_child.orders VALUES(1, 'N''1', 'north'), (2, 'N''1', 'north'), (3, NULL, 'north');
                 CREATE TABLE \"dc fk\".customers(id int, region text, PRIMARY KEY(id, region)) PARTITION BY LIST(region);
                 CREATE TABLE \"dc fk\".customers_p1 PARTITION OF \"dc fk\".customers FOR VALUES IN ('north');
                 CREATE TABLE \"dc fk\".customers_p2 PARTITION OF \"dc fk\".customers FOR VALUES IN ('south');
                 CREATE TABLE dc_fk_child.invoices(id int, region text, customer_id int, PRIMARY KEY(id, region), FOREIGN KEY(customer_id, region) REFERENCES \"dc fk\".customers) PARTITION BY LIST(region);
                 CREATE TABLE dc_fk_child.invoices_p1 PARTITION OF dc_fk_child.invoices FOR VALUES IN ('north');
                 CREATE TABLE dc_fk_child.invoices_p2 PARTITION OF dc_fk_child.invoices FOR VALUES IN ('south');
                 INSERT INTO \"dc fk\".customers VALUES(7, 'north'); INSERT INTO dc_fk_child.invoices VALUES(1, 'north', 7), (2, 'north', 7);",
                0,
            )
            .await
            .unwrap();
        let parent = load(&mut session, Driver::Postgres, &db, "dc fk", "Shops").await.unwrap();
        assert!(parent.outgoing.is_empty());
        assert_eq!(parent.incoming.len(), 1);
        assert_eq!((parent.incoming[0].schema.as_str(), parent.incoming[0].columns.clone()), ("dc_fk_child", vec!["shop".to_string(), "region".to_string()]));
        let child = load(&mut session, Driver::Postgres, &db, "dc_fk_child", "orders").await.unwrap();
        assert_eq!(child.outgoing[0].ref_columns, ["code", "region"]);
        let key = &child.outgoing[0];
        let filter = key_filter(Driver::Postgres, &key.ref_columns, &[Some("N'1".into()), Some("north".into())]).unwrap();
        let rows = session.execute(&format!("SELECT name FROM \"dc fk\".\"Shops\" WHERE {filter}"), 10).await.unwrap();
        assert_eq!(rows[0].rows, [vec![Some("O'Brien".to_string())]]);
        let probe = Probe { schema: key.schema.clone(), table: key.table.clone(), filter: key_filter(Driver::Postgres, &key.columns, &[Some("N'1".into()), Some("north".into())]).unwrap() };
        assert_eq!(count(cfg.clone(), &db, &[probe], &AtomicBool::new(false), PROBE_TIMEOUT).await.unwrap(), [Ok(2)]);
        // Partitioned tables: one key between the partitioned tables, none per partition.
        let customers = load(&mut session, Driver::Postgres, &db, "dc fk", "customers").await.unwrap();
        assert_eq!(customers.incoming.iter().map(|k| k.table.as_str()).collect::<Vec<_>>(), ["invoices"]);
        let invoices = load(&mut session, Driver::Postgres, &db, "dc_fk_child", "invoices").await.unwrap();
        assert_eq!(invoices.outgoing.iter().map(|k| k.ref_table.as_str()).collect::<Vec<_>>(), ["customers"]);
        let probe = Probe { schema: "dc_fk_child".into(), table: "invoices".into(), filter: key_filter(Driver::Postgres, &invoices.outgoing[0].columns, &[Some("7".into()), Some("north".into())]).unwrap() };
        assert_eq!(count(cfg.clone(), &db, &[probe], &AtomicBool::new(false), PROBE_TIMEOUT).await.unwrap(), [Ok(2)]);
        // A slow count is interrupted on the server and the next one still runs.
        let slow = Probe { schema: "dc fk".into(), table: "Shops".into(), filter: "pg_sleep(30) IS NOT NULL".into() };
        let quick = Probe { schema: "dc fk".into(), table: "Shops".into(), filter: "code = 'N''1'".into() };
        let counted = count(cfg.clone(), &db, &[slow, quick], &AtomicBool::new(false), Duration::from_millis(500)).await.unwrap();
        assert_eq!(counted, [Err(tr("Counting timed out").to_string()), Ok(2)]);
        session.execute("DROP SCHEMA \"dc fk\" CASCADE; DROP SCHEMA dc_fk_child CASCADE;", 0).await.unwrap();
    }
}
