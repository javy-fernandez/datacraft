//! Foreign-key links of a result grid: which result columns hold which declared key, and the
//! jump a key cell's follow button or the row menu asks the app for.

use crate::db::fk_nav::TableKeys;
use crate::db::subset::ForeignKey;
use crate::db::{Cell, ColumnInfo};

/// Keys of the result's table mapped onto the result's columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Links {
    /// Keys whose child columns are all in the result, with those result columns.
    pub outgoing: Vec<(ForeignKey, Vec<usize>)>,
    /// Keys referencing the table, with the result columns of the referenced columns (`None`
    /// when the result lacks one of them).
    pub incoming: Vec<(ForeignKey, Option<Vec<usize>>)>,
    pub inferred: Vec<(ForeignKey, Vec<usize>)>,
}

/// A navigation asked for by the grid.
#[derive(Clone, Debug, PartialEq)]
pub enum Jump {
    Choose { targets: Vec<Target>, anchor: egui::Pos2 },
    Null,
    /// The parent row of `key` whose referenced columns hold `values`.
    Parent { key: ForeignKey, values: Vec<Cell> },
    /// "Referenced by…" for one row: each incoming key with the row's referenced values.
    Children { keys: Vec<(ForeignKey, Option<Vec<Cell>>)> },
}

/// Table column of each result column: the exact name, else the only case-insensitive match.
/// A table column the result shows twice, and binary columns (shown in hex), map to nothing.
pub fn column_names(result_cols: &[String], columns: &[ColumnInfo]) -> Vec<Option<String>> {
    let found: Vec<Option<&ColumnInfo>> = result_cols
        .iter()
        .map(|rc| {
            columns.iter().find(|c| c.name == *rc).or_else(|| {
                let mut same = columns.iter().filter(|c| c.name.eq_ignore_ascii_case(rc));
                same.next().filter(|_| same.next().is_none())
            })
        })
        .collect();
    found
        .iter()
        .map(|c| {
            c.filter(|c| !crate::ui::editing::is_binary_type(&c.data_type) && found.iter().filter(|o| o.is_some_and(|o| o.name == c.name)).count() == 1)
                .map(|c| c.name.clone())
        })
        .collect()
}

/// Candidate resolution is pure: it only reads cached keys, schema and stored cell values.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    key: ForeignKey,
    values: Vec<Cell>,
    kind: &'static str,
}

impl Target {
    pub fn label(&self) -> String {
        let mapping = self.key.columns.iter().zip(&self.key.ref_columns).map(|(from, to)| format!("{from} → {}.{to}", self.key.ref_table)).collect::<Vec<_>>().join(", ");
        format!("{}.{}   · {}   · {mapping}", self.key.ref_schema, self.key.ref_table, self.kind)
    }
    pub fn jump(self) -> Jump { Jump::Parent { key: self.key, values: self.values } }
}

impl Links {
    /// Infer only a conventional `<table>_id` → single-column primary key relation with
    /// matching types. These are candidates, never claims that a constraint exists.
    pub fn with_schema(keys: &TableKeys, names: &[Option<String>], schemas: &[crate::db::SchemaInfo], schema: &str, table: &str) -> Option<Self> {
        let mut links = Self::new(keys, names).unwrap_or_default();
        let source = schemas.iter().find(|s| s.name == schema)?.tables.iter().find(|t| t.name == table)?;
        for s in schemas {
            for target in &s.tables {
                if target.kind != crate::db::ObjectKind::Table { continue; }
                for (from_schema, from, to_schema, to) in [(schema, source, s.name.as_str(), target), (s.name.as_str(), target, schema, source)] {
                    let pk: Vec<_> = to.columns.iter().filter(|c| c.primary_key).collect();
                    if pk.len() != 1 { continue; }
                    let singular = to.name.strip_suffix('s').unwrap_or(&to.name);
                    for column in &from.columns {
                        if ![format!("{singular}_id"), format!("{}_id", to.name)].iter().any(|n| n.eq_ignore_ascii_case(&column.name)) || !column.data_type.eq_ignore_ascii_case(&pk[0].data_type) { continue; }
                        let key = ForeignKey { schema: from_schema.into(), table: from.name.clone(), columns: vec![column.name.clone()], ref_schema: to_schema.into(), ref_table: to.name.clone(), ref_columns: vec![pk[0].name.clone()] };
                        if keys.outgoing.contains(&key) || keys.incoming.contains(&key) { continue; }
                        let key = if from_schema == schema && from.name == table { key } else { reverse(key) };
                        let Some(col) = names.iter().position(|n| n.as_ref() == key.columns.first()) else { continue; };
                        if !links.inferred.iter().any(|(k, _)| k == &key) { links.inferred.push((key, vec![col])); }
                    }
                }
            }
        }
        (!links.outgoing.is_empty() || !links.incoming.is_empty() || !links.inferred.is_empty()).then_some(links)
    }

