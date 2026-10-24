//! Subset that keeps foreign keys valid, for "Copy Table To…": starting from the copied rows,
//! the rows that reference them (their orders, and the orders' lines…) and the rows they
//! reference (each order's customer and products…), read from the source by key.

use super::{Cell, ColumnInfo, Driver, ResultSet, SchemaInfo, Session};
use crate::edit::literal;
use crate::i18n::{tr, trf};
use std::collections::{HashSet, VecDeque};

/// Keys per `IN (…)` / `OR` list.
const KEYS_PER_QUERY: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

impl ForeignKey {
    fn child(&self) -> (&str, &str) {
        (&self.schema, &self.table)
    }

    fn parent(&self) -> (&str, &str) {
        (&self.ref_schema, &self.ref_table)
    }
}

fn quote_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Query that lists the foreign keys of `db` (`schema`: the one of the copied table, for the
/// engines whose catalogs are per schema). One row per column: name, schema, table, column,
/// referenced schema, table and column, position.
pub fn fk_sql(driver: Driver, db: &str, schema: &str) -> String {
    match driver {
        Driver::Postgres => "SELECT con.conname, cn.nspname, cl.relname, a.attname, pn.nspname, pl.relname, af.attname, k.i \
             FROM pg_constraint con \
             JOIN pg_class cl ON cl.oid = con.conrelid JOIN pg_namespace cn ON cn.oid = cl.relnamespace \
             JOIN pg_class pl ON pl.oid = con.confrelid JOIN pg_namespace pn ON pn.oid = pl.relnamespace \
             CROSS JOIN LATERAL generate_subscripts(con.conkey, 1) AS k(i) \
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[k.i] \
             JOIN pg_attribute af ON af.attrelid = con.confrelid AND af.attnum = con.confkey[k.i] \
             WHERE con.contype = 'f' AND cn.nspname NOT IN ('pg_catalog', 'information_schema')"
            .into(),
        Driver::MySql | Driver::MariaDb => format!(
            "SELECT CONSTRAINT_NAME, TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_SCHEMA, \
             REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME, ORDINAL_POSITION \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE REFERENCED_TABLE_NAME IS NOT NULL AND TABLE_SCHEMA = '{}'",
            quote_lit(db)
        ),
        Driver::MsSql => "SELECT fk.name, sch.name, t.name, c.name, rsch.name, rt.name, rc.name, fkc.constraint_column_id \
             FROM sys.foreign_key_columns fkc \
             JOIN sys.foreign_keys fk ON fk.object_id = fkc.constraint_object_id \
             JOIN sys.tables t ON t.object_id = fkc.parent_object_id JOIN sys.schemas sch ON sch.schema_id = t.schema_id \
             JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id \
             JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id JOIN sys.schemas rsch ON rsch.schema_id = rt.schema_id \
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id"
            .into(),
        Driver::Oracle => format!(
            "SELECT c.constraint_name, c.owner, c.table_name, cc.column_name, r.owner, r.table_name, rc.column_name, cc.position \
             FROM all_constraints c \
             JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
             JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
             JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
             WHERE c.constraint_type = 'R' AND c.owner = '{}'",
            quote_lit(schema)
        ),
        Driver::Sqlite => "SELECT m.name || ':' || p.id, 'main', m.name, p.\"from\", 'main', p.\"table\", p.\"to\", p.seq \
             FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) p WHERE m.type = 'table'"
            .into(),
        Driver::Snowflake => format!("SHOW IMPORTED KEYS IN SCHEMA {}", Session::quote_table(driver, db, schema)),
        // No foreign keys (copies are not offered for them).
        Driver::Redis | Driver::MongoDb => String::new(),
        // ClickHouse declares FOREIGN KEY constraints but never enforces them: subsets
        // have no graph to walk.
        Driver::ClickHouse => String::new(),
    }
}

/// One foreign key while it is read: (schema, table, name), its (position, column, referenced
/// column) pairs and the referenced (schema, table).
type FkGroup = ((String, String, String), Vec<(i64, String, String)>, (String, String));

/// Foreign keys from the rows of `fk_sql`. SQLite leaves the referenced column empty when the key
/// points at the parent's primary key: it is taken from `catalog`.
pub fn parse_fks(driver: Driver, rs: &ResultSet, catalog: &[SchemaInfo]) -> Vec<ForeignKey> {
    // Snowflake's SHOW names its columns; the other queries give them in order.
    let idx: Vec<usize> = if driver == Driver::Snowflake {
        let pos = |name: &str| rs.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
        let names = ["fk_name", "fk_schema_name", "fk_table_name", "fk_column_name", "pk_schema_name", "pk_table_name", "pk_column_name", "key_sequence"];
        match names.iter().map(|n| pos(n)).collect::<Option<Vec<_>>>() {
            Some(v) => v,
            None => return Vec::new(),
        }
    } else {
        (0..8).collect()
    };
    let get = |row: &Vec<Cell>, i: usize| row.get(idx[i]).cloned().flatten().unwrap_or_default();
    let mut groups: Vec<FkGroup> = Vec::new();
    for row in &rs.rows {
        let key = (get(row, 1), get(row, 2), get(row, 0));
        let seq = get(row, 7).trim().parse().unwrap_or(0);
        let pair = (seq, get(row, 3), get(row, 6));
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.1.push(pair),
            None => groups.push((key, vec![pair], (get(row, 4), get(row, 5)))),
        }
    }
    groups
        .into_iter()
        .filter_map(|((schema, table, _), mut cols, (ref_schema, ref_table))| {
            cols.sort_by_key(|c| c.0);
            let columns: Vec<String> = cols.iter().map(|c| c.1.clone()).collect();
            let mut ref_columns: Vec<String> = cols.iter().map(|c| c.2.clone()).collect();
            if ref_columns.iter().any(String::is_empty) {
                ref_columns = table_info(catalog, &ref_schema, &ref_table)?
                    .iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect();
            }
            (ref_columns.len() == columns.len() && !columns.is_empty()).then_some(ForeignKey {
                schema,
                table,
                columns,
                ref_schema,
                ref_table,
                ref_columns,
            })
        })
        .collect()
}

