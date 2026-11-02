//! Manual transactions of the SQL console: how a statement the user runs affects the
//! transaction it runs in, for engines whose server state the worker cannot ask for.

use super::Driver;

/// What a console statement does to the open transaction, beyond running in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// `BEGIN`, `START TRANSACTION`…: the user opens the transaction.
    Begin,
    /// `COMMIT` / `END`.
    Commit,
    /// `ROLLBACK` / `ABORT`.
    Rollback,
    /// `ROLLBACK TO [SAVEPOINT]`: the transaction stays open (and recovers on PostgreSQL).
    RollbackTo,
    /// DDL that commits the open transaction by itself (MySQL, MariaDB, Oracle, Snowflake).
    ImplicitCommit,
}

/// Leading words of a statement in upper case, skipping comments.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = sql;
    while words.len() < n {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, after)| after);
            continue;
        }
        if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, after)| after);
            continue;
        }
        let len = rest.char_indices().find(|(_, c)| !(c.is_alphanumeric() || *c == '_')).map_or(rest.len(), |(i, _)| i);
        if len == 0 {
            // Punctuation (`;`, `(`…) ends the words that matter here.
            break;
        }
        words.push(rest[..len].to_ascii_uppercase());
        rest = &rest[len..];
    }
    words
}

/// How `statement` affects the console's transaction on `driver` (None: it just runs in it).
pub fn control(driver: Driver, statement: &str) -> Option<Control> {
    let w = leading_words(statement, 3);
    let word = |i: usize| w.get(i).map(String::as_str);
    match word(0)? {
        // Oracle's BEGIN opens a PL/SQL block, T-SQL's a statement block unless it says TRAN;
        // a Snowflake BEGIN followed by anything else is a scripting block.
        "BEGIN" => match driver {
            Driver::Oracle => None,
            Driver::MsSql => matches!(word(1), Some("TRAN" | "TRANSACTION" | "DISTRIBUTED")).then_some(Control::Begin),
            Driver::Snowflake => matches!(word(1), None | Some("WORK" | "TRANSACTION" | "NAME")).then_some(Control::Begin),
            _ => Some(Control::Begin),
        },
        "START" if word(1) == Some("TRANSACTION") => Some(Control::Begin),
        "COMMIT" => Some(Control::Commit),
        "END" if matches!(driver, Driver::Postgres | Driver::Sqlite) => Some(Control::Commit),
        "ABORT" if driver == Driver::Postgres => Some(Control::Rollback),
        "ROLLBACK" => {
            let to = word(1) == Some("TO") || (matches!(word(1), Some("WORK" | "TRANSACTION" | "TRAN")) && word(2) == Some("TO"));
            Some(if to { Control::RollbackTo } else { Control::Rollback })
        }
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "COMMENT" => {
            let implicit = match driver {
                // Temporary tables are the exception on MySQL and MariaDB.
                Driver::MySql | Driver::MariaDb => word(1) != Some("TEMPORARY") && word(0) != Some("COMMENT"),
                Driver::Oracle | Driver::Snowflake => true,
                _ => false,
            };
            implicit.then_some(Control::ImplicitCommit)
        }
        _ => None,
    }
}

/// Server error after which MySQL / MariaDB has rolled the whole transaction back (deadlock).
pub fn rolls_back_on_error(driver: Driver, error: &str) -> bool {
    matches!(driver, Driver::MySql | Driver::MariaDb) && error.contains("Deadlock found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_statements_per_dialect() {
        use Control::*;
        for (driver, sql, expected) in [
            (Driver::Postgres, "begin", Some(Begin)),
            (Driver::Postgres, "  -- note\nBEGIN ISOLATION LEVEL SERIALIZABLE;", Some(Begin)),
            (Driver::Postgres, "start transaction read only", Some(Begin)),
            (Driver::Postgres, "end", Some(Commit)),
            (Driver::Postgres, "abort", Some(Rollback)),
            (Driver::Postgres, "rollback to savepoint s1", Some(RollbackTo)),
            (Driver::Postgres, "ROLLBACK WORK TO s1", Some(RollbackTo)),
            (Driver::Postgres, "create table t (id int)", None),
            (Driver::Sqlite, "BEGIN IMMEDIATE", Some(Begin)),
            (Driver::Sqlite, "end transaction", Some(Commit)),
            (Driver::MsSql, "BEGIN TRAN", Some(Begin)),
            (Driver::MsSql, "BEGIN\n  SELECT 1\nEND", None),
            (Driver::MsSql, "/* c */ COMMIT TRANSACTION", Some(Commit)),
            (Driver::MySql, "START TRANSACTION", Some(Begin)),
            (Driver::MySql, "end", None),
            (Driver::MySql, "create table t (id int)", Some(ImplicitCommit)),
            (Driver::MariaDb, "CREATE TEMPORARY TABLE t (id int)", None),
            (Driver::Oracle, "BEGIN dbms_output.put_line('x'); END;", None),
            (Driver::Oracle, "commit", Some(Commit)),
            (Driver::Oracle, "comment on table t is 'x'", Some(ImplicitCommit)),
            (Driver::Snowflake, "BEGIN TRANSACTION", Some(Begin)),
            (Driver::Snowflake, "BEGIN RETURN 1; END", None),
            (Driver::Snowflake, "drop table t", Some(ImplicitCommit)),
            (Driver::Postgres, "select 1", None),
            (Driver::Postgres, "", None),
        ] {
            assert_eq!(control(driver, sql), expected, "{driver:?}: {sql}");
        }
    }

    #[test]
    fn mysql_deadlocks_roll_back() {
        assert!(rolls_back_on_error(Driver::MariaDb, "Deadlock found when trying to get lock; try restarting transaction"));
        assert!(!rolls_back_on_error(Driver::Postgres, "Deadlock found"));
    }
}
