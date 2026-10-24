//! "Modify Table…": loads a table's full structure (types, defaults, primary key, indexes) and
//! turns the edited structure into the ALTER TABLE statements of each engine.

use crate::db::{Cell, Driver, ResultSet, Session};
use crate::i18n::{tr, trf};
use crate::ui::create_dialog::NewColumn;
use crate::ui::drop_dialog::{DropKind, effective_name, object_sql};
use crate::ui::quote_ident_if_needed;

/// A column as the server describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    /// SQL expression, as the server shows it.
    pub default: Option<String>,
    /// MySQL: the EXTRA attributes (`auto_increment`, `on update …`) a CHANGE must repeat.
    pub extra: String,
    /// MySQL: column comment and collation, also repeated by CHANGE / MODIFY.
    pub comment: String,
    pub collation: Option<String>,
    /// SQL Server: the default constraint, dropped before changing or removing the default.
    pub default_constraint: Option<String>,
    /// Generated / computed column: it can be renamed or dropped, not redefined.
    pub generated: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    /// Backs a UNIQUE constraint: dropped with DROP CONSTRAINT.
    pub constraint: bool,
    /// false for expression or partial indexes, which can't be rebuilt from their columns.
    pub plain: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableDetails {
    pub columns: Vec<ColumnDef>,
    /// Primary key columns, in key order.
    pub primary_key: Vec<String>,
    pub pk_name: Option<String>,
    /// Indexes other than the primary key's.
    pub indexes: Vec<IndexDef>,
    /// Server version (MySQL/MariaDB: RENAME COLUMN; SQLite: DROP COLUMN).
    pub version: String,
    /// SQLite: why the table can't be rebuilt (None = it can).
    pub rebuild_blocker: Option<String>,
    /// SQLite: the INTEGER primary key is AUTOINCREMENT (kept by a rebuild).
    pub autoincrement: bool,
}

/// A row of the dialog's column list: `original` is its index in `TableDetails::columns`
/// (None for a new column).
#[derive(Clone, Debug, PartialEq)]
pub struct EditColumn {
    pub original: Option<usize>,
    pub column: NewColumn,
}

/// A row of the dialog's index list. Existing indexes can only be dropped (removed from the list).
#[derive(Clone, Debug, PartialEq)]
pub struct EditIndex {
    pub original: Option<usize>,
    pub name: String,
    /// Comma-separated column names.
    pub columns: String,
    pub unique: bool,
}

pub struct TableEdit<'a> {
    pub schema: &'a str,
    pub table: &'a str,
    pub new_name: &'a str,
    pub columns: &'a [EditColumn],
    pub indexes: &'a [EditIndex],
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub statements: Vec<String>,
    /// Changes that can lose data, shown above the preview.
    pub warnings: Vec<String>,
    /// Run all statements in one transaction (engines with transactional DDL).
    pub transaction: bool,
}

/// Engines whose indexes can be listed and edited here (Snowflake has none on standard tables).
pub fn has_indexes(driver: Driver) -> bool {
    !matches!(driver, Driver::Snowflake | Driver::Redis | Driver::MongoDb)
}

pub fn initial_columns(d: &TableDetails) -> Vec<EditColumn> {
    d.columns
        .iter()
        .enumerate()
        .map(|(i, c)| EditColumn {
            original: Some(i),
            column: NewColumn {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                not_null: !c.nullable,
                primary_key: d.primary_key.iter().any(|p| p == &c.name),
                default: c.default.clone().unwrap_or_default(),
            },
        })
        .collect()
}

