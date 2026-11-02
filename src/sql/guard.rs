//! Statements that act on whole tables, found before the console runs them: `UPDATE` and
//! `DELETE` without `WHERE` (nor MySQL's `LIMIT` or T-SQL's `TOP`), `TRUNCATE`, `DROP`,
//! `ALTER TABLE … DROP COLUMN / PARTITION`, `MERGE … THEN DELETE` and `CREATE OR REPLACE
//! TABLE`; Redis `FLUSHALL`, MongoDB `deleteMany({})` / `drop()` and Kusto `.drop` / `.clear`.
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
    /// `ALTER TABLE … DROP [COLUMN]`: removes the column and its values.
    DropColumn,
    /// `ALTER TABLE … DROP PARTITION`: removes the partition's rows.
    DropPartition,
    /// `MERGE … WHEN … THEN DELETE` without an `AND` condition.
    MergeDelete,
    /// `CREATE OR REPLACE TABLE` (and ClickHouse's `REPLACE TABLE`): drops the current one.
    Replace,
    /// Redis `FLUSHALL` / `FLUSHDB`: removes every key.
    Flush,
}

/// A statement the console asks about before running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dangerous {
    pub danger: Danger,
    /// The object type, uppercased (`TABLE`, `MATERIALIZED VIEW`) of `DROP` and `CREATE OR
    /// REPLACE`, Redis' command; empty otherwise.
    pub object: String,
    /// The target as written (`public.orders`, `a, b`, `orders.note`).
    pub target: String,
}

impl Dangerous {
    fn new(danger: Danger, object: &str, target: String) -> Self {
        Dangerous { danger, object: object.to_ascii_uppercase(), target }
    }
}

/// Significant token; `up` is the uppercased text.
struct Tok<'a> {
    kind: Kind,
    text: &'a str,
    up: String,
}

