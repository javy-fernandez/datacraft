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
    /// DDL (and on MySQL / MariaDB `LOCK TABLES`, `ANALYZE TABLE`…) that commits the open
    /// transaction by itself (MySQL, MariaDB, Oracle, Snowflake). The console's session never
    /// commits on its own, so the statements after it run in a new transaction.
    ImplicitCommit,
    /// `SET autocommit …` (MySQL, MariaDB) or `ALTER SESSION SET AUTOCOMMIT …` (Snowflake): the
    /// worker turns autocommit off again afterwards.
    Autocommit,
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
    let mysql = matches!(driver, Driver::MySql | Driver::MariaDb);
    let session_setting = (mysql && word(0) == Some("SET")) || (driver == Driver::Snowflake && word(0) == Some("ALTER") && word(1) == Some("SESSION"));
    if session_setting && statement.to_ascii_uppercase().contains("AUTOCOMMIT") {
        return Some(Control::Autocommit);
    }
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
        // PostgreSQL's two-phase COMMIT / ROLLBACK PREPARED end another, prepared transaction;
        // PREPARE TRANSACTION detaches the current one from the session.
        "COMMIT" | "ROLLBACK" if driver == Driver::Postgres && word(1) == Some("PREPARED") => None,
        "PREPARE" if driver == Driver::Postgres && word(1) == Some("TRANSACTION") => Some(Control::Commit),
        "COMMIT" => Some(Control::Commit),
        "END" if matches!(driver, Driver::Postgres | Driver::Sqlite) => Some(Control::Commit),
        "ABORT" if driver == Driver::Postgres => Some(Control::Rollback),
        "ROLLBACK" => {
            let to = word(1) == Some("TO") || (matches!(word(1), Some("WORK" | "TRANSACTION" | "TRAN")) && word(2) == Some("TO"));
            Some(if to { Control::RollbackTo } else { Control::Rollback })
        }
        // Oracle's ALTER SESSION / SYSTEM don't commit; Snowflake's ALTER SESSION is kept as
        // not committing too (the open transaction stays marked open).
        "ALTER" if matches!(driver, Driver::Oracle | Driver::Snowflake) && matches!(word(1), Some("SESSION" | "SYSTEM")) => None,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "COMMENT" => {
            let implicit = match driver {
                // Temporary tables are the exception on MySQL and MariaDB.
                Driver::MySql | Driver::MariaDb => word(1) != Some("TEMPORARY") && word(0) != Some("COMMENT"),
                Driver::Oracle | Driver::Snowflake => true,
                _ => false,
            };
            implicit.then_some(Control::ImplicitCommit)
        }
        // MySQL / MariaDB statements documented to commit implicitly.
        "LOCK" | "UNLOCK" | "ANALYZE" | "OPTIMIZE" | "REPAIR" | "CHECK" | "FLUSH" | "RESET" | "CACHE" | "INSTALL" | "UNINSTALL" if mysql => Some(Control::ImplicitCommit),
        "LOAD" if mysql && word(1) == Some("INDEX") => Some(Control::ImplicitCommit),
        _ => None,
    }
}

/// Server error after which MySQL / MariaDB has rolled the whole transaction back: a deadlock
/// (error 1213, SQLSTATE 40001; the numbers survive the message's translation).
pub fn rolls_back_on_error(driver: Driver, error: &str) -> bool {
    matches!(driver, Driver::MySql | Driver::MariaDb) && error.contains("1213") && error.contains("40001")
}

/// `SET autocommit = 1` and the like: turning autocommit on commits the open transaction.
pub fn enables_autocommit(statement: &str) -> bool {
    let s = statement.to_ascii_uppercase();
    let after = s.split_once("AUTOCOMMIT").map_or("", |(_, v)| v);
    let value: String = after.trim_start_matches(|c: char| c.is_whitespace() || c == '=').chars().take_while(|c| c.is_alphanumeric()).collect();
    matches!(value.as_str(), "1" | "ON" | "TRUE")
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
            (Driver::Postgres, "COMMIT PREPARED 'tx1'", None),
            (Driver::Postgres, "rollback prepared 'tx1'", None),
            (Driver::Postgres, "PREPARE TRANSACTION 'tx1'", Some(Commit)),
            (Driver::Postgres, "prepare q as select 1", None),
            (Driver::Oracle, "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY'", None),
            (Driver::Oracle, "alter system flush shared_pool", None),
            (Driver::Oracle, "alter table t add c int", Some(ImplicitCommit)),
            (Driver::Snowflake, "ALTER SESSION SET QUERY_TAG = 'x'", None),
            (Driver::Snowflake, "alter session set autocommit = true", Some(Autocommit)),
            (Driver::Snowflake, "alter table t add column c int", Some(ImplicitCommit)),
            (Driver::MySql, "LOCK TABLES t WRITE", Some(ImplicitCommit)),
            (Driver::MariaDb, "analyze table t", Some(ImplicitCommit)),
            (Driver::MySql, "FLUSH PRIVILEGES", Some(ImplicitCommit)),
            (Driver::MySql, "LOAD INDEX INTO CACHE t", Some(ImplicitCommit)),
            (Driver::MySql, "LOAD DATA INFILE 'x' INTO TABLE t", None),
            (Driver::MySql, "SET @@session.autocommit = 1", Some(Autocommit)),
            (Driver::MariaDb, "set autocommit=0", Some(Autocommit)),
            (Driver::Postgres, "set lock_timeout = 1000", None),
            (Driver::Postgres, "select 1", None),
            (Driver::Postgres, "", None),
        ] {
            assert_eq!(control(driver, sql), expected, "{driver:?}: {sql}");
        }
    }

    #[test]
    fn mysql_deadlocks_roll_back_whatever_the_language() {
        assert!(rolls_back_on_error(Driver::MariaDb, "Error 1213 (SQLSTATE 40001): Deadlock found when trying to get lock"));
        assert!(rolls_back_on_error(Driver::MySql, "Fehler 1213 (SQLSTATE 40001): Deadlock beim Versuch, eine Sperre zu bekommen"));
        assert!(!rolls_back_on_error(Driver::MySql, "Error 1205 (SQLSTATE HY000): Lock wait timeout exceeded"));
        assert!(!rolls_back_on_error(Driver::Postgres, "ERROR 1213 40001"));
    }

    #[test]
    fn turning_autocommit_on() {
        for (sql, on) in [("SET autocommit=1", true), ("set @@session.autocommit = ON;", true), ("SET autocommit = 1, sql_mode = ''", true), ("ALTER SESSION SET AUTOCOMMIT = TRUE", true), ("SET autocommit=0", false), ("alter session set autocommit = false", false)] {
            assert_eq!(enables_autocommit(sql), on, "{sql}");
        }
    }
}
