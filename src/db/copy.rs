//! "Copy Table To…" (DataGrip): copies a table's rows (all, a percentage or those matching a
//! filter) to another connection, creating the target table when it doesn't exist. It runs on
//! dedicated sessions, like the schema search, and inserts in one transaction (all or nothing).
//! MongoDB collections can be the source (read with `find` and a JSON filter) or the target
//! (written with `insertMany` in batches, without a transaction); values travel as text and
//! get their BSON type back from the source column's type.

use super::subset::{self, ForeignKey, TableRows};
use super::{Cell, ColumnInfo, ConnectionConfig, Driver, SchemaInfo, Session};
use crate::edit::literal;
use crate::i18n::{tr, trf};
use crate::ui::create_dialog::{NewColumn, create_table_sql};
use std::sync::Arc;
use tokio::sync::Notify;

/// Rows read at most: the copy keeps them in memory before inserting (the texts say 1,000,000).
pub const MAX_ROWS: usize = 1_000_000;
/// Rows per multi-row INSERT (SQL Server allows at most 1000).
const BATCH: usize = 100;
/// Documents per `insertMany` on a MongoDB target.
const MONGO_BATCH: usize = 500;

#[derive(Clone, Debug)]
pub struct CopyRequest {
    pub source_db: String,
    pub source_schema: String,
    pub source_table: String,
    /// Source columns to copy, in order (binary columns are left out by the dialog).
    pub columns: Vec<ColumnInfo>,
    /// Target name of each column (an existing table may spell them differently).
    pub target_names: Vec<String>,
    /// WHERE condition, without the keyword (empty = every row).
    pub filter: String,
    /// Percentage of the (filtered) rows to copy, 1–100.
    pub percent: u8,
    pub target_db: String,
    pub target_schema: String,
    pub target_table: String,
    /// Create the target table first (it doesn't exist).
    pub create: bool,
    /// Delete the target's rows first, in the same transaction.
    pub clear: bool,
    /// Masking rule of each column (`Keep` = copied as is).
    pub masks: Vec<Mask>,
    /// Secret for the deterministic masks (Hash, Email, Name, Phone).
    pub mask_key: String,
    /// Also copy the rows the copied ones reference through foreign keys (and so on).
    pub parents: bool,
    /// Also copy the rows that reference the copied ones (their orders, the orders' lines…);
    /// implies `parents`.
    pub children: bool,
    /// Related tables get the masks suggested by their column names.
    pub mask_related: bool,
}

/// Masking rule for a column. Every rule is deterministic: with the same key, the same value
/// always gives the same masked value, so masked keys still match across tables and copies.
/// NULL stays NULL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mask {
    #[default]
    Keep,
    Null,
    Blank,
    Hash,
    /// Keeps the last 4 characters: `************1234`.
    Partial,
    Email,
    Name,
    Phone,
}

const FIRST_NAMES: [&str; 24] = [
    "Alex", "Sam", "Jordan", "Taylor", "Morgan", "Casey", "Robin", "Jamie", "Avery", "Riley", "Quinn", "Drew", "Elena",
    "Marco", "Yuki", "Amara", "Lucas", "Nadia", "Omar", "Ingrid", "Mateo", "Leila", "Hugo", "Sofia",
];
const LAST_NAMES: [&str; 24] = [
    "Smith", "Garcia", "Muller", "Rossi", "Silva", "Kim", "Novak", "Dubois", "Tanaka", "Jensen", "Lopez", "Martin",
    "Costa", "Weber", "Moreau", "Ivanova", "Okafor", "Nguyen", "Larsen", "Romero", "Fischer", "Sato", "Brown", "Ruiz",
];

impl Mask {
    pub const ALL: [Mask; 8] = [Mask::Keep, Mask::Null, Mask::Blank, Mask::Hash, Mask::Partial, Mask::Email, Mask::Name, Mask::Phone];

    pub fn label(self) -> &'static str {
        match self {
            Mask::Keep => tr("Keep"),
            Mask::Null => tr("Set NULL"),
            Mask::Blank => tr("Empty text"),
            Mask::Hash => tr("Hash"),
            Mask::Partial => tr("Keep last 4"),
            Mask::Email => tr("Fake email"),
            Mask::Name => tr("Fake name"),
            Mask::Phone => tr("Fake phone"),
        }
    }

    /// Rules that fit a column: NULL needs a nullable column that isn't a key, and the others
    /// write text (an empty text can't be a key either).
    pub fn allowed(self, driver: Driver, column: &ColumnInfo) -> bool {
        let text = matches!(classify(driver, &column.data_type), Kind::Varchar(_) | Kind::Text);
        // MongoDB's `_id` must stay unique: only the hash keeps distinct values distinct.
        if driver == Driver::MongoDb && column.name == "_id" {
            return self == Mask::Keep || (self == Mask::Hash && text);
        }
        match self {
            Mask::Keep => true,
            Mask::Null => column.nullable && !column.primary_key,
            Mask::Blank => text && !column.primary_key,
            _ => text,
        }
    }

    /// Rule suggested from a column's name (`email`, `phone`, `first_name`, `iban`…).
    pub fn suggest(name: &str) -> Mask {
        let n = name.to_ascii_lowercase().replace(['-', ' '], "_");
        let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
        if has(&["email", "e_mail", "mail", "correo"]) {
            Mask::Email
        } else if has(&["phone", "mobile", "cell", "fax", "telef", "tel_", "movil"]) || n == "tel" {
            Mask::Phone
        } else if has(&["first_name", "firstname", "last_name", "lastname", "full_name", "fullname", "surname", "given_name", "family_name", "nombre", "apellido"])
            || matches!(n.as_str(), "name" | "contact" | "contact_name" | "customer_name" | "person_name" | "user_name" | "username")
        {
            Mask::Name
        } else if has(&["iban", "card", "tarjeta", "account_number", "ssn", "dni", "nif", "nie", "passport", "pasaporte", "tax_id", "vat"]) {
            Mask::Partial
        } else if has(&["password", "passwd", "secret", "token", "address", "street", "direccion", "domicilio", "ip_addr"]) {
            Mask::Hash
        } else {
            Mask::Keep
        }
    }

    /// Masked value, cut to `max_len` characters when the column has a limit.
    pub fn apply(self, key: &[u8], value: Cell, max_len: Option<u32>) -> Cell {
        let v = value?;
        let digest = || {
            let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, key);
            aws_lc_rs::hmac::sign(&key, v.as_bytes()).as_ref().to_vec()
        };
        let hex = |d: &[u8], n: usize| d.iter().take(n.div_ceil(2)).map(|b| format!("{b:02x}")).collect::<String>()[..n].to_string();
        let out = match self {
            Mask::Keep => v.clone(),
            Mask::Null => return None,
            Mask::Blank => String::new(),
            Mask::Hash => hex(&digest(), 16),
            Mask::Partial => {
                let chars: Vec<char> = v.chars().collect();
                let keep = if chars.len() > 4 { 4 } else { 0 };
                let mut s = "*".repeat(chars.len() - keep);
                s.extend(&chars[chars.len() - keep..]);
                s
            }
            Mask::Email => format!("user_{}@example.com", hex(&digest(), 10)),
            Mask::Name => {
                let d = digest();
                format!("{} {}", FIRST_NAMES[d[0] as usize % FIRST_NAMES.len()], LAST_NAMES[d[1] as usize % LAST_NAMES.len()])
            }
            Mask::Phone => {
                let d = digest();
                let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 10_000_000;
                format!("555-{:03}-{:04}", n / 10_000, n % 10_000)
            }
        };
        Some(match max_len {
            Some(n) if out.chars().count() > n as usize => out.chars().take(n as usize).collect(),
            _ => out,
        })
    }
}

