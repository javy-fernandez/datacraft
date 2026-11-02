//! Statements that act on whole tables, found before the console runs them: `UPDATE` and
//! `DELETE` without `WHERE` (nor MySQL's `LIMIT` or T-SQL's `TOP`), `TRUNCATE` and `DROP`.
//! Routine bodies (`CREATE FUNCTION`, procedures, triggers, rules) only run when called, so
//! they are not looked into; anonymous blocks (`BEGIN … END`) run now, so their statements are.

use super::{Dialect, Kind, Lexer, Statement, split_batches, split_statements};
use crate::db::Driver;

/// What a statement does to a whole table or object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Danger {
    /// `UPDATE` without `WHERE`: changes every row.
    Update,
    /// `DELETE` without `WHERE`: deletes every row.
    Delete,
    /// `TRUNCATE`: removes every row.
    Truncate,
    /// `DROP`: removes the object.
    Drop,
}

/// A statement the console asks about before running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dangerous {
    pub danger: Danger,
    /// `DROP`'s object type, uppercased (`TABLE`, `MATERIALIZED VIEW`); empty otherwise.
    pub object: String,
    /// The target as written (`public.orders`, `a, b`).
    pub target: String,
}

/// Significant token; `up` is the uppercased text.
struct Tok<'a> {
    kind: Kind,
    text: &'a str,
    up: String,
}

/// Dangerous statements of `script` (one or more statements, as the console sends them), in
/// order. Only SQL engines: Redis, MongoDB, InfluxDB and Kusto have their own languages.
pub fn dangerous_statements(script: &str, driver: Driver) -> Vec<Dangerous> {
    let mut out = Vec::new();
    if matches!(driver, Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto) {
        return out;
    }
    let dialect = Dialect::for_driver(driver);
    let tsql = driver == Driver::MsSql;
    // Oracle and MySQL keep a routine in one statement; elsewhere its body's `;` split it.
    let tracked = matches!(driver, Driver::Oracle | Driver::MySql | Driver::MariaDb);
    // A T-SQL routine is the only statement of its batch: the rest of the batch is its body.
    let batches = if tsql {
        split_batches(script, driver)
    } else {
        vec![Statement { text: script.to_string(), start: 0, end: script.len() }]
    };
    for batch in batches {
        // Open `BEGIN`/`CASE` of a routine body that continues past a `;`.
        let mut body = 0usize;
        for stmt in split_statements(&batch.text, driver) {
            let toks = tokens(&stmt.text, dialect);
            if body > 0 {
                body = body.saturating_add_signed(balance(&toks));
            } else if is_routine(&toks) {
                if tsql {
                    break;
                }
                if !tracked {
                    body = balance(&toks).max(0) as usize;
                }
            } else {
                scan(&toks, tsql, &mut out);
            }
        }
    }
    out
}

fn tokens(text: &str, dialect: Dialect) -> Vec<Tok<'_>> {
    Lexer::new(text, dialect)
        .filter(|t| !matches!(t.kind, Kind::Whitespace | Kind::LineComment | Kind::BlockComment))
        .map(|t| {
            let text = &text[t.start..t.end];
            let up = if t.kind == Kind::Word { text.to_ascii_uppercase() } else { text.to_string() };
            Tok { kind: t.kind, text, up }
        })
        .collect()
}

fn word<'a>(t: &'a [Tok], i: usize) -> &'a str {
    t.get(i).filter(|x| x.kind == Kind::Word).map_or("", |x| x.up.as_str())
}

fn punct(t: &[Tok], i: usize, p: &str) -> bool {
    t.get(i).is_some_and(|x| x.kind != Kind::Word && x.text == p)
}