pub fn initial_indexes(d: &TableDetails) -> Vec<EditIndex> {
    d.indexes
        .iter()
        .enumerate()
        .map(|(i, x)| EditIndex { original: Some(i), name: x.name.clone(), columns: x.columns.join(", "), unique: x.unique })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Loading

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Rows of the first result set with columns.
async fn query(s: &mut Session, sql: &str) -> anyhow::Result<ResultSet> {
    let sets = s.execute(sql, 100_000).await?;
    Ok(sets.into_iter().find(|r| !r.columns.is_empty()).unwrap_or_default())
}

fn text(c: &Cell) -> String {
    c.clone().unwrap_or_default()
}

fn flag(c: &Cell) -> bool {
    matches!(c.as_deref().map(str::trim), Some("1" | "t" | "true" | "TRUE" | "YES" | "Y" | "y"))
}

/// Groups `(index name, unique, column, constraint?)` rows (ordered by name, then position).
fn group_indexes(rows: impl IntoIterator<Item = (String, bool, Option<String>, bool)>) -> Vec<IndexDef> {
    let mut out: Vec<IndexDef> = Vec::new();
    for (name, unique, column, constraint) in rows {
        if out.last().is_none_or(|x| x.name != name) {
            out.push(IndexDef { name: name.clone(), unique, constraint, plain: true, columns: vec![] });
        }
        let x = out.last_mut().expect("pushed");
        match column {
            Some(c) => x.columns.push(c),
            None => {
                x.plain = false;
                x.columns.push(tr("(expression)").into());
            }
        }
    }
    out
}

/// Full structure of `schema.table`, read with the engine's catalog queries.
pub async fn load_details(s: &mut Session, driver: Driver, schema: &str, table: &str) -> anyhow::Result<TableDetails> {
    let (sch, tab) = (lit(schema), lit(table));
    let mut d = TableDetails::default();
    match driver {
        Driver::Postgres => {
            let rel = format!("n.nspname = {sch} and c.relname = {tab}");
            let rs = query(
                s,
                &format!(
                    "select a.attname, format_type(a.atttypid, a.atttypmod), case when a.attnotnull then '0' else '1' end, \
                     pg_get_expr(ad.adbin, ad.adrelid), case when a.attgenerated <> '' then '1' else '0' end \
                     from pg_attribute a join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace \
                     left join pg_attrdef ad on ad.adrelid = a.attrelid and ad.adnum = a.attnum \
                     where {rel} and a.attnum > 0 and not a.attisdropped order by a.attnum"
                ),
            )
            .await?;
            d.columns = rs
                .rows
                .iter()
                .map(|r| {
                    let generated = flag(&r[4]);
                    ColumnDef {
                        name: text(&r[0]),
                        data_type: text(&r[1]),
                        nullable: flag(&r[2]),
                        default: if generated { None } else { r[3].clone() },
                        generated,
                        ..Default::default()
                    }
                })
                .collect();
            let rs = query(
                s,
                &format!(
                    "select con.conname, a.attname from pg_constraint con join pg_class c on c.oid = con.conrelid \
                     join pg_namespace n on n.oid = c.relnamespace \
                     cross join lateral unnest(con.conkey) with ordinality k(attnum, ord) \
                     join pg_attribute a on a.attrelid = c.oid and a.attnum = k.attnum \
                     where con.contype = 'p' and {rel} order by k.ord"
                ),
            )
            .await?;
            d.pk_name = rs.rows.first().map(|r| text(&r[0]));
            d.primary_key = rs.rows.iter().map(|r| text(&r[1])).collect();
            let rs = query(
                s,
                &format!(
                    "select i.relname, case when ix.indisunique then '1' else '0' end, a.attname, \
                     case when exists (select 1 from pg_constraint k where k.conindid = ix.indexrelid and k.contype in ('u', 'x')) then '1' else '0' end, \
                     case when ix.indpred is null then '1' else '0' end \
                     from pg_index ix join pg_class i on i.oid = ix.indexrelid join pg_class c on c.oid = ix.indrelid \
                     join pg_namespace n on n.oid = c.relnamespace \
                     cross join lateral unnest(ix.indkey::int2[]) with ordinality k(attnum, ord) \
                     left join pg_attribute a on a.attrelid = c.oid and a.attnum = k.attnum and k.attnum > 0 \
                     where {rel} and not ix.indisprimary order by i.relname, k.ord"
                ),
            )
            .await?;
            let partial: Vec<String> = rs.rows.iter().filter(|r| !flag(&r[4])).map(|r| text(&r[0])).collect();
            d.indexes = group_indexes(rs.rows.iter().map(|r| (text(&r[0]), flag(&r[1]), r[2].clone(), flag(&r[3]))));
            for x in d.indexes.iter_mut().filter(|x| partial.contains(&x.name)) {
                x.plain = false;
            }
        }
        Driver::MySql | Driver::MariaDb => {
            d.version = query(s, "select version()").await?.rows.first().map(|r| text(&r[0])).unwrap_or_default();
            let mariadb = d.version.to_ascii_lowercase().contains("mariadb");
            let rs = query(
                s,
                &format!(
                    "select column_name, column_type, is_nullable, column_default, extra, column_comment, collation_name \
                     from information_schema.columns where table_schema = {sch} and table_name = {tab} order by ordinal_position"
                ),
            )
            .await?;
            d.columns = rs
                .rows
                .iter()
                .map(|r| {
                    let extra = text(&r[4]);
                    let lower = extra.to_ascii_lowercase();
                    let generated = lower.contains("virtual generated") || lower.contains("stored generated") || lower.contains("persistent");
                    let default_generated = lower.contains("default_generated");
                    let extra = extra.split_whitespace().filter(|w| !w.eq_ignore_ascii_case("DEFAULT_GENERATED")).collect::<Vec<_>>().join(" ");
                    let default = match r[3].clone() {
                        _ if generated => None,
                        // MariaDB writes NULL as the word and quotes literals.
                        Some(v) if mariadb && v == "NULL" => None,
                        Some(v) if mariadb || default_generated => Some(v),
                        // MySQL shows literals without quotes.
                        Some(v) => Some(lit(&v)),
                        None => None,
                    };
                    ColumnDef {
                        name: text(&r[0]),
                        data_type: text(&r[1]),
                        nullable: text(&r[2]).eq_ignore_ascii_case("YES"),
                        default,
                        extra,
                        comment: text(&r[5]),
                        collation: r[6].clone().filter(|c| !c.is_empty()),
                        generated,
                        ..Default::default()
                    }
                })
                .collect();
            let rs = query(
                s,
                &format!(
                    "select index_name, non_unique, column_name from information_schema.statistics \
                     where table_schema = {sch} and table_name = {tab} order by index_name = 'PRIMARY' desc, index_name, seq_in_index"
                ),
            )
            .await?;
            d.primary_key = rs.rows.iter().filter(|r| text(&r[0]) == "PRIMARY").map(|r| text(&r[2])).collect();
            d.indexes = group_indexes(
                rs.rows.iter().filter(|r| text(&r[0]) != "PRIMARY").map(|r| (text(&r[0]), text(&r[1]) == "0", r[2].clone(), false)),
            );
        }
        Driver::ClickHouse => {
            let rs = query(
                s,
                &format!(
                    "select name, type, startsWith(type, 'Nullable('), default_kind, default_expression, comment, is_in_primary_key \
                     from system.columns where database = {sch} and table = {tab} order by position"
                ),
            )
            .await?;
            d.columns = rs
                .rows
                .iter()
                .map(|r| {
                    // ALIAS and MATERIALIZED are computed columns (not stored); DEFAULT is a
                    // stored default.
                    let kind = text(&r[3]);
                    let generated = matches!(kind.as_str(), "ALIAS" | "MATERIALIZED");
                    ColumnDef {
                        name: text(&r[0]),
                        data_type: text(&r[1]),
                        nullable: text(&r[2]) == "1",
                        default: if kind == "DEFAULT" && !text(&r[4]).is_empty() { Some(text(&r[4])) } else { None },
                        extra: if generated { kind } else { String::new() },
                        comment: text(&r[5]),
                        generated,
                        ..Default::default()
                    }
                })
                .collect();
            // Data-skipping indexes are not part of this view (they can't be changed here).
            d.primary_key = rs.rows.iter().filter(|r| text(&r[6]) == "1").map(|r| text(&r[0])).collect();
        }
        Driver::MsSql => {
            let obj = lit(&format!("{}.{}", Driver::MsSql.quote_ident(schema), Driver::MsSql.quote_ident(table)));
            let rs = query(
                s,
                &format!(
                    "select c.name, type_name(c.user_type_id) + case \
                     when type_name(c.user_type_id) in ('varchar', 'char', 'varbinary', 'binary') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length as varchar(10)) end + ')' \
                     when type_name(c.user_type_id) in ('nvarchar', 'nchar') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length / 2 as varchar(10)) end + ')' \
                     when type_name(c.user_type_id) in ('decimal', 'numeric') then '(' + cast(c.precision as varchar(10)) + ',' + cast(c.scale as varchar(10)) + ')' \
                     when type_name(c.user_type_id) in ('datetime2', 'time', 'datetimeoffset') then '(' + cast(c.scale as varchar(10)) + ')' \
                     else '' end, \
                     case when c.is_nullable = 1 then '1' else '0' end, dc.definition, dc.name, \
                     case when c.is_computed = 1 then '1' else '0' end \
                     from sys.columns c left join sys.default_constraints dc on dc.object_id = c.default_object_id \
                     where c.object_id = object_id({obj}) order by c.column_id"
                ),
            )
            .await?;
            d.columns = rs
                .rows
                .iter()
                .map(|r| ColumnDef {
                    name: text(&r[0]),
                    data_type: text(&r[1]),
                    nullable: flag(&r[2]),
                    default: r[3].clone(),
                    default_constraint: r[4].clone(),
                    generated: flag(&r[5]),
                    ..Default::default()
                })
                .collect();
            let rs = query(
                s,
                &format!(
                    "select kc.name, c.name from sys.key_constraints kc \
                     join sys.index_columns ic on ic.object_id = kc.parent_object_id and ic.index_id = kc.unique_index_id \
                     join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
                     where kc.parent_object_id = object_id({obj}) and kc.type = 'PK' order by ic.key_ordinal"
                ),
            )
            .await?;
            d.pk_name = rs.rows.first().map(|r| text(&r[0]));
            d.primary_key = rs.rows.iter().map(|r| text(&r[1])).collect();
            let rs = query(
                s,
                &format!(
                    "select i.name, case when i.is_unique = 1 then '1' else '0' end, c.name, \
                     case when i.is_unique_constraint = 1 then '1' else '0' end, case when i.has_filter = 1 then '0' else '1' end \
                     from sys.indexes i join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
                     join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
                     where i.object_id = object_id({obj}) and i.is_primary_key = 0 and i.type > 0 and ic.is_included_column = 0 \
                     order by i.name, ic.key_ordinal"
                ),
            )
            .await?;
            let filtered: Vec<String> = rs.rows.iter().filter(|r| !flag(&r[4])).map(|r| text(&r[0])).collect();
            d.indexes = group_indexes(rs.rows.iter().map(|r| (text(&r[0]), flag(&r[1]), r[2].clone(), flag(&r[3]))));
            for x in d.indexes.iter_mut().filter(|x| filtered.contains(&x.name)) {
                x.plain = false;
            }
        }
        Driver::Oracle => {
            let rs = query(
                s,
                &format!(
                    "select column_name, data_type, data_length, char_length, data_precision, data_scale, nullable, data_default, virtual_column \
                     from all_tab_cols where owner = {sch} and table_name = {tab} and hidden_column = 'NO' order by column_id"
                ),
            )
            .await?;
            d.columns = rs
                .rows
                .iter()
                .map(|r| {
                    let generated = text(&r[8]) == "YES";
                    ColumnDef {
                        name: text(&r[0]),
                        data_type: oracle_type(&text(&r[1]), &text(&r[2]), &text(&r[3]), r[4].as_deref(), r[5].as_deref()),
                        nullable: text(&r[6]) == "Y",
                        default: if generated { None } else { r[7].clone().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) },
                        generated,
                        ..Default::default()
                    }
                })
                .collect();
            let rs = query(
                s,
                &format!(
                    "select c.constraint_name, cc.column_name from all_constraints c \
                     join all_cons_columns cc on cc.owner = c.owner and cc.constraint_name = c.constraint_name \
                     where c.owner = {sch} and c.table_name = {tab} and c.constraint_type = 'P' order by cc.position"
                ),
            )
            .await?;
            d.pk_name = rs.rows.first().map(|r| text(&r[0]));
            d.primary_key = rs.rows.iter().map(|r| text(&r[1])).collect();
            let rs = query(
                s,
                &format!(
                    "select i.index_name, case when i.uniqueness = 'UNIQUE' then '1' else '0' end, ic.column_name, \
                     case when exists (select 1 from all_constraints k where k.owner = i.table_owner and k.index_name = i.index_name and k.constraint_type = 'U') then '1' else '0' end, \
                     case when i.index_type like 'FUNCTION%' then '0' else '1' end \
                     from all_indexes i join all_ind_columns ic on ic.index_owner = i.owner and ic.index_name = i.index_name \
                     where i.table_owner = {sch} and i.table_name = {tab} and i.index_type <> 'LOB' \
                     and not exists (select 1 from all_constraints p where p.owner = i.table_owner and p.index_name = i.index_name and p.constraint_type = 'P') \
                     order by i.index_name, ic.column_position"
                ),
            )
            .await?;
            let functional: Vec<String> = rs.rows.iter().filter(|r| !flag(&r[4])).map(|r| text(&r[0])).collect();
            d.indexes = group_indexes(rs.rows.iter().map(|r| (text(&r[0]), flag(&r[1]), r[2].clone(), flag(&r[3]))));
            for x in d.indexes.iter_mut().filter(|x| functional.contains(&x.name)) {
                x.plain = false;
            }
        }
        Driver::Snowflake => {
            let rs = query(
                s,
                &format!(
                    "select column_name, data_type, is_nullable, column_default, numeric_precision, numeric_scale, character_maximum_length \
                     from information_schema.columns where table_schema = {sch} and table_name = {tab} order by ordinal_position"
                ),
            )
            .await?;
            d.columns = rs
                .rows
                .iter()
                .map(|r| ColumnDef {
                    name: text(&r[0]),
                    data_type: snowflake_type(&text(&r[1]), r[4].as_deref(), r[5].as_deref(), r[6].as_deref()),
                    nullable: text(&r[2]).eq_ignore_ascii_case("YES"),
                    default: r[3].clone(),
                    ..Default::default()
                })
                .collect();
            let rs = query(s, &format!("show primary keys in table {}", Session::quote_table(driver, schema, table))).await?;
            let col = |name: &str| rs.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
            if let (Some(c), Some(seq)) = (col("column_name"), col("key_sequence")) {
                let mut keys: Vec<(u32, String)> = rs.rows.iter().map(|r| (text(&r[seq]).parse().unwrap_or(0), text(&r[c]))).collect();
                keys.sort();
                d.primary_key = keys.into_iter().map(|(_, c)| c).collect();
                d.pk_name = col("constraint_name").and_then(|i| rs.rows.first().map(|r| text(&r[i])));
            }
        }
        Driver::Sqlite => {
            d.version = query(s, "select sqlite_version()").await?.rows.first().map(|r| text(&r[0])).unwrap_or_default();
            let rs = query(s, &format!("select name, type, \"notnull\", dflt_value, pk from pragma_table_info({tab})")).await?;
            let mut pk: Vec<(u32, String)> = vec![];
            for r in &rs.rows {
                let pos: u32 = text(&r[4]).parse().unwrap_or(0);
                if pos > 0 {
                    pk.push((pos, text(&r[0])));
                }
                d.columns.push(ColumnDef {
                    name: text(&r[0]),
                    data_type: text(&r[1]),
                    nullable: !flag(&r[2]),
                    default: r[3].clone(),
                    ..Default::default()
                });
            }
            pk.sort();
            d.primary_key = pk.into_iter().map(|(_, c)| c).collect();
            let rs = query(
                s,
                &format!(
                    "select il.name, il.\"unique\", ii.name, case when il.partial then '0' else '1' end \
                     from pragma_index_list({tab}) il join pragma_index_info(il.name) ii \
                     where il.origin = 'c' order by il.name, ii.seqno"
                ),
            )
            .await?;
            let partial: Vec<String> = rs.rows.iter().filter(|r| !flag(&r[3])).map(|r| text(&r[0])).collect();
            d.indexes = group_indexes(rs.rows.iter().map(|r| (text(&r[0]), flag(&r[1]), r[2].clone(), false)));
            for x in d.indexes.iter_mut().filter(|x| partial.contains(&x.name)) {
                x.plain = false;
            }
            let create = query(s, &format!("select sql from sqlite_master where type = 'table' and name = {tab}"))
                .await?
                .rows
                .first()
                .map(|r| text(&r[0]))
                .unwrap_or_default();
            let triggers = query(s, &format!("select count(*) from sqlite_master where type = 'trigger' and tbl_name = {tab}")).await?;
            let referenced = query(
                s,
                &format!(
                    "select count(*) from sqlite_master m join pragma_foreign_key_list(m.name) f \
                     where m.type = 'table' and m.name <> {tab} and lower(f.\"table\") = lower({tab})"
                ),
            )
            .await?;
            let count = |rs: &ResultSet| rs.rows.first().map(|r| text(&r[0])).unwrap_or_default() != "0";
            let lower = create.to_ascii_lowercase();
            d.autoincrement = lower.contains("autoincrement");
            let constraints = ["check", "references", "foreign", "unique", "generated", "collate", "without rowid", "on conflict"];
            d.rebuild_blocker = (constraints.iter().any(|w| lower.contains(w)) || count(&triggers) || count(&referenced)).then(|| {
                tr("SQLite can only make this change by rebuilding the table, and this table has constraints, triggers or foreign keys that a rebuild would lose. Use a console instead.").to_string()
            });
        }
        Driver::Redis | Driver::MongoDb => anyhow::bail!(tr("Not available for this engine")),
    }
    if d.columns.is_empty() {
        anyhow::bail!(trf("Table “{name}” not found", &[("name", &table)]));
    }
    Ok(d)
}

