//! Reads a schema's tables (with `alter::load_details`), foreign keys and views into a
//! `SchemaModel`, on sessions of their own that a cancel interrupts.

use super::{ForeignKeyDef, SchemaModel, TableModel, ViewModel, supported};
use crate::db::alter::{ColumnDef, IndexDef, load_tables};
use std::collections::HashMap;
use crate::db::table_definition::lit;
use crate::db::{ConnectionConfig, Driver, ResultSet, Session};
use crate::i18n::{tr, trf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// One side of the comparison.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub db: String,
    pub schema: String,
}

/// Shared with the dialog: objects read so far, and the cancel flag.
#[derive(Debug, Default)]
pub struct Progress {
    pub cancel: AtomicBool,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
}

async fn query(s: &mut Session, sql: &str) -> anyhow::Result<ResultSet> {
    let set = s.execute(sql, 1_000_000).await?.into_iter().find(|r| !r.columns.is_empty()).unwrap_or_default();
    anyhow::ensure!(!set.truncated, tr("The schema catalog is too large to compare"));
    Ok(set)
}

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// Tables (`t`) and views (`v`) of the schema, and the objects that are not compared:
/// partitioned tables (`p`), partitions (`c`), materialized views (`m`), foreign tables (`f`),
/// extension objects (`x`), SQLite virtual tables (`w`) and their shadow tables (`h`). Name, kind.
fn objects_sql(driver: Driver, schema: &str) -> String {
    let sch = lit(driver, schema);
    match driver {
        Driver::Postgres => format!(
            "select c.relname, case \
             when exists (select 1 from pg_catalog.pg_depend d where d.classid = 'pg_catalog.pg_class'::regclass and d.objid = c.oid and d.deptype = 'e') then 'x' \
             when c.relkind in ('v', 'p', 'm', 'f') then c.relkind::text when c.relispartition then 'c' else 't' end \
             from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = {sch} and c.relkind in ('r', 'v', 'p', 'm', 'f') order by 1"
        ),
        Driver::MySql | Driver::MariaDb => format!(
            "select table_name, case when table_type = 'VIEW' then 'v' else 't' end from information_schema.tables \
             where table_schema = {sch} and table_type in ('BASE TABLE', 'VIEW') order by 1"
        ),
        Driver::MsSql => format!(
            "select o.name, case o.type when 'V' then 'v' else 't' end from sys.objects o \
             where o.schema_id = schema_id({sch}) and o.type in ('U', 'V') and o.is_ms_shipped = 0 order by o.name"
        ),
        Driver::Oracle => format!(
            "select table_name, 't' from all_tables where owner = {sch} and nested = 'NO' and secondary = 'N' and dropped = 'NO' \
             and table_name not in (select mview_name from all_mviews where owner = {sch}) \
             union all select view_name, 'v' from all_views where owner = {sch} \
             union all select mview_name, 'm' from all_mviews where owner = {sch} order by 1"
        ),
        // `pragma_table_list` tells virtual tables, and their shadow tables (`h`, left out).
        Driver::Sqlite => format!(
            "select name, case type when 'view' then 'v' when 'virtual' then 'w' when 'shadow' then 'h' else 't' end \
             from pragma_table_list where schema = {sch} and name not like 'sqlite\\_%' escape '\\' order by name"
        ),
        _ => String::new(),
    }
}