/// Character limit of a text column (`varchar(20)` → 20), for masked values.
fn text_limit(driver: Driver, data_type: &str) -> Option<u32> {
    match classify(driver, data_type) {
        Kind::Varchar(n) => Some(n),
        _ => None,
    }
}

/// Applies each column's mask to the rows.
pub fn mask_rows(driver: Driver, columns: &[ColumnInfo], masks: &[Mask], key: &str, rows: &mut [Vec<Cell>]) {
    let plan: Vec<(usize, Mask, Option<u32>)> = masks
        .iter()
        .enumerate()
        .filter(|(_, m)| **m != Mask::Keep)
        .filter_map(|(i, m)| Some((i, *m, text_limit(driver, &columns.get(i)?.data_type))))
        .collect();
    if plan.is_empty() {
        return;
    }
    for row in rows {
        for &(i, mask, limit) in &plan {
            if let Some(cell) = row.get_mut(i) {
                *cell = mask.apply(key.as_bytes(), cell.take(), limit);
            }
        }
    }
}

/// Broad type class, to translate a column type from one engine to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    SmallInt,
    Int,
    BigInt,
    Decimal(Option<(u32, u32)>),
    Float,
    Bool,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Varchar(u32),
    Text,
    Binary,
}

/// Numbers inside the parentheses of a type: `varchar(20)` → [20], `numeric(10,2)` → [10, 2].
fn type_args(t: &str) -> Vec<u32> {
    let Some(start) = t.find('(') else { return Vec::new() };
    let end = t[start..].find(')').map_or(t.len(), |e| start + e);
    t[start + 1..end].split(',').filter_map(|a| a.trim().split(' ').next()?.parse().ok()).collect()
}

fn classify(driver: Driver, data_type: &str) -> Kind {
    // MongoDB: the BSON type names `introspect` gives the sampled fields. Documents, arrays
    // and fields of several types travel as text (JSON).
    if driver == Driver::MongoDb {
        return match data_type {
            "int" => Kind::Int,
            "long" => Kind::BigInt,
            "double" => Kind::Float,
            "decimal" => Kind::Decimal(None),
            "bool" => Kind::Bool,
            "date" => Kind::Timestamp,
            "objectId" => Kind::Varchar(24),
            "binData" => Kind::Binary,
            _ => Kind::Text,
        };
    }
    let t = data_type.trim().to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    let args = type_args(&t);
    match base {
        "smallint" | "int2" | "smallserial" => Kind::SmallInt,
        "tinyint" if args.first() == Some(&1) => Kind::Bool,
        "tinyint" | "mediumint" | "int" | "integer" | "int4" | "serial" | "year" => Kind::Int,
        "bigint" | "int8" | "bigserial" => Kind::BigInt,
        "number" | "numeric" | "decimal" | "dec" => match args.as_slice() {
            // Oracle and Snowflake write integers as NUMBER(p) / NUMBER(p,0).
            [p] | [p, 0] if *p <= 4 => Kind::SmallInt,
            [p] | [p, 0] if *p <= 9 => Kind::Int,
            [p] | [p, 0] if *p <= 18 => Kind::BigInt,
            [p, s] => Kind::Decimal(Some((*p, *s))),
            [p] => Kind::Decimal(Some((*p, 0))),
            _ => Kind::Decimal(None),
        },
        "money" | "smallmoney" => Kind::Decimal(Some((19, 4))),
        "real" | "float" | "float4" | "float8" | "double" | "double precision" | "binary_double" | "binary_float" => Kind::Float,
        "boolean" | "bool" | "bit" => Kind::Bool,
        "date" if driver == Driver::Oracle => Kind::Timestamp,
        "date" => Kind::Date,
        "time" | "time without time zone" | "time with time zone" | "timetz" => Kind::Time,
        "timestamp" | "timestamp without time zone" | "datetime" | "datetime2" | "smalldatetime" | "timestamp_ntz" => {
            if t.contains("with time zone") { Kind::TimestampTz } else { Kind::Timestamp }
        }
        "timestamptz" | "timestamp with time zone" | "timestamp with local time zone" | "datetimeoffset" | "timestamp_tz"
        | "timestamp_ltz" => Kind::TimestampTz,
        "varchar" | "character varying" | "nvarchar" | "varchar2" | "nvarchar2" | "char" | "character" | "nchar"
        | "bpchar" | "string" => match args.first() {
            Some(&n) if n > 0 && !t.contains("max") => Kind::Varchar(n),
            _ => Kind::Text,
        },
        "uuid" | "uniqueidentifier" => Kind::Varchar(36),
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "raw" | "long raw" | "image"
        | "bfile" => Kind::Binary,
        _ if t.starts_with("timestamp") && t.contains("time zone") => Kind::TimestampTz,
        _ if t.starts_with("timestamp") => Kind::Timestamp,
        _ => Kind::Text,
    }
}

/// true for binary columns: the grid shows them in hex (sometimes cut), so they can't be copied.
pub fn is_binary(driver: Driver, data_type: &str) -> bool {
    classify(driver, data_type) == Kind::Binary
}