    pub fn targets(&self, col: usize, row: &[Cell]) -> Vec<Target> {
        let mut targets = Vec::new();
        let mut add = |key: ForeignKey, cols: &[usize], kind| {
            if !cols.contains(&col) || cols.iter().any(|&c| row.get(c).is_none_or(crate::edit::is_lossy_display)) { return; }
            targets.push(Target { key, values: cols.iter().map(|&c| row[c].clone()).collect(), kind });
        };
        for (key, cols) in &self.outgoing { add(key.clone(), cols, "FK"); }
        for (key, cols) in &self.incoming { if let Some(cols) = cols { add(reverse(key.clone()), cols, "reverse FK"); } }
        for (key, cols) in &self.inferred { add(key.clone(), cols, "inferred"); }
        targets.sort_by_key(|t| (match t.kind { "FK" => 0, "reverse FK" => 1, _ => 2 }, t.key.ref_schema.clone(), t.key.ref_table.clone(), t.key.ref_columns.clone()));
        targets
    }

    /// `None` when no key can be followed from this result.
    pub fn new(keys: &TableKeys, names: &[Option<String>]) -> Option<Self> {
        let at = |cols: &[String]| cols.iter().map(|c| names.iter().position(|n| n.as_deref() == Some(c.as_str()))).collect::<Option<Vec<_>>>();
        let outgoing: Vec<_> = keys.outgoing.iter().filter_map(|k| Some((k.clone(), at(&k.columns)?))).collect();
        let incoming: Vec<_> = keys.incoming.iter().map(|k| (k.clone(), at(&k.ref_columns))).collect();
        (!outgoing.is_empty() || !incoming.is_empty()).then_some(Self { outgoing, incoming, inferred: Vec::new() })
    }

    /// Outgoing keys (indices) that use result column `col`.
    pub fn keys_at(&self, col: usize) -> impl Iterator<Item = usize> + '_ {
        self.outgoing.iter().enumerate().filter(move |(_, (_, cols))| cols.contains(&col)).map(|(i, _)| i)
    }

    /// First key at `col` that `row` can follow (no NULL in its columns).
    #[cfg(test)]
    pub fn link_at(&self, col: usize, row: &[Cell]) -> Option<usize> {
        self.keys_at(col).find(|&k| self.outgoing[k].1.iter().all(|&c| row.get(c).is_some_and(|v| v.is_some() && !crate::edit::is_lossy_display(v))))
    }

    pub fn parent(&self, key: usize, row: &[Cell]) -> Option<Jump> {
        let (key, cols) = self.outgoing.get(key)?;
        let values: Vec<Cell> = cols.iter().map(|&c| row.get(c).cloned().flatten()).collect();
        values.iter().all(|v| v.is_some() && !crate::edit::is_lossy_display(v)).then(|| Jump::Parent { key: key.clone(), values })
    }

    pub fn children(&self, row: &[Cell]) -> Option<Jump> {
        (!self.incoming.is_empty()).then(|| Jump::Children {
            keys: self.incoming.iter().map(|(k, cols)| (k.clone(), cols.as_ref().map(|cols| cols.iter().map(|&c| row.get(c).cloned().flatten()).collect()))).collect(),
        })
    }
}