/// Oracle column type from ALL_TAB_COLUMNS.
fn oracle_type(ty: &str, length: &str, char_length: &str, precision: Option<&str>, scale: Option<&str>) -> String {
    let ty = ty.to_ascii_lowercase();
    match ty.as_str() {
        "varchar2" | "nvarchar2" | "char" | "nchar" => format!("{ty}({char_length})"),
        "raw" => format!("raw({length})"),
        "number" => match (precision, scale) {
            (Some(p), Some("0")) => format!("number({p})"),
            (Some(p), Some(s)) => format!("number({p},{s})"),
            (None, Some("0")) => "integer".into(),
            _ => "number".into(),
        },
        "float" => precision.map_or("float".into(), |p| format!("float({p})")),
        _ => ty,
    }
}

/// Snowflake column type from INFORMATION_SCHEMA.COLUMNS.
fn snowflake_type(ty: &str, precision: Option<&str>, scale: Option<&str>, length: Option<&str>) -> String {
    match ty.to_ascii_uppercase().as_str() {
        "NUMBER" => format!("number({},{})", precision.unwrap_or("38"), scale.unwrap_or("0")),
        "TEXT" => length.map_or("varchar".into(), |l| format!("varchar({l})")),
        other => other.to_ascii_lowercase(),
    }
}

