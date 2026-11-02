//! "Compare Schemas…": two schemas read into plain models (tables with their columns, keys,
//! indexes and foreign keys, and views), their differences, and the script that makes the target
//! match the source. Everything here is pure; `read` loads the models from the servers.

pub mod read;

use super::Driver;
use super::alter::{self, ColumnDef, EditColumn, EditIndex, IndexDef, TableDetails, TableEdit};
use super::views::{ViewDefinition, ViewSpec, create_view_sql};
use crate::i18n::{tr, trf};
use crate::ui::create_dialog::NewColumn;
use crate::ui::drop_dialog::{DropKind, object_sql};
use crate::ui::quote_ident_if_needed;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Engines whose schemas can be compared.
pub fn supported(driver: Driver) -> bool {
    matches!(driver, Driver::Postgres | Driver::Sqlite | Driver::MsSql | Driver::MySql | Driver::MariaDb | Driver::Oracle)
}

/// A script is only generated between engines of one dialect (MySQL and MariaDB share one).
pub fn same_dialect(a: Driver, b: Driver) -> bool {
    let dialect = |d| if d == Driver::MariaDb { Driver::MySql } else { d };
    dialect(a) == dialect(b)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForeignKeyDef {
    /// Constraint name ("" = unnamed, as SQLite reports them).
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    /// `NO ACTION`, `CASCADE`, `SET NULL`…
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableModel {
    pub name: String,
    /// Columns, primary key and indexes (see `alter::load_details`). `ColumnDef::extra` holds the
    /// identity / auto-increment clause of every engine.
    pub details: TableDetails,
    pub foreign_keys: Vec<ForeignKeyDef>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewModel {
    pub name: String,
    pub definition: ViewDefinition,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SchemaModel {
    pub driver: Driver,
    pub db: String,
    pub schema: String,
    pub tables: Vec<TableModel>,
    pub views: Vec<ViewModel>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    OnlyInSource,
    OnlyInTarget,
    Different,
    Identical,
}

impl Status {
    pub const ALL: [Status; 4] = [Status::OnlyInSource, Status::OnlyInTarget, Status::Different, Status::Identical];

    pub fn label(self) -> &'static str {
        match self {
            Status::OnlyInSource => tr("Only in source"),
            Status::OnlyInTarget => tr("Only in target"),
            Status::Different => tr("Different"),
            Status::Identical => tr("Identical"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Table,
    View,
}

/// One compared object.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectDiff {
    pub kind: Kind,
    pub name: String,
    pub status: Status,
    /// Index in the source / target model's `tables` or `views`.
    pub source: Option<usize>,
    pub target: Option<usize>,
    /// What differs, one line each.
    pub changes: Vec<String>,
    /// Each side's definition, written the same way so they compare line by line.
    pub source_ddl: String,
    pub target_ddl: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    pub objects: Vec<ObjectDiff>,
}

impl Comparison {
    pub fn count(&self, status: Status) -> usize {
        self.objects.iter().filter(|o| o.status == status).count()
    }

    pub fn has_differences(&self) -> bool {
        self.objects.iter().any(|o| o.status != Status::Identical)
    }

    /// What goes into the script at first: everything but drops and identical objects.
    pub fn default_selection(&self) -> Vec<bool> {
        self.objects.iter().map(|o| matches!(o.status, Status::OnlyInSource | Status::Different)).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Normalisation

/// Engines whose identifiers match ignoring case.
fn case_insensitive(driver: Driver) -> bool {
    matches!(driver, Driver::Sqlite | Driver::MySql | Driver::MariaDb | Driver::MsSql)
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `text` with the qualifier `from.` (bare or quoted) replaced by `to.` ("" removes it). String
/// literals are included: PostgreSQL defaults name their sequence as `'schema.seq'::regclass`.
pub fn requalify(driver: Driver, text: &str, from: &str, to: &str) -> String {
    if from.is_empty() || from == to {
        return text.to_string();
    }
    let quoted = driver.quote_ident(from);
    let prefix = if to.is_empty() { String::new() } else { format!("{}.", quote_ident_if_needed(driver, to)) };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        let boundary = text[..i].chars().next_back().is_none_or(|p| !(ident_char(p) || matches!(p, '.' | '"' | '`' | ']')));
        let rest = &text[i..];
        let bare = rest.get(..from.len()).is_some_and(|w| w.eq_ignore_ascii_case(from)) && rest[from.len()..].starts_with('.');
        let hit = if !boundary {
            None
        } else if rest.starts_with(&quoted) && rest[quoted.len()..].starts_with('.') {
            Some(quoted.len())
        } else if bare {
            Some(from.len())
        } else {
            None
        };
        match hit {
            Some(n) => {
                out.push_str(&prefix);
                i += n + 1;
            }
            None => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// Lowercase outside quotes, whitespace collapsed and dropped around punctuation.
fn squash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut space = false;
    for c in text.chars() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            space = true;
            continue;
        }
        // No blank after an opening or separating mark, nor before a closing one or an operator.
        let operator = |c: char| matches!(c, '=' | '<' | '>' | '+' | '-' | '*' | '/' | '|');
        let tight_after = |c: char| matches!(c, '(' | ',' | '.') || operator(c);
        let tight_before = matches!(c, '(' | ')' | ',' | ';' | '.') || operator(c);
        if space && !out.is_empty() && !tight_before && !out.ends_with(tight_after) {
            out.push(' ');
        }
        space = false;
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                out.push(c);
            }
            '[' => {
                quote = Some(']');
                out.push(c);
            }
            _ => out.extend(c.to_lowercase()),
        }
    }
    out.trim_end_matches(';').to_string()
}

/// Removes parentheses around the whole text (SQL Server keeps defaults as `((0))`).
fn strip_parens(mut s: &str) -> &str {
    loop {
        let t = s.trim();
        if !(t.starts_with('(') && t.ends_with(')')) {
            return t;
        }
        let mut depth = 0;
        let mut closes_early = false;
        for (i, c) in t.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && i + 1 < t.len() {
                        closes_early = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if closes_early {
            return t;
        }
        s = &t[1..t.len() - 1];
    }
}

/// Type spelling that compares equal across the ways an engine writes it.
fn norm_type(driver: Driver, schema: &str, ty: &str) -> String {
    let t = squash(&requalify(driver, ty, schema, ""));
    let (base, rest) = match t.find('(') {
        Some(i) => (t[..i].trim().to_string(), t[i..].to_string()),
        None => (t.clone(), String::new()),
    };
    match driver {
        Driver::Postgres => {
            let base = match base.as_str() {
                "int" | "int4" => "integer",
                "int8" => "bigint",
                "int2" => "smallint",
                "bool" => "boolean",
                "float8" => "double precision",
                "float4" => "real",
                "varchar" => "character varying",
                "char" | "bpchar" => "character",
                "decimal" => "numeric",
                "timestamptz" => "timestamp with time zone",
                "timestamp" => "timestamp without time zone",
                "timetz" => "time with time zone",
                "time" => "time without time zone",
                other => other,
            };
            format!("{base}{rest}")
        }
        Driver::MySql | Driver::MariaDb => {
            let base = match base.as_str() {
                "integer" => "int",
                "bool" | "boolean" => return "tinyint(1)".into(),
                other => other,
            };
            // Integer display widths (`int(11)`) are gone since MySQL 8.0.19; tinyint(1) is boolean.
            let int = matches!(base, "tinyint" | "smallint" | "mediumint" | "int" | "bigint");
            match rest.find(')') {
                Some(end) if int && !(base == "tinyint" && &rest[..=end] == "(1)") => format!("{base}{}", &rest[end + 1..]),
                _ => format!("{base}{rest}"),
            }
        }
        _ => t,
    }
}

fn norm_default(driver: Driver, schema: &str, default: Option<&str>) -> Option<String> {
    let d = squash(strip_parens(&requalify(driver, default?, schema, "")));
    (!d.is_empty() && d != "null" && !d.starts_with("null::")).then_some(d)
}

fn norm_extra(extra: &str) -> String {
    squash(extra)
}

/// Delete / update rule: SQL spelling, `NO ACTION` when not given (MySQL treats RESTRICT the same).
fn norm_rule(driver: Driver, rule: &str) -> String {
    let r = rule.trim().replace('_', " ").to_ascii_uppercase();
    match r.as_str() {
        "" | "NONE" => "NO ACTION".into(),
        "RESTRICT" if matches!(driver, Driver::MySql | Driver::MariaDb) => "NO ACTION".into(),
        _ => r,
    }
}

/// The view's SELECT as it compares: own-schema qualifiers dropped, case and blanks normalised.
fn norm_view(driver: Driver, schema: &str, v: &ViewDefinition) -> String {
    format!("{}|{}", squash(&v.columns), squash(&requalify(driver, &v.body, schema, "")))
}

// ---------------------------------------------------------------------------------------------
// Comparison

/// Both sides, with the identifier folding they share.
struct Ctx<'a> {
    src: &'a SchemaModel,
    tgt: &'a SchemaModel,
    /// Names match ignoring case (case-insensitive engine, or two different engines).
    fold: bool,
}

impl<'a> Ctx<'a> {
    fn new(src: &'a SchemaModel, tgt: &'a SchemaModel) -> Self {
        let fold = src.driver != tgt.driver || case_insensitive(tgt.driver);
        Self { src, tgt, fold }
    }

    fn key(&self, name: &str) -> String {
        if self.fold { name.to_lowercase() } else { name.to_string() }
    }

    fn keys(&self, names: &[String]) -> Vec<String> {
        names.iter().map(|n| self.key(n)).collect()
    }

    fn src_type(&self, c: &ColumnDef) -> String {
        norm_type(self.src.driver, &self.src.schema, &c.data_type)
    }

    fn tgt_type(&self, c: &ColumnDef) -> String {
        norm_type(self.tgt.driver, &self.tgt.schema, &c.data_type)
    }

    fn src_default(&self, c: &ColumnDef) -> Option<String> {
        norm_default(self.src.driver, &self.src.schema, c.default.as_deref())
    }

    fn tgt_default(&self, c: &ColumnDef) -> Option<String> {
        norm_default(self.tgt.driver, &self.tgt.schema, c.default.as_deref())
    }

    /// Comparable form of a foreign key (names are not compared: SQLite has none).
    fn fk_key(&self, model: &SchemaModel, fk: &ForeignKeyDef) -> String {
        let schema = if fk.ref_schema.is_empty() || fk.ref_schema.eq_ignore_ascii_case(&model.schema) { String::new() } else { self.key(&fk.ref_schema) };
        format!(
            "{}>{}.{}({})|{}|{}",
            self.keys(&fk.columns).join(","),
            schema,
            self.key(&fk.ref_table),
            self.keys(&fk.ref_columns).join(","),
            norm_rule(model.driver, &fk.on_delete),
            norm_rule(model.driver, &fk.on_update)
        )
    }

    /// Index identity: its name, or for a SQLite UNIQUE constraint (auto-named) its columns.
    fn index_key(&self, driver: Driver, x: &IndexDef) -> String {
        if driver == Driver::Sqlite && x.constraint { format!("unique({})", self.keys(&x.columns).join(",")) } else { self.key(&x.name) }
    }

    fn index_eq(&self, a: &IndexDef, b: &IndexDef) -> bool {
        self.keys(&a.columns) == self.keys(&b.columns) && a.unique == b.unique && a.plain == b.plain
    }
}

/// A column present on both sides that differs.
struct ColumnChange<'a> {
    src: &'a ColumnDef,
    tgt: &'a ColumnDef,
    /// Index of `tgt` in the target's columns.
    tgt_index: usize,
    ty: bool,
    null: bool,
    default: bool,
    extra: bool,
}

/// What differs between two versions of a table.
struct TableDelta<'a> {
    added: Vec<&'a ColumnDef>,
    dropped: Vec<&'a ColumnDef>,
    changed: Vec<ColumnChange<'a>>,
    pk: bool,
    /// Indexes only in the source / only in the target (a changed one is in both lists).
    indexes_added: Vec<&'a IndexDef>,
    indexes_dropped: Vec<&'a IndexDef>,
    fks_added: Vec<&'a ForeignKeyDef>,
    fks_dropped: Vec<&'a ForeignKeyDef>,
    autoincrement: bool,
}

fn table_delta<'a>(cx: &Ctx, s: &'a TableModel, t: &'a TableModel) -> TableDelta<'a> {
    let tcols: HashMap<String, usize> = t.details.columns.iter().enumerate().map(|(i, c)| (cx.key(&c.name), i)).collect();
    let scols: HashSet<String> = s.details.columns.iter().map(|c| cx.key(&c.name)).collect();
    let mut delta = TableDelta {
        added: vec![],
        dropped: t.details.columns.iter().filter(|c| !scols.contains(&cx.key(&c.name))).collect(),
        changed: vec![],
        pk: cx.keys(&s.details.primary_key).into_iter().collect::<BTreeSet<_>>() != cx.keys(&t.details.primary_key).into_iter().collect::<BTreeSet<_>>(),
        indexes_added: vec![],
        indexes_dropped: vec![],
        fks_added: vec![],
        fks_dropped: vec![],
        autoincrement: cx.tgt.driver == Driver::Sqlite && s.details.autoincrement != t.details.autoincrement,
    };
    for sc in &s.details.columns {
        let Some(&j) = tcols.get(&cx.key(&sc.name)) else {
            delta.added.push(sc);
            continue;
        };
        let tc = &t.details.columns[j];
        let c = ColumnChange {
            src: sc,
            tgt: tc,
            tgt_index: j,
            ty: cx.src_type(sc) != cx.tgt_type(tc),
            null: sc.nullable != tc.nullable,
            default: cx.src_default(sc) != cx.tgt_default(tc),
            extra: norm_extra(&sc.extra) != norm_extra(&tc.extra),
        };
        if c.ty || c.null || c.default || c.extra {
            delta.changed.push(c);
        }
    }
    let src_idx: HashMap<String, &IndexDef> = s.details.indexes.iter().map(|x| (cx.index_key(cx.src.driver, x), x)).collect();
    let tgt_idx: HashMap<String, &IndexDef> = t.details.indexes.iter().map(|x| (cx.index_key(cx.tgt.driver, x), x)).collect();
    for x in &s.details.indexes {
        if !tgt_idx.get(&cx.index_key(cx.src.driver, x)).is_some_and(|y| cx.index_eq(x, y)) {
            delta.indexes_added.push(x);
        }
    }
    for x in &t.details.indexes {
        if !src_idx.get(&cx.index_key(cx.tgt.driver, x)).is_some_and(|y| cx.index_eq(x, y)) {
            delta.indexes_dropped.push(x);
        }
    }
    let src_fks: HashSet<String> = s.foreign_keys.iter().map(|f| cx.fk_key(cx.src, f)).collect();
    let tgt_fks: HashSet<String> = t.foreign_keys.iter().map(|f| cx.fk_key(cx.tgt, f)).collect();
    delta.fks_added = s.foreign_keys.iter().filter(|f| !tgt_fks.contains(&cx.fk_key(cx.src, f))).collect();
    delta.fks_dropped = t.foreign_keys.iter().filter(|f| !src_fks.contains(&cx.fk_key(cx.tgt, f))).collect();
    delta
}

fn null_word(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

fn or_none(s: Option<&str>) -> String {
    s.filter(|s| !s.trim().is_empty()).map_or_else(|| tr("(none)").to_string(), |s| s.trim().to_string())
}

fn fk_label(fk: &ForeignKeyDef) -> String {
    format!("({}) → {}", fk.columns.join(", "), fk.ref_table)
}

fn describe(delta: &TableDelta) -> Vec<String> {
    let mut out = vec![];
    for c in &delta.added {
        out.push(trf("Column “{name}” added", &[("name", &c.name)]));
    }
    for c in &delta.dropped {
        out.push(trf("Column “{name}” dropped", &[("name", &c.name)]));
    }
    for c in &delta.changed {
        let name = &c.src.name;
        if c.ty {
            out.push(trf("Column “{name}”: type {from} → {to}", &[("name", name), ("from", &c.tgt.data_type), ("to", &c.src.data_type)]));
        }
        if c.null {
            out.push(trf("Column “{name}”: {from} → {to}", &[("name", name), ("from", &null_word(c.tgt.nullable)), ("to", &null_word(c.src.nullable))]));
        }
        if c.default {
            let (from, to) = (or_none(c.tgt.default.as_deref()), or_none(c.src.default.as_deref()));
            out.push(trf("Column “{name}”: default {from} → {to}", &[("name", name), ("from", &from), ("to", &to)]));
        }
        if c.extra {
            let (from, to) = (or_none(Some(&c.tgt.extra)), or_none(Some(&c.src.extra)));
            out.push(trf("Column “{name}”: {from} → {to}", &[("name", name), ("from", &from), ("to", &to)]));
        }
    }
    if delta.pk {
        out.push(tr("Primary key changed").into());
    }
    if delta.autoincrement {
        out.push(tr("AUTOINCREMENT changed").into());
    }
    let changed: Vec<&str> = delta.indexes_added.iter().filter(|x| delta.indexes_dropped.iter().any(|y| y.name == x.name)).map(|x| x.name.as_str()).collect();
    for x in &delta.indexes_added {
        if changed.contains(&x.name.as_str()) {
            out.push(trf("Index “{name}” changed", &[("name", &x.name)]));
        } else {
            out.push(trf("Index “{name}” added", &[("name", &x.name)]));
        }
    }
    for x in delta.indexes_dropped.iter().filter(|x| !changed.contains(&x.name.as_str())) {
        out.push(trf("Index “{name}” dropped", &[("name", &x.name)]));
    }
    for f in &delta.fks_added {
        out.push(trf("Foreign key {key} added", &[("key", &fk_label(f))]));
    }
    for f in &delta.fks_dropped {
        out.push(trf("Foreign key {key} dropped", &[("key", &fk_label(f))]));
    }
    out
}

/// Compares every table and view of the two schemas.
pub fn compare(src: &SchemaModel, tgt: &SchemaModel) -> Comparison {
    let cx = Ctx::new(src, tgt);
    let mut objects = vec![];
    let tgt_tables: HashMap<String, usize> = tgt.tables.iter().enumerate().map(|(i, t)| (cx.key(&t.name), i)).collect();
    let mut seen = HashSet::new();
    for (i, s) in src.tables.iter().enumerate() {
        let j = tgt_tables.get(&cx.key(&s.name)).copied();
        let (status, changes) = match j {
            Some(j) => {
                seen.insert(j);
                let changes = describe(&table_delta(&cx, s, &tgt.tables[j]));
                (if changes.is_empty() { Status::Identical } else { Status::Different }, changes)
            }
            None => (Status::OnlyInSource, vec![]),
        };
        objects.push(ObjectDiff {
            kind: Kind::Table,
            name: s.name.clone(),
            status,
            source: Some(i),
            target: j,
            changes,
            source_ddl: table_text(src, s),
            target_ddl: j.map(|j| table_text(tgt, &tgt.tables[j])).unwrap_or_default(),
        });
    }
    for (j, t) in tgt.tables.iter().enumerate().filter(|(j, _)| !seen.contains(j)) {
        objects.push(ObjectDiff {
            kind: Kind::Table,
            name: t.name.clone(),
            status: Status::OnlyInTarget,
            source: None,
            target: Some(j),
            changes: vec![],
            source_ddl: String::new(),
            target_ddl: table_text(tgt, t),
        });
    }
    let tgt_views: HashMap<String, usize> = tgt.views.iter().enumerate().map(|(i, v)| (cx.key(&v.name), i)).collect();
    let mut seen = HashSet::new();
    for (i, s) in src.views.iter().enumerate() {
        let j = tgt_views.get(&cx.key(&s.name)).copied();
        let status = match j {
            Some(j) => {
                seen.insert(j);
                let same = norm_view(src.driver, &src.schema, &s.definition) == norm_view(tgt.driver, &tgt.schema, &tgt.views[j].definition);
                if same { Status::Identical } else { Status::Different }
            }
            None => Status::OnlyInSource,
        };
        objects.push(ObjectDiff {
            kind: Kind::View,
            name: s.name.clone(),
            status,
            source: Some(i),
            target: j,
            changes: if status == Status::Different { vec![tr("View definition differs").into()] } else { vec![] },
            source_ddl: view_text(src, s),
            target_ddl: j.map(|j| view_text(tgt, &tgt.views[j])).unwrap_or_default(),
        });
    }
    for (j, t) in tgt.views.iter().enumerate().filter(|(j, _)| !seen.contains(j)) {
        objects.push(ObjectDiff {
            kind: Kind::View,
            name: t.name.clone(),
            status: Status::OnlyInTarget,
            source: None,
            target: Some(j),
            changes: vec![],
            source_ddl: String::new(),
            target_ddl: view_text(tgt, t),
        });
    }
    objects.sort_by_key(|o| (o.status, o.kind, o.name.to_lowercase()));
    Comparison { objects }
}

// ---------------------------------------------------------------------------------------------
// Definitions shown side by side

fn column_text(driver: Driver, schema: &str, c: &ColumnDef) -> String {
    let q = |s: &str| quote_ident_if_needed(driver, s);
    let mut l = format!("    {} {}", q(&c.name), requalify(driver, c.data_type.trim(), schema, ""));
    if !c.extra.trim().is_empty() {
        l.push_str(&format!(" {}", c.extra.trim().to_lowercase()));
    }
    if let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        l.push_str(&format!(" default {}", requalify(driver, d.trim(), schema, "")));
    }
    if !c.nullable {
        l.push_str(" not null");
    }
    if c.generated {
        l.push_str(" -- generated");
    }
    l
}

fn fk_text(driver: Driver, schema: &str, fk: &ForeignKeyDef) -> String {
    let q = |s: &str| quote_ident_if_needed(driver, s);
    let cols = |v: &[String]| v.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let table = if fk.ref_schema.is_empty() || fk.ref_schema == schema { q(&fk.ref_table) } else { format!("{}.{}", q(&fk.ref_schema), q(&fk.ref_table)) };
    let mut s = format!("foreign key ({}) references {table}", cols(&fk.columns));
    if !fk.ref_columns.is_empty() {
        s.push_str(&format!(" ({})", cols(&fk.ref_columns)));
    }
    for (word, rule) in [("delete", &fk.on_delete), ("update", &fk.on_update)] {
        let rule = norm_rule(driver, rule);
        if rule != "NO ACTION" {
            s.push_str(&format!(" on {word} {}", rule.to_lowercase()));
        }
    }
    s
}

/// A table as `create table` (without schema), its indexes and foreign keys, in a fixed order.
pub fn table_text(model: &SchemaModel, t: &TableModel) -> String {
    let d = model.driver;
    let q = |s: &str| quote_ident_if_needed(d, s);
    let cols = |v: &[String]| v.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let mut lines: Vec<String> = t.details.columns.iter().map(|c| column_text(d, &model.schema, c)).collect();
    if !t.details.primary_key.is_empty() {
        let auto = if t.details.autoincrement { " autoincrement" } else { "" };
        lines.push(format!("    primary key ({}){auto}", cols(&t.details.primary_key)));
    }
    let mut fks: Vec<String> = t.foreign_keys.iter().map(|f| format!("    {}", fk_text(d, &model.schema, f))).collect();
    fks.sort();
    lines.extend(fks);
    let mut out = format!("create table {}\n(\n{}\n)", q(&t.name), lines.join(",\n"));
    let mut indexes: Vec<&IndexDef> = t.details.indexes.iter().collect();
    indexes.sort_by_key(|x| (x.constraint && d == Driver::Sqlite, x.name.to_lowercase()));
    for x in indexes {
        let unique = if x.unique { "unique " } else { "" };
        if x.constraint && d == Driver::Sqlite {
            out.push_str(&format!("\nunique ({})", cols(&x.columns)));
        } else {
            out.push_str(&format!("\ncreate {unique}index {} on {} ({})", q(&x.name), q(&t.name), x.columns.join(", ")));
        }
    }
    out
}

pub fn view_text(model: &SchemaModel, v: &ViewModel) -> String {
    let columns = if v.definition.columns.trim().is_empty() { String::new() } else { format!(" ({})", v.definition.columns.trim()) };
    let body = requalify(model.driver, super::views::clean_body(&v.definition.body), &model.schema, "");
    format!("create view {}{columns} as\n{}", quote_ident_if_needed(model.driver, &v.name), body)
}

/// Lines of `a` and `b` that are not in their longest common subsequence (blanks and case
/// ignored): the changed lines of each side.
pub fn changed_lines(a: &str, b: &str) -> (Vec<bool>, Vec<bool>) {
    let la: Vec<String> = a.lines().map(squash).collect();
    let lb: Vec<String> = b.lines().map(squash).collect();
    let (n, m) = (la.len(), lb.len());
    if n * m > 4_000_000 {
        return (la.iter().map(|l| !lb.contains(l)).collect(), lb.iter().map(|l| !la.contains(l)).collect());
    }
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if la[i] == lb[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut ca, mut cb) = (vec![true; n], vec![true; m]);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if la[i] == lb[j] {
            ca[i] = false;
            cb[j] = false;
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (ca, cb)
}

// ---------------------------------------------------------------------------------------------
// Script

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    pub statements: Vec<String>,
    /// What the script can't do or can lose, shown above it (and as comments in it).
    pub warnings: Vec<String>,
}

impl Script {
    /// The script as text: warnings as comments, then each statement ending in `;`.
    pub fn text(&self, src: &SchemaModel, tgt: &SchemaModel) -> String {
        let mut out = format!("-- {}\n", trf("Makes {target} match {source}", &[("target", &side_label(tgt)), ("source", &side_label(src))]));
        for w in &self.warnings {
            out.push_str(&format!("-- {}\n", w.replace('\n', " ")));
        }
        if self.statements.is_empty() {
            out.push_str(&format!("-- {}\n", tr("No changes")));
        }
        for s in &self.statements {
            out.push_str(&format!("\n{s};\n"));
        }
        out
    }
}

fn side_label(m: &SchemaModel) -> String {
    if m.schema.is_empty() || m.schema == m.db || m.driver == Driver::Sqlite { m.db.clone() } else { format!("{}.{}", m.db, m.schema) }
}

/// Statements in their order of execution.
#[derive(Default)]
struct Phases {
    pre: Vec<String>,
    drop_fks: Vec<String>,
    drop_views: Vec<String>,
    drop_tables: Vec<String>,
    create_tables: Vec<String>,
    alter_tables: Vec<String>,
    add_fks: Vec<String>,
    create_views: Vec<String>,
}

struct Gen<'a> {
    cx: Ctx<'a>,
    d: Driver,
    warnings: Vec<String>,
}

/// `nextval('seq'::regclass)` → `seq`.
fn nextval_sequence(default: &str) -> Option<&str> {
    let rest = default.trim().strip_prefix("nextval('")?;
    let end = rest.find("'::regclass)")?;
    Some(&rest[..end])
}

/// Literal default SQLite accepts in ADD COLUMN (no expressions, CURRENT_TIME…).
fn constant_default(d: &str) -> bool {
    let d = d.trim();
    d.starts_with('\'') || d.starts_with('-') || d.starts_with('+') || d.chars().next().is_some_and(|c| c.is_ascii_digit()) || ["null", "true", "false"].contains(&d.to_ascii_lowercase().as_str())
}

/// Identifiers of a SQL text (bare and quoted), lowercase.
fn idents(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                }
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let word: String = chars.by_ref().take_while(|&c| c != close).collect();
                out.insert(word.to_lowercase());
            }
            c if ident_char(c) => {
                let mut word = c.to_string();
                while let Some(&c) = chars.peek().filter(|c| ident_char(**c)) {
                    word.push(c);
                    chars.next();
                }
                out.insert(word.to_lowercase());
            }
            _ => {}
        }
    }
    out
}

