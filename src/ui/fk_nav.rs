//! Foreign-key links of a result grid: which result columns hold which declared key, and the
//! jump a Ctrl/⌘-click or the row menu asks the app for.

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
}

/// A navigation asked for by the grid.
#[derive(Clone, Debug, PartialEq)]
pub enum Jump {
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

impl Links {
    /// `None` when no key can be followed from this result.
    pub fn new(keys: &TableKeys, names: &[Option<String>]) -> Option<Self> {
        let at = |cols: &[String]| cols.iter().map(|c| names.iter().position(|n| n.as_deref() == Some(c.as_str()))).collect::<Option<Vec<_>>>();
        let outgoing: Vec<_> = keys.outgoing.iter().filter_map(|k| Some((k.clone(), at(&k.columns)?))).collect();
        let incoming: Vec<_> = keys.incoming.iter().map(|k| (k.clone(), at(&k.ref_columns))).collect();
        (!outgoing.is_empty() || !incoming.is_empty()).then_some(Self { outgoing, incoming })
    }

    /// Outgoing keys (indices) that use result column `col`.
    pub fn keys_at(&self, col: usize) -> impl Iterator<Item = usize> + '_ {
        self.outgoing.iter().enumerate().filter(move |(_, (_, cols))| cols.contains(&col)).map(|(i, _)| i)
    }

    /// First key at `col` that `row` can follow (no NULL in its columns).
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