pub fn table_info<'a>(catalog: &'a [SchemaInfo], schema: &str, table: &str) -> Option<&'a [ColumnInfo]> {
    catalog
        .iter()
        .find(|s| s.name == schema)?
        .tables
        .iter()
        .find(|t| t.name == table)
        .map(|t| t.columns.as_slice())
}

/// Rows of one table in the subset.
#[derive(Debug, Clone)]
pub struct TableRows {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Cell>>,
}

impl TableRows {
    fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Identity of a row: its primary key, or the whole row without one.
    fn identity(&self, row: &[Cell]) -> Vec<Cell> {
        let pk: Vec<usize> = self.columns.iter().enumerate().filter(|(_, c)| c.primary_key).map(|(i, _)| i).collect();
        if pk.is_empty() { row.to_vec() } else { pk.iter().map(|&i| row[i].clone()).collect() }
    }

    /// Values of `columns` in `row`; None if any is NULL (it references nothing).
    fn key(&self, row: &[Cell], columns: &[usize]) -> Option<Vec<String>> {
        columns.iter().map(|&i| row.get(i).cloned().flatten()).collect()
    }
}

/// WHERE that selects the rows whose `columns` match one of `keys`.
pub fn key_filter(driver: Driver, columns: &[String], keys: &[Vec<String>]) -> String {
    if let [col] = columns {
        let vals: Vec<String> = keys.iter().map(|k| literal(driver, &Some(k[0].clone()))).collect();
        return format!("{} IN ({})", driver.quote_ident(col), vals.join(", "));
    }
    keys.iter()
        .map(|k| {
            let parts: Vec<String> = columns
                .iter()
                .zip(k)
                .map(|(c, v)| format!("{} = {}", driver.quote_ident(c), literal(driver, &Some(v.clone()))))
                .collect();
            format!("({})", parts.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// The columns copied from a table: every non-binary one.
pub fn copied_columns(driver: Driver, columns: &[ColumnInfo]) -> Vec<ColumnInfo> {
    columns.iter().filter(|c| !super::copy::is_binary(driver, &c.data_type)).cloned().collect()
}

struct Collector<'a> {
    driver: Driver,
    catalog: &'a [SchemaInfo],
    tables: Vec<TableRows>,
    seen: Vec<HashSet<Vec<Cell>>>,
    total: usize,
}

impl Collector<'_> {
    fn index(&mut self, schema: &str, table: &str) -> anyhow::Result<usize> {
        if let Some(i) = self.tables.iter().position(|t| t.schema == schema && t.table == table) {
            return Ok(i);
        }
        let columns = table_info(self.catalog, schema, table)
            .ok_or_else(|| anyhow::anyhow!(trf("Table {table} not found", &[("table", &format!("{schema}.{table}"))])))?;
        self.tables.push(TableRows {
            schema: schema.into(),
            table: table.into(),
            columns: copied_columns(self.driver, columns),
            rows: Vec::new(),
        });
        self.seen.push(HashSet::new());
        Ok(self.tables.len() - 1)
    }

    /// Adds the rows not in the subset yet; returns them.
    fn add(&mut self, t: usize, rows: Vec<Vec<Cell>>) -> anyhow::Result<Vec<Vec<Cell>>> {
        let mut new = Vec::new();
        for row in rows {
            if self.seen[t].insert(self.tables[t].identity(&row)) {
                self.tables[t].rows.push(row.clone());
                new.push(row);
            }
        }
        self.total += new.len();
        if self.total > super::copy::MAX_ROWS {
            anyhow::bail!(tr("The table has more than 1,000,000 rows: copy fewer with a filter or a percentage"));
        }
        Ok(new)
    }

    /// Rows of table `t` whose `columns` match `keys`, read in batches.
    async fn fetch(&mut self, s: &mut Session, t: usize, columns: &[String], keys: Vec<Vec<String>>) -> anyhow::Result<Vec<Vec<Cell>>> {
        let mut out = Vec::new();
        for chunk in keys.chunks(KEYS_PER_QUERY) {
            let tbl = &self.tables[t];
            let filter = key_filter(self.driver, columns, chunk);
            let sql = super::copy::select_sql(self.driver, &tbl.schema, &tbl.table, &tbl.columns, &filter, None);
            let rs = s.execute(&sql, super::copy::MAX_ROWS).await?.into_iter().next().unwrap_or_default();
            out.extend(rs.rows);
        }
        Ok(out)
    }
}

/// Distinct non-NULL values of `columns` in `rows`.
fn keys_of(t: &TableRows, rows: &[Vec<Cell>], columns: &[String]) -> Vec<Vec<String>> {
    let Some(idx) = columns.iter().map(|c| t.col(c)).collect::<Option<Vec<_>>>() else { return Vec::new() };
    let mut seen = HashSet::new();
    rows.iter().filter_map(|r| t.key(r, &idx)).filter(|k| seen.insert(k.clone())).collect()
}

/// Columns of the root table that a foreign key needs, missing from `columns`.
pub fn missing_key_columns(fks: &[ForeignKey], schema: &str, table: &str, columns: &[ColumnInfo]) -> Vec<String> {
    let mut missing = Vec::new();
    for fk in fks {
        let cols = if fk.child() == (schema, table) {
            &fk.columns
        } else if fk.parent() == (schema, table) {
            &fk.ref_columns
        } else {
            continue;
        };
        for c in cols {
            if !columns.iter().any(|x| &x.name == c) && !missing.contains(c) {
                missing.push(c.clone());
            }
        }
    }
    missing
}

/// The subset: the root rows, then (with `children`) the rows that reference them, recursively,
/// and finally every row those reference, so the foreign keys hold. The root comes first.
pub async fn collect(
    s: &mut Session,
    driver: Driver,
    catalog: &[SchemaInfo],
    fks: &[ForeignKey],
    root: TableRows,
    children: bool,
) -> anyhow::Result<Vec<TableRows>> {
    let mut c = Collector { driver, catalog, tables: Vec::new(), seen: Vec::new(), total: 0 };
    let root_rows = root.rows.clone();
    c.tables.push(TableRows { rows: Vec::new(), ..root });
    c.seen.push(HashSet::new());
    let first = c.add(0, root_rows)?;

    // Down: rows that reference the new rows (only from rows reached going down).
    let mut queue: VecDeque<(usize, Vec<Vec<Cell>>)> = VecDeque::from([(0, first)]);
    let mut pending_up: Vec<(usize, Vec<Vec<Cell>>)> = Vec::new();
    while let Some((t, rows)) = queue.pop_front() {
        pending_up.push((t, rows.clone()));
        if !children || rows.is_empty() {
            continue;
        }
        let parent = (c.tables[t].schema.clone(), c.tables[t].table.clone());
        for fk in fks.iter().filter(|fk| fk.parent() == (parent.0.as_str(), parent.1.as_str())) {
            let keys = keys_of(&c.tables[t], &rows, &fk.ref_columns);
            if keys.is_empty() {
                continue;
            }
            let child = c.index(&fk.schema, &fk.table)?;
            let found = c.fetch(s, child, &fk.columns, keys).await?;
            let new = c.add(child, found)?;
            if !new.is_empty() {
                queue.push_back((child, new));
            }
        }
    }

    // Up: every row a copied row references, until nothing is missing.
    while let Some((t, rows)) = pending_up.pop() {
        let child = (c.tables[t].schema.clone(), c.tables[t].table.clone());
        for fk in fks.iter().filter(|fk| fk.child() == (child.0.as_str(), child.1.as_str())) {
            let keys = keys_of(&c.tables[t], &rows, &fk.columns);
            if keys.is_empty() {
                continue;
            }
            let parent = c.index(&fk.ref_schema, &fk.ref_table)?;
            let have: HashSet<Vec<String>> = keys_of(&c.tables[parent], &c.tables[parent].rows, &fk.ref_columns).into_iter().collect();
            let wanted: Vec<Vec<String>> = keys.into_iter().filter(|k| !have.contains(k)).collect();
            if wanted.is_empty() {
                continue;
            }
            let found = c.fetch(s, parent, &fk.ref_columns, wanted).await?;
            let new = c.add(parent, found)?;
            if !new.is_empty() {
                pending_up.push((parent, new));
            }
        }
    }
    Ok(c.tables)
}

/// Insert order: each table after the ones it references (cycles keep their current order),
/// and inside a self-referencing table, each row after the row it points at.
pub fn insert_order(tables: &mut Vec<TableRows>, fks: &[ForeignKey]) {
    let n = tables.len();
    let deps: Vec<HashSet<usize>> = (0..n)
        .map(|i| {
            fks.iter()
                .filter(|fk| fk.child() == (tables[i].schema.as_str(), tables[i].table.as_str()))
                .filter_map(|fk| tables.iter().position(|t| (t.schema.as_str(), t.table.as_str()) == fk.parent()))
                .filter(|&p| p != i)
                .collect()
        })
        .collect();
    let mut done = vec![false; n];
    let mut order = Vec::new();
    while order.len() < n {
        let next = (0..n).find(|&i| !done[i] && deps[i].iter().all(|&d| done[d])).or_else(|| (0..n).find(|&i| !done[i]));
        let Some(i) = next else { break };
        done[i] = true;
        order.push(i);
    }
    let mut taken: Vec<Option<TableRows>> = std::mem::take(tables).into_iter().map(Some).collect();
    *tables = order.into_iter().filter_map(|i| taken[i].take()).collect();
    for t in tables.iter_mut() {
        let own: Vec<&ForeignKey> =
            fks.iter().filter(|fk| fk.child() == (t.schema.as_str(), t.table.as_str()) && fk.parent() == fk.child()).collect();
        for fk in own {
            sort_self_referencing(t, fk);
        }
    }
}

fn sort_self_referencing(t: &mut TableRows, fk: &ForeignKey) {
    let (Some(from), Some(to)) = (
        fk.columns.iter().map(|c| t.col(c)).collect::<Option<Vec<_>>>(),
        fk.ref_columns.iter().map(|c| t.col(c)).collect::<Option<Vec<_>>>(),
    ) else {
        return;
    };
    let present: HashSet<Vec<String>> = t.rows.iter().filter_map(|r| t.key(r, &to)).collect();
    let mut placed: HashSet<Vec<String>> = HashSet::new();
    let mut rest: Vec<Vec<Cell>> = std::mem::take(&mut t.rows);
    let mut out = Vec::with_capacity(rest.len());
    loop {
        let before = rest.len();
        let mut keep = Vec::new();
        for row in rest {
            let ready = match t.key(&row, &from) {
                None => true,
                Some(k) => !present.contains(&k) || placed.contains(&k),
            };
            if ready {
                if let Some(k) = t.key(&row, &to) {
                    placed.insert(k);
                }
                out.push(row);
            } else {
                keep.push(row);
            }
        }
        rest = keep;
        if rest.is_empty() || rest.len() == before {
            break;
        }
    }
    out.extend(rest);
    t.rows = out;
}

/// Makes the masks of columns joined by a foreign key the same (the stronger one wins), so
/// masked keys still match. `masks[i]` belongs to `tables[i]`.
pub fn propagate_masks(tables: &[TableRows], masks: &mut [Vec<super::copy::Mask>], fks: &[ForeignKey]) {
    use super::copy::Mask;
    let find = |schema: &str, table: &str, col: &str| {
        let t = tables.iter().position(|t| t.schema == schema && t.table == table)?;
        Some((t, tables[t].col(col)?))
    };
    for _ in 0..=fks.len() {
        let mut changed = false;
        for fk in fks {
            for (c, rc) in fk.columns.iter().zip(&fk.ref_columns) {
                let (Some((ct, ci)), Some((pt, pi))) = (find(&fk.schema, &fk.table, c), find(&fk.ref_schema, &fk.ref_table, rc)) else {
                    continue;
                };
                let (cm, pm) = (masks[ct][ci], masks[pt][pi]);
                let m = if pm != Mask::Keep { pm } else { cm };
                if cm != m || pm != m {
                    masks[ct][ci] = m;
                    masks[pt][pi] = m;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Keys per lookup of rows already in the target.
pub fn existing_key_sql(driver: Driver, table: &str, pk: &[String], keys: &[Vec<String>]) -> String {
    let cols = pk.iter().map(|c| driver.quote_ident(c)).collect::<Vec<_>>().join(", ");
    format!("SELECT {cols} FROM {table} WHERE {}", key_filter(driver, pk, keys))
}

/// Groups `keys` for `existing_key_sql`.
pub fn key_chunks(keys: &[Vec<String>]) -> std::slice::Chunks<'_, Vec<String>> {
    keys.chunks(KEYS_PER_QUERY)
}

/// Lookup by text: the target may print a key differently (`1` vs `1.0`), so numbers are
/// compared by value.
pub fn normalize_key(k: &[Cell]) -> Vec<String> {
    k.iter()
        .map(|v| {
            let v = v.clone().unwrap_or_default();
            match v.trim().parse::<f64>() {
                Ok(f) if v.trim().chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')) => format!("{f}"),
                _ => v,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn fk(schema: &str, table: &str, cols: &[&str], ref_table: &str, ref_cols: &[&str]) -> ForeignKey {
    ForeignKey {
        schema: schema.into(),
        table: table.into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        ref_schema: schema.into(),
        ref_table: ref_table.into(),
        ref_columns: ref_cols.iter().map(|c| c.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::copy::Mask;

    fn col(name: &str, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: "text".into(), nullable: !pk, primary_key: pk }
    }

    fn rows(t: &str, cols: &[ColumnInfo], data: &[&[Option<&str>]]) -> TableRows {
        TableRows {
            schema: "s".into(),
            table: t.into(),
            columns: cols.to_vec(),
            rows: data.iter().map(|r| r.iter().map(|v| v.map(str::to_string)).collect()).collect(),
        }
    }

    #[test]
    fn parses_foreign_keys_by_position() {
        let rs = ResultSet {
            columns: (0..8).map(|i| i.to_string()).collect(),
            rows: vec![
                ["fk1", "s", "lines", "order_id", "s", "orders", "id", "1"].map(|v| Some(v.to_string())).to_vec(),
                ["fk2", "s", "lines", "b", "s", "pair", "y", "2"].map(|v| Some(v.to_string())).to_vec(),
                ["fk2", "s", "lines", "a", "s", "pair", "x", "1"].map(|v| Some(v.to_string())).to_vec(),
            ],
            ..Default::default()
        };
        let fks = parse_fks(Driver::Postgres, &rs, &[]);
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[1].columns, vec!["a", "b"]);
        assert_eq!(fks[1].ref_columns, vec!["x", "y"]);
    }

    #[test]
    fn sqlite_keys_to_the_primary_key() {
        let catalog = vec![SchemaInfo {
            name: "main".into(),
            tables: vec![crate::db::TableInfo { name: "orders".into(), kind: crate::db::ObjectKind::Table, columns: vec![col("id", true)] }],
        }];
        let rs = ResultSet {
            columns: (0..8).map(|i| i.to_string()).collect(),
            rows: vec![vec![Some("lines:0".into()), Some("main".into()), Some("lines".into()), Some("order_id".into()), Some("main".into()), Some("orders".into()), None, Some("0".into())]],
            ..Default::default()
        };
        assert_eq!(parse_fks(Driver::Sqlite, &rs, &catalog)[0].ref_columns, vec!["id"]);
    }

    #[test]
    fn builds_key_filters() {
        let keys = vec![vec!["1".to_string()], vec!["it's".to_string()]];
        assert_eq!(key_filter(Driver::Postgres, &["id".into()], &keys), "\"id\" IN ('1', 'it''s')");
        let pairs = vec![vec!["1".to_string(), "a".to_string()], vec!["2".to_string(), "b".to_string()]];
        assert_eq!(key_filter(Driver::MsSql, &["x".into(), "y".into()], &pairs), "([x] = N'1' AND [y] = N'a') OR ([x] = N'2' AND [y] = N'b')");
    }

    #[test]
    fn orders_parents_first_and_self_references() {
        let fks = vec![fk("s", "orders", &["customer_id"], "customers", &["id"]), fk("s", "staff", &["boss"], "staff", &["id"])];
        let staff = rows(
            "staff",
            &[col("id", true), col("boss", false)],
            &[&[Some("3"), Some("2")], &[Some("2"), Some("1")], &[Some("1"), None], &[Some("4"), Some("9")]],
        );
        let mut tables = vec![rows("orders", &[col("id", true), col("customer_id", false)], &[]), staff, rows("customers", &[col("id", true)], &[])];
        insert_order(&mut tables, &fks);
        let names: Vec<&str> = tables.iter().map(|t| t.table.as_str()).collect();
        assert_eq!(names, vec!["staff", "customers", "orders"]);
        let ids: Vec<&str> = tables[0].rows.iter().map(|r| r[0].as_deref().unwrap()).collect();
        assert_eq!(ids, vec!["1", "4", "2", "3"]);
    }

    #[test]
    fn masks_follow_foreign_keys() {
        let fks = vec![fk("s", "orders", &["email"], "customers", &["email"])];
        let tables = vec![rows("customers", &[col("email", true)], &[]), rows("orders", &[col("id", true), col("email", false)], &[])];
        let mut masks = vec![vec![Mask::Email], vec![Mask::Keep, Mask::Keep]];
        propagate_masks(&tables, &mut masks, &fks);
        assert_eq!(masks[1], vec![Mask::Keep, Mask::Email]);
    }

    #[test]
    fn missing_key_columns_are_reported() {
        let fks = vec![fk("s", "orders", &["customer_id"], "customers", &["id"])];
        assert_eq!(missing_key_columns(&fks, "s", "orders", &[col("id", true)]), vec!["customer_id"]);
        assert!(missing_key_columns(&fks, "s", "customers", &[col("id", true)]).is_empty());
        assert_eq!(normalize_key(&[Some("1.0".into()), Some("a".into())]), vec!["1", "a"]);
    }
}