/// Type for the target table's column. Same engine: the source type as is.
pub fn target_type(from: Driver, to: Driver, data_type: &str, primary_key: bool) -> String {
    let family = |d: Driver| if d == Driver::MariaDb { Driver::MySql } else { d };
    let lower = data_type.to_ascii_lowercase();
    // PostgreSQL arrays and user-defined types don't exist elsewhere either.
    if family(from) == family(to) && !lower.ends_with("[]") {
        return data_type.to_string();
    }
    if to == Driver::MongoDb {
        return mongo_type(from, data_type).into();
    }
    let kind = match classify(from, data_type) {
        // A key needs a bounded type on MySQL and Oracle.
        Kind::Text if primary_key && matches!(to, Driver::MySql | Driver::MariaDb | Driver::Oracle) => Kind::Varchar(255),
        k => k,
    };
    use Driver::*;
    match (kind, to) {
        // Never a copy target (the dialog isn't offered for it).
        (_, Redis | MongoDb) => "string".into(),
        (_, Sqlite) => match kind {
            Kind::SmallInt | Kind::Int | Kind::BigInt => "INTEGER".into(),
            Kind::Decimal(_) => "NUMERIC".into(),
            Kind::Float => "REAL".into(),
            Kind::Bool => "BOOLEAN".into(),
            Kind::Date => "DATE".into(),
            Kind::Timestamp | Kind::TimestampTz => "DATETIME".into(),
            Kind::Binary => "BLOB".into(),
            Kind::Time | Kind::Varchar(_) | Kind::Text => "TEXT".into(),
        },
        (Kind::SmallInt, Oracle) => "number(5)".into(),
        (Kind::Int, Oracle) => "number(10)".into(),
        (Kind::BigInt, Oracle) => "number(19)".into(),
        (Kind::SmallInt | Kind::Int | Kind::BigInt, Snowflake) => "number(38,0)".into(),
        (Kind::SmallInt, _) => "smallint".into(),
        (Kind::Int, Postgres) => "integer".into(),
        (Kind::Int, _) => "int".into(),
        (Kind::BigInt, _) => "bigint".into(),
        (Kind::Decimal(Some((p, s))), Oracle | Snowflake) => format!("number({},{})", p.min(38), s.min(p.min(38))),
        (Kind::Decimal(Some((p, s))), _) => format!("decimal({},{})", p.min(38), s.min(p.min(38))),
        (Kind::Decimal(None), Postgres) => "numeric".into(),
        (Kind::Decimal(None), Oracle) => "number".into(),
        (Kind::Decimal(None), _) => "decimal(38,10)".into(),
        (Kind::Float, Postgres) => "double precision".into(),
        (Kind::Float, MySql | MariaDb) => "double".into(),
        (Kind::Float, Oracle) => "binary_double".into(),
        (Kind::Float, _) => "float".into(),
        (Kind::Bool, MsSql) => "bit".into(),
        (Kind::Bool, Oracle) => "number(1)".into(),
        (Kind::Bool, _) => "boolean".into(),
        (Kind::Date, ClickHouse) => "Date".into(),
        (Kind::Date, _) => "date".into(),
        // ClickHouse: Time exists only on recent servers; text is the portable choice.
        (Kind::Time, ClickHouse) => "String".into(),
        (Kind::Time, Oracle) => "varchar2(32)".into(),
        (Kind::Time, _) => "time".into(),
        (Kind::Timestamp | Kind::TimestampTz, ClickHouse) => "DateTime".into(),
        (Kind::Timestamp, MsSql) => "datetime2".into(),
        (Kind::Timestamp, MySql | MariaDb) => "datetime(6)".into(),
        (Kind::Timestamp, Snowflake) => "timestamp_ntz".into(),
        (Kind::Timestamp, _) => "timestamp".into(),
        (Kind::TimestampTz, Postgres) => "timestamptz".into(),
        (Kind::TimestampTz, MsSql) => "datetimeoffset".into(),
        (Kind::TimestampTz, MySql | MariaDb) => "datetime(6)".into(),
        (Kind::TimestampTz, Oracle) => "timestamp with time zone".into(),
        (Kind::TimestampTz, _) => "timestamp_tz".into(),
        (Kind::Varchar(n), MsSql) if n <= 4000 => format!("nvarchar({n})"),
        (Kind::Varchar(n), MySql | MariaDb) if n <= 16_000 => format!("varchar({n})"),
        (Kind::Varchar(n), Oracle) if n <= 4000 => format!("varchar2({n} char)"),
        (Kind::Varchar(n), Postgres | Snowflake) => format!("varchar({n})"),
        // ClickHouse has no sized strings and no separate text/blob types.
        (Kind::Varchar(_) | Kind::Text | Kind::Binary, ClickHouse) => "String".into(),
        (Kind::Text, Postgres) => "text".into(),
        (Kind::Varchar(_) | Kind::Text, MsSql) => "nvarchar(max)".into(),
        (Kind::Varchar(_) | Kind::Text, MySql | MariaDb) => "longtext".into(),
        (Kind::Varchar(_) | Kind::Text, Oracle) => "clob".into(),
        (Kind::Text, Snowflake) => "varchar".into(),
        (Kind::Binary, Postgres) => "bytea".into(),
        (Kind::Binary, MsSql) => "varbinary(max)".into(),
        (Kind::Binary, Oracle | MySql | MariaDb) => "blob".into(),
        (Kind::Binary, Snowflake) => "binary".into(),
    }
}

/// BSON type a SQL column's values get in a MongoDB document (JSON columns become documents).
fn mongo_type(from: Driver, data_type: &str) -> &'static str {
    let base = data_type.trim().to_ascii_lowercase();
    if from != Driver::MongoDb && matches!(base.as_str(), "json" | "jsonb") {
        return "object";
    }
    match classify(from, data_type) {
        Kind::SmallInt | Kind::Int => "int",
        Kind::BigInt => "long",
        Kind::Decimal(_) => "decimal",
        Kind::Float => "double",
        Kind::Bool => "bool",
        Kind::Date | Kind::Timestamp | Kind::TimestampTz => "date",
        Kind::Binary => "binData",
        Kind::Varchar(24) if from == Driver::MongoDb => "objectId",
        Kind::Time | Kind::Varchar(_) | Kind::Text => "string",
    }
}

/// true for a SQL-style truth value (`t`, `true`, `1`, `yes`…); None if it isn't one.
fn truth(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "1" | "y" | "yes" => Some(true),
        "f" | "false" | "0" | "n" | "no" => Some(false),
        _ => None,
    }
}

/// A date or timestamp read as text (`2026-01-02`, `2026-01-02 03:04:05.5`, `…+02`, RFC 3339)
/// as a BSON date; values without a zone are taken as UTC.
fn parse_date(v: &str) -> Option<mongodb::bson::DateTime> {
    let v = v.trim();
    let mut s = if v.len() == 10 { format!("{v}T00:00:00") } else { v.replacen(' ', "T", 1) };
    s.retain(|c| c != ' ');
    // `+02` / `-0530` (PostgreSQL, SQL Server) → `+02:00` / `-05:30`.
    let tail = s.rfind(['+', '-']).filter(|&i| i > 10).map(|i| s[i + 1..].to_string());
    match tail {
        Some(t) if t.len() == 2 && t.bytes().all(|b| b.is_ascii_digit()) => s.push_str(":00"),
        Some(t) if t.len() == 4 && t.bytes().all(|b| b.is_ascii_digit()) => s.insert(s.len() - 2, ':'),
        Some(_) => {}
        None if !s.ends_with(['Z', 'z']) => s.push('Z'),
        None => {}
    }
    mongodb::bson::DateTime::parse_rfc3339_str(&s).ok()
}

/// A text cell as the BSON value of a field of `mongo_type` (see `target_type`). A value that
/// doesn't fit its type (a sampled `int` field holding a word) is kept as a string.
pub fn to_bson(mongo_type: &str, value: Cell) -> mongodb::bson::Bson {
    use mongodb::bson::{Bson, oid::ObjectId};
    let Some(v) = value else { return Bson::Null };
    let t = v.trim();
    let typed = match mongo_type {
        "string" => None,
        "int" => t.parse::<i32>().map(Bson::Int32).or_else(|_| t.parse::<i64>().map(Bson::Int64)).ok(),
        "long" => t.parse::<i64>().map(Bson::Int64).ok(),
        "double" => t.parse::<f64>().map(Bson::Double).ok(),
        "decimal" => t.parse().map(Bson::Decimal128).ok(),
        "bool" => truth(t).map(Bson::Boolean),
        "date" => parse_date(t).map(Bson::DateTime),
        "objectId" => ObjectId::parse_str(t).map(Bson::ObjectId).ok(),
        // Documents, arrays and other types were rendered as (extended) JSON.
        _ if t.starts_with(['{', '[']) => serde_json::from_str::<serde_json::Value>(t).ok().and_then(|j| Bson::try_from(j).ok()),
        _ => None,
    };
    typed.unwrap_or(Bson::String(v))
}