impl<'a> Gen<'a> {
    fn q(&self, n: &str) -> String {
        quote_ident_if_needed(self.d, n)
    }

    fn cols(&self, v: &[String]) -> String {
        v.iter().map(|c| self.q(c)).collect::<Vec<_>>().join(", ")
    }

    fn schema(&self) -> &str {
        &self.cx.tgt.schema
    }

    /// A table or view of the target schema.
    fn object(&self, name: &str) -> String {
        if self.d == Driver::Sqlite || self.schema().is_empty() { self.q(name) } else { format!("{}.{}", self.q(self.schema()), self.q(name)) }
    }

    /// Source SQL text (type, default, view body) moved to the target schema.
    fn moved(&self, text: &str) -> String {
        requalify(self.d, text, &self.cx.src.schema, self.schema())
    }

    fn rule_clauses(&self, fk: &ForeignKeyDef) -> String {
        let mut s = String::new();
        for (word, rule) in [("delete", &fk.on_delete), ("update", &fk.on_update)] {
            let rule = norm_rule(self.d, rule);
            // Oracle has no ON UPDATE and only CASCADE / SET NULL on delete.
            let allowed = match self.d {
                Driver::Oracle => word == "delete" && matches!(rule.as_str(), "CASCADE" | "SET NULL"),
                _ => rule != "NO ACTION",
            };
            if allowed {
                s.push_str(&format!(" on {word} {}", rule.to_lowercase()));
            }
        }
        s
    }