/// Foreign keys of the schema's tables, one row per column: group, name, table, column,
/// referenced schema, table and column, position, delete rule, update rule.
fn foreign_keys_sql(driver: Driver, schema: &str) -> String {
    let sch = lit(driver, schema);
    match driver {
        Driver::Postgres => format!(
            "select con.conname, con.conname, c.relname, a.attname, rn.nspname, rc.relname, ra.attname, k.ord, \
             con.confdeltype::text, con.confupdtype::text \
             from pg_catalog.pg_constraint con \
             join pg_catalog.pg_class c on c.oid = con.conrelid join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             join pg_catalog.pg_class rc on rc.oid = con.confrelid join pg_catalog.pg_namespace rn on rn.oid = rc.relnamespace \
             cross join lateral unnest(con.conkey, con.confkey) with ordinality k(attnum, refnum, ord) \
             join pg_catalog.pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.attnum \
             join pg_catalog.pg_attribute ra on ra.attrelid = con.confrelid and ra.attnum = k.refnum \
             where con.contype = 'f' and n.nspname = {sch} order by c.relname, con.conname, k.ord"
        ),
        Driver::MySql | Driver::MariaDb => format!(
            "select k.constraint_name, k.constraint_name, k.table_name, k.column_name, k.referenced_table_schema, \
             k.referenced_table_name, k.referenced_column_name, k.ordinal_position, r.delete_rule, r.update_rule \
             from information_schema.key_column_usage k join information_schema.referential_constraints r \
             on r.constraint_schema = k.constraint_schema and r.constraint_name = k.constraint_name and r.table_name = k.table_name \
             where k.table_schema = {sch} and k.referenced_table_name is not null \
             order by k.table_name, k.constraint_name, k.ordinal_position"
        ),
        Driver::MsSql => format!(
            "select fk.name, fk.name, t.name, c.name, schema_name(rt.schema_id), rt.name, rc.name, fkc.constraint_column_id, \
             fk.delete_referential_action_desc, fk.update_referential_action_desc \
             from sys.foreign_keys fk join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
             join sys.tables t on t.object_id = fk.parent_object_id \
             join sys.columns c on c.object_id = fkc.parent_object_id and c.column_id = fkc.parent_column_id \
             join sys.tables rt on rt.object_id = fk.referenced_object_id \
             join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id \
             where t.schema_id = schema_id({sch}) order by t.name, fk.name, fkc.constraint_column_id"
        ),
        Driver::Oracle => format!(
            "select c.constraint_name, c.constraint_name, c.table_name, cc.column_name, r.owner, r.table_name, rc.column_name, \
             cc.position, c.delete_rule, 'NO ACTION' \
             from all_constraints c join all_cons_columns cc on cc.owner = c.owner and cc.constraint_name = c.constraint_name \
             join all_constraints r on r.owner = c.r_owner and r.constraint_name = c.r_constraint_name \
             join all_cons_columns rc on rc.owner = r.owner and rc.constraint_name = r.constraint_name and rc.position = cc.position \
             where c.constraint_type = 'R' and c.owner = {sch} order by c.table_name, c.constraint_name, cc.position"
        ),
        // SQLite keys have no name: they are grouped by table and id.
        Driver::Sqlite => format!(
            "select m.name || ':' || p.id, '', m.name, p.\"from\", {sch}, p.\"table\", p.\"to\", p.seq, p.on_delete, p.on_update \
             from {}.sqlite_master m join pragma_foreign_key_list(m.name, {sch}) p where m.type = 'table' order by m.name, p.id, p.seq",
            driver.quote_ident(schema)
        ),
        _ => String::new(),
    }
}

/// What `kind` of `objects_sql` stands for, for the list of objects not compared.
fn skipped_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "p" => tr("partitioned table"),
        "c" => tr("partition"),
        "m" => tr("materialized view"),
        "f" => tr("foreign table"),
        "x" => tr("extension object"),
        "w" => tr("virtual table"),
        _ => return None,
    })
}

/// Generated columns with their expression: table, column, the clause that recreates them
/// (SQL Server's computed columns can't be written that way, and are left out of scripts).
fn generated_sql(driver: Driver, schema: &str) -> Option<String> {
    let sch = lit(driver, schema);
    Some(match driver {
        Driver::Postgres => format!(
            "select c.relname, a.attname, 'generated always as (' || pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) || ') stored' \
             from pg_catalog.pg_attribute a join pg_catalog.pg_class c on c.oid = a.attrelid \
             join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             join pg_catalog.pg_attrdef ad on ad.adrelid = a.attrelid and ad.adnum = a.attnum \
             where n.nspname = {sch} and a.attgenerated = 's'"
        ),
        Driver::MySql | Driver::MariaDb => format!(
            "select table_name, column_name, concat('generated always as (', generation_expression, ') ', \
             case when upper(extra) like '%STORED%' or upper(extra) like '%PERSISTENT%' then 'stored' else 'virtual' end) \
             from information_schema.columns where table_schema = {sch} and generation_expression is not null and generation_expression <> ''"
        ),
        Driver::Oracle => format!("select table_name, column_name, data_default from all_tab_cols where owner = {sch} and virtual_column = 'YES' and hidden_column = 'NO'"),
        _ => return None,
    })
}