/// Dangerous statements of `script` (one or more statements, as the console sends them), in
/// order, in the engine's own language.
pub fn dangerous_statements(script: &str, driver: Driver) -> Vec<Dangerous> {
    match driver {
        Driver::Redis => return split_statements(script, driver).iter().filter_map(|s| redis(&s.text)).collect(),
        Driver::MongoDb => return split_statements(script, driver).iter().filter_map(|s| mongo(&s.text)).collect(),
        Driver::Kusto => return split_statements(script, driver).iter().filter_map(|s| kusto(&s.text)).collect(),
        _ => {}
    }
    let mut out = Vec::new();
    let dialect = Dialect::for_driver(driver);
    let tsql = driver == Driver::MsSql;
    // Oracle and MySQL keep a routine in one statement; elsewhere its body's `;` split it.
    let tracked = matches!(driver, Driver::Oracle | Driver::MySql | Driver::MariaDb);
    // A T-SQL routine or view is the only statement of its batch: the rest is its body.
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
            } else if is_routine(&toks, driver) {
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

/// `FLUSHALL` / `FLUSHDB`.
fn redis(command: &str) -> Option<Dangerous> {
    let name = command.split_whitespace().next()?.to_ascii_uppercase();
    matches!(name.as_str(), "FLUSHALL" | "FLUSHDB").then(|| Dangerous::new(Danger::Flush, &name, String::new()))
}

/// `deleteMany({})` / `updateMany({}, …)` with an empty filter, `drop()`, and `runCommand` with
/// `drop` / `dropDatabase`.
fn mongo(command: &str) -> Option<Dangerous> {
    use crate::db::mongo::{Command, parse_command};
    use mongodb::bson::Bson;
    match parse_command(command.trim().trim_end_matches(';')).ok()? {
        Command::Collection { name, method, args, .. } => {
            let everything = match args.first() {
                None => true,
                Some(Bson::Document(d)) => d.is_empty(),
                _ => false,
            };
            match method.as_str() {
                "deleteMany" if everything => Some(Dangerous::new(Danger::Delete, "", name)),
                "updateMany" if everything => Some(Dangerous::new(Danger::Update, "", name)),
                "drop" => Some(Dangerous::new(Danger::Drop, "collection", name)),
                _ => None,
            }
        }
        Command::RunOn { command, .. } => match command.iter().next()? {
            (k, _) if k == "dropDatabase" => Some(Dangerous::new(Danger::Drop, "database", String::new())),
            (k, Bson::String(c)) if k == "drop" => Some(Dangerous::new(Danger::Drop, "collection", c.clone())),
            _ => None,
        },
        _ => None,
    }
}

/// Management commands that remove data: `.drop …`, `.clear table T data`, `.purge table T`.
fn kusto(command: &str) -> Option<Dangerous> {
    let rest = crate::db::kusto::skip_comments(command).strip_prefix('.')?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    let low = |i: usize| words.get(i).map_or(String::new(), |w| w.to_ascii_lowercase());
    let target = |from: usize| {
        let end = words
            .iter()
            .skip(from)
            .position(|w| matches!(w.to_ascii_lowercase().as_str(), "ifexists" | "with" | "<|" | "whatif" | "data" | "records" | "in"))
            .map_or(words.len(), |p| from + p);
        words.get(from..end).unwrap_or_default().join(" ")
    };
    match (low(0).as_str(), low(1).as_str()) {
        ("drop", kind) if !kind.is_empty() => Some(Dangerous::new(Danger::Drop, kind, target(2))),
        ("clear", "table") if low(3) == "data" => Some(Dangerous::new(Danger::Truncate, "", target(2))),
        ("purge", "table") => Some(Dangerous::new(Danger::Delete, "", target(2))),
        _ => None,
    }
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

/// Tokens as text, spaced like SQL (`tuple(2024, 1)`).
fn spell(t: &[Tok]) -> String {
    let mut out = String::new();
    for (i, x) in t.iter().enumerate() {
        if i > 0 && !matches!(x.text, ")" | "," | "." | "(") && !matches!(t[i - 1].text, "(" | ".") {
            out.push(' ');
        }
        out.push_str(x.text);
    }
    out
}

/// `CREATE [OR REPLACE | OR ALTER] … FUNCTION / PROCEDURE / TRIGGER …` and T-SQL's `ALTER
/// PROCEDURE`: a routine whose body runs later. Packages and types have bodies on Oracle only,
/// events on MySQL, rules on PostgreSQL; a T-SQL view takes its whole batch.
fn is_routine(t: &[Tok], driver: Driver) -> bool {
    let words: Vec<&str> = t.iter().take_while(|x| matches!(x.kind, Kind::Word | Kind::QuotedIdent | Kind::Str) || x.text == "=" || x.text == "@").map(|x| x.up.as_str()).collect();
    let Some((first, rest)) = words.split_first() else { return false };
    if !matches!(*first, "CREATE" | "ALTER") {
        return false;
    }
    for w in rest.iter().take(8) {
        match *w {
            "FUNCTION" | "PROCEDURE" | "PROC" | "TRIGGER" => return true,
            "PACKAGE" | "TYPE" => return driver == Driver::Oracle,
            "EVENT" => return matches!(driver, Driver::MySql | Driver::MariaDb),
            "RULE" => return driver == Driver::Postgres,
            "VIEW" => return driver == Driver::MsSql,
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
    // First word of the current statement.
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
            // T-SQL has no `THEN` blocks; `MERGE … THEN UPDATE / DELETE` are clauses.
            || (!tsql && stmt != "MERGE" && starts && matches!(prev, "BEGIN" | "THEN" | "ELSE" | "LOOP" | "DO" | "ATOMIC"))
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
    let clause = matches!(stmt, "MERGE" | "GRANT" | "REVOKE" | "DENY")
        || matches!(
            (stmt, w),
            ("UPDATE", "SET") | ("INSERT", "SELECT" | "EXEC" | "EXECUTE" | "WITH") | ("DROP", "IF") | ("ALTER", "DROP" | "ALTER" | "SET" | "WITH")
        );
    // `FOR UPDATE OF`, `UNION SELECT`, `INSTEAD OF DELETE`, column lists, names (`FROM log`);
    // `UPDATE(column)` in triggers, `WITH (NOLOCK)` table hints.
    // `ON DELETE CASCADE` / `ON UPDATE NO ACTION` of a foreign key (`SET NOCOUNT ON DELETE …` is not one).
    let inside = matches!(prev, "FOR" | "OF" | "UNION" | "ALL" | "EXCEPT" | "INTERSECT" | "AFTER" | "INSTEAD" | "BEFORE" | "FROM" | "TABLE" | "INTO" | "," | ".")
        || (matches!(w, "UPDATE" | "WITH") && next == "(")
        || (prev == "ON" && matches!(w, "UPDATE" | "DELETE") && ["CASCADE", "NO", "SET", "RESTRICT"].iter().any(|a| a.eq_ignore_ascii_case(next)));
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

/// Reserved words that never start a name where one is expected (`MERGE … THEN UPDATE SET`,
/// `THEN DELETE WHEN`).
const NOT_NAMES: &[&str] = &["SET", "WHERE", "WHEN", "FROM", "USING", "ON", "SELECT", "VALUES"];

/// Words after a table name that are not its alias.
const CLAUSES: &[&str] = &[
    "WHERE", "SET", "FROM", "USING", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "OUTER", "NATURAL",
    "STRAIGHT_JOIN", "ON", "LIMIT", "ORDER", "RETURNING", "OUTPUT", "WITH", "OPTION", "PARTITION", "TABLESAMPLE",
    "CASCADE", "RESTRICT", "PURGE", "IF", "WHEN", "VALUES", "SELECT", "FINAL", "SETTINGS", "SYNC", "IN", "INTO",
    "GROUP", "HAVING", "AS", "RESTART", "CONTINUE", "FORCE", "LOG", "ERRORS",
];

/// A (qualified) name at `i`, quoted parts kept: the name and the index after it. Any word but
/// [`NOT_NAMES`] can be one (`log`, `settings`).
fn qualified(t: &[Tok], i: usize) -> Option<(String, usize)> {
    let x = t.get(i).filter(|x| matches!(x.kind, Kind::QuotedIdent | Kind::Str) || (x.kind == Kind::Word && !NOT_NAMES.contains(&x.up.as_str())))?;
    let (mut name, mut i) = (x.text.to_string(), i + 1);
    while punct(t, i, ".") && t.get(i + 1).is_some_and(|x| matches!(x.kind, Kind::Word | Kind::QuotedIdent)) {
        name.push('.');
        name.push_str(t[i + 1].text);
        i += 2;
    }
    Some((name, i))
}

/// Comma-separated table (or object) names from `i`, without their aliases and argument lists:
/// the names and the index after them.
fn names(t: &[Tok], mut i: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    while let Some((name, next)) = qualified(t, i) {
        i = next;
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
        } else if t.get(i).is_some_and(|x| x.kind == Kind::Word && !CLAUSES.contains(&x.up.as_str())) {
            i += 1;
        }
        if !punct(t, i, ",") {
            break;
        }
        i += 1;
    }
    (out, i)
}

/// Does `t` (the rest of an `UPDATE` / `DELETE`) have a statement-level `WHERE` that can leave
/// rows out, or a `LIMIT`?
fn filtered(t: &[Tok]) -> bool {
    let mut depth = 0usize;
    for (i, x) in t.iter().enumerate() {
        match (x.kind, x.text) {
            (Kind::Punct, "(") => depth += 1,
            (Kind::Punct, ")") => depth = depth.saturating_sub(1),
            _ if depth > 0 || x.kind != Kind::Word => {}
            _ if x.up == "LIMIT" => return true,
            _ if x.up == "WHERE" => return !always_true(&t[i + 1..]),
            _ => {}
        }
    }
    false
}

/// A condition that holds for every row: `1`, `TRUE`, `1 = 1`, `'a' = 'a'` (in parentheses or not).
fn always_true(t: &[Tok]) -> bool {
    let end = t
        .iter()
        .position(|x| x.kind == Kind::Word && matches!(x.up.as_str(), "RETURNING" | "ORDER" | "LIMIT" | "OUTPUT" | "OPTION" | "SETTINGS"))
        .unwrap_or(t.len());
    let mut p = &t[..end];
    while p.len() > 2 && punct(p, 0, "(") && close(p, 0) == p.len() - 1 {
        p = &p[1..p.len() - 1];
    }
    let literal = |x: &Tok| matches!(x.kind, Kind::Number | Kind::Str);
    match p {
        [x] => x.up == "TRUE" || (x.kind == Kind::Number && !x.text.trim_start_matches(['0', '.']).is_empty()),
        [a, eq, b] => literal(a) && literal(b) && a.text == b.text && eq.text == "=",
        _ => false,
    }
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
    "LANGUAGE", "TRANSFORM", "TEXT", "PARSER", "TEMPLATE", "CONVERSION", "MEASUREMENT", "SERIES", "SHARD", "RETENTION",
];

/// `ALTER TABLE` actions; after `DROP COLUMN a,` any other word is one more column (T-SQL).
const ALTER_ACTIONS: &[&str] = &[
    "ADD", "DROP", "ALTER", "MODIFY", "CHANGE", "RENAME", "SET", "RESET", "ENABLE", "DISABLE", "VALIDATE", "OWNER",
    "ATTACH", "DETACH", "INHERIT", "NO", "CLUSTER", "REPLICA", "FORCE", "ALGORITHM", "LOCK", "ORDER", "CONVERT",
    "DEFAULT", "CHARACTER", "COLLATE", "ENGINE", "AUTO_INCREMENT", "COMMENT", "DELETE", "UPDATE", "MATERIALIZE",
    "CLEAR", "FREEZE", "UNFREEZE", "MOVE", "REPLACE", "FETCH", "APPLY", "WITH", "CHECK", "NOCHECK", "SWITCH",
    "REBUILD", "ROW_FORMAT", "OF", "NOT", "TRUNCATE", "COALESCE", "REORGANIZE", "EXCHANGE", "ANALYZE", "OPTIMIZE",
    "REPAIR", "REMOVE", "UPGRADE", "IMPORT", "DISCARD",
];

/// What `ALTER TABLE … DROP` removes when it isn't a column.
const NOT_COLUMNS: &[&str] = &[
    "INDEX", "KEY", "PRIMARY", "FOREIGN", "CONSTRAINT", "CHECK", "DEFAULT", "NOT", "IDENTITY", "EXPRESSION",
    "PERIOD", "SYSTEM", "ROW", "PROJECTION", "STATISTICS", "TTL", "DETACHED", "UNIQUE", "SPATIAL", "FULLTEXT",
    "COMMENT", "SETTING", "ATTRIBUTE",
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
        // PostgreSQL's and MySQL's `EXPLAIN ANALYZE` and MariaDB's `ANALYZE` run the statement
        // to measure it; a plain `EXPLAIN` doesn't (nor PostgreSQL's `ANALYZE table`).
        "EXPLAIN" | "ANALYZE" | "ANALYSE" => {
            let mut analyze = first != "EXPLAIN";
            loop {
                match word(t, i) {
                    "ANALYZE" | "ANALYSE" => analyze = true,
                    "VERBOSE" => {}
                    // `FORMAT = TREE` (MySQL, MariaDB).
                    "FORMAT" if punct(t, i + 1, "=") => i += 2,
                    _ if punct(t, i, "(") => {
                        let end = close(t, i);
                        let opts = &t[i + 1..end.min(t.len())];
                        analyze |= opts.iter().enumerate().any(|(k, x)| {
                            matches!(x.up.as_str(), "ANALYZE" | "ANALYSE") && !matches!(opts.get(k + 1).map_or("", |n| n.up.as_str()), "FALSE" | "OFF" | "0")
                        });
                        i = end;
                    }
                    _ => break,
                }
                i += 1;
            }
            if analyze && i < t.len() {
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
            if !targets.is_empty() && !bounded && !filtered(&t[i..]) {
                out.push(Dangerous::new(if delete { Danger::Delete } else { Danger::Update }, "", targets.join(", ")));
            }
        }
        "TRUNCATE" => {
            while matches!(word(t, i), "TABLE" | "ONLY" | "IF" | "EXISTS") {
                i += 1;
            }
            let (targets, _) = names(t, i);
            if !targets.is_empty() {
                out.push(Dangerous::new(Danger::Truncate, "", targets.join(", ")));
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
            // InfluxQL's `DROP SERIES FROM m [WHERE …]`.
            if word(t, j) == "FROM" {
                let (targets, k) = names(t, j + 1);
                if !targets.is_empty() && !filtered(&t[k..]) {
                    out.push(Dangerous::new(Danger::Drop, &kinds.join(" "), targets.join(", ")));
                }
                return;
            }
            let (mut targets, _) = names(t, j);
            // `DROP TABLE user`: the last "type" was the name.
            if targets.is_empty() && kinds.len() > 1 && j == i {
                targets.push(kinds.pop().unwrap_or_default().to_string());
            }
            if !kinds.is_empty() && !targets.is_empty() {
                out.push(Dangerous::new(Danger::Drop, &kinds.join(" "), targets.join(", ")));
            }
        }
        // `WHEN [NOT] MATCHED … THEN DELETE` without `AND` deletes every row the join reaches.
        "MERGE" => {
            if word(t, i) == "TOP" {
                i += 1;
                i = if punct(t, i, "(") { close(t, i) + 1 } else { i + 1 };
                i += usize::from(word(t, i) == "PERCENT");
            }
            i += usize::from(word(t, i) == "INTO");
            let Some((target, i)) = qualified(t, i) else { return };
            let (mut depth, mut when, mut condition) = (0usize, false, false);
            for (k, x) in t.iter().enumerate().skip(i) {
                match (x.kind, x.text) {
                    (Kind::Punct, "(") => depth += 1,
                    (Kind::Punct, ")") => depth = depth.saturating_sub(1),
                    _ if depth > 0 || x.kind != Kind::Word => {}
                    _ if x.up == "WHEN" => (when, condition) = (true, false),
                    _ if x.up == "AND" && when => condition = true,
                    _ if x.up == "THEN" && when => {
                        when = false;
                        if word(t, k + 1) == "DELETE" && !condition {
                            out.push(Dangerous::new(Danger::MergeDelete, "", target));
                            return;
                        }
                    }
                    _ => {}
                }
            }
        }
        // `CREATE OR REPLACE TABLE` (Snowflake, ClickHouse, MariaDB) and ClickHouse's `REPLACE
        // TABLE` drop the table that is there.
        "CREATE" | "REPLACE" => {
            if first == "CREATE" {
                if word(t, 1) != "OR" || word(t, 2) != "REPLACE" {
                    return;
                }
                i = 3;
            }
            while matches!(word(t, i), "TRANSIENT" | "TEMPORARY" | "TEMP" | "VOLATILE" | "LOCAL" | "GLOBAL" | "DYNAMIC" | "ICEBERG" | "HYBRID") {
                i += 1;
            }
            let object = word(t, i);
            if matches!(object, "TABLE" | "DATABASE" | "SCHEMA")
                && let Some((target, _)) = qualified(t, i + 1)
            {
                out.push(Dangerous::new(Danger::Replace, object, target));
            }
        }
        "ALTER" => alter_table(t, out),
        _ => {}
    }
}

/// `ALTER TABLE t DROP [COLUMN] c` / `DROP PARTITION p`, and ClickHouse's `ALTER TABLE t DELETE
/// WHERE …` / `UPDATE … WHERE …` mutations whose condition holds for every row.
fn alter_table(t: &[Tok], out: &mut Vec<Dangerous>) {
    if word(t, 1) != "TABLE" {
        return;
    }
    let mut i = 2;
    while matches!(word(t, i), "IF" | "EXISTS" | "ONLY") {
        i += 1;
    }
    let Some((table, next)) = qualified(t, i) else { return };
    i = next + usize::from(punct(t, next, "*"));
    if word(t, i) == "ON" && word(t, i + 1) == "CLUSTER" {
        i += 3;
    }
    // The kind of the `DROP` before a comma, which bare names after it continue (T-SQL's
    // `DROP COLUMN a, b`, MySQL's `DROP PARTITION p1, p2`).
    let mut dropping = None;
    while i < t.len() {
        let mut depth = 0usize;
        let end = t[i..]
            .iter()
            .position(|x| {
                match (x.kind, x.text) {
                    (Kind::Punct, "(") => depth += 1,
                    (Kind::Punct, ")") => depth = depth.saturating_sub(1),
                    _ => {}
                }
                depth == 0 && x.kind == Kind::Punct && x.text == ","
            })
            .map_or(t.len(), |p| i + p);
        let action = &t[i..end];
        match word(action, 0) {
            "DROP" => dropping = drop_item(&action[1..], &table, None, out),
            "DELETE" if !filtered(action) => out.push(Dangerous::new(Danger::Delete, "", table.clone())),
            "UPDATE" if !filtered(action) => out.push(Dangerous::new(Danger::Update, "", table.clone())),
            w if dropping.is_some() && (w == "COLUMN" || !ALTER_ACTIONS.contains(&w)) => dropping = drop_item(action, &table, dropping, out),
            _ => dropping = None,
        }
        i = end + 1;
    }
}

/// What follows `ALTER TABLE … DROP` (or a comma after one, continuing `kind`): the kind it drops.
fn drop_item(a: &[Tok], table: &str, kind: Option<Danger>, out: &mut Vec<Dangerous>) -> Option<Danger> {
    let mut j = 1;
    let kind = match word(a, 0) {
        "COLUMN" => Danger::DropColumn,
        "PARTITION" => Danger::DropPartition,
        w if NOT_COLUMNS.contains(&w) => return Some(Danger::Drop),
        _ => {
            j = 0;
            kind.unwrap_or(Danger::DropColumn)
        }
    };
    while matches!(word(a, j), "IF" | "EXISTS") {
        j += 1;
    }
    match kind {
        Danger::DropPartition if j < a.len() => out.push(Dangerous::new(kind, "", format!("{table} {}", spell(&a[j..])))),
        Danger::DropColumn => {
            if let Some((column, _)) = qualified(a, j) {
                out.push(Dangerous::new(kind, "", format!("{table}.{column}")));
            }
        }
        _ => {}
    }
    Some(kind)
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
        assert!(found("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED BY SOURCE AND t.old = 1 THEN DELETE;", d).is_empty());
        assert_eq!(found("MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED BY SOURCE THEN DELETE;", d), one(Danger::MergeDelete, "t"));
        assert!(found("ALTER TABLE t DROP CONSTRAINT pk_t", d).is_empty());
        // A statement after CREATE / ALTER without `;` is still found.
        assert_eq!(found("CREATE TYPE dbo.Ids AS TABLE (id int)\nDELETE FROM orders", d), one(Danger::Delete, "orders"));
        assert_eq!(found("CREATE TABLE t (id int REFERENCES u ON DELETE CASCADE)\nTRUNCATE TABLE log", d), one(Danger::Truncate, "log"));
        assert_eq!(found("ALTER TABLE t ADD c int\nDELETE FROM t", d), one(Danger::Delete, "t"));
        // Foreign keys added by SSMS scripts: their referential actions are not statements.
        for action in ["ON DELETE CASCADE", "ON UPDATE CASCADE", "ON DELETE NO ACTION", "on update set null", "ON DELETE SET DEFAULT ON UPDATE NO ACTION"] {
            let fk = format!("ALTER TABLE [dbo].[Orders] WITH CHECK ADD CONSTRAINT [FK_x] FOREIGN KEY([c]) REFERENCES [dbo].[Customers]([Id]) {action}\nGO\nALTER TABLE [dbo].[Orders] CHECK CONSTRAINT [FK_x]");
            assert!(found(&fk, d).is_empty(), "{action}");
        }
        assert_eq!(found("ALTER TABLE t ADD FOREIGN KEY (c) REFERENCES u (id) ON DELETE CASCADE\nDELETE FROM t", d), one(Danger::Delete, "t"));
        assert_eq!(found("SET XACT_ABORT ON UPDATE t SET a = 1", d), one(Danger::Update, "t"));
        // A view, like a routine, is the whole batch.
        assert!(found("CREATE VIEW v AS SELECT 1 AS a\nDELETE FROM x", d).is_empty());
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
        assert!(found("ALTER TABLE t ALTER COLUMN c DROP DEFAULT, DROP CONSTRAINT k, ADD x int", pg).is_empty());
    }

    /// Tables named like a keyword the alias check stops at (`log`, `settings`, `errors`).
    #[test]
    fn keyword_like_table_names() {
        for d in SQL {
            assert_eq!(found("DELETE FROM log", d), one(Danger::Delete, "log"), "{d:?}");
            assert_eq!(found("TRUNCATE TABLE log", d), one(Danger::Truncate, "log"), "{d:?}");
            assert_eq!(found("UPDATE settings SET x = 1", d), one(Danger::Update, "settings"), "{d:?}");
            assert_eq!(found("DROP TABLE errors", d), drop("TABLE", "errors"), "{d:?}");
            for name in ["output", "option", "final", "sync", "force", "restart", "continue", "partition"] {
                assert_eq!(found(&format!("DELETE FROM {name}"), d), one(Danger::Delete, name), "{d:?} {name}");
            }
            assert!(found("DELETE FROM log WHERE id = 1", d).is_empty(), "{d:?}");
            assert!(found("UPDATE settings s SET x = 1 WHERE s.id = 2", d).is_empty(), "{d:?}");
        }
    }

    #[test]
    fn explain_analyze_runs_the_statement() {
        for d in [Driver::Postgres, Driver::MySql] {
            assert_eq!(found("EXPLAIN ANALYZE DELETE FROM orders", d), one(Danger::Delete, "orders"), "{d:?}");
            assert!(found("EXPLAIN ANALYZE DELETE FROM orders WHERE id = 1", d).is_empty(), "{d:?}");
            assert!(found("EXPLAIN DELETE FROM orders", d).is_empty(), "{d:?}");
            assert!(found("EXPLAIN ANALYZE SELECT * FROM orders", d).is_empty(), "{d:?}");
        }
        // The console's own Explain Analyze statements.
        assert_eq!(found("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) UPDATE orders SET a = 1", Driver::Postgres), one(Danger::Update, "orders"));
        assert!(found("EXPLAIN (ANALYZE false, FORMAT JSON) DELETE FROM orders", Driver::Postgres).is_empty());
        assert_eq!(found("EXPLAIN ANALYSE VERBOSE WITH x AS (SELECT 1) DELETE FROM t", Driver::Postgres), one(Danger::Delete, "t"));
        assert_eq!(found("ANALYZE FORMAT=JSON DELETE FROM orders", Driver::MariaDb), one(Danger::Delete, "orders"));
        assert!(found("EXPLAIN FORMAT=TREE DELETE FROM orders", Driver::MySql).is_empty());
        assert!(found("ANALYZE orders", Driver::Postgres).is_empty());
        assert!(found("EXPLAIN PLAN FOR DELETE FROM orders", Driver::Oracle).is_empty());
    }

    #[test]
    fn conditions_that_hold_for_every_row() {
        for d in SQL {
            for w in ["1", "TRUE", "1 = 1", "(1=1)", "'a' = 'a'"] {
                assert_eq!(found(&format!("DELETE FROM t WHERE {w}"), d), one(Danger::Delete, "t"), "{d:?} {w}");
            }
            for w in ["0", "1 = 0", "id = 1", "1 = 1 AND id = 2", "a = a"] {
                assert!(found(&format!("UPDATE t SET x = 1 WHERE {w}"), d).is_empty(), "{d:?} {w}");
            }
        }
    }

    #[test]
    fn merge_with_unconditional_delete() {
        for d in [Driver::Postgres, Driver::Oracle, Driver::Snowflake, Driver::MsSql] {
            assert_eq!(found("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE", d), one(Danger::MergeDelete, "t"), "{d:?}");
            assert!(found("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.gone THEN DELETE", d).is_empty(), "{d:?}");
            assert!(found("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = s.a WHEN NOT MATCHED THEN INSERT (a) VALUES (s.a)", d).is_empty(), "{d:?}");
        }
        assert_eq!(found("MERGE TOP (10) INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE;", Driver::MsSql), one(Danger::MergeDelete, "t"));
        // Oracle's `UPDATE … DELETE WHERE` only deletes what its condition matches.
        assert!(found("MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET a = s.a DELETE WHERE a = 0", Driver::Oracle).is_empty());
    }

    #[test]
    fn create_or_replace_table() {
        for d in [Driver::Snowflake, Driver::ClickHouse, Driver::MariaDb] {
            assert_eq!(found("CREATE OR REPLACE TABLE orders (id int)", d), vec![(Danger::Replace, "TABLE".into(), "orders".into())], "{d:?}");
            assert!(found("CREATE OR REPLACE VIEW v AS SELECT 1", d).is_empty(), "{d:?}");
            assert!(found("CREATE TABLE orders (id int)", d).is_empty(), "{d:?}");
        }
        assert_eq!(found("CREATE OR REPLACE TRANSIENT TABLE db.s.t AS SELECT 1", Driver::Snowflake), vec![(Danger::Replace, "TABLE".into(), "db.s.t".into())]);
        assert_eq!(found("CREATE OR REPLACE DATABASE shop", Driver::Snowflake), vec![(Danger::Replace, "DATABASE".into(), "shop".into())]);
        assert_eq!(found("REPLACE TABLE t ENGINE = MergeTree ORDER BY id AS SELECT 1", Driver::ClickHouse), vec![(Danger::Replace, "TABLE".into(), "t".into())]);
        assert!(found("REPLACE INTO t VALUES (1)", Driver::MySql).is_empty());
    }

    #[test]
    fn alter_table_drops_and_clickhouse_mutations() {
        let col = |t: &str| one(Danger::DropColumn, t);
        assert_eq!(found("ALTER TABLE orders DROP COLUMN note", Driver::Postgres), col("orders.note"));
        assert_eq!(found("ALTER TABLE IF EXISTS ONLY orders DROP IF EXISTS note CASCADE", Driver::Postgres), col("orders.note"));
        assert_eq!(found("ALTER TABLE orders DROP note, ADD x int", Driver::MySql), col("orders.note"));
        assert_eq!(found("ALTER TABLE dbo.orders DROP COLUMN a, b", Driver::MsSql), vec![
            (Danger::DropColumn, String::new(), "dbo.orders.a".into()),
            (Danger::DropColumn, String::new(), "dbo.orders.b".into()),
        ]);
        assert_eq!(found("ALTER TABLE t DROP CONSTRAINT c, COLUMN d", Driver::MsSql), col("t.d"));
        for sql in ["ALTER TABLE t DROP PRIMARY KEY", "ALTER TABLE t DROP INDEX ix", "ALTER TABLE t DROP FOREIGN KEY fk", "ALTER TABLE t ALTER COLUMN c DROP NOT NULL"] {
            assert!(found(sql, Driver::MySql).is_empty(), "{sql}");
        }
        assert_eq!(found("ALTER TABLE t DROP PARTITION p0, p1", Driver::MySql), vec![
            (Danger::DropPartition, String::new(), "t p0".into()),
            (Danger::DropPartition, String::new(), "t p1".into()),
        ]);
        let ch = Driver::ClickHouse;
        assert_eq!(found("ALTER TABLE events ON CLUSTER main DROP PARTITION tuple(2024, 1)", ch), one(Danger::DropPartition, "events tuple(2024, 1)"));
        assert_eq!(found("ALTER TABLE events DELETE WHERE 1", ch), one(Danger::Delete, "events"));
        assert_eq!(found("ALTER TABLE events UPDATE n = 0 WHERE 1 = 1", ch), one(Danger::Update, "events"));
        assert!(found("ALTER TABLE events DELETE WHERE day < '2024-01-01'", ch).is_empty());
        assert!(found("ALTER TABLE events UPDATE n = 0 IN PARTITION 202401 WHERE id = 5", ch).is_empty());
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
    fn other_engines_in_their_own_language() {
        assert_eq!(found("FLUSHALL\nGET k\nflushdb ASYNC", Driver::Redis), vec![
            (Danger::Flush, "FLUSHALL".into(), String::new()),
            (Danger::Flush, "FLUSHDB".into(), String::new()),
        ]);
        assert!(found("DEL log\nSET settings 1", Driver::Redis).is_empty());
        let m = Driver::MongoDb;
        assert_eq!(found("db.users.deleteMany({})", m), one(Danger::Delete, "users"));
        assert_eq!(found("db.getCollection(\"log\").updateMany({}, {$set: {a: 1}})", m), one(Danger::Update, "log"));
        assert_eq!(found("db.users.drop()", m), drop("COLLECTION", "users"));
        assert_eq!(found("db.runCommand({drop: \"users\"})", m), drop("COLLECTION", "users"));
        assert_eq!(found("db.runCommand({dropDatabase: 1})", m), drop("DATABASE", ""));
        assert!(found("db.users.deleteMany({age: {$lt: 18}})", m).is_empty());
        assert!(found("db.users.find({})", m).is_empty());
        let k = Driver::Kusto;
        assert_eq!(found(".drop table StormEvents ifexists", k), drop("TABLE", "StormEvents"));
        assert_eq!(found("// cleanup\n.clear table StormEvents data", k), one(Danger::Truncate, "StormEvents"));
        assert_eq!(found(".purge table StormEvents records <| where State == 'X'", k), one(Danger::Delete, "StormEvents"));
        assert!(found(".show tables", k).is_empty());
        assert!(found("StormEvents | where State == 'TEXAS' | take 10", k).is_empty());
        let i = Driver::InfluxDb;
        assert_eq!(found("DROP MEASUREMENT cpu", i), drop("MEASUREMENT", "cpu"));
        assert_eq!(found("DELETE FROM cpu", i), one(Danger::Delete, "cpu"));
        assert_eq!(found("DROP SERIES FROM cpu", i), drop("SERIES", "cpu"));
        assert!(found("DROP SERIES FROM cpu WHERE host = 'a'", i).is_empty());
        assert!(found("DELETE FROM cpu WHERE time < '2024-01-01'", i).is_empty());
        assert!(found("from(bucket: \"b\") |> range(start: -1h) |> drop(columns: [\"x\"])", i).is_empty());
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