    /// `[constraint n] foreign key (…) references t (…) …`; `from` is the model the key comes from.
    fn fk_clause(&self, fk: &ForeignKeyDef, from: &SchemaModel) -> String {
        let own = fk.ref_schema.is_empty() || fk.ref_schema.eq_ignore_ascii_case(&from.schema);
        let table = if own { self.object(&fk.ref_table) } else { format!("{}.{}", self.q(&fk.ref_schema), self.q(&fk.ref_table)) };
        let name = if fk.name.is_empty() { String::new() } else { format!("constraint {} ", self.q(&fk.name)) };
        let refs = if fk.ref_columns.is_empty() { String::new() } else { format!(" ({})", self.cols(&fk.ref_columns)) };
        format!("{name}foreign key ({}) references {table}{refs}{}", self.cols(&fk.columns), self.rule_clauses(fk))
    }

    fn drop_fk(&self, table: &str, fk: &ForeignKeyDef) -> String {
        let kind = if matches!(self.d, Driver::MySql | Driver::MariaDb) { "foreign key" } else { "constraint" };
        format!("alter table {} drop {kind} {}", self.object(table), self.q(&fk.name))
    }

    /// PostgreSQL `integer default nextval('<table>_<col>_seq')` is a serial column.
    fn serial(&self, table: &str, c: &ColumnDef) -> Option<&'static str> {
        if self.d != Driver::Postgres {
            return None;
        }
        let default = requalify(self.d, c.default.as_deref()?, &self.cx.src.schema, "");
        let seq = nextval_sequence(&default)?;
        if seq != format!("{table}_{}_seq", c.name) {
            return None;
        }
        match norm_type(self.d, "", &c.data_type).as_str() {
            "integer" => Some("serial"),
            "bigint" => Some("bigserial"),
            "smallint" => Some("smallserial"),
            _ => None,
        }
    }

    /// PostgreSQL: the sequence a source default takes values from, created first.
    fn sequence_for(&self, pre: &mut Vec<String>, c: &ColumnDef) {
        if self.d != Driver::Postgres {
            return;
        }
        if let Some(seq) = c.default.as_deref().map(|d| self.moved(d)).as_deref().and_then(nextval_sequence) {
            let stmt = format!("create sequence if not exists {seq}");
            if !pre.contains(&stmt) {
                pre.push(stmt);
            }
        }
    }

    /// Column definition: `name type [identity] [default x] [not null]`.
    fn column_sql(&self, c: &ColumnDef, serial: Option<&str>) -> String {
        let mut l = format!("{} {}", self.q(&c.name), serial.map_or_else(|| self.moved(c.data_type.trim()), str::to_string));
        if !c.extra.trim().is_empty() {
            l.push_str(&format!(" {}", c.extra.trim()));
        }
        if serial.is_none()
            && let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty())
        {
            l.push_str(&format!(" default {}", self.moved(d.trim())));
        }
        if !c.nullable {
            l.push_str(" not null");
        }
        l
    }

    fn generated_warning(&mut self, table: &str, c: &ColumnDef) {
        if c.generated {
            self.warnings.push(trf("{table}.{name} is a generated column: its expression is not copied", &[("table", &table), ("name", &c.name)]));
        }
    }

    fn create_index(&mut self, table: &str, x: &IndexDef) -> Option<String> {
        if !x.plain {
            self.warnings.push(trf("Index “{name}” is an expression or partial index and is not recreated", &[("name", &x.name)]));
            return None;
        }
        let name = if self.d == Driver::Oracle { self.object(&x.name) } else { self.q(&x.name) };
        Some(format!("create {}index {name} on {} ({})", if x.unique { "unique " } else { "" }, self.object(table), self.cols(&x.columns)))
    }

    /// `create table` for a source table named `name` (SQLite: with its foreign keys and UNIQUE
    /// constraints inline), then its indexes.
    fn create_table(&mut self, pre: &mut Vec<String>, s: &TableModel, name: &str) -> Vec<String> {
        let sqlite = self.d == Driver::Sqlite;
        let pk = &s.details.primary_key;
        let inline_pk = sqlite && s.details.autoincrement && pk.len() == 1;
        let mut lines = vec![];
        for c in &s.details.columns {
            self.generated_warning(&s.name, c);
            let serial = self.serial(&s.name, c);
            if serial.is_none() {
                self.sequence_for(pre, c);
            }
            let mut l = format!("    {}", self.column_sql(c, serial));
            if inline_pk && pk[0] == c.name {
                l.push_str(" primary key autoincrement");
            }
            lines.push(l);
        }
        if !pk.is_empty() && !inline_pk {
            lines.push(format!("    primary key ({})", self.cols(pk)));
        }
        if sqlite {
            for x in s.details.indexes.iter().filter(|x| x.constraint) {
                lines.push(format!("    unique ({})", self.cols(&x.columns)));
            }
            for fk in &s.foreign_keys {
                lines.push(format!("    {}", self.fk_clause(fk, self.cx.src)));
            }
        }
        let mut out = vec![format!("create table {}\n(\n{}\n)", self.object(name), lines.join(",\n"))];
        for x in s.details.indexes.iter().filter(|x| !(sqlite && x.constraint)) {
            out.extend(self.create_index(name, x));
        }
        out
    }

    /// SQLite rebuild: a new table with the source definition, the common columns copied, the
    /// old table dropped and the new one renamed, then its indexes.
    fn sqlite_rebuild(&mut self, pre: &mut Vec<String>, s: &TableModel, t: &TableModel) -> Vec<String> {
        let tmp = format!("_dc_new_{}", t.name);
        let mut out = self.create_table(pre, s, &tmp);
        let indexes = out.split_off(1);
        let tcols: HashMap<String, &ColumnDef> = t.details.columns.iter().map(|c| (self.cx.key(&c.name), c)).collect();
        let common: Vec<(&str, &str)> = s.details.columns.iter().filter_map(|c| tcols.get(&self.cx.key(&c.name)).map(|o| (c.name.as_str(), o.name.as_str()))).collect();
        if !common.is_empty() {
            let to = common.iter().map(|(n, _)| self.q(n)).collect::<Vec<_>>().join(", ");
            let from = common.iter().map(|(_, o)| self.q(o)).collect::<Vec<_>>().join(", ");
            out.push(format!("insert into {} ({to}) select {from} from {}", self.q(&tmp), self.q(&t.name)));
        }
        out.push(format!("drop table {}", self.q(&t.name)));
        out.push(format!("alter table {} rename to {}", self.q(&tmp), self.q(&t.name)));
        // The indexes were written for the temporary name: create them on the final one.
        let tmp_ref = format!(" on {} (", self.q(&tmp));
        let final_ref = format!(" on {} (", self.q(&t.name));
        out.extend(indexes.into_iter().map(|x| x.replacen(&tmp_ref, &final_ref, 1)));
        out
    }

    /// SQLite: the changes ALTER TABLE can make (indexes, new nullable or constant-default columns).
    fn sqlite_in_place(&self, delta: &TableDelta, s: &TableModel) -> bool {
        let fk_columns: HashSet<String> = s.foreign_keys.iter().flat_map(|f| self.cx.keys(&f.columns)).collect();
        delta.changed.is_empty()
            && delta.dropped.is_empty()
            && !delta.pk
            && !delta.autoincrement
            && delta.fks_added.is_empty()
            && delta.fks_dropped.is_empty()
            && !delta.indexes_added.iter().chain(&delta.indexes_dropped).any(|x| x.constraint)
            && delta.added.iter().all(|c| {
                let default = c.default.as_deref().unwrap_or("").trim();
                (c.nullable || !default.is_empty())
                    && (default.is_empty() || constant_default(default))
                    && !c.generated
                    && !fk_columns.contains(&self.cx.key(&c.name))
                    && !s.details.primary_key.iter().any(|p| self.cx.key(p) == self.cx.key(&c.name))
            })
    }

    /// The edit that turns target table `t` into source table `s`, for `alter::plan`.
    fn edit(&mut self, s: &TableModel, t: &TableModel) -> (Vec<EditColumn>, Vec<EditIndex>) {
        let cx = &self.cx;
        let tcols: HashMap<String, usize> = t.details.columns.iter().enumerate().map(|(i, c)| (cx.key(&c.name), i)).collect();
        let pk: HashSet<String> = cx.keys(&s.details.primary_key).into_iter().collect();
        let mut columns = vec![];
        for sc in &s.details.columns {
            let primary_key = pk.contains(&cx.key(&sc.name));
            let source_default = sc.default.as_deref().map(|d| self.moved(d)).unwrap_or_default();
            let column = match tcols.get(&cx.key(&sc.name)) {
                // What compares equal keeps the target's spelling: no needless ALTER.
                Some(&j) => {
                    let tc = &t.details.columns[j];
                    let same_type = cx.src_type(sc) == cx.tgt_type(tc);
                    let same_default = cx.src_default(sc) == cx.tgt_default(tc);
                    EditColumn {
                        original: Some(j),
                        column: NewColumn {
                            name: tc.name.clone(),
                            data_type: if same_type { tc.data_type.clone() } else { self.moved(&sc.data_type) },
                            not_null: !sc.nullable,
                            primary_key,
                            default: if same_default { tc.default.clone().unwrap_or_default() } else { source_default },
                        },
                    }
                }
                // New columns carry their identity clause in the type (`int identity(1,1)`).
                None => {
                    let ty = self.moved(&sc.data_type);
                    let data_type = if sc.extra.trim().is_empty() { ty } else { format!("{ty} {}", sc.extra.trim()) };
                    EditColumn { original: None, column: NewColumn { name: sc.name.clone(), data_type, not_null: !sc.nullable, primary_key, default: source_default } }
                }
            };
            columns.push(column);
        }
        let src_idx: HashMap<String, &IndexDef> = s.details.indexes.iter().map(|x| (cx.index_key(cx.src.driver, x), x)).collect();
        let mut indexes = vec![];
        let mut kept = HashSet::new();
        for (j, x) in t.details.indexes.iter().enumerate() {
            let key = cx.index_key(cx.tgt.driver, x);
            if src_idx.get(&key).is_some_and(|y| cx.index_eq(x, y)) {
                kept.insert(key);
                indexes.push(EditIndex { original: Some(j), name: x.name.clone(), columns: x.columns.join(", "), unique: x.unique });
            }
        }
        let mut skipped = vec![];
        for x in &s.details.indexes {
            if kept.contains(&cx.index_key(cx.src.driver, x)) {
                continue;
            }
            if !x.plain {
                skipped.push(x.name.clone());
                continue;
            }
            indexes.push(EditIndex { original: None, name: x.name.clone(), columns: x.columns.join(", "), unique: x.unique });
        }
        for name in skipped {
            self.warnings.push(trf("Index “{name}” is an expression or partial index and is not recreated", &[("name", &name)]));
        }
        (columns, indexes)
    }

    /// Statements for a column whose identity / auto-increment clause differs.
    fn extra_sql(&mut self, table: &str, c: &ColumnChange) -> Vec<String> {
        let t = self.object(table);
        let col = self.q(&c.tgt.name);
        let (from, to) = (norm_extra(&c.tgt.extra), norm_extra(&c.src.extra));
        match self.d {
            Driver::Postgres => {
                let kind = |e: &str| if e.contains("always") { "always" } else { "by default" };
                vec![match (from.is_empty(), to.is_empty()) {
                    (true, false) => format!("alter table {t} alter column {col} add {}", c.src.extra.trim()),
                    (false, true) => format!("alter table {t} alter column {col} drop identity if exists"),
                    _ => format!("alter table {t} alter column {col} set generated {}", kind(&to)),
                }]
            }
            Driver::Oracle => vec![if to.is_empty() {
                format!("alter table {t} modify ({col} drop identity)")
            } else {
                format!("alter table {t} modify ({col} {})", c.src.extra.trim())
            }],
            Driver::MySql | Driver::MariaDb => {
                let mut def = format!("{} {}", self.moved(&c.src.data_type), if c.src.nullable { "null" } else { "not null" });
                if let Some(d) = c.src.default.as_deref().filter(|d| !d.trim().is_empty()) {
                    def.push_str(&format!(" default {}", d.trim()));
                }
                if !to.is_empty() {
                    def.push_str(&format!(" {}", c.src.extra.trim()));
                }
                vec![format!("alter table {t} modify column {col} {def}")]
            }
            _ => {
                self.warnings.push(trf("{table}.{name}: the identity of a column can't be changed in place", &[("table", &table), ("name", &c.src.name)]));
                vec![]
            }
        }
    }
}