/// Sequences and user types (not those of extensions): names `schema.name` may point at.
fn sequences_sql(driver: Driver, schema: &str) -> Option<String> {
    let sch = lit(driver, schema);
    Some(match driver {
        Driver::Postgres => format!(
            "select c.relname, 's' from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = {sch} and c.relkind = 'S' \
             union all select t.typname, 't' from pg_catalog.pg_type t join pg_catalog.pg_namespace n on n.oid = t.typnamespace \
             where n.nspname = {sch} and (t.typtype in ('e', 'd', 'r', 'm') \
             or (t.typtype = 'c' and (select relkind from pg_catalog.pg_class where oid = t.typrelid) = 'c')) \
             and not exists (select 1 from pg_catalog.pg_depend d where d.classid = 'pg_catalog.pg_type'::regclass and d.objid = t.oid and d.deptype = 'e')"
        ),
        Driver::MsSql => format!("select name, 's' from sys.sequences where schema_id = schema_id({sch})"),
        Driver::Oracle => format!("select sequence_name, 's' from all_sequences where sequence_owner = {sch}"),
        _ => return None,
    })
}

/// PostgreSQL views: their columns (view, column, type).
fn view_columns_sql(schema: &str) -> String {
    format!(
        "select c.relname, a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) from pg_catalog.pg_attribute a \
         join pg_catalog.pg_class c on c.oid = a.attrelid join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
         where n.nspname = {} and c.relkind = 'v' and a.attnum > 0 and not a.attisdropped order by c.relname, a.attnum",
        lit(Driver::Postgres, schema)
    )
}

/// PostgreSQL views: the columns of this schema's tables and views each one uses.
fn view_uses_sql(schema: &str) -> String {
    let sch = lit(Driver::Postgres, schema);
    format!(
        "select distinct v.relname, t.relname, a.attname from pg_catalog.pg_depend d \
         join pg_catalog.pg_rewrite r on r.oid = d.objid join pg_catalog.pg_class v on v.oid = r.ev_class \
         join pg_catalog.pg_namespace vn on vn.oid = v.relnamespace \
         join pg_catalog.pg_class t on t.oid = d.refobjid join pg_catalog.pg_namespace tn on tn.oid = t.relnamespace \
         join pg_catalog.pg_attribute a on a.attrelid = t.oid and a.attnum = d.refobjsubid \
         where d.classid = 'pg_catalog.pg_rewrite'::regclass and d.refclassid = 'pg_catalog.pg_class'::regclass \
         and v.relkind = 'v' and v.oid <> t.oid and vn.nspname = {sch} and tn.nspname = {sch} order by 1, 2, 3"
    )
}

/// Identity columns: table, column, the clause that recreates them.
fn identity_sql(driver: Driver, schema: &str) -> Option<String> {
    let sch = lit(driver, schema);
    Some(match driver {
        Driver::Postgres => format!(
            "select c.relname, a.attname, case a.attidentity when 'a' then 'generated always as identity' else 'generated by default as identity' end \
             from pg_catalog.pg_attribute a join pg_catalog.pg_class c on c.oid = a.attrelid \
             join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = {sch} and a.attnum > 0 and not a.attisdropped and a.attidentity in ('a', 'd')"
        ),
        Driver::MsSql => format!(
            "select t.name, c.name, 'identity(' + cast(c.seed_value as varchar(40)) + ',' + cast(c.increment_value as varchar(40)) + ')' \
             from sys.identity_columns c join sys.tables t on t.object_id = c.object_id where t.schema_id = schema_id({sch})"
        ),
        Driver::Oracle => format!(
            "select table_name, column_name, case generation_type when 'ALWAYS' then 'generated always as identity' \
             when 'BY DEFAULT ON NULL' then 'generated by default on null as identity' else 'generated by default as identity' end \
             from all_tab_identity_columns where owner = {sch}"
        ),
        _ => return None,
    })
}

/// PostgreSQL's one-letter delete / update rules.
fn rule(driver: Driver, r: &str) -> String {
    match (driver, r) {
        (Driver::Postgres, "a") => "NO ACTION".into(),
        (Driver::Postgres, "r") => "RESTRICT".into(),
        (Driver::Postgres, "c") => "CASCADE".into(),
        (Driver::Postgres, "n") => "SET NULL".into(),
        (Driver::Postgres, "d") => "SET DEFAULT".into(),
        _ => r.replace('_', " ").to_ascii_uppercase(),
    }
}