/// `major.minor.patch` at the start of a version string.
fn version_at_least(version: &str, want: (u32, u32, u32)) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().unwrap_or(0));
    let v = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    v >= want
}

/// MySQL 8.0+ and MariaDB 10.5+ have RENAME COLUMN (older ones only CHANGE).
fn mysql_has_rename_column(version: &str) -> bool {
    if version.to_ascii_lowercase().contains("mariadb") { version_at_least(version, (10, 5, 0)) } else { version_at_least(version, (8, 0, 0)) }
}

// ---------------------------------------------------------------------------------------------
// Diff

struct Changed<'a> {
    old: &'a ColumnDef,
    new: &'a NewColumn,
    /// New (effective) name.
    name: String,
    renamed: bool,
    ty: bool,
    null: bool,
    default: bool,
}

struct Diff<'a> {
    dropped: Vec<&'a ColumnDef>,
    /// Existing columns that are renamed or redefined.
    kept: Vec<Changed<'a>>,
    added: Vec<(String, &'a NewColumn)>,
    pk_changed: bool,
    /// New primary key, by new names.
    new_pk: Vec<String>,
    dropped_indexes: Vec<&'a IndexDef>,
    /// (name, columns by new names, unique).
    added_indexes: Vec<(String, Vec<String>, bool)>,
    /// New table name, if renamed.
    new_name: Option<String>,
}

fn norm_type(t: &str) -> String {
    t.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase()
}

fn validate(edit: &TableEdit) -> Result<(), String> {
    if edit.new_name.trim().is_empty() {
        return Err(tr("Name is required").into());
    }
    if edit.columns.is_empty() {
        return Err(tr("Add at least one column").into());
    }
    let mut seen = std::collections::HashSet::new();
    for c in edit.columns.iter().map(|c| &c.column) {
        let name = c.name.trim();
        if name.is_empty() {
            return Err(tr("Every column needs a name").into());
        }
        if c.data_type.trim().is_empty() {
            return Err(trf("Column “{name}” needs a type", &[("name", &name)]));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(trf("Duplicate column “{name}”", &[("name", &name)]));
        }
    }
    let mut names = std::collections::HashSet::new();
    for x in edit.indexes {
        let name = x.name.trim();
        if name.is_empty() {
            return Err(tr("Every index needs a name").into());
        }
        if !names.insert(name.to_lowercase()) {
            return Err(trf("Duplicate index “{name}”", &[("name", &name)]));
        }
        if x.original.is_none() {
            let cols = split_columns(&x.columns);
            if cols.is_empty() {
                return Err(trf("Index “{name}” needs at least one column", &[("name", &name)]));
            }
            for c in cols {
                if !seen.contains(&c.to_lowercase()) {
                    return Err(trf("Index “{name}”: unknown column “{column}”", &[("name", &name), ("column", &c)]));
                }
            }
        }
    }
    Ok(())
}

fn split_columns(s: &str) -> Vec<String> {
    s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
}

fn diff<'a>(driver: Driver, d: &'a TableDetails, edit: &'a TableEdit) -> Diff<'a> {
    let eff = |n: &str| effective_name(driver, n);
    // A name left as loaded stays as is (Oracle: a quoted lowercase name is not uppercased).
    let col_name = |e: &EditColumn| match e.original.and_then(|i| d.columns.get(i)) {
        Some(old) if old.name == e.column.name.trim() => old.name.clone(),
        _ => eff(&e.column.name),
    };
    let kept_originals: Vec<usize> = edit.columns.iter().filter_map(|c| c.original).collect();
    let dropped = d.columns.iter().enumerate().filter(|(i, _)| !kept_originals.contains(i)).map(|(_, c)| c).collect();
    let mut kept = vec![];
    let mut added = vec![];
    for e in edit.columns {
        let new = &e.column;
        let name = col_name(e);
        match e.original.and_then(|i| d.columns.get(i)) {
            Some(old) => {
                let c = Changed {
                    old,
                    new,
                    renamed: name != old.name,
                    ty: norm_type(&new.data_type) != norm_type(&old.data_type),
                    null: (new.not_null || new.primary_key) == old.nullable,
                    default: new.default.trim() != old.default.as_deref().unwrap_or("").trim(),
                    name,
                };
                if c.renamed || c.ty || c.null || c.default {
                    kept.push(c);
                }
            }
            None => added.push((name, new)),
        }
    }
    // The primary key changes when its set of (original) columns does.
    let mut old_pk: Vec<&str> = d.primary_key.iter().map(String::as_str).collect();
    let mut new_pk_orig: Vec<Option<&str>> = edit
        .columns
        .iter()
        .filter(|c| c.column.primary_key)
        .map(|c| c.original.and_then(|i| d.columns.get(i)).map(|o| o.name.as_str()))
        .collect();
    old_pk.sort_unstable();
    new_pk_orig.sort_unstable();
    let pk_changed = new_pk_orig.iter().any(Option::is_none) || new_pk_orig.iter().map(|o| o.unwrap_or_default()).collect::<Vec<_>>() != old_pk;
    let new_pk = edit.columns.iter().filter(|c| c.column.primary_key).map(col_name).collect();

    // Indexes removed from the list, or on a column that is dropped.
    let kept_indexes: Vec<usize> = edit.indexes.iter().filter_map(|x| x.original).collect();
    let dropped_indexes = d
        .indexes
        .iter()
        .enumerate()
        .filter(|(i, x)| !kept_indexes.contains(i) || x.columns.iter().any(|c| d.columns.iter().zip(0..).any(|(o, j)| &o.name == c && !kept_originals.contains(&j))))
        .map(|(_, x)| x)
        .collect();
    // Index columns are typed with the new column names (in any case).
    let column_name = |typed: &str| {
        edit.columns.iter().find(|c| c.column.name.trim().eq_ignore_ascii_case(typed)).map(col_name).unwrap_or_else(|| typed.to_string())
    };
    let added_indexes = edit
        .indexes
        .iter()
        .filter(|x| x.original.is_none())
        .map(|x| (eff(&x.name), split_columns(&x.columns).iter().map(|c| column_name(c)).collect(), x.unique))
        .collect();
    let new_name = (edit.new_name.trim() != edit.table).then(|| eff(edit.new_name)).filter(|n| n != edit.table);
    Diff { dropped, kept, added, pk_changed, new_pk, dropped_indexes, added_indexes, new_name }
}

/// Statements that turn the loaded structure `d` into the edited one.
pub fn plan(driver: Driver, d: &TableDetails, edit: &TableEdit) -> Result<Plan, String> {
    validate(edit)?;
    let diff = diff(driver, d, edit);
    for c in &diff.kept {
        if c.old.generated && (c.ty || c.null || c.default) {
            return Err(trf("“{name}” is a generated column: it can only be renamed or dropped", &[("name", &c.old.name)]));
        }
    }
    let mut warnings: Vec<String> = diff.dropped.iter().map(|c| trf("Drops column “{name}” and all its data", &[("name", &c.name)])).collect();
    warnings.extend(
        diff.kept
            .iter()
            .filter(|c| c.ty)
            .map(|c| trf("Changing the type of “{name}” can truncate or lose data, or fail on existing rows", &[("name", &c.old.name)])),
    );
    let sqlite_rebuild = driver == Driver::Sqlite
        && (diff.pk_changed
            || diff.kept.iter().any(|c| c.ty || c.null || c.default)
            || (!diff.dropped.is_empty() && !version_at_least(&d.version, (3, 35, 0))));
    let mut plan = Plan { statements: vec![], warnings, transaction: false };
    if sqlite_rebuild {
        if let Some(why) = &d.rebuild_blocker {
            return Err(why.clone());
        }
        plan.statements = sqlite_rebuild_sql(d, edit, &diff)?;
        plan.warnings.push(tr("SQLite can't alter columns in place: the table is copied into a new one and the old one dropped, in one transaction.").into());
    } else {
        plan.statements = alter_sql(driver, d, edit, &diff)?;
    }
    plan.transaction = matches!(driver, Driver::Postgres | Driver::Sqlite | Driver::MsSql) && plan.statements.len() > 1;
    if plan.statements.is_empty() {
        plan.warnings.clear();
    }
    Ok(plan)
}