/// Index of the `)` that closes the `(` at `open` (the end if it is not closed).
fn close(t: &[Tok], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, x) in t.iter().enumerate().skip(open) {
        match x.text {
            "(" if x.kind == Kind::Punct => depth += 1,
            ")" if x.kind == Kind::Punct => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    t.len()
}

/// `CREATE [OR REPLACE | OR ALTER] … FUNCTION / PROCEDURE / TRIGGER …` and T-SQL's `ALTER
/// PROCEDURE`: a routine whose body runs later.
fn is_routine(t: &[Tok]) -> bool {
    let words: Vec<&str> = t.iter().take_while(|x| matches!(x.kind, Kind::Word | Kind::QuotedIdent | Kind::Str) || x.text == "=" || x.text == "@").map(|x| x.up.as_str()).collect();
    let Some((first, rest)) = words.split_first() else { return false };
    if !matches!(*first, "CREATE" | "ALTER") {
        return false;
    }
    for w in rest.iter().take(8) {
        match *w {
            "FUNCTION" | "PROCEDURE" | "PROC" | "TRIGGER" | "PACKAGE" | "TYPE" | "EVENT" | "RULE" => return true,
            "OR" | "REPLACE" | "ALTER" | "TEMP" | "TEMPORARY" | "EDITIONABLE" | "NONEDITIONABLE" | "DEFINER" | "=" | "@"
            | "AGGREGATE" | "CONSTRAINT" | "SECURE" => {}
            // `DEFINER = user@host` (MySQL), quoted or not.
            _ if rest.contains(&"DEFINER") => {}
            _ => return false,
        }
    }
    false
}

/// Blocks a statement opens minus those it closes: `BEGIN` and `CASE` against `END` (`END IF`
/// and `END LOOP` close blocks that are not counted).
fn balance(t: &[Tok]) -> isize {
    let mut n = 0isize;
    for (i, x) in t.iter().enumerate().filter(|(_, x)| x.kind == Kind::Word) {
        match x.up.as_str() {
            "BEGIN" | "CASE" => n += 1,
            "END" if !matches!(word(t, i + 1), "IF" | "LOOP" | "WHILE" | "REPEAT" | "FOR") => n -= 1,
            _ => {}
        }
    }
    n
}

/// Words that start a T-SQL statement, which needs no `;` before the next one.
const TSQL_STARTS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "DROP", "CREATE", "ALTER", "EXEC", "EXECUTE",
    "DECLARE", "SET", "PRINT", "IF", "ELSE", "WHILE", "BEGIN", "END", "COMMIT", "ROLLBACK", "RETURN", "USE", "GRANT",
    "REVOKE", "DENY", "WITH", "RAISERROR", "THROW", "WAITFOR", "BREAK", "CONTINUE", "GOTO", "SAVE", "OPEN", "CLOSE",
    "FETCH", "DEALLOCATE",
];