/// One row as a document. A NULL `_id` is left out so MongoDB generates one.
pub fn to_document(names: &[String], types: &[&str], row: Vec<Cell>) -> mongodb::bson::Document {
    let mut d = mongodb::bson::Document::new();
    for ((name, ty), cell) in names.iter().zip(types).zip(row) {
        let v = to_bson(ty, cell);
        if !(name == "_id" && v == mongodb::bson::Bson::Null) {
            d.insert(name.clone(), v);
        }
    }
    d
}

/// The dialog's filter as a `find` filter document: braces are optional (`status: "active"`).
pub fn mongo_filter(filter: &str) -> String {
    let f = filter.trim().trim_end_matches(';').trim();
    if f.is_empty() {
        "{}".into()
    } else if f.starts_with('{') {
        f.to_string()
    } else {
        format!("{{{f}}}")
    }
}

fn mongo_collection(name: &str) -> String {
    format!("db.getCollection({})", serde_json::to_string(name).unwrap_or_default())
}

/// `find` of the source documents with the selected fields; `limit` caps them.
pub fn find_command(collection: &str, columns: &[ColumnInfo], filter: &str, limit: Option<u64>) -> String {
    let mut fields: Vec<String> = columns.iter().map(|c| format!("{}: 1", serde_json::to_string(&c.name).unwrap_or_default())).collect();
    if !columns.iter().any(|c| c.name == "_id") {
        fields.push("\"_id\": 0".into());
    }
    let mut cmd = format!("{}.find({}, {{{}}})", mongo_collection(collection), mongo_filter(filter), fields.join(", "));
    if let Some(n) = limit {
        cmd.push_str(&format!(".limit({n})"));
    }
    cmd
}

/// `countDocuments` of the documents the filter selects (for a percentage).
pub fn count_command(collection: &str, filter: &str) -> String {
    format!("{}.countDocuments({})", mongo_collection(collection), mongo_filter(filter))
}

/// true if the filter is one document that only reads (no `$where` tricks past `find`).
pub fn is_read_filter(filter: &str) -> bool {
    let cmd = format!("{}.find({})", mongo_collection("x"), mongo_filter(filter));
    super::mongo::is_read_command(&cmd)
        && matches!(
            super::mongo::parse_command(&cmd),
            Ok(super::mongo::Command::Collection { args, chain, .. }) if args.len() == 1 && args[0].as_document().is_some() && chain.is_empty()
        )
}

/// What the copy runs on a MongoDB target, for the dialog's preview.
pub fn mongo_insert_preview(collection: &str, names: &[String], types: &[&str], clear: bool) -> String {
    let coll = mongo_collection(collection);
    let fields: Vec<String> = names.iter().zip(types).map(|(n, t)| format!("{}: <{t}>", serde_json::to_string(n).unwrap_or_default())).collect();
    let insert = format!("{coll}.insertMany([{{{}}}, …])", fields.join(", "));
    if clear { format!("{coll}.deleteMany({{}});\n\n{insert}") } else { insert }
}

/// Source value as the target expects it: booleans read as `t`/`true` become `1`/`0` on the
/// engines that store them as numbers; MongoDB dates (`2026-01-02T03:04:05.123Z`) lose the
/// `T` and the `Z` every engine doesn't accept.
fn convert(from: Driver, to: Driver, data_type: &str, value: Cell) -> Cell {
    let v = value?;
    let kind = classify(from, data_type);
    if from == Driver::MongoDb && kind == Kind::Timestamp {
        return Some(match v.strip_suffix('Z') {
            Some(d) if d.len() >= 19 && d.as_bytes()[10] == b'T' => d.replacen('T', " ", 1),
            _ => v,
        });
    }
    if kind != Kind::Bool {
        return Some(v);
    }
    let Some(t) = truth(&v) else { return Some(v) };
    Some(match (to, t) {
        (Driver::Postgres | Driver::Snowflake, true) => "true".into(),
        (Driver::Postgres | Driver::Snowflake, false) => "false".into(),
        (_, true) => "1".into(),
        (_, false) => "0".into(),
    })
}