/// The script that makes `tgt` match `src` for the selected objects (`selected[i]` is
/// `cmp.objects[i]`). Only between engines of one dialect.
pub fn script(src: &SchemaModel, tgt: &SchemaModel, cmp: &Comparison, selected: &[bool]) -> Result<Script, String> {
    if !same_dialect(src.driver, tgt.driver) {
        return Err(tr("Source and target use different engines: the differences are shown, but no script is generated.").into());
    }
    let mut g = Gen { cx: Ctx::new(src, tgt), d: tgt.driver, warnings: vec![] };
    let d = g.d;
    let sqlite = d == Driver::Sqlite;
    let chosen: Vec<&ObjectDiff> =
        cmp.objects.iter().zip(selected.iter().chain(std::iter::repeat(&false))).filter(|(o, on)| **on && o.status != Status::Identical).map(|(o, _)| o).collect();
    let tables = |status: Status| chosen.iter().filter(move |o| o.kind == Kind::Table && o.status == status).copied().collect::<Vec<_>>();
    let (created, dropped, altered) = (tables(Status::OnlyInSource), tables(Status::OnlyInTarget), tables(Status::Different));
    let mut p = Phases::default();

    // Changed tables first: what they drop or retype decides which foreign keys must go.
    let dropped_names: HashSet<String> = dropped.iter().map(|o| g.cx.key(&o.name)).collect();
    let mut touched: HashMap<String, HashSet<String>> = HashMap::new();
    let mut rebuilt = false;
    let mut alter_out = vec![];
    for o in &altered {
        let (s, t) = (&src.tables[o.source.unwrap_or_default()], &tgt.tables[o.target.unwrap_or_default()]);
        let delta = table_delta(&g.cx, s, t);
        let mut cols: HashSet<String> = delta.dropped.iter().map(|c| g.cx.key(&c.name)).collect();
        cols.extend(delta.changed.iter().filter(|c| c.ty).map(|c| g.cx.key(&c.tgt.name)));
        if delta.pk {
            cols.extend(g.cx.keys(&t.details.primary_key));
        }
        for x in delta.indexes_dropped.iter().filter(|x| x.unique) {
            cols.extend(g.cx.keys(&x.columns));
        }
        touched.insert(g.cx.key(&t.name), cols);
        if sqlite {
            if g.sqlite_in_place(&delta, s) {
                for x in &delta.indexes_dropped {
                    alter_out.push(format!("drop index {}", g.q(&x.name)));
                }
                for c in &delta.added {
                    alter_out.push(format!("alter table {} add column {}", g.q(&t.name), g.column_sql(c, None)));
                }
                for x in &delta.indexes_added {
                    alter_out.extend(g.create_index(&t.name, x));
                }
            } else {
                rebuilt = true;
                let mut pre = std::mem::take(&mut p.pre);
                alter_out.extend(g.sqlite_rebuild(&mut pre, s, t));
                p.pre = pre;
            }
            continue;
        }
        for c in &delta.added {
            g.generated_warning(&t.name, c);
            g.sequence_for(&mut p.pre, c);
            let default = c.default.as_deref().unwrap_or("").trim();
            if !c.nullable && default.is_empty() && c.extra.trim().is_empty() {
                g.warnings.push(trf("Adding NOT NULL column “{name}” without a default fails if “{table}” has rows", &[("name", &c.name), ("table", &t.name)]));
            }
        }
        for c in delta.changed.iter().filter(|c| c.default) {
            g.sequence_for(&mut p.pre, c.src);
        }
        let (columns, indexes) = g.edit(s, t);
        // MySQL repeats a column's EXTRA in MODIFY: give it the source's.
        let mut details = t.details.clone();
        for c in &delta.changed {
            details.columns[c.tgt_index].extra.clone_from(&c.src.extra);
        }
        let edit = TableEdit { schema: &tgt.schema, table: &t.name, new_name: &t.name, columns: &columns, indexes: &indexes };
        match alter::plan(d, &details, &edit) {
            Ok(plan) => {
                alter_out.extend(plan.statements);
                g.warnings.extend(plan.warnings);
            }
            Err(e) => g.warnings.push(trf("Table “{name}”: {error}", &[("name", &t.name), ("error", &e)])),
        }
        // MySQL: a MODIFY from `alter::plan` already carries the new EXTRA.
        let mysql = matches!(d, Driver::MySql | Driver::MariaDb);
        for c in delta.changed.iter().filter(|c| c.extra && !(mysql && (c.ty || c.null || c.default))) {
            alter_out.extend(g.extra_sql(&t.name, c));
        }
    }

    // Foreign keys of the target that must go first, and those to (re)create at the end.
    let mut add_fks: Vec<(String, String)> = vec![];
    let mut push_add = |table: &str, clause: String| {
        if !add_fks.iter().any(|(t, c)| t == table && *c == clause) {
            add_fks.push((table.to_string(), clause));
        }
    };
    if !sqlite {
        let altered_names: HashSet<String> = altered.iter().map(|o| g.cx.key(&o.name)).collect();
        let src_tables: HashMap<String, &TableModel> = src.tables.iter().map(|t| (g.cx.key(&t.name), t)).collect();
        for t in &tgt.tables {
            let tkey = g.cx.key(&t.name);
            let source = src_tables.get(&tkey);
            for fk in &t.foreign_keys {
                let own_ref = fk.ref_schema.is_empty() || fk.ref_schema.eq_ignore_ascii_case(&tgt.schema);
                let rkey = g.cx.key(&fk.ref_table);
                let key = g.cx.fk_key(tgt, fk);
                let in_source = source.and_then(|s| s.foreign_keys.iter().find(|f| g.cx.fk_key(src, f) == key));
                let gone = altered_names.contains(&tkey) && in_source.is_none();
                let parent_dropped = own_ref && dropped_names.contains(&rkey);
                let touches = |table: &str, cols: &[String]| touched.get(table).is_some_and(|set| g.cx.keys(cols).iter().any(|c| set.contains(c)));
                let rebuilt_key = touches(&tkey, &fk.columns) || (own_ref && touches(&rkey, &fk.ref_columns));
                // A dropped table takes its keys with it, unless its parent is dropped too
                // (the parent may go first).
                let owner_dropped = dropped_names.contains(&tkey);
                let drop = if owner_dropped { parent_dropped } else { gone || parent_dropped || rebuilt_key };
                if fk.name.is_empty() || !drop {
                    continue;
                }
                p.drop_fks.push(g.drop_fk(&t.name, fk));
                if rebuilt_key && !gone && !parent_dropped && !owner_dropped {
                    match in_source {
                        Some(f) => push_add(&t.name, g.fk_clause(f, src)),
                        None if !altered_names.contains(&tkey) => push_add(&t.name, g.fk_clause(fk, tgt)),
                        None => {}
                    }
                }
            }
        }
        for o in &altered {
            let (s, t) = (&src.tables[o.source.unwrap_or_default()], &tgt.tables[o.target.unwrap_or_default()]);
            for f in table_delta(&g.cx, s, t).fks_added {
                push_add(&t.name, g.fk_clause(f, src));
            }
        }
        for o in &created {
            let s = &src.tables[o.source.unwrap_or_default()];
            for f in &s.foreign_keys {
                push_add(&s.name, g.fk_clause(f, src));
            }
        }
    }

    // Views: dropped ones, and changed ones SQLite and PostgreSQL drop and create again
    // (PostgreSQL's OR REPLACE can't change the columns). PostgreSQL also drops the views on
    // dropped or retyped columns, which block the ALTER, and the views on those views; the ones
    // not chosen come back with their current definition.
    let views = |status: Status| chosen.iter().filter(move |o| o.kind == Kind::View && o.status == status).copied().collect::<Vec<_>>();
    let mut drop_views: Vec<&ViewModel> = views(Status::OnlyInTarget).iter().map(|o| &tgt.views[o.target.unwrap_or_default()]).collect();
    // (view to create, from the source?)
    let mut create_views: Vec<(&ViewModel, bool)> = views(Status::OnlyInSource).iter().map(|o| (&src.views[o.source.unwrap_or_default()], true)).collect();
    let mut queue: Vec<&ViewModel> = vec![];
    for o in views(Status::Different) {
        let (s, t) = (&src.views[o.source.unwrap_or_default()], &tgt.views[o.target.unwrap_or_default()]);
        if sqlite || d == Driver::Postgres {
            queue.push(t);
        }
        create_views.push((s, true));
    }
    if d == Driver::Postgres {
        let blocked: HashSet<String> =
            touched.iter().filter(|(_, c)| !c.is_empty()).map(|(t, _)| t.to_lowercase()).chain(dropped_names.iter().map(|t| t.to_lowercase())).collect();
        queue.extend(tgt.views.iter().filter(|v| idents(&v.definition.body).iter().any(|w| blocked.contains(w))));
    }
    while let Some(v) = queue.pop() {
        if drop_views.iter().any(|x| std::ptr::eq(*x, v)) {
            continue;
        }
        drop_views.push(v);
        if d == Driver::Postgres {
            let name = v.name.to_lowercase();
            queue.extend(tgt.views.iter().filter(|w| idents(&w.definition.body).contains(&name)));
        }
        if !create_views.iter().any(|(c, _)| g.cx.key(&c.name) == g.cx.key(&v.name)) {
            create_views.push((v, false));
        }
    }
    // Dependents first when dropping, after their bases when creating.
    let order = |list: &[&ViewModel]| -> Vec<usize> {
        let names: Vec<String> = list.iter().map(|v| v.name.to_lowercase()).collect();
        let deps: Vec<HashSet<usize>> = list.iter().map(|v| {
            let words = idents(&v.definition.body);
            names.iter().enumerate().filter(|(_, n)| words.contains(*n)).map(|(i, _)| i).collect()
        }).collect();
        let mut done: Vec<usize> = vec![];
        while done.len() < list.len() {
            let next = (0..list.len()).find(|i| !done.contains(i) && deps[*i].iter().all(|j| *j == *i || done.contains(j)));
            done.push(next.unwrap_or_else(|| (0..list.len()).find(|i| !done.contains(i)).unwrap_or_default()));
        }
        done
    };
    for i in order(&drop_views).into_iter().rev() {
        p.drop_views.push(object_sql(d, DropKind::View, &tgt.schema, &drop_views[i].name, ""));
    }
    let create_list: Vec<&ViewModel> = create_views.iter().map(|(v, _)| *v).collect();
    for i in order(&create_list) {
        let (v, from_source) = create_views[i];
        let dropped_first = drop_views.iter().any(|x| g.cx.key(&x.name) == g.cx.key(&v.name));
        let body = if from_source { g.moved(&v.definition.body) } else { v.definition.body.clone() };
        let columns = if from_source { g.moved(&v.definition.columns) } else { v.definition.columns.clone() };
        let spec = ViewSpec { schema: tgt.schema.clone(), name: v.name.clone(), columns, body, algorithm: v.definition.algorithm.clone(), security: v.definition.security.clone(), ..Default::default() };
        p.create_views.push(create_view_sql(d, &spec, &v.name, !sqlite && !dropped_first));
    }

    for o in &dropped {
        p.drop_tables.push(object_sql(d, DropKind::Table, &tgt.schema, &o.name, ""));
    }
    // Created tables: SQLite in foreign key order (parents first), for readability.
    let mut pending: Vec<&TableModel> = created.iter().map(|o| &src.tables[o.source.unwrap_or_default()]).collect();
    while !pending.is_empty() {
        let names: HashSet<String> = pending.iter().map(|t| g.cx.key(&t.name)).collect();
        let next = pending
            .iter()
            .position(|t| t.foreign_keys.iter().all(|f| !names.contains(&g.cx.key(&f.ref_table)) || g.cx.key(&f.ref_table) == g.cx.key(&t.name)))
            .unwrap_or(0);
        let t = pending.remove(next);
        let mut pre = std::mem::take(&mut p.pre);
        let stmts = g.create_table(&mut pre, t, &t.name);
        p.pre = pre;
        p.create_tables.extend(stmts);
    }
    p.alter_tables = alter_out;
    p.add_fks = add_fks.into_iter().map(|(t, clause)| format!("alter table {} add {clause}", g.object(&t))).collect();
    if sqlite && (rebuilt || !p.drop_tables.is_empty()) {
        p.pre.insert(0, "pragma foreign_keys = off".into());
    }
    if sqlite && rebuilt {
        // Renaming the rebuilt table must not re-check the views on the old one (see alter.rs).
        p.pre.insert(1, "pragma legacy_alter_table = on".into());
        g.warnings.push(tr("SQLite rebuilds changed tables: their CHECK constraints, collations and triggers are not carried over.").into());
    }
    let Phases { pre, drop_fks, drop_views, drop_tables, create_tables, alter_tables, add_fks, create_views } = p;
    let statements = [pre, drop_fks, drop_views, drop_tables, create_tables, alter_tables, add_fks, create_views].concat();
    let mut warnings = vec![];
    for w in g.warnings {
        if !warnings.contains(&w) {
            warnings.push(w);
        }
    }
    Ok(Script { statements, warnings })
}