/// Statement starts within a statement from [`split_statements`]: its first word, those after a
/// `;` or a block keyword (`BEGIN`, `THEN`, `ELSE`, `LOOP`, `DO`) and, in T-SQL, every statement
/// word that cannot belong to the current statement.
fn heads(t: &[Tok], tsql: bool) -> Vec<usize> {
    let mut out = Vec::new();
    let (mut depth, mut case) = (0usize, 0usize);
    // First word of the current T-SQL statement.
    let mut stmt = "";
    for (i, x) in t.iter().enumerate() {
        match (x.kind, x.text) {
            (Kind::Punct, "(") => depth += 1,
            (Kind::Punct, ")") => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth > 0 || x.kind != Kind::Word {
            continue;
        }
        let w = x.up.as_str();
        let prev = if i == 0 { "" } else { t[i - 1].up.as_str() };
        if tsql && i > 0 {
            // `CASE … ELSE … END` is an expression.
            if w == "CASE" {
                case += 1;
                continue;
            }
            if case > 0 {
                case -= usize::from(w == "END");
                continue;
            }
        }
        let starts = matches!(w, "UPDATE" | "DELETE" | "TRUNCATE" | "DROP" | "WITH");
        let head = i == 0
            || prev == ";"
            // T-SQL has no `THEN` blocks, and its `MERGE … THEN UPDATE` is a clause.
            || (!tsql && starts && matches!(prev, "BEGIN" | "THEN" | "ELSE" | "LOOP" | "DO" | "ATOMIC"))
            || (tsql && tsql_head(w, stmt, prev, t.get(i + 1).map_or("", |n| n.text)));
        if head {
            out.push(i);
            stmt = w;
        }
    }
    out
}

/// Does the T-SQL word `w` (outside parentheses) start a statement, after `prev`, while in a
/// statement that started with `stmt`?
fn tsql_head(w: &str, stmt: &str, prev: &str, next: &str) -> bool {
    // Their own clauses use these words; they end at a `;` (MERGE needs one).
    let clause = matches!(stmt, "MERGE" | "CREATE" | "ALTER" | "GRANT" | "REVOKE" | "DENY")
        || matches!((stmt, w), ("UPDATE", "SET") | ("INSERT", "SELECT" | "EXEC" | "EXECUTE" | "WITH") | ("DROP", "IF"));
    // `FOR UPDATE OF`, `UNION SELECT`, `INSTEAD OF DELETE`, column lists; `UPDATE(column)` in
    // triggers, `WITH (NOLOCK)` table hints.
    let inside = matches!(prev, "FOR" | "OF" | "UNION" | "ALL" | "EXCEPT" | "INTERSECT" | "AFTER" | "INSTEAD" | "BEFORE" | "," | ".")
        || (matches!(w, "UPDATE" | "WITH") && next == "(");
    TSQL_STARTS.contains(&w) && !(clause || inside)
}

/// Classifies each statement of `t` (see [`heads`]).
fn scan(t: &[Tok], tsql: bool, out: &mut Vec<Dangerous>) {
    let heads = heads(t, tsql);
    for (n, &h) in heads.iter().enumerate() {
        let end = heads.get(n + 1).copied().unwrap_or(t.len());
        let end = t[h..end].iter().position(|x| x.kind == Kind::Semicolon).map_or(end, |p| h + p);
        classify(&t[h..end], tsql, out);
    }
}

/// Words after a table name that are not its alias.
const CLAUSES: &[&str] = &[
    "WHERE", "SET", "FROM", "USING", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "OUTER", "NATURAL",
    "STRAIGHT_JOIN", "ON", "LIMIT", "ORDER", "RETURNING", "OUTPUT", "WITH", "OPTION", "PARTITION", "TABLESAMPLE",
    "CASCADE", "RESTRICT", "PURGE", "IF", "WHEN", "VALUES", "SELECT", "FINAL", "SETTINGS", "SYNC", "IN", "INTO",
    "GROUP", "HAVING", "AS", "RESTART", "CONTINUE", "FORCE", "LOG", "ERRORS",
];

/// Comma-separated table (or object) names from `i`, without their aliases and argument lists:
/// the names and the index after them.
fn names(t: &[Tok], mut i: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let is_name = |x: &Tok| matches!(x.kind, Kind::QuotedIdent | Kind::Str) || (x.kind == Kind::Word && !CLAUSES.contains(&x.up.as_str()));
    while let Some(x) = t.get(i).filter(|x| is_name(x)) {
        let mut name = x.text.to_string();
        i += 1;
        while punct(t, i, ".") && t.get(i + 1).is_some_and(|x| matches!(x.kind, Kind::Word | Kind::QuotedIdent)) {
            name.push('.');
            name.push_str(t[i + 1].text);
            i += 2;
        }
        // PostgreSQL's `ONLY t *`; `DROP FUNCTION f(int)`.
        if punct(t, i, "*") {
            i += 1;
        }
        if punct(t, i, "(") {
            i = close(t, i) + 1;
        }
        out.push(name);
        if word(t, i) == "AS" {
            i += 2;
        } else if t.get(i).is_some_and(|x| x.kind == Kind::Word && is_name(x)) {
            i += 1;
        }
        if !punct(t, i, ",") {
            break;
        }
        i += 1;
    }
    (out, i)
}

/// Object types of `DROP` (and their modifiers).
const DROP_KINDS: &[&str] = &[
    "TABLE", "TABLES", "VIEW", "MATERIALIZED", "DATABASE", "SCHEMA", "INDEX", "SEQUENCE", "FUNCTION", "PROCEDURE",
    "PROC", "TRIGGER", "TYPE", "USER", "ROLE", "LOGIN", "EXTENSION", "FOREIGN", "EXTERNAL", "TEMPORARY", "TEMP",
    "PACKAGE", "BODY", "SYNONYM", "EVENT", "DICTIONARY", "STAGE", "WAREHOUSE", "STREAM", "TASK", "PIPE", "DOMAIN",
    "OWNED", "BY", "POLICY", "RULE", "SERVER", "PUBLICATION", "SUBSCRIPTION", "TABLESPACE", "AGGREGATE", "ROUTINE",
    "COLLATION", "CONSTRAINT", "PUBLIC", "SHARE", "INTEGRATION", "FILE", "FORMAT", "LIBRARY", "DIRECTORY", "CLUSTER",
    "LINK", "UNIQUE", "CLUSTERED", "NONCLUSTERED", "ASSEMBLY", "STATISTICS", "TRANSIENT", "DYNAMIC", "ICEBERG",
    "HYBRID", "UNLOGGED", "VOLATILE", "UNDO", "PROFILE", "SECRET", "NETWORK", "ALERT", "CONNECTION", "SEARCH",
    "CONFIGURATION", "DATA", "WRAPPER", "MAPPING", "FOR", "ACCESS", "METHOD", "OPERATOR", "CLASS", "FAMILY", "CAST",
    "LANGUAGE", "TRANSFORM", "TEXT", "PARSER", "TEMPLATE", "CONVERSION",
];

fn classify(t: &[Tok], tsql: bool, out: &mut Vec<Dangerous>) {
    let first = word(t, 0);
    let mut i = 1;
    match first {
        "WITH" => {
            if word(t, i) == "RECURSIVE" {
                i += 1;
            }
            loop {
                if !t.get(i).is_some_and(|x| matches!(x.kind, Kind::Word | Kind::QuotedIdent)) {
                    return;
                }
                i += 1;
                if punct(t, i, "(") {
                    i = close(t, i) + 1;
                }
                if word(t, i) != "AS" {
                    return;
                }
                i += 1;
                if word(t, i) == "NOT" {
                    i += 1;
                }
                if word(t, i) == "MATERIALIZED" {
                    i += 1;
                }
                if !punct(t, i, "(") {
                    return;
                }
                // PostgreSQL's data-modifying CTEs run whatever the main statement is.
                let end = close(t, i);
                scan(&t[i + 1..end.min(t.len())], tsql, out);
                i = end + 1;
                if !punct(t, i, ",") {
                    break;
                }
                i += 1;
            }
            if i < t.len() {
                classify(&t[i..], tsql, out);
            }
        }
        "UPDATE" | "DELETE" => {
            let delete = first == "DELETE";
            let mut bounded = false;
            loop {
                match word(t, i) {
                    "LOW_PRIORITY" | "QUICK" | "IGNORE" | "ONLY" => i += 1,
                    // SQLite's `UPDATE OR REPLACE`.
                    "OR" if !delete => i += 2,
                    "TOP" => {
                        i += 1;
                        i = if punct(t, i, "(") { close(t, i) + 1 } else { i + 1 };
                        if word(t, i) == "PERCENT" {
                            i += 1;
                        } else {
                            bounded = true;
                        }
                    }
                    "FROM" if delete => i += 1,
                    _ => break,
                }
            }
            let (targets, i) = names(t, i);
            let mut depth = 0usize;
            let filtered = t[i..].iter().any(|x| {
                match (x.kind, x.text) {
                    (Kind::Punct, "(") => depth += 1,
                    (Kind::Punct, ")") => depth = depth.saturating_sub(1),
                    _ => {}
                }
                depth == 0 && x.kind == Kind::Word && matches!(x.up.as_str(), "WHERE" | "LIMIT")
            });
            if !targets.is_empty() && !filtered && !bounded {
                let danger = if delete { Danger::Delete } else { Danger::Update };
                out.push(Dangerous { danger, object: String::new(), target: targets.join(", ") });
            }
        }
        "TRUNCATE" => {
            while matches!(word(t, i), "TABLE" | "ONLY" | "IF" | "EXISTS") {
                i += 1;
            }
            let (targets, _) = names(t, i);
            if !targets.is_empty() {
                out.push(Dangerous { danger: Danger::Truncate, object: String::new(), target: targets.join(", ") });
            }
        }
        "DROP" => {
            let mut kinds: Vec<&str> = Vec::new();
            while DROP_KINDS.contains(&word(t, i)) && !punct(t, i + 1, ".") {
                kinds.push(t[i].text);
                i += 1;
            }
            if kinds.is_empty() && !word(t, i).is_empty() {
                kinds.push(t[i].text);
                i += 1;
            }
            let mut j = i;
            while matches!(word(t, j), "IF" | "NOT" | "EXISTS" | "CONCURRENTLY") {
                j += 1;
            }
            let (mut targets, _) = names(t, j);
            // `DROP TABLE user`: the last "type" was the name.
            if targets.is_empty() && kinds.len() > 1 && j == i {
                targets.push(kinds.pop().unwrap_or_default().to_string());
            }
            if !kinds.is_empty() && !targets.is_empty() {
                let object = kinds.iter().map(|k| k.to_ascii_uppercase()).collect::<Vec<_>>().join(" ");
                out.push(Dangerous { danger: Danger::Drop, object, target: targets.join(", ") });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (danger, object, target) of each dangerous statement.
    fn found(sql: &str, driver: Driver) -> Vec<(Danger, String, String)> {
        dangerous_statements(sql, driver).into_iter().map(|d| (d.danger, d.object, d.target)).collect()
    }

    fn one(danger: Danger, target: &str) -> Vec<(Danger, String, String)> {
        vec![(danger, String::new(), target.to_string())]
    }

    fn drop(object: &str, target: &str) -> Vec<(Danger, String, String)> {
        vec![(Danger::Drop, object.to_string(), target.to_string())]
    }

    const SQL: [Driver; 8] = [
        Driver::Postgres, Driver::MySql, Driver::MariaDb, Driver::Sqlite, Driver::MsSql, Driver::Oracle, Driver::Snowflake, Driver::ClickHouse,
    ];

    #[test]
    fn update_and_delete_without_where_on_every_engine() {
        for d in SQL {
            assert_eq!(found("DELETE FROM orders", d), one(Danger::Delete, "orders"), "{d:?}");
            assert_eq!(found("delete from shop.orders;", d), one(Danger::Delete, "shop.orders"), "{d:?}");
            assert_eq!(found("UPDATE orders SET status = 'x'", d), one(Danger::Update, "orders"), "{d:?}");
            assert_eq!(found("update orders o set status = 'x'", d), one(Danger::Update, "orders"), "{d:?}");
            assert!(found("DELETE FROM orders WHERE id = 1", d).is_empty(), "{d:?}");
            assert!(found("UPDATE orders SET status = 'x' WHERE id = 1", d).is_empty(), "{d:?}");
            assert!(found("SELECT * FROM orders", d).is_empty(), "{d:?}");
            assert!(found("INSERT INTO orders (id) VALUES (1)", d).is_empty(), "{d:?}");
        }
    }

    #[test]
    fn where_only_counts_at_the_statement_level() {
        let d = Driver::Postgres;
        // The subquery's WHERE does not filter the rows the UPDATE changes.
        assert_eq!(found("UPDATE t SET a = (SELECT max(b) FROM u WHERE u.id = 1)", d), one(Danger::Update, "t"));
        assert_eq!(found("DELETE FROM t USING (SELECT id FROM u WHERE x) s", d), one(Danger::Delete, "t"));
        assert!(found("UPDATE t SET a = 1 WHERE id IN (SELECT id FROM u)", d).is_empty());
        assert!(found("UPDATE t SET a = CASE WHEN b THEN 1 ELSE 0 END WHERE id = 5", d).is_empty());
    }

    #[test]
    fn strings_and_comments_are_not_statements() {
        for d in SQL {
            assert!(found("SELECT 'DELETE FROM orders' AS s", d).is_empty(), "{d:?}");
            assert!(found("-- DELETE FROM orders\nSELECT 1", d).is_empty(), "{d:?}");
            assert!(found("/* DROP TABLE orders */ SELECT 1", d).is_empty(), "{d:?}");
            // A comment between the table and WHERE does not hide the filter.
            assert!(found("DELETE FROM orders /* all? */ WHERE id = 1", d).is_empty(), "{d:?}");
            assert!(found("DELETE FROM orders -- careful\n WHERE id = 1", d).is_empty(), "{d:?}");
            // Nor does a string that contains WHERE make one.
            assert_eq!(found("UPDATE orders SET note = 'WHERE id = 1'", d), one(Danger::Update, "orders"), "{d:?}");
        }
        assert!(found("SELECT $$DELETE FROM orders$$", Driver::Postgres).is_empty());
        assert!(found("# DELETE FROM orders\nSELECT 1", Driver::MySql).is_empty());
        assert!(found("SELECT q'[DELETE FROM orders]' FROM dual", Driver::Oracle).is_empty());
        assert!(found("// DROP TABLE orders\nSELECT 1", Driver::Snowflake).is_empty());
    }

    #[test]
    fn quoted_names_keep_their_quotes() {
        assert_eq!(found("DELETE FROM \"Sales\".\"Orders\"", Driver::Postgres), one(Danger::Delete, "\"Sales\".\"Orders\""));
        assert_eq!(found("DELETE FROM `shop`.`orders`", Driver::MySql), one(Danger::Delete, "`shop`.`orders`"));
        assert_eq!(found("DELETE FROM [dbo].[Orders]", Driver::MsSql), one(Danger::Delete, "[dbo].[Orders]"));
    }

    #[test]
    fn ctes() {
        for d in [Driver::Postgres, Driver::MySql, Driver::MsSql, Driver::Sqlite, Driver::Snowflake] {
            assert_eq!(found("WITH old AS (SELECT id FROM orders WHERE y < 2020) UPDATE orders SET archived = 1", d), one(Danger::Update, "orders"), "{d:?}");
            assert!(found("WITH old AS (SELECT id FROM orders) DELETE FROM orders WHERE id IN (SELECT id FROM old)", d).is_empty(), "{d:?}");
            assert!(found("WITH a AS (SELECT 1), b (x) AS (SELECT 2) SELECT * FROM a, b", d).is_empty(), "{d:?}");
        }
        assert_eq!(found(";WITH c AS (SELECT TOP 10 id FROM t) DELETE FROM c", Driver::MsSql), one(Danger::Delete, "c"));
        // PostgreSQL: data-modifying CTEs run too.
        assert_eq!(
            found("WITH gone AS (DELETE FROM orders RETURNING *) INSERT INTO archive SELECT * FROM gone", Driver::Postgres),
            one(Danger::Delete, "orders")
        );
        assert!(found("WITH RECURSIVE gone AS MATERIALIZED (DELETE FROM orders WHERE id = 1 RETURNING *) SELECT 1", Driver::Postgres).is_empty());
    }

    #[test]
    fn mysql_limit_and_multi_table() {
        for d in [Driver::MySql, Driver::MariaDb] {
            // Like `--safe-updates`, a LIMIT bounds the rows.
            assert!(found("DELETE FROM logs ORDER BY created LIMIT 1000", d).is_empty(), "{d:?}");
            assert!(found("UPDATE LOW_PRIORITY IGNORE jobs SET state = 'x' LIMIT 10", d).is_empty(), "{d:?}");
            assert_eq!(found("DELETE LOW_PRIORITY QUICK IGNORE FROM logs", d), one(Danger::Delete, "logs"), "{d:?}");
            assert_eq!(found("DELETE a, b FROM a JOIN b ON a.id = b.a_id", d), one(Danger::Delete, "a, b"), "{d:?}");
            assert!(found("DELETE a, b FROM a JOIN b ON a.id = b.a_id WHERE a.x = 1", d).is_empty(), "{d:?}");
            assert_eq!(found("UPDATE a JOIN b ON a.id = b.id SET a.x = b.x", d), one(Danger::Update, "a"), "{d:?}");
            assert!(found("INSERT INTO t (id, n) VALUES (1, 1) ON DUPLICATE KEY UPDATE n = n + 1", d).is_empty(), "{d:?}");
            assert!(found("SELECT * FROM t FOR UPDATE", d).is_empty(), "{d:?}");
        }
    }

    #[test]
    fn postgres_update_from_and_only() {
        let d = Driver::Postgres;
        assert_eq!(found("UPDATE orders SET total = p.total FROM prices p", d), one(Danger::Update, "orders"));
        assert!(found("UPDATE orders o SET total = p.total FROM prices p WHERE p.id = o.price_id", d).is_empty());
        assert_eq!(found("UPDATE ONLY orders SET a = 1", d), one(Danger::Update, "orders"));
        assert_eq!(found("DELETE FROM ONLY orders *", d), one(Danger::Delete, "orders"));
        assert!(found("DELETE FROM orders WHERE CURRENT OF c", d).is_empty());
        assert!(found("CREATE TABLE t (id int REFERENCES u ON DELETE CASCADE ON UPDATE CASCADE)", d).is_empty());
    }

    #[test]
    fn sql_server_delete_from_from_and_top() {
        let d = Driver::MsSql;
        assert_eq!(found("DELETE FROM o FROM orders o JOIN customers c ON c.id = o.customer_id", d), one(Danger::Delete, "o"));
        assert!(found("DELETE FROM o FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.closed = 1", d).is_empty());
        assert!(found("DELETE TOP (100) FROM logs", d).is_empty());
        assert!(found("UPDATE TOP (5) jobs SET state = 1", d).is_empty());
        assert_eq!(found("DELETE TOP (100) PERCENT FROM logs", d), one(Danger::Delete, "logs"));
        assert_eq!(found("DELETE dbo.logs OUTPUT deleted.id", d), one(Danger::Delete, "dbo.logs"));
        assert_eq!(found("UPDATE t WITH (ROWLOCK) SET a = 1", d), one(Danger::Update, "t"));
        assert!(found("DELETE FROM t WITH (ROWLOCK) WHERE id = 1", d).is_empty());
        assert_eq!(found("UPDATE @rows SET done = 1", d), one(Danger::Update, "@rows"));
        assert_eq!(found("TRUNCATE TABLE #staging", d), one(Danger::Truncate, "#staging"));
    }

    #[test]
    fn sql_server_statements_without_semicolons() {
        let d = Driver::MsSql;
        // The WHERE of the first statement does not cover the second.
        assert_eq!(found("UPDATE a SET x = 1 WHERE id = 1\nDELETE FROM b", d), one(Danger::Delete, "b"));
        assert_eq!(found("BEGIN TRAN\nDELETE FROM b\nCOMMIT", d), one(Danger::Delete, "b"));
        assert_eq!(found("IF EXISTS (SELECT 1 FROM b) DELETE FROM b ELSE PRINT 'none'", d), one(Danger::Delete, "b"));
        assert_eq!(found("DECLARE @n int SET @n = 1 UPDATE t SET a = @n", d), one(Danger::Update, "t"));
        assert_eq!(found("SET NOCOUNT ON DELETE FROM t", d), one(Danger::Delete, "t"));
        assert_eq!(found("BEGIN TRY DELETE FROM t END TRY BEGIN CATCH SELECT 1 END CATCH", d), one(Danger::Delete, "t"));
        assert!(found("UPDATE t SET a = CASE WHEN b = 1 THEN 1 ELSE 0 END WHERE id = 5", d).is_empty());
        assert!(found("UPDATE t SET a = 1 WHERE id = 1 SELECT @@ROWCOUNT", d).is_empty());
        assert!(found("INSERT INTO t SELECT * FROM u", d).is_empty());
        assert!(found("SELECT a FROM t UNION SELECT a FROM u", d).is_empty());
        assert!(found("DECLARE c CURSOR FOR SELECT a FROM t FOR UPDATE OF a", d).is_empty());
        assert!(found("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED BY SOURCE THEN DELETE;", d).is_empty());
        assert!(found("ALTER TABLE t DROP COLUMN c", d).is_empty());
        assert!(found("GRANT SELECT, UPDATE, DELETE ON t TO r", d).is_empty());
        assert_eq!(found("DELETE FROM a\nGO\nUPDATE b SET x = 1 WHERE y = 2\nGO\nTRUNCATE TABLE c", d), vec![
            (Danger::Delete, String::new(), "a".into()),
            (Danger::Truncate, String::new(), "c".into()),
        ]);
    }

    #[test]
    fn truncate_and_drop() {
        for d in SQL {
            assert_eq!(found("TRUNCATE TABLE logs", d), one(Danger::Truncate, "logs"), "{d:?}");
            assert_eq!(found("DROP TABLE orders", d), drop("TABLE", "orders"), "{d:?}");
            assert_eq!(found("drop view shop.v_orders", d), drop("VIEW", "shop.v_orders"), "{d:?}");
        }
        let pg = Driver::Postgres;
        assert_eq!(found("TRUNCATE ONLY a, b RESTART IDENTITY CASCADE", pg), one(Danger::Truncate, "a, b"));
        assert_eq!(found("TRUNCATE logs", Driver::MySql), one(Danger::Truncate, "logs"));
        assert_eq!(found("DROP TABLE IF EXISTS a, b CASCADE", pg), drop("TABLE", "a, b"));
        assert_eq!(found("DROP MATERIALIZED VIEW IF EXISTS mv", pg), drop("MATERIALIZED VIEW", "mv"));
        assert_eq!(found("DROP INDEX CONCURRENTLY ix_orders", pg), drop("INDEX", "ix_orders"));
        assert_eq!(found("DROP FUNCTION f(int, text)", pg), drop("FUNCTION", "f"));
        assert_eq!(found("DROP SCHEMA sales CASCADE", pg), drop("SCHEMA", "sales"));
        assert_eq!(found("DROP OWNED BY app", pg), drop("OWNED BY", "app"));
        assert_eq!(found("DROP TABLE user", pg), drop("TABLE", "user"));
        assert_eq!(found("DROP DATABASE shop", Driver::MySql), drop("DATABASE", "shop"));
        assert_eq!(found("DROP TEMPORARY TABLE IF EXISTS tmp", Driver::MySql), drop("TEMPORARY TABLE", "tmp"));
        assert_eq!(found("DROP INDEX ix ON orders", Driver::MySql), drop("INDEX", "ix"));
        assert_eq!(found("DROP USER 'bob'@'localhost'", Driver::MySql), drop("USER", "'bob'"));
        assert_eq!(found("DROP TABLE orders CASCADE CONSTRAINTS PURGE", Driver::Oracle), drop("TABLE", "orders"));
        assert_eq!(found("DROP TABLE IF EXISTS db.t ON CLUSTER main SYNC", Driver::ClickHouse), drop("TABLE", "db.t"));
        assert_eq!(found("DROP STAGE my_stage", Driver::Snowflake), drop("STAGE", "my_stage"));
        assert_eq!(found("DROP PROCEDURE dbo.cleanup", Driver::MsSql), drop("PROCEDURE", "dbo.cleanup"));
        assert!(found("ALTER TABLE t DROP COLUMN c", pg).is_empty());
    }

    #[test]
    fn routine_bodies_are_not_looked_into() {
        let pg = "CREATE OR REPLACE FUNCTION purge() RETURNS void LANGUAGE plpgsql AS $$\nBEGIN\n  DELETE FROM logs;\n  DROP TABLE tmp;\nEND $$";
        assert!(found(pg, Driver::Postgres).is_empty());
        let atomic = "CREATE PROCEDURE purge() LANGUAGE sql BEGIN ATOMIC\n  DELETE FROM logs;\n  UPDATE jobs SET done = true;\nEND;\nDELETE FROM after_it";
        assert_eq!(found(atomic, Driver::Postgres), one(Danger::Delete, "after_it"));
        let rule = "CREATE RULE keep AS ON DELETE TO orders DO INSTEAD UPDATE orders SET gone = true";
        assert!(found(rule, Driver::Postgres).is_empty());
        let mysql = "CREATE DEFINER=`root`@`%` PROCEDURE purge()\nBEGIN\n  DELETE FROM logs;\n  IF 1 THEN UPDATE jobs SET a = 1; END IF;\nEND;";
        assert!(found(mysql, Driver::MySql).is_empty());
        let delimiter = "DELIMITER $$\nCREATE TRIGGER t BEFORE DELETE ON a FOR EACH ROW BEGIN DELETE FROM b; END$$\nDELIMITER ;\nTRUNCATE c;";
        assert_eq!(found(delimiter, Driver::MariaDb), one(Danger::Truncate, "c"));
        let oracle = "CREATE OR REPLACE PROCEDURE purge IS\nBEGIN\n  DELETE FROM logs;\n  EXECUTE IMMEDIATE 'DROP TABLE tmp';\nEND;\n/\nDELETE FROM x";
        assert_eq!(found(oracle, Driver::Oracle), one(Danger::Delete, "x"));
        let sqlite = "CREATE TRIGGER tr AFTER INSERT ON a BEGIN\n  DELETE FROM b WHERE id = new.id;\n  DELETE FROM c;\n  UPDATE d SET n = CASE WHEN 1 THEN 2 END;\nEND;\nDROP TABLE e";
        assert_eq!(found(sqlite, Driver::Sqlite), drop("TABLE", "e"));
        let mssql = "CREATE OR ALTER PROCEDURE dbo.purge AS\nBEGIN\n  DELETE FROM logs;\n  TRUNCATE TABLE tmp\nEND\nGO\nDELETE FROM x";
        assert_eq!(found(mssql, Driver::MsSql), one(Danger::Delete, "x"));
        assert!(found("ALTER PROCEDURE p AS DELETE FROM logs", Driver::MsSql).is_empty());
        assert!(found("CREATE TRIGGER tr ON t AFTER DELETE AS UPDATE u SET a = 1", Driver::MsSql).is_empty());
        let snowflake = "CREATE OR REPLACE PROCEDURE purge() RETURNS STRING LANGUAGE SQL AS $$\nBEGIN\n  DELETE FROM logs;\n  RETURN 'ok';\nEND;\n$$";
        assert!(found(snowflake, Driver::Snowflake).is_empty());
    }

    #[test]
    fn anonymous_blocks_run_now() {
        let oracle = "BEGIN\n  DELETE FROM logs;\n  UPDATE jobs SET a = 1 WHERE id = 2;\nEND;";
        assert_eq!(found(oracle, Driver::Oracle), one(Danger::Delete, "logs"));
        let declare = "DECLARE n NUMBER;\nBEGIN\n  IF n > 0 THEN UPDATE jobs SET a = 1; ELSE DELETE FROM jobs WHERE id = n; END IF;\nEND;";
        assert_eq!(found(declare, Driver::Oracle), one(Danger::Update, "jobs"));
        let mariadb = "BEGIN NOT ATOMIC\n  TRUNCATE logs;\nEND;";
        assert_eq!(found(mariadb, Driver::MariaDb), one(Danger::Truncate, "logs"));
        let tsql = "BEGIN\n  UPDATE jobs SET a = 1;\n  DELETE FROM done WHERE id = 1;\nEND";
        assert_eq!(found(tsql, Driver::MsSql), one(Danger::Update, "jobs"));
    }

    #[test]
    fn not_sql_engines_are_left_alone() {
        for d in [Driver::Redis, Driver::MongoDb, Driver::InfluxDb, Driver::Kusto] {
            assert!(found("DELETE FROM cpu", d).is_empty(), "{d:?}");
            assert!(found("DROP MEASUREMENT cpu", d).is_empty(), "{d:?}");
        }
    }

    #[test]
    fn several_statements_in_order() {
        let sql = "DELETE FROM a; SELECT 1; UPDATE b SET x = 1; DELETE FROM c WHERE y; DROP VIEW d;";
        assert_eq!(found(sql, Driver::Postgres), vec![
            (Danger::Delete, String::new(), "a".into()),
            (Danger::Update, String::new(), "b".into()),
            (Danger::Drop, "VIEW".into(), "d".into()),
        ]);
    }
}