/// SELECT of the source rows; `limit` caps them.
pub fn select_sql(driver: Driver, schema: &str, table: &str, columns: &[ColumnInfo], filter: &str, limit: Option<u64>) -> String {
    let cols = columns.iter().map(|c| driver.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let t = Session::quote_table(driver, schema, table);
    let top = match (driver, limit) {
        (Driver::MsSql, Some(n)) => format!("TOP ({n}) "),
        _ => String::new(),
    };
    let mut sql = format!("SELECT {top}{cols} FROM {t}");
    let w = filter.trim().trim_end_matches(';').trim();
    if !w.is_empty() {
        sql.push_str(&format!(" WHERE {w}"));
    }
    match (driver, limit) {
        (Driver::MsSql, _) | (_, None) => sql,
        (Driver::Oracle, Some(n)) => format!("SELECT * FROM ({sql}) WHERE ROWNUM <= {n}"),
        (_, Some(n)) => format!("{sql} LIMIT {n}"),
    }
}

/// COUNT(*) of the rows the filter selects (for a percentage).
pub fn count_sql(driver: Driver, schema: &str, table: &str, filter: &str) -> String {
    let mut sql = format!("SELECT COUNT(*) FROM {}", Session::quote_table(driver, schema, table));
    let w = filter.trim().trim_end_matches(';').trim();
    if !w.is_empty() {
        sql.push_str(&format!(" WHERE {w}"));
    }
    sql
}

/// Columns of the table to create on `to`, with their types translated.
pub fn target_columns(from: Driver, to: Driver, columns: &[ColumnInfo]) -> Vec<NewColumn> {
    columns
        .iter()
        .map(|c| NewColumn {
            name: c.name.clone(),
            data_type: target_type(from, to, &c.data_type, c.primary_key),
            not_null: !c.nullable,
            primary_key: c.primary_key,
            default: String::new(),
        })
        .collect()
}

/// `CREATE TABLE` for the target. Names keep their case (quoted when needed), so the INSERTs,
/// which always quote, find them.
pub fn create_sql(from: Driver, to: Driver, schema: &str, table: &str, columns: &[ColumnInfo]) -> String {
    let schema = if to.flat_schema() { "" } else { schema };
    create_table_sql(to, schema, table, &target_columns(from, to, columns))
}

/// Multi-row INSERTs into the `names` columns (Oracle: one row each, it has no multi-row VALUES).
/// `columns` are the source columns, for their types.
pub fn insert_sql(from: Driver, to: Driver, table: &str, columns: &[ColumnInfo], names: &[String], rows: Vec<Vec<Cell>>) -> Vec<String> {
    if columns.is_empty() {
        return Vec::new();
    }
    let cols = names.iter().map(|n| to.quote_ident(n)).collect::<Vec<_>>().join(", ");
    let batch = if to == Driver::Oracle { 1 } else { BATCH };
    let mut out = Vec::new();
    let mut tuples = Vec::new();
    for row in rows {
        let vals: Vec<String> = row
            .into_iter()
            .zip(columns)
            .map(|(v, c)| literal(to, &convert(from, to, &c.data_type, v)))
            .collect();
        tuples.push(format!("({})", vals.join(", ")));
        if tuples.len() == batch {
            out.push(format!("INSERT INTO {table} ({cols})\nVALUES {}", tuples.join(",\n       ")));
            tuples.clear();
        }
    }
    if !tuples.is_empty() {
        out.push(format!("INSERT INTO {table} ({cols})\nVALUES {}", tuples.join(",\n       ")));
    }
    out
}

/// Session for `db` on its own connection (Oracle keeps its service name).
async fn open(mut cfg: ConnectionConfig, db: &str) -> anyhow::Result<Session> {
    if !matches!(cfg.driver, Driver::Oracle | Driver::Sqlite) && !db.is_empty() {
        cfg.database = db.to_string();
    }
    let mut s = Session::connect(&cfg).await?;
    s.use_database(db).await?;
    Ok(s)
}

/// Runs the copy and returns the rows inserted. A cancel stops it and rolls the target back.
/// One table to write: the source rows and where they go.
struct Part {
    columns: Vec<ColumnInfo>,
    names: Vec<String>,
    rows: Vec<Vec<Cell>>,
    target_table: String,
    create: bool,
    clear: bool,
    /// Leave out the rows whose primary key is already in the target.
    skip_existing: bool,
}

/// Rows read from the source: the copied table, plus the related ones (insert order) and their
/// foreign keys when a subset was asked for.
async fn read_source(s: &mut Session, from: Driver, req: &CopyRequest) -> anyhow::Result<(Vec<TableRows>, Vec<ForeignKey>)> {
    if from == Driver::MongoDb {
        let root = TableRows {
            schema: req.source_schema.clone(),
            table: req.source_table.clone(),
            columns: req.columns.clone(),
            rows: read_documents(s, req).await?,
        };
        return Ok((vec![root], Vec::new()));
    }
    let limit = if req.percent >= 100 {
        None
    } else {
        let rs = s.execute(&count_sql(from, &req.source_schema, &req.source_table, &req.filter), 1).await?;
        let total: u64 = rs
            .first()
            .and_then(|r| r.rows.first())
            .and_then(|r| r.first().cloned().flatten())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        Some((total * u64::from(req.percent)).div_ceil(100))
    };
    let sql = select_sql(from, &req.source_schema, &req.source_table, &req.columns, &req.filter, limit);
    let rs = s.execute(&sql, MAX_ROWS).await?.into_iter().next().unwrap_or_default();
    if rs.truncated {
        anyhow::bail!(tr("The table has more than 1,000,000 rows: copy fewer with a filter or a percentage"));
    }
    let root = TableRows {
        schema: req.source_schema.clone(),
        table: req.source_table.clone(),
        columns: req.columns.clone(),
        rows: rs.rows,
    };
    if !(req.parents || req.children) {
        return Ok((vec![root], Vec::new()));
    }
    let catalog = s.introspect_database(&req.source_db).await?;
    let fk_rows = s.execute(&subset::fk_sql(from, &req.source_db, &req.source_schema), MAX_ROWS).await?;
    let fks = subset::parse_fks(from, &fk_rows.into_iter().next().unwrap_or_default(), &catalog);
    let missing = subset::missing_key_columns(&fks, &req.source_schema, &req.source_table, &req.columns);
    if !missing.is_empty() {
        anyhow::bail!(trf("Select the key columns the foreign keys need: {columns}", &[("columns", &missing.join(", "))]));
    }
    let mut tables = subset::collect(s, from, &catalog, &fks, root, req.children).await?;
    subset::insert_order(&mut tables, &fks);
    Ok((tables, fks))
}

/// The source collection's documents as rows of the selected fields (missing fields are NULL).
async fn read_documents(s: &mut Session, req: &CopyRequest) -> anyhow::Result<Vec<Vec<Cell>>> {
    let first_cell = |rs: Vec<super::ResultSet>| rs.into_iter().next().and_then(|r| r.rows.into_iter().next()).and_then(|r| r.into_iter().next().flatten());
    let limit = if req.percent >= 100 {
        None
    } else {
        let rs = s.execute(&count_command(&req.source_table, &req.filter), 1).await?;
        let total: u64 = first_cell(rs).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        Some((total * u64::from(req.percent)).div_ceil(100))
    };
    // `limit(0)` means no limit to MongoDB.
    if limit == Some(0) {
        return Ok(Vec::new());
    }
    let rs = s.execute(&find_command(&req.source_table, &req.columns, &req.filter, limit), MAX_ROWS).await?.into_iter().next().unwrap_or_default();
    if rs.truncated {
        anyhow::bail!(tr("The table has more than 1,000,000 rows: copy fewer with a filter or a percentage"));
    }
    // The result's columns are the fields the documents have, in their order.
    let at: Vec<Option<usize>> = req.columns.iter().map(|c| rs.columns.iter().position(|n| *n == c.name)).collect();
    Ok(rs.rows.into_iter().map(|mut r| at.iter().map(|i| i.and_then(|i| r.get_mut(i)?.take())).collect()).collect())
}

/// Writes the rows to a MongoDB collection (created when it doesn't exist) in unordered
/// `insertMany` batches. There is no transaction (a standalone server has none): a failure or a
/// cancel keeps the batches already inserted. Documents whose `_id` the collection already has
/// are skipped. Returns the documents inserted.
async fn write_documents(t: &mut Session, from: Driver, req: &CopyRequest, columns: &[ColumnInfo], rows: Vec<Vec<Cell>>) -> anyhow::Result<u64> {
    let Session::MongoDb(m) = t else { anyhow::bail!(tr("Not available for this engine")) };
    if req.clear {
        m.execute(&format!("{}.deleteMany({{}})", mongo_collection(&req.target_table)), 0).await?;
    }
    let types: Vec<String> = columns.iter().map(|c| target_type(from, Driver::MongoDb, &c.data_type, c.primary_key)).collect();
    let types: Vec<&str> = types.iter().map(String::as_str).collect();
    let mut rows = rows.into_iter();
    let mut inserted = 0;
    loop {
        let docs: Vec<_> = rows.by_ref().take(MONGO_BATCH).map(|r| to_document(&req.target_names, &types, r)).collect();
        if docs.is_empty() {
            return Ok(inserted);
        }
        inserted += m.insert_skipping_duplicates(&req.target_table, docs).await?.0;
    }
}

/// Masks of each table: the dialog's for the copied one; for the others, those suggested by
/// their column names (never on key columns) if asked; then made equal along foreign keys.
fn table_masks(from: Driver, req: &CopyRequest, tables: &[TableRows], fks: &[ForeignKey]) -> Vec<Vec<Mask>> {
    let mut masks: Vec<Vec<Mask>> = tables
        .iter()
        .map(|t| {
            if t.schema == req.source_schema && t.table == req.source_table {
                return req.masks.clone();
            }
            t.columns
                .iter()
                .map(|c| {
                    let key = c.primary_key || fks.iter().any(|fk| fk.schema == t.schema && fk.table == t.table && fk.columns.contains(&c.name));
                    let m = Mask::suggest(&c.name);
                    if req.mask_related && !key && m.allowed(from, c) { m } else { Mask::Keep }
                })
                .collect()
        })
        .collect();
    subset::propagate_masks(tables, &mut masks, fks);
    masks
}

/// Target tables, by name ignoring case, when the subset needs to know which exist.
fn find_target<'a>(catalog: &'a [SchemaInfo], schema: &str, table: &str) -> Option<&'a [ColumnInfo]> {
    let s = catalog.iter().find(|s| s.name.eq_ignore_ascii_case(schema)).or_else(|| (catalog.len() == 1).then(|| &catalog[0]))?;
    s.tables.iter().find(|t| t.name.eq_ignore_ascii_case(table)).map(|t| t.columns.as_slice())
}