/// Synthetic `shop` schemas (production and staging) with every kind of difference.
#[cfg(test)]
pub(crate) fn demo_models() -> (SchemaModel, SchemaModel) {
    let col = |name: &str, ty: &str, nullable: bool, default: Option<&str>| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        default: default.map(str::to_string),
        ..Default::default()
    };
    let index = |name: &str, columns: &[&str], unique: bool| IndexDef { name: name.into(), columns: columns.iter().map(|c| c.to_string()).collect(), unique, constraint: false, plain: true };
    let fk = |name: &str, columns: &[&str], table: &str, schema: &str| ForeignKeyDef {
        name: name.into(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        ref_schema: schema.into(),
        ref_table: table.into(),
        ref_columns: vec!["id".into()],
        on_delete: "NO ACTION".into(),
        on_update: "NO ACTION".into(),
    };
    let table = |name: &str, columns: Vec<ColumnDef>, pk: &[&str], indexes: Vec<IndexDef>, foreign_keys: Vec<ForeignKeyDef>| TableModel {
        name: name.into(),
        details: TableDetails { columns, primary_key: pk.iter().map(|c| c.to_string()).collect(), pk_name: Some(format!("{name}_pkey")), indexes, ..Default::default() },
        foreign_keys,
    };
    let view = |name: &str, body: &str| ViewModel { name: name.into(), definition: ViewDefinition { body: body.into(), ..Default::default() } };
    let build = |schema: &str, prod: bool| {
        let serial = |table: &str| format!("nextval('{schema}.{table}_id_seq'::regclass)");
        let id = |table: &str| col("id", "integer", false, Some(&serial(table)));
        let mut orders = vec![
            id("orders"),
            col("customer_id", "integer", false, None),
            col("status", if prod { "character varying(20)" } else { "character varying(10)" }, false, prod.then_some("'pending'::character varying")),
            col("total", if prod { "numeric(12,2)" } else { "numeric(10,2)" }, false, None),
            col("created_at", "timestamp with time zone", false, Some("now()")),
        ];
        if prod {
            orders.push(col("shipped_at", "timestamp with time zone", true, None));
        }
        let mut order_indexes = vec![index("orders_customer_idx", &["customer_id"], false)];
        if prod {
            order_indexes.push(index("orders_status_idx", &["status", "created_at"], false));
        }
        let mut tables = vec![
            table(
                "customers",
                vec![
                    id("customers"),
                    col("email", "character varying(255)", false, None),
                    col("name", "character varying(120)", false, None),
                    col("created_at", "timestamp with time zone", false, Some("now()")),
                ],
                &["id"],
                vec![index("customers_email_key", &["email"], true)],
                vec![],
            ),
            table("orders", orders, &["id"], order_indexes, vec![fk("orders_customer_id_fkey", &["customer_id"], "customers", schema)]),
            table(
                "order_items",
                vec![
                    col("order_id", "integer", false, None),
                    col("product_id", "integer", false, None),
                    col("quantity", "integer", false, Some("1")),
                    col("unit_price", "numeric(12,2)", false, None),
                ],
                &["order_id", "product_id"],
                vec![],
                vec![fk("order_items_order_id_fkey", &["order_id"], "orders", schema), fk("order_items_product_id_fkey", &["product_id"], "products", schema)],
            ),
            table(
                "products",
                vec![
                    id("products"),
                    col("sku", "character varying(40)", false, None),
                    col("name", "character varying(200)", false, None),
                    col("price", "numeric(10,2)", false, None),
                    col("active", "boolean", false, Some("true")),
                ],
                &["id"],
                vec![index("products_sku_key", &["sku"], true)],
                vec![],
            ),
        ];
        let mut views = vec![view("active_products", &format!(" SELECT p.id,\n    p.sku,\n    p.name\n   FROM {schema}.products p\n  WHERE p.active;"))];
        if prod {
            tables.push(table(
                "payments",
                vec![id("payments"), col("order_id", "integer", false, None), col("amount", "numeric(12,2)", false, None), col("paid_at", "timestamp with time zone", true, None)],
                &["id"],
                vec![index("payments_order_idx", &["order_id"], false)],
                vec![fk("payments_order_id_fkey", &["order_id"], "orders", schema)],
            ));
            views.push(view(
                "order_totals",
                &format!(" SELECT o.id,\n    o.status,\n    sum(i.quantity::numeric * i.unit_price) AS total\n   FROM {schema}.orders o\n     JOIN {schema}.order_items i ON i.order_id = o.id\n  WHERE o.status::text <> 'cancelled'::text\n  GROUP BY o.id, o.status;"),
            ));
        } else {
            tables.push(table("coupons", vec![id("coupons"), col("code", "character varying(30)", false, None), col("percent", "integer", false, None)], &["id"], vec![], vec![]));
            views.push(view(
                "order_totals",
                &format!(" SELECT o.id,\n    o.status,\n    sum(i.quantity::numeric * i.unit_price) AS total\n   FROM {schema}.orders o\n     JOIN {schema}.order_items i ON i.order_id = o.id\n  GROUP BY o.id, o.status;"),
            ));
        }
        SchemaModel { driver: Driver::Postgres, db: "shop".into(), schema: schema.into(), tables, views }
    };
    (build("shop", true), build("shop_staging", false))
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod live_tests;