/// Foreign keys from the rows of `foreign_keys_sql`, by table.
fn parse_foreign_keys(driver: Driver, rs: &ResultSet) -> Vec<(String, ForeignKeyDef)> {
    let mut out: Vec<(String, String, ForeignKeyDef)> = vec![];
    for r in &rs.rows {
        let group = text(r, 0);
        if out.last().is_none_or(|(g, ..)| *g != group) {
            let fk = ForeignKeyDef {
                name: text(r, 1),
                ref_schema: text(r, 4),
                ref_table: text(r, 5),
                on_delete: rule(driver, &text(r, 8)),
                on_update: rule(driver, &text(r, 9)),
                ..Default::default()
            };
            out.push((group, text(r, 2), fk));
        }
        let fk = &mut out.last_mut().expect("pushed").2;
        fk.columns.push(text(r, 3));
        fk.ref_columns.push(text(r, 6));
    }
    out.into_iter()
        .map(|(_, table, mut fk)| {
            // SQLite leaves the referenced columns out when the key points at the primary key.
            if fk.ref_columns.iter().any(String::is_empty) {
                fk.ref_columns.clear();
            }
            (table, fk)
        })
        .collect()
}

/// SQLite: each table's statement, constraints and options, UNIQUE constraints (auto-named
/// indexes `load_tables` leaves out), triggers, and generated columns, which `pragma_table_info`
/// leaves out (`pragma_table_xinfo` marks them hidden 2 or 3), in place. One query of each kind.
async fn sqlite_tables(s: &mut Session, schema: &str, tables: &mut [TableModel]) -> anyhow::Result<()> {
    let d = Driver::Sqlite;
    let sch = lit(d, schema);
    let master = format!("{}.sqlite_master m", d.quote_ident(schema));
    let mut by_name: HashMap<String, &mut TableModel> = tables.iter_mut().map(|t| (t.name.clone(), t)).collect();
    let mut generated: HashMap<String, HashMap<String, String>> = HashMap::new();
    for r in &query(s, &format!("select m.name, m.sql from {master} where m.type = 'table'")).await?.rows {
        if let Some(t) = by_name.get_mut(&text(r, 0)) {
            t.sql = text(r, 1);
            let (features, columns) = super::sqlite_features(&t.sql);
            t.features = features;
            generated.insert(t.name.clone(), columns);
        }
    }
    let rs = query(
        s,
        &format!(
            "select m.name, il.name, ii.name from {master} join pragma_index_list(m.name, {sch}) il join pragma_index_info(il.name, {sch}) ii \
             where m.type = 'table' and il.origin = 'u' order by m.name, il.name, ii.seqno"
        ),
    )
    .await?;
    for r in &rs.rows {
        let Some(t) = by_name.get_mut(&text(r, 0)) else { continue };
        let index = text(r, 1);
        if t.details.indexes.last().is_none_or(|x| x.name != index) {
            t.details.indexes.push(IndexDef { name: index, unique: true, constraint: true, plain: true, columns: vec![] });
        }
        t.details.indexes.last_mut().expect("pushed").columns.push(text(r, 2));
    }
    let rs = query(
        s,
        &format!("select m.name, p.name, p.type, p.\"notnull\", p.hidden from {master} join pragma_table_xinfo(m.name, {sch}) p where m.type = 'table' order by m.name, p.cid"),
    )
    .await?;
    let mut columns: HashMap<String, Vec<&Vec<Option<String>>>> = HashMap::new();
    for r in &rs.rows {
        columns.entry(text(r, 0)).or_default().push(r);
    }
    for (name, rows) in columns {
        let Some(t) = by_name.get_mut(&name) else { continue };
        let exprs = generated.remove(&name).unwrap_or_default();
        let mut plain = std::mem::take(&mut t.details.columns).into_iter();
        for r in rows {
            match text(r, 4).as_str() {
                "2" | "3" => t.details.columns.push(ColumnDef {
                    name: text(r, 1),
                    data_type: text(r, 2),
                    nullable: text(r, 3) == "0",
                    extra: exprs.get(&text(r, 1).to_lowercase()).cloned().unwrap_or_default(),
                    generated: true,
                    ..Default::default()
                }),
                "0" => t.details.columns.extend(plain.next()),
                _ => {}
            }
        }
        t.details.columns.extend(plain);
    }
    for r in &query(s, &format!("select m.tbl_name, m.name from {master} where m.type = 'trigger' order by m.name")).await?.rows {
        if let Some(t) = by_name.get_mut(&text(r, 0)) {
            t.triggers.push(text(r, 1));
        }
    }
    Ok(())
}