/// `type [default x] [not null]` (DEFAULT before NOT NULL: Oracle requires that order).
fn column_def(c: &NewColumn) -> String {
    let mut s = c.data_type.trim().to_string();
    if !c.default.trim().is_empty() {
        s.push_str(&format!(" default {}", c.default.trim()));
    }
    if c.not_null || c.primary_key {
        s.push_str(" not null");
    }
    s
}

fn alter_sql(driver: Driver, d: &TableDetails, edit: &TableEdit, diff: &Diff) -> Result<Vec<String>, String> {
    let q = |s: &str| quote_ident_if_needed(driver, s);
    let schema = edit.schema;
    let t = if driver == Driver::Sqlite || schema.is_empty() { q(edit.table) } else { format!("{}.{}", q(schema), q(edit.table)) };
    let in_schema = |n: &str| if driver == Driver::Sqlite || schema.is_empty() { q(n) } else { format!("{}.{}", q(schema), q(n)) };
    let cols = |v: &[String]| v.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let had_pk = !d.primary_key.is_empty();
    let mut out = vec![];
    let alter = |s: String| format!("alter table {t} {s}");

    if driver == Driver::ClickHouse && (diff.pk_changed || !diff.added_indexes.is_empty() || !diff.dropped_indexes.is_empty()) {
        return Err(tr("ClickHouse fixes the primary key and indexes when the table is created; change them from a SQL console").into());
    }
    if driver == Driver::MySql || driver == Driver::MariaDb {
        // One ALTER TABLE: MySQL applies its clauses together (an auto_increment key can move).
        let mut clauses = vec![];
        if diff.pk_changed && had_pk {
            clauses.push("drop primary key".to_string());
        }
        for x in &diff.dropped_indexes {
            clauses.push(format!("drop index {}", q(&x.name)));
        }
        for c in &diff.dropped {
            clauses.push(format!("drop column {}", q(&c.name)));
        }
        let rename_column = mysql_has_rename_column(&d.version);
        for c in &diff.kept {
            let redefined = c.ty || c.null || c.default;
            if c.renamed && !redefined && (rename_column || c.old.generated) {
                clauses.push(format!("rename column {} to {}", q(&c.old.name), q(&c.name)));
                continue;
            }
            let def = column_def_mysql(c.new, c.old, !c.ty);
            if c.renamed {
                clauses.push(format!("change column {} {} {def}", q(&c.old.name), q(&c.name)));
            } else {
                clauses.push(format!("modify column {} {def}", q(&c.name)));
            }
        }
        for (name, c) in &diff.added {
            clauses.push(format!("add column {} {}", q(name), column_def(c)));
        }
        if diff.pk_changed && !diff.new_pk.is_empty() {
            clauses.push(format!("add primary key ({})", cols(&diff.new_pk)));
        }
        for (name, columns, unique) in &diff.added_indexes {
            clauses.push(format!("add {}index {} ({})", if *unique { "unique " } else { "" }, q(name), cols(columns)));
        }
        if !clauses.is_empty() {
            out.push(format!("alter table {t}\n    {}", clauses.join(",\n    ")));
        }
    } else {
        // 1. Primary key and indexes that go away, and dropped columns.
        if diff.pk_changed && had_pk {
            out.push(match driver {
                Driver::Postgres | Driver::MsSql => {
                    let name = d.pk_name.clone().unwrap_or_else(|| format!("{}_pkey", edit.table));
                    alter(format!("drop constraint {}", q(&name)))
                }
                _ => alter("drop primary key".into()),
            });
        }
        for x in &diff.dropped_indexes {
            out.push(match driver {
                _ if x.constraint => alter(format!("drop constraint {}", q(&x.name))),
                Driver::MsSql => format!("drop index {} on {t}", q(&x.name)),
                _ => format!("drop index {}", in_schema(&x.name)),
            });
        }
        for c in &diff.dropped {
            if let Some(df) = c.default_constraint.as_deref().filter(|_| driver == Driver::MsSql) {
                out.push(alter(format!("drop constraint {}", q(df))));
            }
            out.push(alter(format!("drop column {}", q(&c.name))));
        }
        // 2. Renames, then the new definitions (by the new names).
        for c in diff.kept.iter().filter(|c| c.renamed) {
            out.push(match driver {
                Driver::MsSql => {
                    let old = format!("{}.{}.{}", q(schema), q(edit.table), q(&c.old.name)).replace('\'', "''");
                    format!("exec sp_rename '{old}', '{}', 'COLUMN'", c.name.replace('\'', "''"))
                }
                _ => alter(format!("rename column {} to {}", q(&c.old.name), q(&c.name))),
            });
        }
        for c in &diff.kept {
            let col = q(&c.name);
            let not_null = c.new.not_null || c.new.primary_key;
            let default = c.new.default.trim();
            match driver {
                Driver::Postgres => {
                    if c.ty {
                        let ty = c.new.data_type.trim();
                        out.push(alter(format!("alter column {col} type {ty} using {col}::{ty}")));
                    }
                    if c.null {
                        out.push(alter(format!("alter column {col} {} not null", if not_null { "set" } else { "drop" })));
                    }
                    if c.default {
                        out.push(alter(if default.is_empty() { format!("alter column {col} drop default") } else { format!("alter column {col} set default {default}") }));
                    }
                }
                Driver::MsSql => {
                    // The default constraint blocks ALTER COLUMN: drop it and add it back.
                    let df = c.old.default_constraint.as_deref();
                    let redo_default = c.default || (c.ty && df.is_some());
                    if redo_default && let Some(df) = df {
                        out.push(alter(format!("drop constraint {}", q(df))));
                    }
                    if c.ty || c.null {
                        out.push(alter(format!("alter column {col} {} {}", c.new.data_type.trim(), if not_null { "not null" } else { "null" })));
                    }
                    if redo_default && !default.is_empty() {
                        out.push(alter(format!("add default {default} for {col}")));
                    }
                }
                Driver::Oracle => {
                    let mut spec = col.clone();
                    if c.ty {
                        spec.push_str(&format!(" {}", c.new.data_type.trim()));
                    }
                    if c.default {
                        spec.push_str(&format!(" default {}", if default.is_empty() { "null" } else { default }));
                    }
                    if c.null {
                        spec.push_str(if not_null { " not null" } else { " null" });
                    }
                    if c.ty || c.default || c.null {
                        out.push(alter(format!("modify ({spec})")));
                    }
                }
                Driver::Snowflake => {
                    if c.ty {
                        out.push(alter(format!("alter column {col} set data type {}", c.new.data_type.trim())));
                    }
                    if c.null {
                        out.push(alter(format!("alter column {col} {} not null", if not_null { "set" } else { "drop" })));
                    }
                    if c.default {
                        if !default.is_empty() {
                            return Err(tr("Snowflake can only remove a column's default, not set a new one").into());
                        }
                        out.push(alter(format!("alter column {col} drop default")));
                    }
                }
                // Nullability is part of the type (`Nullable(T)`), so it changes with it; the
                // DEFAULT goes in the same MODIFY (the server needs it to fill NULLs when the
                // column becomes NOT NULL).
                Driver::ClickHouse => {
                    let default_clause = if default.is_empty() { String::new() } else { format!(" default {default}") };
                    if c.ty || c.null {
                        out.push(alter(format!("modify column {col} {}{default_clause}", crate::db::clickhouse::column_type(&c.new.data_type, not_null))));
                    } else if c.default && !default.is_empty() {
                        out.push(alter(format!("modify column {col}{default_clause}")));
                    }
                    if c.default && default.is_empty() {
                        out.push(alter(format!("modify column {col} remove default")));
                    }
                }
                // SQLite: only renames get here (the rest rebuilds the table).
                _ => {}
            }
        }
        // 3. New columns, primary key and indexes.
        for (name, c) in &diff.added {
            out.push(match driver {
                Driver::Oracle => alter(format!("add ({} {})", q(name), column_def(c))),
                Driver::MsSql => alter(format!("add {} {}", q(name), column_def(c))),
                Driver::ClickHouse => {
                    let ty = crate::db::clickhouse::column_type(&c.data_type, c.not_null || c.primary_key);
                    let default = c.default.trim();
                    alter(format!("add column {} {ty}{}", q(name), if default.is_empty() { String::new() } else { format!(" default {default}") }))
                }
                _ => alter(format!("add column {} {}", q(name), column_def(c))),
            });
        }
        if diff.pk_changed && !diff.new_pk.is_empty() {
            out.push(alter(format!("add primary key ({})", cols(&diff.new_pk))));
        }
        for (name, columns, unique) in &diff.added_indexes {
            let name = if driver == Driver::Oracle { in_schema(name) } else { q(name) };
            out.push(format!("create {}index {name} on {t} ({})", if *unique { "unique " } else { "" }, cols(columns)));
        }
    }
    // 4. The table's new name, last (everything above uses the old one).
    if diff.new_name.is_some() {
        out.push(object_sql(driver, DropKind::Rename, schema, edit.table, edit.new_name));
    }
    Ok(out)
}

