//! Data editing in the grid: pending changes and the SQL that applies them.
//!
//! Values travel as text (just as they are read) and are written as string literals;
//! all three engines implicitly convert `'42'` to the column type. Redis and MongoDB get
//! their own commands instead (`change_statements`), with values converted to the field's type.

use crate::db::{Cell, Driver, Session};
use std::collections::HashMap;

/// A pending change on a row. `key` identifies the row: the PK columns or, if the
/// table has none, every column with its original value.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Update { key: Vec<(String, Cell)>, set: Vec<(String, Cell)> },
    Insert { values: Vec<(String, Cell)> },
    Delete { key: Vec<(String, Cell)> },
}

/// Identifier quoted for the dialect.
pub fn quote_ident(driver: Driver, name: &str) -> String {
    driver.quote_ident(name)
}

/// SQL literal for a value. `None` = NULL. SQL Server uses `N'..'` so Unicode isn't lost.
pub fn literal(driver: Driver, value: &Cell) -> String {
    match value {
        None => "NULL".into(),
        Some(v) => {
            let escaped = v.replace('\'', "''");
            match driver {
                Driver::MsSql => format!("N'{escaped}'"),
                // MySQL and Snowflake also treat a backslash as an escape inside strings; ClickHouse
                // does the same inside '…' (its `"…"` stays an identifier).
                Driver::MySql | Driver::MariaDb | Driver::Snowflake | Driver::ClickHouse => format!("'{}'", escaped.replace('\\', "\\\\")),
                Driver::Postgres | Driver::Sqlite | Driver::Oracle | Driver::Redis | Driver::MongoDb => format!("'{escaped}'"),
            }
        }
    }
}

fn where_clause(driver: Driver, key: &[(String, Cell)]) -> String {
    key.iter()
        .map(|(col, val)| match val {
            None => format!("{} IS NULL", quote_ident(driver, col)),
            Some(_) => format!("{} = {}", quote_ident(driver, col), literal(driver, val)),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Statement that applies `change` to `schema.table`. Each UPDATE/DELETE must affect
/// exactly one row: `Session::apply_changes` checks it and otherwise rolls everything back.
pub fn change_sql(driver: Driver, schema: &str, table: &str, change: &Change) -> String {
    let target = Session::quote_table(driver, schema, table);
    match change {
        Change::Update { key, set } => {
            let assignments = set
                .iter()
                .map(|(col, val)| format!("{} = {}", quote_ident(driver, col), literal(driver, val)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("UPDATE {target} SET {assignments} WHERE {}", where_clause(driver, key))
        }
        Change::Insert { values } => {
            if values.is_empty() {
                return match driver {
                    Driver::MySql | Driver::MariaDb => format!("INSERT INTO {target} () VALUES ()"),
                    // Oracle has no DEFAULT VALUES; this covers single-column tables.
                    Driver::Oracle => format!("INSERT INTO {target} VALUES (DEFAULT)"),
                    _ => format!("INSERT INTO {target} DEFAULT VALUES"),
                };
            }
            let cols = values.iter().map(|(c, _)| quote_ident(driver, c)).collect::<Vec<_>>().join(", ");
            let vals = values.iter().map(|(_, v)| literal(driver, v)).collect::<Vec<_>>().join(", ");
            format!("INSERT INTO {target} ({cols}) VALUES ({vals})")
        }
        Change::Delete { key } => format!("DELETE FROM {target} WHERE {}", where_clause(driver, key)),
    }
}

/// Statements that save `changes` to `schema.table`: SQL, or the engine's own commands on
/// Redis (one or more command lines per change) and MongoDB (one `mongosh` command per change).
/// `types` only matters there: MongoDB, each field's sampled type; Redis, each key's type.
pub fn change_statements(
    driver: Driver,
    schema: &str,
    table: &str,
    changes: &[Change],
    types: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(changes.len());
    for ch in changes {
        match driver {
            Driver::Redis => out.extend(crate::db::redis_db::change_commands(ch, types)?),
            Driver::MongoDb => out.push(crate::db::mongo::change_command(table, ch, types)?),
            _ => out.push(change_sql(driver, schema, table, ch)),
        }
    }
    Ok(out)
}

/// true for values the grid shows truncated (BLOB/binary in hex with "…"): they can't
/// be edited or used as a key because they aren't the real value.
pub fn is_lossy_display(value: &Cell) -> bool {
    value.as_deref().is_some_and(|v| v.starts_with("0x") && v.ends_with('…'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(c: &str, v: Option<&str>) -> (String, Cell) {
        (c.to_string(), v.map(str::to_string))
    }

    #[test]
    fn update_with_pk_and_quotes() {
        let ch = Change::Update { key: vec![kv("id", Some("7"))], set: vec![kv("name", Some("O'Brien")), kv("note", None)] };
        assert_eq!(
            change_sql(Driver::MsSql, "dbo", "users", &ch),
            "UPDATE [dbo].[users] SET [name] = N'O''Brien', [note] = NULL WHERE [id] = N'7'"
        );
        assert_eq!(
            change_sql(Driver::Postgres, "public", "users", &ch),
            "UPDATE \"public\".\"users\" SET \"name\" = 'O''Brien', \"note\" = NULL WHERE \"id\" = '7'"
        );
    }

    #[test]
    fn null_key_uses_is_null() {
        let ch = Change::Delete { key: vec![kv("a", Some("1")), kv("b", None)] };
        assert_eq!(change_sql(Driver::Sqlite, "main", "t", &ch), "DELETE FROM \"t\" WHERE \"a\" = '1' AND \"b\" IS NULL");
    }

    #[test]
    fn insert_with_and_without_values() {
        let ch = Change::Insert { values: vec![kv("x", Some("1")), kv("y", None)] };
        assert_eq!(change_sql(Driver::Sqlite, "main", "t", &ch), "INSERT INTO \"t\" (\"x\", \"y\") VALUES ('1', NULL)");
        let empty = Change::Insert { values: vec![] };
        assert_eq!(change_sql(Driver::MsSql, "dbo", "t", &empty), "INSERT INTO [dbo].[t] DEFAULT VALUES");
    }

    #[test]
    fn odd_identifiers() {
        assert_eq!(quote_ident(Driver::MsSql, "a]b"), "[a]]b]");
        assert_eq!(quote_ident(Driver::Postgres, "a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident(Driver::ClickHouse, r"a`b\"), r"`a\`b\\`");
        assert!(is_lossy_display(&Some("0xFF…".into())));
        assert!(!is_lossy_display(&Some("0xFF".into())));
    }
}