/// Orient every candidate as source → destination, including incoming references.
fn reverse(key: ForeignKey) -> ForeignKey {
    ForeignKey { schema: key.ref_schema, table: key.ref_table, columns: key.ref_columns, ref_schema: key.schema, ref_table: key.table, ref_columns: key.columns }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: true, primary_key: false }
    }

    fn key(table: &str, columns: &[&str], ref_table: &str, ref_columns: &[&str]) -> ForeignKey {
        let v = |c: &[&str]| c.iter().map(|s| s.to_string()).collect();
        ForeignKey { schema: "s".into(), table: table.into(), columns: v(columns), ref_schema: "s".into(), ref_table: ref_table.into(), ref_columns: v(ref_columns) }
    }

    #[test]
    fn candidates_include_all_directions_self_references_and_inference_in_order() {
        let outgoing = key("orders", &["id"], "archive", &["id"]);
        let self_key = key("orders", &["parent_id"], "orders", &["id"]);
        let keys = TableKeys { outgoing: vec![outgoing, self_key.clone()], incoming: vec![self_key, key("lines", &["order_id"], "orders", &["id"])] };
        let mut id = col("id", "int"); id.primary_key = true;
        let schemas = vec![crate::db::SchemaInfo { name: "s".into(), tables: vec![
            crate::db::TableInfo { name: "orders".into(), kind: crate::db::ObjectKind::Table, columns: vec![id.clone(), col("parent_id", "int")] },
            crate::db::TableInfo { name: "notes".into(), kind: crate::db::ObjectKind::Table, columns: vec![id, col("order_id", "int")] },
        ] }];
        let links = Links::with_schema(&keys, &[Some("id".into()), Some("parent_id".into())], &schemas, "s", "orders").unwrap();
        let targets = links.targets(0, &[Some("42".into()), Some("7".into())]);
        assert_eq!(targets.iter().map(|t| (t.kind, t.key.ref_table.as_str())).collect::<Vec<_>>(), [("FK", "archive"), ("reverse FK", "lines"), ("reverse FK", "orders"), ("inferred", "notes")]);
        assert_eq!(targets[1].key.ref_columns, ["order_id"]);
        assert_eq!(targets[1].values, [Some("42".into())]);
        assert!(links.targets(9, &[None, None]).is_empty());
        assert!(links.targets(0, &[None, None])[0].values[0].is_none());
        assert_eq!(links.targets(1, &[None, Some("7".into())])[0].key.ref_table, "orders");
    }

    #[test]
    fn result_columns_map_only_unambiguously() {
        let columns = [col("Id", "int"), col("id", "int"), col("Name", "text"), col("photo", "bytea")];
        let names = column_names(&["Id".into(), "name".into(), "ID".into(), "photo".into(), "extra".into()], &columns);
        assert_eq!(names, [Some("Id".into()), Some("Name".into()), None, None, None]);
        let twice = column_names(&["Name".into(), "name".into()], &columns);
        assert_eq!(twice, [None, None]);
    }

    #[test]
    fn composite_links_need_every_column_and_skip_nulls() {
        let keys = TableKeys {
            outgoing: vec![key("lines", &["order_id", "shop"], "orders", &["id", "shop"]), key("lines", &["missing"], "x", &["id"])],
            incoming: vec![key("notes", &["line_id"], "lines", &["id"]), key("audit", &["l"], "lines", &["hidden"])],
        };
        let names = ["id", "shop", "order_id"].map(|n| Some(n.to_string()));
        let links = Links::new(&keys, &names).unwrap();
        assert_eq!(links.outgoing.len(), 1);
        assert_eq!(links.outgoing[0].1, [2, 1]);
        let row = vec![Some("9".into()), Some("north".into()), Some("4".into())];
        assert_eq!(links.link_at(1, &row), Some(0));
        assert_eq!(links.link_at(0, &row), None);
        assert_eq!(links.parent(0, &row), Some(Jump::Parent { key: keys.outgoing[0].clone(), values: vec![Some("4".into()), Some("north".into())] }));
        let null = vec![Some("9".into()), None, Some("4".into())];
        assert_eq!((links.link_at(2, &null), links.parent(0, &null)), (None, None));
        let Some(Jump::Children { keys: children }) = links.children(&row) else { panic!() };
        assert_eq!(children[0].1, Some(vec![Some("9".into())]));
        assert_eq!(children[1].1, None);
        assert!(Links::new(&TableKeys::default(), &names).is_none());
    }
}