/// MySQL column definition for CHANGE / MODIFY: the type, NOT NULL, default and the attributes
/// it would otherwise lose (auto_increment, on update, comment, collation).
fn column_def_mysql(c: &NewColumn, old: &ColumnDef, same_type: bool) -> String {
    let mut s = c.data_type.trim().to_string();
    if same_type && let Some(coll) = &old.collation {
        s.push_str(&format!(" collate {coll}"));
    }
    s.push_str(if c.not_null || c.primary_key { " not null" } else { " null" });
    if !c.default.trim().is_empty() {
        s.push_str(&format!(" default {}", c.default.trim()));
    }
    if !old.extra.is_empty() {
        s.push_str(&format!(" {}", old.extra));
    }
    if !old.comment.is_empty() {
        s.push_str(&format!(" comment {}", lit(&old.comment).replace('\\', "\\\\")));
    }
    s
}

/// SQLite's "12-step" rebuild: a new table with the edited definition, the rows copied, the old
/// table dropped and the new one renamed, then its indexes created again.
fn sqlite_rebuild_sql(d: &TableDetails, edit: &TableEdit, diff: &Diff) -> Result<Vec<String>, String> {
    let q = |s: &str| quote_ident_if_needed(Driver::Sqlite, s);
    let final_name = diff.new_name.clone().unwrap_or_else(|| edit.table.to_string());
    let tmp = format!("_dc_new_{final_name}");
    let pk: Vec<&NewColumn> = edit.columns.iter().map(|c| &c.column).filter(|c| c.primary_key).collect();
    let inline_pk = d.autoincrement && pk.len() == 1 && pk[0].data_type.trim().eq_ignore_ascii_case("integer");
    let mut lines: Vec<String> = edit
        .columns
        .iter()
        .map(|e| {
            let c = &e.column;
            let mut l = format!("    {} {}", q(c.name.trim()), column_def(c));
            if inline_pk && c.primary_key {
                l.push_str(" primary key autoincrement");
            }
            l
        })
        .collect();
    if !pk.is_empty() && !inline_pk {
        lines.push(format!("    primary key ({})", pk.iter().map(|c| q(c.name.trim())).collect::<Vec<_>>().join(", ")));
    }
    let mut out = vec![format!("create table {}\n(\n{}\n)", q(&tmp), lines.join(",\n"))];
    let copied: Vec<(&str, String)> =
        edit.columns.iter().filter_map(|e| e.original.and_then(|i| d.columns.get(i)).map(|o| (o.name.as_str(), q(e.column.name.trim())))).collect();
    if !copied.is_empty() {
        let to = copied.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>().join(", ");
        let from = copied.iter().map(|(o, _)| q(o)).collect::<Vec<_>>().join(", ");
        out.push(format!("insert into {} ({to}) select {from} from {}", q(&tmp), q(edit.table)));
    }
    out.push(format!("drop table {}", q(edit.table)));
    out.push(format!("alter table {} rename to {}", q(&tmp), q(&final_name)));
    // Kept indexes, with their columns renamed; those on a dropped column go with it.
    let renamed = |old: &str| {
        edit.columns.iter().find(|e| e.original.and_then(|i| d.columns.get(i)).is_some_and(|o| o.name == old)).map(|e| e.column.name.trim().to_string())
    };
    let mut indexes: Vec<(String, Vec<String>, bool)> = vec![];
    for x in edit.indexes.iter().filter_map(|x| x.original.and_then(|i| d.indexes.get(i))) {
        if !x.plain {
            return Err(trf("Index “{name}” can't be rebuilt (expression or partial index): drop it first", &[("name", &x.name)]));
        }
        let cols: Option<Vec<String>> = x.columns.iter().map(|c| renamed(c)).collect();
        if let Some(cols) = cols {
            indexes.push((x.name.clone(), cols, x.unique));
        }
    }
    indexes.extend(diff.added_indexes.iter().cloned());
    for (name, cols, unique) in indexes {
        let cols = cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        out.push(format!("create {}index {} on {} ({cols})", if unique { "unique " } else { "" }, q(&name), q(&final_name)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, ty: &str, nullable: bool, default: Option<&str>) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, default: default.map(String::from), ..Default::default() }
    }

    /// `items(id int pk, name varchar(50) not null, price int default 0, note text)` with an
    /// index on name.
    fn details(int: &str, text: &str) -> TableDetails {
        TableDetails {
            columns: vec![
                def("id", int, false, None),
                def("name", "varchar(50)", false, None),
                def("price", int, true, Some("0")),
                def("note", text, true, None),
            ],
            primary_key: vec!["id".into()],
            pk_name: Some("items_pkey".into()),
            indexes: vec![IndexDef { name: "items_name".into(), columns: vec!["name".into()], unique: false, constraint: false, plain: true }],
            version: String::new(),
            rebuild_blocker: None,
            autoincrement: false,
        }
    }

    /// The same edit on every engine: rename name → title (and widen it), price nullable → not
    /// null with default 1 (was 0), drop note, add email, primary key (id) → (id, title), drop the
    /// name index, add a unique index on email, rename the table to goods.
    fn edit_all(d: &TableDetails) -> (Vec<EditColumn>, Vec<EditIndex>) {
        let mut cols = initial_columns(d);
        cols[1].column.name = "title".into();
        cols[1].column.data_type = "varchar(100)".into();
        cols[1].column.primary_key = true;
        cols[2].column.not_null = true;
        cols[2].column.default = "1".into();
        cols.remove(3);
        cols.push(EditColumn {
            original: None,
            column: NewColumn { name: "email".into(), data_type: "varchar(255)".into(), not_null: false, primary_key: false, default: String::new() },
        });
        let indexes = vec![EditIndex { original: None, name: "goods_email".into(), columns: "EMAIL".into(), unique: true }];
        (cols, indexes)
    }

    fn run(driver: Driver, schema: &str, d: &TableDetails) -> Plan {
        let (cols, idx) = edit_all(d);
        let table = if matches!(driver, Driver::Oracle | Driver::Snowflake) { "ITEMS" } else { "items" };
        plan(driver, d, &TableEdit { schema, table, new_name: "goods", columns: &cols, indexes: &idx }).unwrap()
    }

    #[test]
    fn no_changes_no_statements() {
        let d = details("integer", "text");
        let cols = initial_columns(&d);
        let idx = initial_indexes(&d);
        for driver in [Driver::Postgres, Driver::MySql, Driver::MsSql, Driver::Oracle, Driver::Snowflake, Driver::Sqlite] {
            let p = plan(driver, &d, &TableEdit { schema: "s", table: "items", new_name: "items", columns: &cols, indexes: &idx }).unwrap();
            assert_eq!(p, Plan::default(), "{driver:?}");
        }
    }

    #[test]
    fn postgres() {
        let p = run(Driver::Postgres, "public", &details("integer", "text"));
        assert_eq!(
            p.statements,
            [
                "alter table public.items drop constraint items_pkey",
                "drop index public.items_name",
                "alter table public.items drop column note",
                "alter table public.items rename column name to title",
                "alter table public.items alter column title type varchar(100) using title::varchar(100)",
                "alter table public.items alter column price set not null",
                "alter table public.items alter column price set default 1",
                "alter table public.items add column email varchar(255)",
                "alter table public.items add primary key (id, title)",
                "create unique index goods_email on public.items (email)",
                "alter table public.items rename to goods",
            ]
        );
        assert!(p.transaction);
        assert_eq!(p.warnings.len(), 2, "drop + type change: {:?}", p.warnings);
    }

    #[test]
    fn postgres_drop_not_null_and_default() {
        let d = details("integer", "text");
        let mut cols = initial_columns(&d);
        cols[1].column.not_null = false;
        cols[2].column.default = String::new();
        let p = plan(Driver::Postgres, &d, &TableEdit { schema: "public", table: "items", new_name: "items", columns: &cols, indexes: &initial_indexes(&d) }).unwrap();
        assert_eq!(p.statements, ["alter table public.items alter column name drop not null", "alter table public.items alter column price drop default"]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn mysql_one_alter_table() {
        let mut d = details("int(11)", "text");
        d.version = "10.11.6-MariaDB".into();
        d.columns[0].extra = "auto_increment".into();
        d.columns[1].collation = Some("utf8mb4_general_ci".into());
        let p = run(Driver::MariaDb, "shop", &d);
        assert_eq!(
            p.statements,
            [
                "alter table shop.items\n    drop primary key,\n    drop index items_name,\n    drop column note,\n    \
                 change column name title varchar(100) not null,\n    modify column price int(11) not null default 1,\n    \
                 add column email varchar(255),\n    add primary key (id, title),\n    add unique index goods_email (email)",
                "rename table shop.items to shop.goods",
            ]
        );
        assert!(!p.transaction);
    }

    #[test]
    fn mysql_rename_keeps_attributes() {
        let mut d = details("int", "text");
        d.columns[0].extra = "auto_increment".into();
        d.columns[1].comment = "it's".into();
        d.columns[1].collation = Some("utf8mb4_bin".into());
        let mut cols = initial_columns(&d);
        cols[0].column.name = "item_id".into();
        cols[1].column.name = "title".into();
        let edit = |d: &TableDetails, cols: &[EditColumn]| plan(Driver::MySql, d, &TableEdit { schema: "shop", table: "items", new_name: "items", columns: cols, indexes: &initial_indexes(d) });
        // MySQL 5.7: no RENAME COLUMN, so CHANGE with the full definition.
        d.version = "5.7.44".into();
        let p = edit(&d, &cols).unwrap();
        assert_eq!(
            p.statements,
            ["alter table shop.items\n    change column id item_id int not null auto_increment,\n    \
              change column name title varchar(50) collate utf8mb4_bin not null comment 'it''s'"]
        );
        d.version = "8.0.36".into();
        let p = edit(&d, &cols).unwrap();
        assert_eq!(p.statements, ["alter table shop.items\n    rename column id to item_id,\n    rename column name to title"]);
        // Making a column nullable says so explicitly.
        cols[1].column.not_null = false;
        let p = edit(&d, &cols).unwrap();
        assert!(p.statements[0].contains("change column name title varchar(50) collate utf8mb4_bin null comment"), "{}", p.statements[0]);
    }

    #[test]
    fn sql_server() {
        let mut d = details("int", "nvarchar(max)");
        d.pk_name = Some("PK__items__3213E83F".into());
        d.columns[2].default_constraint = Some("DF__items__price".into());
        d.columns[3].default_constraint = Some("DF__items__note".into());
        let p = run(Driver::MsSql, "dbo", &d);
        assert_eq!(
            p.statements,
            [
                "alter table dbo.items drop constraint PK__items__3213E83F",
                "drop index items_name on dbo.items",
                "alter table dbo.items drop constraint DF__items__note",
                "alter table dbo.items drop column note",
                "exec sp_rename 'dbo.items.name', 'title', 'COLUMN'",
                "alter table dbo.items alter column title varchar(100) not null",
                "alter table dbo.items drop constraint DF__items__price",
                "alter table dbo.items alter column price int not null",
                "alter table dbo.items add default 1 for price",
                "alter table dbo.items add email varchar(255)",
                "alter table dbo.items add primary key (id, title)",
                "create unique index goods_email on dbo.items (email)",
                "exec sp_rename 'dbo.items', 'goods'",
            ]
        );
        assert!(p.transaction);
    }

    #[test]
    fn oracle() {
        let mut d = details("NUMBER(10)", "CLOB");
        for c in &mut d.columns {
            c.name = c.name.to_uppercase();
        }
        d.primary_key = vec!["ID".into()];
        d.indexes[0].columns = vec!["NAME".into()];
        let p = run(Driver::Oracle, "HR", &d);
        assert_eq!(
            p.statements,
            [
                "alter table HR.ITEMS drop primary key",
                "drop index HR.items_name",
                "alter table HR.ITEMS drop column NOTE",
                "alter table HR.ITEMS rename column NAME to TITLE",
                "alter table HR.ITEMS modify (TITLE varchar(100))",
                "alter table HR.ITEMS modify (PRICE default 1 not null)",
                "alter table HR.ITEMS add (EMAIL varchar(255))",
                "alter table HR.ITEMS add primary key (ID, TITLE)",
                "create unique index HR.GOODS_EMAIL on HR.ITEMS (EMAIL)",
                "alter table HR.ITEMS rename to GOODS",
            ]
            .map(|s| s.replace("HR.items_name", "HR.\"items_name\""))
        );
        assert!(!p.transaction);
    }

    #[test]
    fn clickhouse() {
        let mut d = details("UInt64", "String");
        d.indexes.clear();
        // The primary key and indexes are fixed when the table is created.
        let (cols, indexes) = edit_all(&d);
        assert!(plan(Driver::ClickHouse, &d, &TableEdit { schema: "db", table: "items", new_name: "goods", columns: &cols, indexes: &indexes }).is_err());
        let mut cols = initial_columns(&d);
        cols[1].column.name = "title".into();
        cols[1].column.data_type = "LowCardinality(String)".into();
        cols[2].column.not_null = true;
        cols[2].column.default = "1".into();
        cols.remove(3);
        cols.push(EditColumn {
            original: None,
            column: NewColumn { name: "email".into(), data_type: "String".into(), not_null: false, primary_key: false, default: String::new() },
        });
        let p = plan(Driver::ClickHouse, &d, &TableEdit { schema: "db", table: "items", new_name: "goods", columns: &cols, indexes: &[] }).unwrap();
        assert_eq!(
            p.statements,
            [
                "alter table db.items drop column note",
                "alter table db.items rename column name to title",
                "alter table db.items modify column title LowCardinality(String)",
                "alter table db.items modify column price UInt64 default 1",
                "alter table db.items add column email String null",
                "rename table db.items to db.goods",
            ]
        );
        assert!(!p.transaction);
        // Removing a default alone.
        let mut cols = initial_columns(&d);
        cols[2].column.default = String::new();
        let p = plan(Driver::ClickHouse, &d, &TableEdit { schema: "db", table: "items", new_name: "items", columns: &cols, indexes: &[] }).unwrap();
        assert_eq!(p.statements, ["alter table db.items modify column price remove default"]);
    }

    #[test]
    fn snowflake() {
        let mut d = details("number(38,0)", "varchar");
        for c in &mut d.columns {
            c.name = c.name.to_uppercase();
        }
        d.primary_key = vec!["ID".into()];
        d.indexes.clear();
        d.columns[2].default = None;
        let (mut cols, _) = edit_all(&d);
        cols[2].column.default = String::new();
        let p = plan(Driver::Snowflake, &d, &TableEdit { schema: "PUBLIC", table: "ITEMS", new_name: "goods", columns: &cols, indexes: &[] }).unwrap();
        assert_eq!(
            p.statements,
            [
                "alter table PUBLIC.ITEMS drop primary key",
                "alter table PUBLIC.ITEMS drop column NOTE",
                "alter table PUBLIC.ITEMS rename column NAME to TITLE",
                "alter table PUBLIC.ITEMS alter column TITLE set data type varchar(100)",
                "alter table PUBLIC.ITEMS alter column PRICE set not null",
                "alter table PUBLIC.ITEMS add column EMAIL varchar(255)",
                "alter table PUBLIC.ITEMS add primary key (ID, TITLE)",
                "alter table PUBLIC.ITEMS rename to PUBLIC.GOODS",
            ]
        );
        // A new default can't be set.
        cols[2].column.default = "1".into();
        assert!(plan(Driver::Snowflake, &d, &TableEdit { schema: "PUBLIC", table: "ITEMS", new_name: "ITEMS", columns: &cols, indexes: &[] }).is_err());
    }

    #[test]
    fn sqlite_simple_changes_alter_in_place() {
        let mut d = details("INTEGER", "TEXT");
        d.version = "3.45.0".into();
        let mut cols = initial_columns(&d);
        cols[1].column.name = "title".into();
        cols.remove(3);
        cols.push(EditColumn { original: None, column: NewColumn { name: "email".into(), data_type: "TEXT".into(), not_null: false, primary_key: false, default: String::new() } });
        let p = plan(Driver::Sqlite, &d, &TableEdit { schema: "main", table: "items", new_name: "goods", columns: &cols, indexes: &[] }).unwrap();
        assert_eq!(
            p.statements,
            [
                "drop index items_name",
                "alter table items drop column note",
                "alter table items rename column name to title",
                "alter table items add column email TEXT",
                "alter table items rename to goods",
            ]
        );
        assert!(p.transaction);
        // Old SQLite has no DROP COLUMN: rebuild.
        d.version = "3.31.1".into();
        let p = plan(Driver::Sqlite, &d, &TableEdit { schema: "main", table: "items", new_name: "goods", columns: &cols, indexes: &[] }).unwrap();
        assert!(p.statements[0].starts_with("create table _dc_new_goods"));
    }

    #[test]
    fn sqlite_rebuild() {
        let mut d = details("INTEGER", "TEXT");
        d.version = "3.45.0".into();
        d.autoincrement = true;
        let p = run(Driver::Sqlite, "main", &d);
        assert_eq!(
            p.statements,
            [
                "create table _dc_new_goods\n(\n    id INTEGER not null,\n    title varchar(100) not null,\n    price INTEGER default 1 not null,\n    \
                 email varchar(255),\n    primary key (id, title)\n)",
                "insert into _dc_new_goods (id, title, price) select id, name, price from items",
                "drop table items",
                "alter table _dc_new_goods rename to goods",
                "create unique index goods_email on goods (email)",
            ]
        );
        assert!(p.transaction);
        assert_eq!(p.warnings.len(), 3);
        // AUTOINCREMENT is kept on a single INTEGER key; kept indexes are created again.
        let mut cols = initial_columns(&d);
        cols[1].column.name = "title".into();
        cols[2].column.not_null = true;
        let p = plan(Driver::Sqlite, &d, &TableEdit { schema: "main", table: "items", new_name: "items", columns: &cols, indexes: &initial_indexes(&d) }).unwrap();
        assert!(p.statements[0].contains("    id INTEGER not null primary key autoincrement,\n"), "{}", p.statements[0]);
        assert!(!p.statements[0].contains("primary key (id)"));
        assert_eq!(p.statements.last().unwrap(), "create index items_name on items (title)");
        // Tables with constraints a rebuild would lose are refused.
        d.rebuild_blocker = Some("no".into());
        assert_eq!(plan(Driver::Sqlite, &d, &TableEdit { schema: "main", table: "items", new_name: "items", columns: &cols, indexes: &[] }), Err("no".into()));
    }

    #[test]
    fn validation_errors() {
        let d = details("integer", "text");
        let mut cols = initial_columns(&d);
        let ok = |cols: &[EditColumn], idx: &[EditIndex]| plan(Driver::Postgres, &d, &TableEdit { schema: "s", table: "items", new_name: "items", columns: cols, indexes: idx });
        let idx = vec![EditIndex { original: None, name: "x".into(), columns: "nope".into(), unique: false }];
        assert!(ok(&cols, &idx).is_err(), "unknown index column");
        cols[1].column.name = "ID".into();
        assert!(ok(&cols, &[]).is_err(), "duplicate column");
        assert!(ok(&[], &[]).is_err(), "no columns");
        let mut d2 = d.clone();
        d2.columns[3].generated = true;
        let mut cols = initial_columns(&d2);
        cols[3].column.data_type = "varchar".into();
        assert!(plan(Driver::Postgres, &d2, &TableEdit { schema: "s", table: "items", new_name: "items", columns: &cols, indexes: &[] }).is_err());
    }

    #[test]
    fn types_and_versions() {
        assert_eq!(oracle_type("NUMBER", "22", "0", Some("10"), Some("2")), "number(10,2)");
        assert_eq!(oracle_type("NUMBER", "22", "0", None, Some("0")), "integer");
        assert_eq!(oracle_type("VARCHAR2", "400", "100", None, None), "varchar2(100)");
        assert_eq!(snowflake_type("TEXT", None, None, Some("16777216")), "varchar(16777216)");
        assert!(mysql_has_rename_column("10.5.1-MariaDB"));
        assert!(!mysql_has_rename_column("10.4.30-MariaDB-log"));
        assert!(mysql_has_rename_column("8.0.36"));
        assert!(!mysql_has_rename_column("5.7.44-log"));
        assert!(version_at_least("3.35.0", (3, 35, 0)));
        assert!(!version_at_least("3.31.1", (3, 35, 0)));
    }
}