/// Runs the copy and returns the rows inserted per target table. A cancel stops it and rolls
/// the target back.
pub async fn copy(source: ConnectionConfig, target: ConnectionConfig, req: CopyRequest, cancel: Arc<Notify>) -> anyhow::Result<Vec<(String, u64)>> {
    let (from, to) = (source.driver, target.driver);
    let mut source = source;
    source.read_only = true;
    let mut req = req;
    // MongoDB has no foreign keys to follow.
    if from == Driver::MongoDb || to == Driver::MongoDb {
        req.parents = false;
        req.children = false;
    }
    let related = req.parents || req.children;
    let cancelled = || anyhow::anyhow!(tr("Copy cancelled"));

    let read = async {
        let mut s = open(source, &req.source_db).await?;
        read_source(&mut s, from, &req).await
    };
    let (mut tables, fks) = tokio::select! {
        _ = cancel.notified() => return Err(cancelled()),
        r = read => r?,
    };
    let masks = table_masks(from, &req, &tables, &fks);
    for (t, m) in tables.iter_mut().zip(&masks) {
        mask_rows(from, &t.columns, m, &req.mask_key, &mut t.rows);
    }

    if to == Driver::MongoDb {
        let rows = tables.into_iter().next().map(|t| t.rows).unwrap_or_default();
        let write = async {
            let mut t = open(target, &req.target_db).await?;
            write_documents(&mut t, from, &req, &req.columns, rows).await
        };
        return tokio::select! {
            _ = cancel.notified() => Err(cancelled()),
            r = write => r.map(|n| vec![(req.target_table.clone(), n)]),
        };
    }

    let schema = if to.flat_schema() && to != Driver::Sqlite { req.target_db.as_str() } else { req.target_schema.as_str() };
    let prepare = async {
        let mut t = open(target, &req.target_db).await?;
        let catalog = if related { t.introspect_database(&req.target_db).await? } else { Vec::new() };
        let mut parts = Vec::new();
        for tr in tables {
            let root = tr.schema == req.source_schema && tr.table == req.source_table;
            let mut part = if root {
                Part {
                    columns: tr.columns,
                    names: req.target_names.clone(),
                    rows: tr.rows,
                    target_table: req.target_table.clone(),
                    create: req.create,
                    clear: req.clear,
                    skip_existing: related && !req.create && !req.clear,
                }
            } else {
                match find_target(&catalog, &req.target_schema, &tr.table) {
                    // Existing table: only the columns it has, by name.
                    Some(existing) => {
                        let keep: Vec<(usize, String)> = tr
                            .columns
                            .iter()
                            .enumerate()
                            .filter_map(|(i, c)| existing.iter().find(|e| e.name.eq_ignore_ascii_case(&c.name)).map(|e| (i, e.name.clone())))
                            .collect();
                        Part {
                            columns: keep.iter().map(|(i, _)| tr.columns[*i].clone()).collect(),
                            names: keep.iter().map(|(_, n)| n.clone()).collect(),
                            rows: tr.rows.iter().map(|r| keep.iter().map(|(i, _)| r[*i].clone()).collect()).collect(),
                            target_table: tr.table.clone(),
                            create: false,
                            clear: false,
                            skip_existing: true,
                        }
                    }
                    None => Part {
                        names: tr.columns.iter().map(|c| c.name.clone()).collect(),
                        columns: tr.columns,
                        rows: tr.rows,
                        target_table: tr.table,
                        create: true,
                        clear: false,
                        skip_existing: false,
                    },
                }
            };
            if part.skip_existing {
                skip_existing_rows(&mut t, to, schema, &mut part).await?;
            }
            parts.push(part);
        }
        for p in parts.iter().filter(|p| p.create) {
            let mut cols = p.columns.clone();
            for (c, n) in cols.iter_mut().zip(&p.names) {
                c.name.clone_from(n);
            }
            t.execute(&create_sql(from, to, &req.target_schema, &p.target_table, &cols), 0).await?;
        }
        anyhow::Ok((t, parts))
    };
    let (mut t, parts) = tokio::select! {
        _ = cancel.notified() => return Err(cancelled()),
        r = prepare => r?,
    };

    let mut statements = Vec::new();
    let mut report = Vec::new();
    for p in parts {
        let table = Session::quote_table(to, schema, &p.target_table);
        if p.clear {
            statements.push(format!("DELETE FROM {table}"));
        }
        report.push((p.target_table, p.rows.len() as u64));
        statements.extend(insert_sql(from, to, &table, &p.columns, &p.names, p.rows));
    }
    let handle = t.cancel_handle();
    // Dropping the session on a cancel closes it, so the open transaction is rolled back.
    tokio::select! {
        _ = cancel.notified() => {
            if let Some(h) = handle {
                let _ = h.cancel().await;
            }
            Err(cancelled())
        }
        r = t.run_transaction(&statements) => r.map(|_| report),
    }
}