/// The definitions of the schema's views in one query (MySQL / MariaDB: none, read one by one).
fn views_sql(driver: Driver, schema: &str) -> Option<String> {
    let sch = lit(driver, schema);
    Some(match driver {
        Driver::Postgres => format!(
            "select c.relname, pg_catalog.pg_get_viewdef(c.oid, true) from pg_catalog.pg_class c \
             join pg_catalog.pg_namespace n on n.oid = c.relnamespace where n.nspname = {sch} and c.relkind = 'v'"
        ),
        Driver::MsSql => format!("select v.name, m.definition from sys.views v join sys.sql_modules m on m.object_id = v.object_id where v.schema_id = schema_id({sch})"),
        Driver::Oracle => format!("select view_name, text from all_views where owner = {sch}"),
        Driver::Sqlite => format!("select name, sql from {}.sqlite_master where type = 'view'", driver.quote_ident(schema)),
        _ => return None,
    })
}

/// The model of `schema` in the session's database: a fixed number of catalog queries,
/// however many tables and views it has.
pub async fn load(s: &mut Session, driver: Driver, db: &str, schema: &str, progress: &Progress) -> anyhow::Result<SchemaModel> {
    anyhow::ensure!(supported(driver), tr("Not available for this engine"));
    let cancelled = || progress.cancel.load(Ordering::Relaxed);
    let objects = query(s, &objects_sql(driver, schema)).await?;
    progress.total.fetch_add(objects.rows.len(), Ordering::Relaxed);
    let mut model = SchemaModel::new(driver, db, schema);
    if matches!(driver, Driver::MySql | Driver::MariaDb) {
        // 0: table names keep their case (Linux); 1 and 2 compare them ignoring it.
        let lower = query(s, "select @@lower_case_table_names").await?.rows.first().map(|r| text(r, 0)).unwrap_or_default();
        model.names_ignore_case = lower.trim() != "0";
    }
    let (mut tables, mut views) = (vec![], vec![]);
    for r in &objects.rows {
        let (name, kind) = (text(r, 0), text(r, 1));
        match kind.as_str() {
            // Part of a virtual table, which is listed.
            "h" => {}
            "v" => views.push(name),
            "t" => tables.push(name),
            _ => model.skipped.extend(skipped_kind(&kind).map(|label| format!("{name} ({label})"))),
        }
    }
    anyhow::ensure!(!cancelled(), tr("Cancelled"));
    let mut details = load_tables(s, driver, schema, None).await?;
    model.tables = tables.into_iter().map(|name| TableModel { details: details.remove(&name).unwrap_or_default(), name, ..Default::default() }).collect();
    if driver == Driver::Sqlite {
        sqlite_tables(s, schema, &mut model.tables).await?;
    }
    progress.done.fetch_add(model.tables.len(), Ordering::Relaxed);
    anyhow::ensure!(!cancelled(), tr("Cancelled"));
    let mut texts: HashMap<String, String> = HashMap::new();
    if let Some(sql) = views_sql(driver, schema) {
        texts = query(s, &sql).await?.rows.iter().map(|r| (text(r, 0), text(r, 1))).filter(|(_, t)| !t.trim().is_empty()).collect();
    }
    for name in views {
        anyhow::ensure!(!cancelled(), tr("Cancelled"));
        let definition = match texts.remove(&name) {
            Some(text) => crate::db::views::parse_definition(driver, &text),
            None => s.view_definition(driver, db, schema, &name).await?,
        };
        model.views.push(ViewModel { name, definition, ..Default::default() });
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    anyhow::ensure!(!cancelled(), tr("Cancelled"));
    for (table, fk) in parse_foreign_keys(driver, &query(s, &foreign_keys_sql(driver, schema)).await?) {
        if let Some(t) = model.tables.iter_mut().find(|t| t.name == table) {
            t.foreign_keys.push(fk);
        }
    }
    // Identity and generation clauses replace the column's EXTRA (and the default Oracle shows
    // for them: the identity's sequence, or the expression of a virtual column).
    for sql in [identity_sql(driver, schema), generated_sql(driver, schema)].into_iter().flatten() {
        for r in &query(s, &sql).await?.rows {
            let column = model.tables.iter_mut().find(|t| t.name == text(r, 0)).and_then(|t| t.details.columns.iter_mut().find(|c| c.name == text(r, 1)));
            if let Some(c) = column {
                c.extra = match driver {
                    Driver::Oracle if c.generated => format!("generated always as ({}) virtual", text(r, 2).trim()),
                    _ => text(r, 2),
                };
                c.default = None;
            }
        }
    }
    if let Some(sql) = sequences_sql(driver, schema) {
        for r in &query(s, &sql).await?.rows {
            if text(r, 1) == "s" { model.sequences.push(text(r, 0)) } else { model.types.push(text(r, 0)) }
        }
    }
    if driver == Driver::Postgres {
        for r in &query(s, &view_columns_sql(schema)).await?.rows {
            if let Some(v) = model.views.iter_mut().find(|v| v.name == text(r, 0)) {
                v.columns.push((text(r, 1), text(r, 2)));
            }
        }
        for r in &query(s, &view_uses_sql(schema)).await?.rows {
            if let Some(v) = model.views.iter_mut().find(|v| v.name == text(r, 0)) {
                v.uses.push((text(r, 1), text(r, 2)));
            }
        }
    }
    if driver == Driver::Sqlite {
        for name in ["foreign_keys", "legacy_alter_table"] {
            let value = query(s, &format!("pragma {name}")).await?.rows.first().map(|r| text(r, 0)).unwrap_or_default();
            model.pragmas.push((name.into(), if value == "1" { "on".into() } else { "off".into() }));
        }
    }
    // SQLite keys that point at the primary key: its columns.
    let pks: Vec<(String, Vec<String>)> = model.tables.iter().map(|t| (t.name.to_lowercase(), t.details.primary_key.clone())).collect();
    for fk in model.tables.iter_mut().flat_map(|t| t.foreign_keys.iter_mut()).filter(|fk| fk.ref_columns.is_empty()) {
        if let Some((_, pk)) = pks.iter().find(|(t, _)| *t == fk.ref_table.to_lowercase()) {
            fk.ref_columns.clone_from(pk);
        }
    }
    Ok(model)
}

/// Session on `db` (Oracle and SQLite keep their own), with PostgreSQL's search path emptied so
/// every catalog text names its schema (and can be moved to the other one).
async fn open(mut cfg: ConnectionConfig, db: &str) -> anyhow::Result<Session> {
    let driver = cfg.driver;
    if !matches!(driver, Driver::Sqlite | Driver::Oracle) && !db.is_empty() {
        cfg.database = db.into();
    }
    let mut s = tokio::time::timeout(Duration::from_secs(30), Session::connect(&cfg)).await.map_err(|_| anyhow::anyhow!(tr("Connection timed out")))??;
    if !db.is_empty() {
        tokio::time::timeout(Duration::from_secs(30), s.use_database(db)).await.map_err(|_| anyhow::anyhow!(tr("Connection timed out")))??;
    }
    if driver == Driver::Postgres {
        s.execute("select pg_catalog.set_config('search_path', '', false)", 1).await?;
    }
    Ok(s)
}

/// Opens one side's session (its cancel handle goes to `handles`) and reads its schema.
async fn read_side(cfg: ConnectionConfig, driver: Driver, side: &Side, handles: &std::sync::Mutex<Vec<crate::db::CancelHandle>>, progress: &Progress) -> anyhow::Result<SchemaModel> {
    let mut session = open(cfg, &side.db).await?;
    handles.lock().unwrap_or_else(|e| e.into_inner()).extend(session.cancel_handle());
    load(&mut session, driver, &side.db, &side.schema, progress).await
}

/// Reads both schemas. A cancel stops a connection being opened, or interrupts the running
/// catalog query.
pub async fn run(source: ConnectionConfig, target: ConnectionConfig, src: Side, tgt: Side, progress: Arc<Progress>) -> anyhow::Result<(SchemaModel, SchemaModel)> {
    for cfg in [&source, &target] {
        anyhow::ensure!(supported(cfg.driver), trf("Schemas can't be compared on {engine}", &[("engine", &cfg.driver.label())]));
    }
    anyhow::ensure!(!progress.cancel.load(Ordering::Relaxed), tr("Cancelled"));
    let (sd, td) = (source.driver, target.driver);
    let handles = std::sync::Mutex::new(Vec::new());
    // Both sides at once, each on its own session: one still connecting doesn't hold up the other.
    let work = async { tokio::try_join!(read_side(source, sd, &src, &handles, &progress), read_side(target, td, &tgt, &handles, &progress)) };
    tokio::pin!(work);
    let cancelled = async {
        while !progress.cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::select! {
        result = &mut work => result,
        _ = cancelled => {
            let handles = std::mem::take(&mut *handles.lock().unwrap_or_else(|e| e.into_inner()));
            for h in handles {
                let _ = tokio::time::timeout(Duration::from_secs(3), h.cancel()).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), &mut work).await;
            Err(anyhow::anyhow!(tr("Cancelled")))
        }
    }
}
