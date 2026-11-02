//! Reads a schema's tables (with `alter::load_details`), foreign keys and views into a
//! `SchemaModel`, on sessions of their own that a cancel interrupts.

use super::{ForeignKeyDef, SchemaModel, TableModel, ViewModel, supported};
use crate::db::alter::{IndexDef, load_details};
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

/// Tables (`t`) and views (`v`) of the schema: name, kind.
fn objects_sql(driver: Driver, schema: &str) -> String {
    let sch = lit(driver, schema);
    match driver {
        Driver::Postgres => format!(
            "select c.relname, case c.relkind when 'v' then 'v' else 't' end from pg_catalog.pg_class c \
             join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = {sch} and c.relkind in ('r', 'v') and not c.relispartition order by 1"
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
             union all select view_name, 'v' from all_views where owner = {sch} order by 1"
        ),
        Driver::Sqlite => format!(
            "select name, case type when 'view' then 'v' else 't' end from {}.sqlite_master \
             where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' order by name",
            driver.quote_ident(schema)
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

/// The model of `schema` in the session's database.
pub async fn load(s: &mut Session, driver: Driver, db: &str, schema: &str, progress: &Progress) -> anyhow::Result<SchemaModel> {
    anyhow::ensure!(supported(driver), tr("Not available for this engine"));
    let cancelled = || progress.cancel.load(Ordering::Relaxed);
    let objects = query(s, &objects_sql(driver, schema)).await?;
    progress.total.fetch_add(objects.rows.len(), Ordering::Relaxed);
    let mut model = SchemaModel { driver, db: db.into(), schema: schema.into(), tables: vec![], views: vec![] };
    for r in &objects.rows {
        anyhow::ensure!(!cancelled(), tr("Cancelled"));
        let name = text(r, 0);
        if text(r, 1) == "v" {
            let definition = s.view_definition(driver, db, schema, &name).await?;
            model.views.push(ViewModel { name, definition });
        } else {
            let mut details = load_details(s, driver, schema, &name).await.map_err(|e| anyhow::anyhow!("{name}: {e:#}"))?;
            details.rebuild_blocker = None;
            if driver == Driver::Sqlite {
                // UNIQUE constraints (auto-named indexes `load_details` leaves out).
                let rs = query(
                    s,
                    &format!(
                        "select il.name, ii.name from pragma_index_list({0}, {1}) il join pragma_index_info(il.name, {1}) ii \
                         where il.origin = 'u' order by il.name, ii.seqno",
                        lit(driver, &name),
                        lit(driver, schema)
                    ),
                )
                .await?;
                for r in &rs.rows {
                    let index = text(r, 0);
                    if details.indexes.last().is_none_or(|x| x.name != index) {
                        details.indexes.push(IndexDef { name: index, unique: true, constraint: true, plain: true, columns: vec![] });
                    }
                    details.indexes.last_mut().expect("pushed").columns.push(text(r, 1));
                }
            }
            model.tables.push(TableModel { name, details, foreign_keys: vec![] });
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    anyhow::ensure!(!cancelled(), tr("Cancelled"));
    for (table, fk) in parse_foreign_keys(driver, &query(s, &foreign_keys_sql(driver, schema)).await?) {
        if let Some(t) = model.tables.iter_mut().find(|t| t.name == table) {
            t.foreign_keys.push(fk);
        }
    }
    if let Some(sql) = identity_sql(driver, schema) {
        for r in &query(s, &sql).await?.rows {
            let column = model.tables.iter_mut().find(|t| t.name == text(r, 0)).and_then(|t| t.details.columns.iter_mut().find(|c| c.name == text(r, 1)));
            if let Some(c) = column {
                c.extra = text(r, 2);
                // Oracle shows the identity's sequence as the default.
                c.default = None;
            }
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

/// Reads both schemas; a cancel interrupts the running catalog query.
pub async fn run(source: ConnectionConfig, target: ConnectionConfig, src: Side, tgt: Side, progress: Arc<Progress>) -> anyhow::Result<(SchemaModel, SchemaModel)> {
    for cfg in [&source, &target] {
        anyhow::ensure!(supported(cfg.driver), trf("Schemas can't be compared on {engine}", &[("engine", &cfg.driver.label())]));
    }
    anyhow::ensure!(!progress.cancel.load(Ordering::Relaxed), tr("Cancelled"));
    let (sd, td) = (source.driver, target.driver);
    let mut a = open(source, &src.db).await?;
    let mut b = open(target, &tgt.db).await?;
    let handles = [a.cancel_handle(), b.cancel_handle()];
    let work = async {
        let s = load(&mut a, sd, &src.db, &src.schema, &progress).await?;
        let t = load(&mut b, td, &tgt.db, &tgt.schema, &progress).await?;
        anyhow::Ok((s, t))
    };
    tokio::pin!(work);
    let cancelled = async {
        while !progress.cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::select! {
        result = &mut work => result,
        _ = cancelled => {
            for h in handles.into_iter().flatten() {
                let _ = tokio::time::timeout(Duration::from_secs(3), h.cancel()).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), &mut work).await;
            Err(anyhow::anyhow!(tr("Cancelled")))
        }
    }
}