/// Leaves out the rows whose primary key the target table already has (a related customer
/// copied before, say), so copying again adds what is missing instead of failing.
async fn skip_existing_rows(t: &mut Session, to: Driver, schema: &str, part: &mut Part) -> anyhow::Result<()> {
    let pk: Vec<usize> = part.columns.iter().enumerate().filter(|(_, c)| c.primary_key).map(|(i, _)| i).collect();
    if pk.is_empty() || part.rows.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = pk.iter().map(|&i| part.names[i].clone()).collect();
    let keys: Vec<Vec<String>> = part
        .rows
        .iter()
        .filter_map(|r| pk.iter().map(|&i| r[i].clone()).collect::<Option<Vec<_>>>())
        .collect();
    let table = Session::quote_table(to, schema, &part.target_table);
    let mut found = std::collections::HashSet::new();
    for chunk in subset::key_chunks(&keys) {
        let rs = t.execute(&subset::existing_key_sql(to, &table, &names, chunk), MAX_ROWS).await?;
        for row in rs.into_iter().flat_map(|r| r.rows) {
            found.insert(subset::normalize_key(&row));
        }
    }
    if !found.is_empty() {
        part.rows.retain(|r| !found.contains(&subset::normalize_key(&pk.iter().map(|&i| r[i].clone()).collect::<Vec<_>>())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk }
    }

    #[test]
    fn translates_types_between_engines() {
        assert_eq!(target_type(Driver::Postgres, Driver::Postgres, "character varying(20)", false), "character varying(20)");
        assert_eq!(target_type(Driver::MySql, Driver::MariaDb, "tinyint(1)", false), "tinyint(1)");
        assert_eq!(target_type(Driver::Postgres, Driver::MsSql, "character varying(20)", false), "nvarchar(20)");
        assert_eq!(target_type(Driver::Postgres, Driver::MySql, "text", true), "varchar(255)");
        assert_eq!(target_type(Driver::Postgres, Driver::Oracle, "boolean", false), "number(1)");
        assert_eq!(target_type(Driver::Oracle, Driver::Postgres, "number(10)", false), "bigint");
        assert_eq!(target_type(Driver::Oracle, Driver::Postgres, "number(10,2)", false), "decimal(10,2)");
        assert_eq!(target_type(Driver::Oracle, Driver::Postgres, "date", false), "timestamp");
        assert_eq!(target_type(Driver::MsSql, Driver::Postgres, "datetimeoffset", false), "timestamptz");
        assert_eq!(target_type(Driver::MsSql, Driver::Sqlite, "nvarchar(max)", false), "TEXT");
        assert_eq!(target_type(Driver::MySql, Driver::Snowflake, "int", false), "number(38,0)");
        assert_eq!(target_type(Driver::Postgres, Driver::MySql, "integer[]", false), "longtext");
        assert!(is_binary(Driver::Postgres, "bytea") && is_binary(Driver::MsSql, "varbinary(max)"));
        assert!(!is_binary(Driver::Oracle, "varchar2(10 char)"));
    }

    #[test]
    fn masks_are_deterministic_and_fit_the_column() {
        let key = b"k";
        let email = Mask::Email.apply(key, Some("ana@corp.com".into()), None).unwrap();
        assert!(email.starts_with("user_") && email.ends_with("@example.com") && email.len() == 27, "{email}");
        assert_eq!(Mask::Email.apply(key, Some("ana@corp.com".into()), None).unwrap(), email);
        assert_ne!(Mask::Email.apply(b"other", Some("ana@corp.com".into()), None).unwrap(), email);
        assert_ne!(Mask::Hash.apply(key, Some("a".into()), None), Mask::Hash.apply(key, Some("b".into()), None));
        assert_eq!(Mask::Hash.apply(key, Some("a".into()), Some(6)).unwrap().len(), 6);
        assert_eq!(Mask::Partial.apply(key, Some("ES91 2100 1234".into()), None).unwrap(), "**********1234");
        assert_eq!(Mask::Partial.apply(key, Some("123".into()), None).unwrap(), "***");
        assert!(Mask::Phone.apply(key, Some("+34 600".into()), None).unwrap().starts_with("555-"));
        assert_eq!(Mask::Name.apply(key, None, None), None);
        assert_eq!(Mask::Null.apply(key, Some("x".into()), None), None);
        assert_eq!(Mask::Blank.apply(key, Some("x".into()), None), Some(String::new()));

        let pk = ColumnInfo { name: "id".into(), data_type: "varchar(10)".into(), nullable: false, primary_key: true };
        let num = ColumnInfo { name: "n".into(), data_type: "integer".into(), nullable: true, primary_key: false };
        assert!(!Mask::Null.allowed(Driver::Postgres, &pk) && !Mask::Blank.allowed(Driver::Postgres, &pk));
        assert!(Mask::Hash.allowed(Driver::Postgres, &pk));
        assert!(Mask::Null.allowed(Driver::Postgres, &num) && !Mask::Hash.allowed(Driver::Postgres, &num));

        let cols = [pk, num];
        let mut rows = vec![vec![Some("abc".into()), Some("7".into())]];
        mask_rows(Driver::Postgres, &cols, &[Mask::Hash, Mask::Null], "k", &mut rows);
        assert_eq!(rows[0][0].as_ref().map(String::len), Some(10));
        assert_eq!(rows[0][1], None);
    }

    #[test]
    fn suggests_masks_from_column_names() {
        assert_eq!(Mask::suggest("Email"), Mask::Email);
        assert_eq!(Mask::suggest("mobile_phone"), Mask::Phone);
        assert_eq!(Mask::suggest("first_name"), Mask::Name);
        assert_eq!(Mask::suggest("name"), Mask::Name);
        assert_eq!(Mask::suggest("product_name"), Mask::Keep);
        assert_eq!(Mask::suggest("IBAN"), Mask::Partial);
        assert_eq!(Mask::suggest("password_hash"), Mask::Hash);
        assert_eq!(Mask::suggest("created_at"), Mask::Keep);
    }

    #[test]
    fn converts_booleans() {
        assert_eq!(convert(Driver::Postgres, Driver::MySql, "boolean", Some("t".into())), Some("1".into()));
        assert_eq!(convert(Driver::MsSql, Driver::Postgres, "bit", Some("false".into())), Some("false".into()));
        assert_eq!(convert(Driver::Postgres, Driver::MySql, "integer", Some("t".into())), Some("t".into()));
        assert_eq!(convert(Driver::Postgres, Driver::MySql, "boolean", None), None);
    }

    #[test]
    fn builds_select_with_filter_and_limit() {
        let cols = [col("id", "int", true), col("name", "text", false)];
        assert_eq!(
            select_sql(Driver::Postgres, "public", "t", &cols, "id > 3;", Some(5)),
            "SELECT \"id\", \"name\" FROM \"public\".\"t\" WHERE id > 3 LIMIT 5"
        );
        assert_eq!(select_sql(Driver::MsSql, "dbo", "t", &cols, "", Some(5)), "SELECT TOP (5) [id], [name] FROM [dbo].[t]");
        assert_eq!(
            select_sql(Driver::Oracle, "HR", "T", &cols, "", Some(5)),
            "SELECT * FROM (SELECT \"id\", \"name\" FROM \"HR\".\"T\") WHERE ROWNUM <= 5"
        );
        assert_eq!(count_sql(Driver::Sqlite, "main", "t", " a = 1 "), "SELECT COUNT(*) FROM \"t\" WHERE a = 1");
    }

    #[test]
    fn maps_mongodb_types() {
        use Driver::*;
        let to_pg = |t: &str| target_type(MongoDb, Postgres, t, false);
        assert_eq!(
            ["int", "long", "double", "decimal", "bool", "date", "objectId", "string", "object", "array", "mixed", "null"].map(to_pg),
            ["integer", "bigint", "double precision", "numeric", "boolean", "timestamp", "varchar(24)", "text", "text", "text", "text", "text"]
        );
        assert_eq!(target_type(MongoDb, Sqlite, "objectId", true), "TEXT");
        assert_eq!(target_type(MongoDb, MySql, "string", true), "varchar(255)");
        assert_eq!(target_type(MongoDb, MongoDb, "object", false), "object");
        assert!(is_binary(MongoDb, "binData"));
        let sql_to_mongo = |t: &str| target_type(Postgres, MongoDb, t, false);
        assert_eq!(
            ["integer", "bigint", "numeric(10,2)", "double precision", "boolean", "date", "timestamptz", "varchar(20)", "jsonb", "bytea"].map(sql_to_mongo),
            ["int", "long", "decimal", "double", "bool", "date", "date", "string", "object", "binData"]
        );
        let cols = [col("_id", "objectId", true), col("name", "string", false), col("n", "int", false)];
        assert!(Mask::Name.allowed(MongoDb, &cols[1]) && Mask::Blank.allowed(MongoDb, &cols[1]));
        assert!(Mask::Hash.allowed(MongoDb, &cols[0]) && !Mask::Email.allowed(MongoDb, &cols[0]) && !Mask::Null.allowed(MongoDb, &cols[0]));
        assert!(!Mask::Hash.allowed(MongoDb, &cols[2]) && Mask::Null.allowed(MongoDb, &cols[2]));
        // MongoDB dates go to SQL without the `T` and the `Z`; booleans as each engine wants them.
        assert_eq!(convert(MongoDb, MySql, "date", Some("2026-01-02T03:04:05.120Z".into())), Some("2026-01-02 03:04:05.120".into()));
        assert_eq!(convert(MongoDb, Sqlite, "bool", Some("true".into())), Some("1".into()));
    }

    #[test]
    fn converts_text_to_bson() {
        use mongodb::bson::{Bson, doc, oid::ObjectId};
        assert_eq!(to_bson("int", Some("42".into())), Bson::Int32(42));
        assert_eq!(to_bson("int", Some("9007199254740993".into())), Bson::Int64(9007199254740993));
        assert_eq!(to_bson("long", Some("7".into())), Bson::Int64(7));
        assert_eq!(to_bson("double", Some("1.5".into())), Bson::Double(1.5));
        assert_eq!(to_bson("decimal", Some("10.20".into())), Bson::Decimal128("10.20".parse().unwrap()));
        assert_eq!(to_bson("bool", Some("t".into())), Bson::Boolean(true));
        assert_eq!(to_bson("bool", Some("0".into())), Bson::Boolean(false));
        assert_eq!(to_bson("int", Some("abc".into())), Bson::String("abc".into()));
        assert_eq!(to_bson("string", Some("{\"a\": 1}".into())), Bson::String("{\"a\": 1}".into()));
        assert_eq!(to_bson("int", None), Bson::Null);
        let date = |s: &str| match to_bson("date", Some(s.into())) {
            Bson::DateTime(d) => d.try_to_rfc3339_string().unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(date("2026-01-02"), "2026-01-02T00:00:00Z");
        assert_eq!(date("2026-01-02 03:04:05.5"), "2026-01-02T03:04:05.5Z");
        assert_eq!(date("2026-01-02 03:04:05+02"), "2026-01-02T01:04:05Z");
        assert_eq!(date("2026-01-02 03:04:05 +0530").len(), 20);
        assert_eq!(date("2026-01-02T03:04:05.123Z"), "2026-01-02T03:04:05.123Z");
        let oid = "64b7f1f1f1f1f1f1f1f1f1f1";
        assert_eq!(to_bson("objectId", Some(oid.into())), Bson::ObjectId(ObjectId::parse_str(oid).unwrap()));
        // A hashed `_id` is no longer an ObjectId: it stays a string.
        assert_eq!(to_bson("objectId", Some("0123abcd".into())), Bson::String("0123abcd".into()));
        // Documents and arrays come back from the relaxed extended JSON `render` writes.
        let nested = Bson::Document(doc! { "k": true, "at": mongodb::bson::DateTime::from_millis(0), "id": ObjectId::parse_str(oid).unwrap() });
        let text = super::super::mongo::render(&nested);
        assert_eq!(to_bson("object", text), nested);
        assert_eq!(to_bson("array", Some("[1, \"a\"]".into())), Bson::Array(vec![Bson::Int32(1), Bson::String("a".into())]));
        assert_eq!(to_bson("mixed", Some("plain".into())), Bson::String("plain".into()));

        let names: Vec<String> = ["_id", "n", "tags"].map(String::from).to_vec();
        let d = to_document(&names, &["objectId", "int", "array"], vec![None, Some("3".into()), None]);
        assert_eq!(d, doc! { "n": 3, "tags": Bson::Null });
    }

    #[test]
    fn builds_mongodb_commands() {
        let cols = [col("_id", "objectId", true), col("first name", "string", false)];
        assert_eq!(mongo_filter(""), "{}");
        assert_eq!(mongo_filter(" status: \"active\"; "), "{status: \"active\"}");
        assert_eq!(mongo_filter("{a: 1}"), "{a: 1}");
        assert_eq!(
            find_command("people", &cols, "age: {$gt: 30}", Some(5)),
            "db.getCollection(\"people\").find({age: {$gt: 30}}, {\"_id\": 1, \"first name\": 1}).limit(5)"
        );
        assert_eq!(find_command("p", &cols[1..], "", None), "db.getCollection(\"p\").find({}, {\"first name\": 1, \"_id\": 0})");
        assert_eq!(count_command("p", "a: 1"), "db.getCollection(\"p\").countDocuments({a: 1})");
        for cmd in [find_command("people", &cols, "age: {$gt: 30}", Some(5)), count_command("p", "")] {
            assert!(super::super::mongo::is_read_command(&cmd), "{cmd}");
        }
        assert!(is_read_filter("") && is_read_filter("status: 'a', n: {$in: [1, 2]}") && is_read_filter("{_id: ObjectId(\"64b7f1f1f1f1f1f1f1f1f1f1\")}"));
        assert!(!is_read_filter("a: 1}).limit(1).sort({a: 1"));
        assert!(!is_read_filter("a: 1}, {b: 1"));
        assert!(!is_read_filter("a: "));
        assert!(!is_read_filter("1"));
        assert_eq!(
            mongo_insert_preview("t", &["id".into(), "name".into()], &["int", "string"], true),
            "db.getCollection(\"t\").deleteMany({});\n\ndb.getCollection(\"t\").insertMany([{\"id\": <int>, \"name\": <string>}, …])"
        );
    }

    #[test]
    fn builds_create_and_inserts() {
        let cols = [col("id", "integer", true), col("Name", "character varying(20)", false), col("ok", "boolean", false)];
        assert_eq!(
            create_sql(Driver::Postgres, Driver::MySql, "public", "Users", &cols),
            "create table Users\n(\n    id int not null,\n    Name varchar(20),\n    ok boolean,\n    primary key (id)\n)"
        );
        let rows: Vec<Vec<Cell>> = (0..150).map(|i| vec![Some(i.to_string()), Some("it's".into()), Some("t".into())]).collect();
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let s = insert_sql(Driver::Postgres, Driver::MySql, "`db`.`Users`", &cols, &names, rows);
        assert_eq!(s.len(), 2);
        assert!(s[0].starts_with("INSERT INTO `db`.`Users` (`id`, `Name`, `ok`)\nVALUES ('0', 'it''s', '1'),"));
        let one = insert_sql(Driver::Sqlite, Driver::Oracle, "\"T\"", &cols, &names, vec![vec![None, None, Some("f".into())]]);
        assert_eq!(one, vec!["INSERT INTO \"T\" (\"id\", \"Name\", \"ok\")\nVALUES (NULL, NULL, '0')".to_string()]);
    }
}
