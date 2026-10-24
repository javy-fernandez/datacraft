//! SQL text utilities: statement splitting, statement under the cursor
//! and syntax highlighting for the editor.

use crate::db::Driver;
use egui::Color32;
use egui::text::{LayoutJob, TextFormat};

/// Script fragment with its byte range within the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// Splits a script into statements. Honors single/double quotes, `[ident]` (SQL Server),
/// `$tag$...$tag$` (PostgreSQL), `--` and `/* */` comments. Separators: `;` and, on SQL Server,
/// a line containing only `GO` (case-insensitive). Skips empty or comment-only fragments.
/// `text` is trimmed and has no trailing `;`.
///
/// Oracle: a line with only `/` also ends a statement, and PL/SQL blocks (`BEGIN`, `DECLARE`,
/// `CREATE PROCEDURE/FUNCTION/PACKAGE/TRIGGER/TYPE`) run to the next `/` line or, without one,
/// to the `;` after their outermost `END` (which they keep). MySQL / MariaDB: stored routines
/// run to the `;` after their outermost `END`, and `DELIMITER x` lines change the separator.
pub fn split_statements(script: &str, driver: Driver) -> Vec<Statement> {
    // Redis: one command per line (`#` and `//` comment lines are skipped).
    if driver == Driver::Redis {
        let mut out = Vec::new();
        let mut pos = 0;
        for line in script.split_inclusive('\n') {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with("//") {
                let start = pos + (line.len() - line.trim_start().len());
                out.push(Statement { text: trimmed.to_string(), start, end: start + trimmed.len() });
            }
            pos += line.len();
        }
        return out;
    }
    // MongoDB: `mongosh` commands, by `;` or line (see `mongo::split_script`).
    if driver == Driver::MongoDb {
        return crate::db::mongo::split_script(script)
            .into_iter()
            .map(|(start, end)| Statement { text: script[start..end].to_string(), start, end })
            .collect();
    }
    let dialect = Dialect::for_driver(driver);
    let oracle = driver == Driver::Oracle;
    let mysql = matches!(driver, Driver::MySql | Driver::MariaDb);
    let track = oracle || mysql;
    // Start of every `/` line (Oracle), to know whether a block has one ahead.
    let slashes: Vec<usize> = if dialect.slash_separator {
        Lexer::new(script, dialect)
            .filter(|t| is_slash_line(script, t))
            .map(|t| t.start)
            .collect()
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    let mut cur = Pending::default();
    // MySQL client `DELIMITER` other than `;`.
    let mut delimiter: Option<String> = None;

    let mut lx = Lexer::new(script, dialect);
    while let Some(tok) = lx.next() {
        if matches!(tok.kind, Kind::Whitespace | Kind::LineComment | Kind::BlockComment) {
            continue;
        }
        let text = &script[tok.start..tok.end];
        if mysql
            && cur.range.is_none()
            && tok.kind == Kind::Word
            && text.eq_ignore_ascii_case("delimiter")
            && script[..tok.start].rsplit('\n').next().is_some_and(|l| l.trim().is_empty())
        {
            let line_end = script[tok.end..].find('\n').map_or(script.len(), |i| tok.end + i);
            let rest = &script[tok.end..line_end];
            if rest.starts_with(|c: char| c.is_ascii_whitespace())
                && let Some(d) = rest.split_whitespace().next()
            {
                delimiter = (d != ";").then(|| d.to_string());
                lx.pos = line_end;
                continue;
            }
        }
        if let Some(d) = &delimiter {
            if !matches!(tok.kind, Kind::Str | Kind::QuotedIdent)
                && let Some(k) = find_delimiter(script, tok, d)
            {
                if k > tok.start {
                    cur.extend(tok.start, k);
                }
                cur.flush(script, &mut out);
                lx.pos = k + d.len();
            } else {
                cur.extend(tok.start, tok.end);
            }
            continue;
        }
        if (dialect.slash_separator && is_slash_line(script, &tok))
            || (dialect.go_separator && tok.kind == Kind::Word && is_go_line(script, &tok))
        {
            cur.flush(script, &mut out);
            continue;
        }
        if tok.kind == Kind::Semicolon {
            if let Some((start, _)) = cur.range
                && track
                && is_block_statement(&cur.head, oracle)
            {
                cur.blocks.token(";", false, oracle);
                let slash_ahead = slashes.partition_point(|&p| p < start) < slashes.len();
                if (oracle && slash_ahead) || !cur.blocks.stack.is_empty() {
                    cur.extend(tok.start, tok.end);
                    continue;
                }
                if oracle {
                    // PL/SQL keeps the `;` after its `END`.
                    cur.extend(tok.start, tok.end);
                }
            }
            cur.flush(script, &mut out);
            continue;
        }
        cur.extend(tok.start, tok.end);
        if track {
            let is_word = tok.kind == Kind::Word;
            let up = if is_word { text.to_ascii_uppercase() } else { text.to_string() };
            if is_word && cur.head.len() < 10 {
                cur.head.push(up.clone());
            }
            cur.blocks.token(&up, is_word, oracle);
        }
    }
    cur.flush(script, &mut out);
    out
}

/// Statement being collected by [`split_statements`].
#[derive(Default)]
struct Pending {
    /// Range of significant tokens (neither whitespace nor comment).
    range: Option<(usize, usize)>,
    /// First words, uppercased (to recognize blocks).
    head: Vec<String>,
    blocks: Blocks,
}

impl Pending {
    fn extend(&mut self, start: usize, end: usize) {
        match &mut self.range {
            Some((_, e)) => *e = end,
            None => self.range = Some((start, end)),
        }
    }

    fn flush(&mut self, script: &str, out: &mut Vec<Statement>) {
        if let Some((start, end)) = self.range {
            out.push(Statement {
                text: script[start..end].to_string(),
                start,
                end,
            });
        }
        *self = Pending::default();
    }
}

/// Nesting of procedural blocks (PL/SQL, MySQL stored programs) inside a statement.
#[derive(Default)]
struct Blocks {
    /// Open levels; `true` = declaration section (`DECLARE`, `IS`/`AS` of a subprogram or
    /// package) whose `BEGIN` does not open another level.
    stack: Vec<bool>,
    /// After `PROCEDURE`/`FUNCTION`/`PACKAGE`/`TYPE BODY` (Oracle): the next `IS`/`AS` opens
    /// a declaration section.
    header: bool,
    /// Previous significant token (words uppercased).
    prev: String,
}

impl Blocks {
    fn token(&mut self, t: &str, is_word: bool, oracle: bool) {
        if is_word {
            let after_end = self.prev == "END";
            // Where a procedural statement can start (`IF(…)` and `REPEAT(…)` are also functions).
            let stmt_start = matches!(
                self.prev.as_str(),
                "" | ";" | ":" | ">" | "BEGIN" | "THEN" | "ELSE" | "DO" | "LOOP" | "REPEAT" | "DECLARE" | "IS" | "AS"
            );
            match t {
                "BEGIN" => match self.stack.last_mut() {
                    Some(decl @ true) => *decl = false,
                    _ => self.stack.push(false),
                },
                "DECLARE" if oracle => self.stack.push(true),
                "END" => {
                    self.stack.pop();
                }
                "CASE" | "LOOP" if !after_end => self.stack.push(false),
                "IF" if !after_end && stmt_start => self.stack.push(false),
                "WHILE" | "REPEAT" if !oracle && !after_end && stmt_start => self.stack.push(false),
                "PROCEDURE" | "FUNCTION" | "PACKAGE" if oracle => self.header = true,
                "BODY" if oracle && self.prev == "TYPE" => self.header = true,
                "IS" | "AS" if oracle && self.header => {
                    self.stack.push(true);
                    self.header = false;
                }
                _ => {}
            }
        } else if t == ";" {
            self.header = false;
        }
        self.prev = t.to_string();
    }
}

/// Does a statement starting with the words `head` contain procedural blocks whose inner `;`
/// do not end it? Oracle: anonymous blocks and stored PL/SQL; MySQL / MariaDB: stored
/// routines, triggers, events and `BEGIN NOT ATOMIC` (a plain `BEGIN` starts a transaction).
fn is_block_statement(head: &[String], oracle: bool) -> bool {
    let words: Vec<&str> = head.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["BEGIN" | "DECLARE", ..] if oracle => true,
        ["BEGIN", "NOT", "ATOMIC", ..] => !oracle,
        ["CREATE", rest @ ..] => {
            let mut definer = false;
            for w in rest {
                match *w {
                    "OR" | "REPLACE" => {}
                    "EDITIONABLE" | "NONEDITIONABLE" if oracle => {}
                    "DEFINER" | "AGGREGATE" if !oracle => definer |= *w == "DEFINER",
                    "PROCEDURE" | "FUNCTION" | "TRIGGER" => return true,
                    "PACKAGE" | "TYPE" => return oracle,
                    "EVENT" => return !oracle,
                    // The user and host of `DEFINER = user@host`.
                    _ if definer => {}
                    _ => return false,
                }
            }
            false
        }
        _ => false,
    }
}

/// Position of the client delimiter `d` starting within `tok` (`END$$` is a single word).
/// Inside a word it only matches if it doesn't start with a letter, digit or `_`.
fn find_delimiter(script: &str, tok: Token, d: &str) -> Option<usize> {
    let bytes = script.as_bytes();
    let d = d.as_bytes();
    let inner_ok = !(d[0].is_ascii_alphanumeric() || d[0] == b'_' || d[0] >= 0x80);
    (tok.start..tok.end).find(|&k| {
        (k == tok.start || inner_ok)
            && script.is_char_boundary(k)
            && bytes.len() >= k + d.len()
            && bytes[k..k + d.len()].eq_ignore_ascii_case(d)
    })
}

/// Statement containing the `cursor` position (in bytes). If the cursor is in the space between
/// statements, returns the closest previous one (like DataGrip); if there is none, the next one.
pub fn statement_at(script: &str, cursor: usize, driver: Driver) -> Option<Statement> {
    let stmts = split_statements(script, driver);
    if stmts.is_empty() {
        return None;
    }
    let mut cursor = cursor.min(script.len());
    while !script.is_char_boundary(cursor) {
        cursor -= 1;
    }

    let next_idx = stmts.partition_point(|s| s.start <= cursor);
    // If the cursor is in the indentation of the line where the next statement starts, that one.
    if let Some(next) = stmts.get(next_idx) {
        let line_start = script[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let before_cursor_blank = script[line_start..cursor].trim().is_empty();
        let gap_blank = !script[cursor..next.start].contains('\n')
            && script[cursor..next.start].trim().is_empty();
        if next_idx == 0 || (before_cursor_blank && gap_blank) {
            return Some(next.clone());
        }
    }
    Some(stmts[next_idx - 1].clone())
}

/// Fragments to send to the server when running a whole script or a selection.
/// SQL Server: batches separated only by `GO` (a `DECLARE` or a `BEGIN…END` with `;` stay together).
/// SQLite: the whole script (the driver already walks it statement by statement, honoring triggers).
/// PostgreSQL, MySQL, Oracle and Snowflake: statements by `;` (dollar-quoting protects PG
/// function bodies and Snowflake's `$$…$$` procedure bodies).
pub fn split_batches(script: &str, driver: Driver) -> Vec<Statement> {
    match driver {
        Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::Oracle | Driver::Snowflake | Driver::ClickHouse | Driver::Redis | Driver::MongoDb => {
            split_statements(script, driver)
        }
        Driver::MsSql | Driver::Sqlite => {
            let dialect = Dialect::for_driver(driver);
            let mut out = Vec::new();
            let mut current: Option<(usize, usize)> = None;
            let mut flush = |current: &mut Option<(usize, usize)>| {
                if let Some((start, end)) = current.take() {
                    let text = script[start..end].to_string();
                    // A batch made only of `;` adds nothing.
                    if text.chars().any(|c| c != ';' && !c.is_whitespace()) {
                        out.push(Statement { text, start, end });
                    }
                }
            };
            for tok in Lexer::new(script, dialect) {
                match tok.kind {
                    Kind::Whitespace | Kind::LineComment | Kind::BlockComment => {}
                    Kind::Word if dialect.go_separator && is_go_line(script, &tok) => flush(&mut current),
                    _ => match &mut current {
                        Some((_, end)) => *end = tok.end,
                        None => current = Some((tok.start, tok.end)),
                    },
                }
            }
            flush(&mut current);
            out
        }
    }
}

/// Colored LayoutJob for `egui::TextEdit::layouter`.
pub fn highlight(text: &str, dark_mode: bool, font: egui::FontId) -> egui::text::LayoutJob {
    highlight_dialect(text, Dialect::PERMISSIVE, dark_mode, font)
}

/// Like [`highlight`], with the lexical rules of `driver` (MySQL `\'` escapes, Oracle `q'[…]'`).
pub fn highlight_for(text: &str, driver: Driver, dark_mode: bool, font: egui::FontId) -> egui::text::LayoutJob {
    highlight_dialect(text, Dialect::for_driver(driver), dark_mode, font)
}

fn highlight_dialect(text: &str, dialect: Dialect, dark_mode: bool, font: egui::FontId) -> egui::text::LayoutJob {
    let palette = if dark_mode {
        Palette::DARK
    } else {
        Palette::LIGHT
    };
    let mut job = LayoutJob::default();
    let mut pending: Option<(usize, usize, Style)> = None;

    for tok in Lexer::new(text, dialect) {
        let style = Style::of(&tok, text);
        // Group consecutive tokens with the same style so the sections don't pile up.
        match &mut pending {
            Some((_, end, s)) if *s == style => *end = tok.end,
            _ => {
                if let Some((s, e, st)) = pending.take() {
                    job.append(&text[s..e], 0.0, palette.format(st, &font));
                }
                pending = Some((tok.start, tok.end, style));
            }
        }
    }
    if let Some((s, e, st)) = pending {
        job.append(&text[s..e], 0.0, palette.format(st, &font));
    }
    job
}

/// Common SQL keywords (uppercase), used by highlighting and autocompletion.
pub const KEYWORDS: &[&str] = &[
    "ABORT",
    "ACTION",
    "ADD",
    "AFTER",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "ANY",
    "APPLY",
    "ARRAY",
    "AS",
    "ASC",
    "ATTACH",
    "AUTHORIZATION",
    "AUTOINCREMENT",
    "BACKUP",
    "BEFORE",
    "BEGIN",
    "BETWEEN",
    "BIGINT",
    "BIGSERIAL",
    "BINARY",
    "BIT",
    "BOOLEAN",
    "BOTH",
    "BREAK",
    "BROWSE",
    "BULK",
    "BY",
    "BYTEA",
    "CALL",
    "CASCADE",
    "CASE",
    "CAST",
    "CATCH",
    "CHAR",
    "CHARACTER",
    "CHECK",
    "CHECKPOINT",
    "CLOSE",
    "CLUSTER",
    "CLUSTERED",
    "COALESCE",
    "COLLATE",
    "COLUMN",
    "COMMENT",
    "COMMIT",
    "COMMITTED",
    "CONCURRENTLY",
    "CONFLICT",
    "CONSTRAINT",
    "CONSTRAINTS",
    "CONTINUE",
    "CONVERT",
    "COPY",
    "CREATE",
    "CROSS",
    "CUBE",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "CURSOR",
    "CYCLE",
    "DATABASE",
    "DATE",
    "DATETIME",
    "DATETIME2",
    "DEALLOCATE",
    "DEC",
    "DECIMAL",
    "DECLARE",
    "DEFAULT",
    "DEFERRABLE",
    "DEFERRED",
    "DELETE",
    "DENY",
    "DESC",
    "DETACH",
    "DISTINCT",
    "DO",
    "DOUBLE",
    "DROP",
    "EACH",
    "ELSE",
    "ELSIF",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXCLUSIVE",
    "EXEC",
    "EXECUTE",
    "EXISTS",
    "EXPLAIN",
    "EXTENSION",
    "FALSE",
    "FETCH",
    "FILTER",
    "FIRST",
    "FLOAT",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GLOB",
    "GO",
    "GRANT",
    "GROUP",
    "GROUPING",
    "HAVING",
    "IDENTITY",
    "IF",
    "ILIKE",
    "IMMEDIATE",
    "IN",
    "INDEX",
    "INNER",
    "INOUT",
    "INSERT",
    "INSTEAD",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "JSON",
    "JSONB",
    "KEY",
    "LANGUAGE",
    "LAST",
    "LATERAL",
    "LEADING",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOCK",
    "MATERIALIZED",
    "MERGE",
    "MONEY",
    "NATURAL",
    "NCHAR",
    "NEXT",
    "NO",
    "NOCHECK",
    "NOLOCK",
    "NONCLUSTERED",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NOWAIT",
    "NULL",
    "NULLIF",
    "NULLS",
    "NUMERIC",
    "NVARCHAR",
    "OF",
    "OFF",
    "OFFSET",
    "ON",
    "ONLY",
    "OPEN",
    "OR",
    "ORDER",
    "OUT",
    "OUTER",
    "OUTPUT",
    "OVER",
    "OWNER",
    "PARTITION",
    "PERCENT",
    "PIVOT",
    "PLAN",
    "PRAGMA",
    "PRECEDING",
    "PRECISION",
    "PRIMARY",
    "PRINT",
    "PROC",
    "PROCEDURE",
    "RAISE",
    "RAISERROR",
    "RANGE",
    "READ",
    "REAL",
    "RECURSIVE",
    "REFERENCES",
    "REFRESH",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPEATABLE",
    "REPLACE",
    "RESTRICT",
    "RETURN",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLE",
    "ROLLBACK",
    "ROLLUP",
    "ROW",
    "ROWID",
    "ROWS",
    "SAVEPOINT",
    "SCHEMA",
    "SELECT",
    "SEQUENCE",
    "SERIAL",
    "SERIALIZABLE",
    "SESSION",
    "SET",
    "SETOF",
    "SHOW",
    "SMALLINT",
    "SOME",
    "STRICT",
    "TABLE",
    "TABLESAMPLE",
    "TEMP",
    "TEMPORARY",
    "TEXT",
    "THEN",
    "THROW",
    "TIES",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "TINYINT",
    "TO",
    "TOP",
    "TRAILING",
    "TRAN",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "TRY",
    "TYPE",
    "UNBOUNDED",
    "UNCOMMITTED",
    "UNION",
    "UNIQUE",
    "UNIQUEIDENTIFIER",
    "UNLOGGED",
    "UNPIVOT",
    "UPDATE",
    "USE",
    "USING",
    "UUID",
    "VACUUM",
    "VALUES",
    "VARCHAR",
    "VARYING",
    "VIEW",
    "VIRTUAL",
    "VOLATILE",
    "WAITFOR",
    "WHEN",
    "WHERE",
    "WHILE",
    "WINDOW",
    "WITH",
    "WITHOUT",
    "WORK",
    "XML",
    "ZONE",
];

fn is_keyword(word: &str) -> bool {
    if word.len() > 32 || !word.is_ascii() {
        return false;
    }
    let upper = word.to_ascii_uppercase();
    KEYWORDS.binary_search(&upper.as_str()).is_ok()
}

/// Is `tok` a `/` alone on its line (Oracle: runs the statement or PL/SQL block above)?
fn is_slash_line(script: &str, tok: &Token) -> bool {
    if tok.kind != Kind::Op || &script[tok.start..tok.end] != "/" {
        return false;
    }
    let line_start = script[..tok.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = script[tok.end..].find('\n').map_or(script.len(), |i| tok.end + i);
    script[line_start..tok.start].trim().is_empty() && script[tok.end..line_end].trim().is_empty()
}

/// Is `tok` a `GO` alone on its line (allowing spaces and a trailing `--` comment)?
fn is_go_line(script: &str, tok: &Token) -> bool {
    if !script[tok.start..tok.end].eq_ignore_ascii_case("go") {
        return false;
    }
    let line_start = script[..tok.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = script[tok.end..]
        .find('\n')
        .map_or(script.len(), |i| tok.end + i);
    let before = &script[line_start..tok.start];
    let after = script[tok.end..line_end].trim_start();
    before.trim().is_empty() && (after.is_empty() || after.starts_with("--"))
}

// ---------------------------------------------------------------------------
// Explain plan
// ---------------------------------------------------------------------------

/// Statements that show the execution plan of `stmt` on each engine, to run one by one
/// (on SQL Server each one is its own batch: `SET SHOWPLAN_TEXT` must be alone in its batch).
/// Empty if `stmt` has nothing to explain.
pub fn explain_statements(stmt: &str, driver: Driver) -> Vec<String> {
    let s = stmt.trim().trim_end_matches(';').trim_end();
    if s.is_empty() {
        return Vec::new();
    }
    match driver {
        Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::ClickHouse => vec![format!("EXPLAIN {s}")],
        Driver::Sqlite => vec![format!("EXPLAIN QUERY PLAN {s}")],
        Driver::Snowflake => vec![format!("EXPLAIN USING TEXT {s}")],
        Driver::Redis | Driver::MongoDb => Vec::new(),
        Driver::MsSql => vec!["SET SHOWPLAN_TEXT ON".into(), s.to_string(), "SET SHOWPLAN_TEXT OFF".into()],
        Driver::Oracle => vec![
            format!("EXPLAIN PLAN FOR {s}"),
            "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY())".into(),
        ],
    }
}

// ---------------------------------------------------------------------------
// Formatter
// ---------------------------------------------------------------------------

/// Keywords that are never used as plain identifiers in practice: always uppercased.
/// The rest of [`KEYWORDS`] (types and nouns such as `date`, `type`, `key`, `text`) are
/// uppercased only where they cannot be a table, column or alias name.
const RESERVED: &[&str] = &[
    "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "BEGIN", "BETWEEN", "BY", "CASE", "CAST", "CHECK", "COMMIT",
    "CONSTRAINT", "CREATE", "CROSS", "DECLARE", "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END",
    "EXCEPT", "EXEC", "EXECUTE", "EXISTS", "EXPLAIN", "FALSE", "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT",
    "GROUP", "HAVING", "IF", "ILIKE", "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN",
    "LATERAL", "LEFT", "LIKE", "LIMIT", "NATURAL", "NOT", "NULL", "OFFSET", "ON", "ONLY", "OR", "ORDER", "OUTER",
    "PRIMARY", "PROCEDURE", "REFERENCES", "RETURNING", "REVOKE", "RIGHT", "ROLLBACK", "SELECT", "SET", "SOME",
    "TABLE", "THEN", "TOP", "TRIGGER", "TRUE", "TRUNCATE", "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "VIEW",
    "WHEN", "WHERE", "WITH",
];

/// Words after which a non-reserved keyword is a name (`FROM date`, `AS type`...).
const NAME_INTRODUCERS: &[&str] = &["AS", "FROM", "JOIN", "INTO", "UPDATE", "TABLE", "VIEW"];

/// Keywords followed by a space before `(` (never added after other words: on MySQL a
/// space between a built-in function and `(` changes how it is parsed).
const SPACE_BEFORE_PAREN: &[&str] = &[
    "AND", "AS", "ELSE", "EXCEPT", "EXISTS", "FROM", "IN", "INTERSECT", "JOIN", "NOT", "ON", "OR", "THEN", "UNION",
    "USING", "WHEN", "WHERE",
];

const JOIN_MODIFIERS: &[&str] = &["INNER", "LEFT", "RIGHT", "FULL", "OUTER", "CROSS", "NATURAL"];

/// Words that start a main clause (a comment right before one is indented like it).
const CLAUSE_WORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "UNION", "INTERSECT", "EXCEPT", "VALUES", "SET",
    "JOIN", "RETURNING",
];

/// Clause of a query level, which decides how commas and `AND`/`OR` are laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clause {
    None,
    With,
    Select,
    From,
    Where,
    Having,
    Set,
    Values,
    Other,
}

impl Clause {
    /// One item per line after a comma.
    fn is_list(self) -> bool {
        matches!(self, Clause::With | Clause::Select | Clause::Set | Clause::Values)
    }
}

/// Nesting level: the whole statement or a parenthesis.
struct Level {
    /// Column where its lines start (the statement's or the one right after the `(`).
    base: usize,
    /// Statement or subquery (clauses go on their own lines); otherwise a plain parenthesis
    /// (function arguments, column lists...) laid out on one line.
    query: bool,
    clause: Clause,
    /// Column of the first item of a list clause, where the next items are aligned.
    list_col: Option<usize>,
    /// Inside `BETWEEN … AND`: that `AND` doesn't break the line.
    between: bool,
    case_depth: usize,
    /// No token emitted in this level yet.
    empty: bool,
}

impl Level {
    fn new(base: usize, query: bool) -> Self {
        Level { base, query, clause: Clause::None, list_col: None, between: false, case_depth: 0, empty: true }
    }

    /// Column for a line that continues the current clause.
    fn cont_col(&self) -> usize {
        if !self.query || self.empty {
            self.base
        } else if self.clause.is_list() {
            self.list_col.unwrap_or(self.base + 4)
        } else {
            self.base + 4
        }
    }
}

/// Formatted output with column tracking.
struct Out {
    s: String,
    /// Column where the text starts (the first line is not indented by the formatter).
    indent0: usize,
}

impl Out {
    fn col(&self) -> usize {
        match self.s.rfind('\n') {
            Some(i) => self.s[i + 1..].chars().count(),
            None => self.indent0 + self.s.chars().count(),
        }
    }

    fn at_line_start(&self) -> bool {
        match self.s.rfind('\n') {
            Some(i) => self.s[i + 1..].bytes().all(|b| b == b' '),
            None => self.s.is_empty(),
        }
    }

    /// New line (or the current one if it is still empty) indented to `col`; `blank` leaves
    /// an empty line before it. Nothing at the very start of the text.
    fn newline(&mut self, col: usize, blank: bool) {
        let keep = self.s.trim_end_matches(' ').len();
        self.s.truncate(keep);
        if self.s.is_empty() {
            return;
        }
        if !self.s.ends_with('\n') {
            self.s.push('\n');
        }
        if blank && !self.s.ends_with("\n\n") {
            self.s.push('\n');
        }
        self.s.extend(std::iter::repeat_n(' ', col));
    }
}

/// Reformats SQL like DataGrip's "Reformat Code": keywords in uppercase, each main clause
/// (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, joins, `UNION`,
/// `VALUES`, `SET`, `RETURNING`) on its own line, one select / `SET` / `VALUES` / CTE item per
/// line aligned with the first one, `AND` / `OR` of `WHERE` and `HAVING` on their own lines and
/// subqueries aligned after their `(`.
///
/// Only whitespace and the case of keywords change: strings, comments, quoted identifiers
/// and dollar-quoted bodies are copied verbatim, the spacing around operators is kept as
/// typed (`::`, `->>`, `-1`, `$1`, `@var`), blank lines are kept and a line comment always
/// ends its line. Non-reserved keywords that `is_ident` reports as known names (tables,
/// columns, lowercase) keep their case. The result is stable: formatting it again changes
/// nothing. `indent` is the column where `text` starts in the editor.
pub fn format_sql(text: &str, driver: Driver, indent: usize, is_ident: &dyn Fn(&str) -> bool) -> String {
    let dialect = Dialect::for_driver(driver);
    let toks: Vec<Token> = Lexer::new(text, dialect).filter(|t| t.kind != Kind::Whitespace).collect();
    let src = |t: &Token| &text[t.start..t.end];
    let is_comment = |t: &Token| matches!(t.kind, Kind::LineComment | Kind::BlockComment);
    let upper = |t: &Token| if t.kind == Kind::Word { src(t).to_ascii_uppercase() } else { src(t).to_string() };
    // The next `n` significant tokens after `i` (words uppercased).
    let peek = |i: usize, n: usize| -> Vec<String> {
        toks[i + 1..].iter().filter(|t| !is_comment(t)).take(n).map(upper).collect()
    };

    let mut out = Out { s: String::with_capacity(text.len() + text.len() / 4), indent0: indent };
    let mut stack = vec![Level::new(indent, true)];
    let mut stmt_start = true;
    let mut stmt_kind = String::new();
    // A line comment was just emitted: the next token goes on a new line.
    let mut force_nl = false;
    // After a list comma: column of the next item's line.
    let mut comma_nl: Option<usize> = None;
    // Remaining words of a multi-word clause keyword (`GROUP BY`, `LEFT OUTER JOIN`).
    let mut phrase_left = 0usize;
    let mut want_list_col = false;
    let mut prev: Option<Token> = None;
    let mut prev_sig: Option<Token> = None;
    let mut prev_word = String::new();
    let mut prev_end = 0usize;
    // `ON DUPLICATE KEY UPDATE` / `ON CONFLICT DO UPDATE`: `VALUES(x)` is a function there.
    let mut upsert = false;
    let mut after_fake_go = false;

    for (i, tok) in toks.iter().enumerate() {
        let gap = &text[prev_end..tok.start];
        prev_end = tok.end;
        let had_ws = !gap.is_empty();
        let src_nl = gap.contains('\n');
        let blank = gap.matches('\n').count() >= 2;
        let raw = src(tok);

        if is_comment(tok) {
            let lvl = stack.last().expect("root level");
            let next_breaks = peek(i, 1)
                .first()
                .is_some_and(|w| CLAUSE_WORDS.contains(&w.as_str()) || JOIN_MODIFIERS.contains(&w.as_str()));
            let col = if stmt_start {
                stack[0].base
            } else if next_breaks && lvl.query {
                lvl.base
            } else {
                comma_nl.unwrap_or_else(|| lvl.cont_col())
            };
            if force_nl || src_nl {
                out.newline(col, blank);
            } else if !out.at_line_start() {
                out.s.push(' ');
            }
            out.s.push_str(if tok.kind == Kind::LineComment { raw.trim_end() } else { raw });
            force_nl = tok.kind == Kind::LineComment;
            prev = Some(*tok);
            continue;
        }

        let word = upper(tok);
        let is_word = tok.kind == Kind::Word;

        // T-SQL `GO` / Oracle `/`: alone on its line, ends the batch.
        if (is_word && dialect.go_separator && is_go_line(text, tok))
            || (dialect.slash_separator && is_slash_line(text, tok))
        {
            out.newline(0, blank);
            out.s.push_str(raw);
            force_nl = true;
            stack.truncate(1);
            stack[0] = Level::new(indent, true);
            stmt_start = true;
            upsert = false;
            comma_nl = None;
            phrase_left = 0;
            want_list_col = false;
            prev = Some(*tok);
            prev_sig = Some(*tok);
            prev_word.clear();
            continue;
        }

        let depth = stack.len();
        let lvl = stack.last_mut().expect("root level");
        let in_phrase = phrase_left > 0;
        if in_phrase {
            phrase_left -= 1;
        }

        // Main clause starting at this word?
        let mut clause: Option<(Clause, usize)> = None;
        if is_word && lvl.query && !in_phrase {
            let next = peek(i, 4);
            let n0 = next.first().map(String::as_str).unwrap_or("");
            clause = match word.as_str() {
                "WITH" if lvl.empty => Some((Clause::With, 1)),
                "SELECT" => Some((Clause::Select, 1)),
                "FROM" if prev_word != "DELETE" && prev_word != "DISTINCT" => Some((Clause::From, 1)),
                "WHERE" => Some((Clause::Where, 1)),
                "GROUP" | "ORDER" if n0 == "BY" => Some((Clause::Other, 2)),
                "HAVING" => Some((Clause::Having, 1)),
                "LIMIT" | "RETURNING" => Some((Clause::Other, 1)),
                "UNION" | "INTERSECT" | "EXCEPT" => {
                    Some((Clause::None, if n0 == "ALL" || n0 == "DISTINCT" { 2 } else { 1 }))
                }
                "VALUES" if prev_word != "DEFAULT" && !upsert => Some((Clause::Values, 1)),
                "SET" if depth == 1 && stmt_kind == "UPDATE" => Some((Clause::Set, 1)),
                "JOIN" | "STRAIGHT_JOIN" => Some((Clause::From, 1)),
                w if JOIN_MODIFIERS.contains(&w) => {
                    let k = next.iter().take_while(|n| JOIN_MODIFIERS.contains(&n.as_str())).count();
                    matches!(next.get(k).map(String::as_str), Some("JOIN" | "APPLY")).then_some((Clause::From, k + 2))
                }
                _ => None,
            };
        }

        // Where this token goes: a new line at some column, or the current line.
        let mut nl: Option<usize> = None;
        let pending_comma = comma_nl.take();
        if stmt_start {
            if !out.s.is_empty() {
                nl = Some(lvl.base);
            }
        } else if clause.is_some() && !lvl.empty {
            nl = Some(lvl.base);
        } else if is_word
            && (word == "AND" || word == "OR")
            && !in_phrase
            && matches!(lvl.clause, Clause::Where | Clause::Having)
            && lvl.case_depth == 0
        {
            if word == "AND" && lvl.between {
                lvl.between = false;
            } else {
                nl = Some(lvl.base + if word == "AND" { 2 } else { 3 });
            }
        } else if let Some(col) = pending_comma {
            nl = Some(col);
        }
        let closing =
            tok.kind == Kind::Semicolon || (tok.kind == Kind::Punct && matches!(raw, "," | ")" | "."));
        // Adjacent literals only concatenate with a newline between them (`'a'\n'b'`).
        let split_literal =
            src_nl && tok.kind == Kind::Str && prev_sig.is_some_and(|p| p.kind == Kind::Str);
        if nl.is_none() && (force_nl || (blank && !closing) || split_literal) {
            nl = Some(lvl.cont_col());
        }
        // A T-SQL `go` / Oracle `/` that is not a batch separator must never end up alone on
        // its line (it would become one): it stays with the previous token and the next one with it.
        let fake_go = (is_word && dialect.go_separator && word == "GO")
            || (dialect.slash_separator && tok.kind == Kind::Op && raw == "/");
        if (fake_go || after_fake_go) && !force_nl {
            nl = None;
        }
        after_fake_go = fake_go;
        match nl {
            Some(col) => out.newline(col, blank && !closing),
            None => {
                if let Some(p) = prev
                    && !out.at_line_start()
                    && space_between(&p, src(&p), &prev_word, tok, raw, had_ws)
                {
                    out.s.push(' ');
                }
            }
        }
        force_nl = false;

        if want_list_col && !in_phrase {
            lvl.list_col = Some(out.col());
            want_list_col = false;
        }

        // Keyword case.
        let next_is_dot = peek(i, 1).first().is_some_and(|n| n == ".");
        let prev_is_dot = prev_sig.is_some_and(|p| p.kind == Kind::Punct && src(&p) == ".");
        let upper_it = is_word && is_keyword(raw) && !prev_is_dot && !next_is_dot && {
            let reserved = RESERVED.contains(&word.as_str());
            let cast_type = prev_word == "AS" && !lvl.query;
            let name_pos = NAME_INTRODUCERS.contains(&prev_word.as_str()) && !cast_type;
            reserved || (!name_pos && !is_ident(&raw.to_lowercase()))
        };
        out.s.push_str(if upper_it { &word } else { raw });

        // State after the token.
        lvl.empty = false;
        if let Some((c, len)) = clause {
            lvl.clause = c;
            lvl.list_col = None;
            lvl.between = false;
            want_list_col = c.is_list();
            phrase_left = len - 1;
        }
        if is_word {
            match word.as_str() {
                "CASE" => lvl.case_depth += 1,
                "END" => lvl.case_depth = lvl.case_depth.saturating_sub(1),
                "BETWEEN" => lvl.between = true,
                _ => {}
            }
        }
        if tok.kind == Kind::Punct
            && raw == ","
            && lvl.case_depth == 0
            && (matches!(lvl.clause, Clause::With | Clause::Select | Clause::Set)
                || (lvl.clause == Clause::Values && prev_sig.is_some_and(|p| src(&p) == ")")))
        {
            comma_nl = Some(lvl.list_col.unwrap_or(lvl.base + 4));
        }
        if stmt_start {
            stmt_kind = word.clone();
            stmt_start = false;
        } else if is_word && word == "UPDATE" && lvl.query {
            lvl.clause = Clause::Other;
            upsert = true;
        }
        if tok.kind == Kind::Punct && raw == "(" {
            let query = peek(i, 1).first().is_some_and(|n| matches!(n.as_str(), "SELECT" | "WITH" | "VALUES"));
            let col = out.col();
            stack.push(Level::new(col, query));
        } else if tok.kind == Kind::Punct && raw == ")" {
            if stack.len() > 1 {
                stack.pop();
            }
        } else if tok.kind == Kind::Semicolon {
            stack.truncate(1);
            stack[0] = Level::new(indent, true);
            stmt_start = true;
            upsert = false;
            comma_nl = None;
            phrase_left = 0;
            want_list_col = false;
        }
        prev = Some(*tok);
        prev_sig = Some(*tok);
        prev_word = if is_word { word } else { String::new() };
    }
    out.s
}

/// Whether a space goes between two tokens on the same line. Never glues tokens that were
/// apart in a way that matters, nor separates operators typed together.
fn space_between(p: &Token, ptext: &str, pword: &str, c: &Token, ctext: &str, had_ws: bool) -> bool {
    let is = |t: &Token, text: &str, s: &str| t.kind == Kind::Punct && text == s;
    if c.kind == Kind::Semicolon || is(c, ctext, ",") || is(c, ctext, ")") {
        return false;
    }
    if is(p, ptext, ".") || is(c, ctext, ".") {
        // `1 .x` / `x. 5` would lex as a number if glued.
        return had_ws && (p.kind == Kind::Number || c.kind == Kind::Number);
    }
    if is(p, ptext, "(") {
        return false;
    }
    if is(p, ptext, ",") || matches!(p.kind, Kind::LineComment | Kind::BlockComment) {
        return true;
    }
    let tight =
        |t: &Token, text: &str| t.kind == Kind::Op || (t.kind == Kind::Punct && matches!(text, "[" | "]" | "{" | "}"));
    if tight(p, ptext) || tight(c, ctext) {
        return had_ws;
    }
    // Prefixed literals (`_utf8'x'`, Oracle `q'[…]'`): keep them glued.
    if !had_ws
        && matches!(c.kind, Kind::Str | Kind::QuotedIdent)
        && matches!(p.kind, Kind::Word | Kind::Number | Kind::Str | Kind::QuotedIdent)
    {
        return false;
    }
    if is(c, ctext, "(") {
        return had_ws || (p.kind == Kind::Word && SPACE_BEFORE_PAREN.contains(&pword));
    }
    true
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Whitespace,
    Word,
    Number,
    Str,
    QuotedIdent,
    LineComment,
    BlockComment,
    Semicolon,
    Punct,
    Op,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Dialect {
    brackets: bool,
    backticks: bool,
    dollar_quotes: bool,
    nested_comments: bool,
    /// `@var` and `#temp` as words (T-SQL).
    tsql_names: bool,
    go_separator: bool,
    /// `#` starts a line comment (MySQL / MariaDB).
    hash_comments: bool,
    /// `\` escapes the next char in `'…'` and `"…"` strings, and `"…"` is a string (MySQL / MariaDB).
    backslash_escapes: bool,
    /// Oracle alternative quoting: `q'[…]'`, `nq'{…}'`…
    q_quotes: bool,
    /// A line with only `/` ends the statement or PL/SQL block (Oracle, SQL*Plus style).
    slash_separator: bool,
    /// `\` escapes the next char in `'…'` strings only; `"…"` stays an identifier (Snowflake).
    single_quote_backslash: bool,
    /// `//` starts a line comment (Snowflake).
    slash_comments: bool,
}

impl Dialect {
    /// For highlighting, which doesn't know the engine: accepts every quoting form.
    const PERMISSIVE: Dialect = Dialect {
        brackets: true,
        backticks: true,
        dollar_quotes: true,
        nested_comments: true,
        tsql_names: true,
        go_separator: false,
        hash_comments: false,
        backslash_escapes: false,
        q_quotes: false,
        slash_separator: false,
        single_quote_backslash: false,
        slash_comments: false,
    };

    fn for_driver(driver: Driver) -> Self {
        match driver {
            Driver::Postgres => Dialect {
                brackets: false,
                backticks: false,
                dollar_quotes: true,
                nested_comments: true,
                tsql_names: false,
                go_separator: false,
                hash_comments: false,
                backslash_escapes: false,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: false,
                slash_comments: false,
            },
            Driver::Sqlite => Dialect {
                brackets: true,
                backticks: true,
                dollar_quotes: false,
                nested_comments: false,
                tsql_names: false,
                go_separator: false,
                hash_comments: false,
                backslash_escapes: false,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: false,
                slash_comments: false,
            },
            Driver::MsSql => Dialect {
                brackets: true,
                backticks: false,
                dollar_quotes: false,
                nested_comments: false,
                tsql_names: true,
                go_separator: true,
                hash_comments: false,
                backslash_escapes: false,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: false,
                slash_comments: false,
            },
            Driver::MySql | Driver::MariaDb => Dialect {
                brackets: false,
                backticks: true,
                dollar_quotes: false,
                nested_comments: false,
                tsql_names: false,
                go_separator: false,
                hash_comments: true,
                backslash_escapes: true,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: false,
                slash_comments: false,
            },
            Driver::Oracle => Dialect {
                brackets: false,
                backticks: false,
                dollar_quotes: false,
                nested_comments: false,
                tsql_names: false,
                go_separator: false,
                hash_comments: false,
                backslash_escapes: false,
                q_quotes: true,
                slash_separator: true,
                single_quote_backslash: false,
                slash_comments: false,
            },
            // Only used for highlighting their commands.
            Driver::Redis | Driver::MongoDb => Dialect {
                brackets: false,
                backticks: false,
                dollar_quotes: false,
                nested_comments: false,
                tsql_names: false,
                go_separator: false,
                hash_comments: true,
                backslash_escapes: true,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: true,
                slash_comments: true,
            },
            Driver::Snowflake => Dialect {
                brackets: false,
                backticks: false,
                dollar_quotes: true,
                nested_comments: false,
                tsql_names: false,
                go_separator: false,
                hash_comments: false,
                backslash_escapes: false,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: true,
                slash_comments: true,
            },
            // Backtick identifiers; `#` and `//` are also line comments; `\` escapes only in
            // '…' (its "…" is an identifier, not a string).
            Driver::ClickHouse => Dialect {
                brackets: false,
                backticks: true,
                dollar_quotes: false,
                nested_comments: false,
                tsql_names: false,
                go_separator: false,
                hash_comments: true,
                backslash_escapes: false,
                q_quotes: false,
                slash_separator: false,
                single_quote_backslash: true,
                slash_comments: true,
            },
        }
    }
}

/// Every decision is made on ASCII bytes; bytes >= 0x80 only appear inside words, strings
/// or comments, so token boundaries always fall on char boundaries.
struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    dialect: Dialect,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str, dialect: Dialect) -> Self {
        Lexer {
            src: text.as_bytes(),
            pos: 0,
            dialect,
        }
    }

    fn peek(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn eat_while(&mut self, f: impl Fn(u8) -> bool) {
        while self.pos < self.src.len() && f(self.src[self.pos]) {
            self.pos += 1;
        }
    }

    /// Advances past `close` (to the end if it doesn't appear). `escape_double`: `''` → `'`.
    fn eat_quoted(&mut self, close: u8, escape_double: bool, backslash: bool) {
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            self.pos += 1;
            if backslash && b == b'\\' {
                self.pos = (self.pos + 1).min(self.src.len());
                // The skipped byte could start a multibyte char: complete the char.
                self.eat_while(|b| (0x80..0xC0).contains(&b));
            } else if b == close {
                if escape_double && self.peek(0) == Some(close) {
                    self.pos += 1;
                } else {
                    return;
                }
            }
        }
    }

    fn eat_block_comment(&mut self) {
        self.pos += 2;
        let mut depth = 1usize;
        while self.pos < self.src.len() {
            if self.src[self.pos] == b'*' && self.peek(1) == Some(b'/') {
                self.pos += 2;
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else if self.dialect.nested_comments
                && self.src[self.pos] == b'/'
                && self.peek(1) == Some(b'*')
            {
                self.pos += 2;
                depth += 1;
            } else {
                self.pos += 1;
            }
        }
    }

    /// If there is a `$tag$` at the current position, returns its length.
    fn dollar_tag_len(&self) -> Option<usize> {
        let rest = &self.src[self.pos..];
        let mut i = 1;
        if rest.get(i).is_some_and(|b| b.is_ascii_digit()) {
            return None; // $1: parameter
        }
        while rest
            .get(i)
            .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
        {
            i += 1;
        }
        (rest.get(i) == Some(&b'$')).then_some(i + 1)
    }

    /// If there is an Oracle `q'<d>…<d>'` (optionally `n`-prefixed) literal at the current
    /// position, returns its length (to the end of the text if it is not closed).
    fn q_quote_len(&self) -> Option<usize> {
        let rest = &self.src[self.pos..];
        let skip = usize::from(matches!(rest.first(), Some(b'n' | b'N')));
        if !matches!(rest.get(skip), Some(b'q' | b'Q')) || rest.get(skip + 1) != Some(&b'\'') {
            return None;
        }
        let open = *rest.get(skip + 2)?;
        if open.is_ascii_whitespace() || open == b'\'' || open >= 0x80 {
            return None;
        }
        let close = match open {
            b'[' => b']',
            b'{' => b'}',
            b'(' => b')',
            b'<' => b'>',
            c => c,
        };
        let body = skip + 3;
        Some(
            rest[body..]
                .windows(2)
                .position(|w| w[0] == close && w[1] == b'\'')
                .map_or(rest.len(), |i| body + i + 2),
        )
    }

    fn is_word_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
    }

    fn next_kind(&mut self) -> Kind {
        let b = self.src[self.pos];
        let next = self.peek(1);
        match b {
            b if b.is_ascii_whitespace() => {
                self.eat_while(|b| b.is_ascii_whitespace());
                Kind::Whitespace
            }
            b'-' if next == Some(b'-') => {
                self.eat_while(|b| b != b'\n');
                Kind::LineComment
            }
            b'/' if self.dialect.slash_comments && next == Some(b'/') => {
                self.eat_while(|b| b != b'\n');
                Kind::LineComment
            }
            b'#' if self.dialect.hash_comments => {
                self.eat_while(|b| b != b'\n');
                Kind::LineComment
            }
            b'/' if next == Some(b'*') => {
                self.eat_block_comment();
                Kind::BlockComment
            }
            b'\'' => {
                self.pos += 1;
                self.eat_quoted(b'\'', true, self.dialect.backslash_escapes || self.dialect.single_quote_backslash);
                Kind::Str
            }
            // Oracle `q'[…]'` / `nq'{…}'`: nothing inside is special.
            b'q' | b'Q' | b'n' | b'N' if self.dialect.q_quotes && self.q_quote_len().is_some() => {
                self.pos += self.q_quote_len().unwrap_or(1);
                Kind::Str
            }
            // String prefixes: E'..' (escapes with \), N'..', X'..', B'..'.
            b'e' | b'E' | b'n' | b'N' | b'x' | b'X' | b'b' | b'B' if next == Some(b'\'') => {
                self.pos += 2;
                let backslash = matches!(b, b'e' | b'E') || self.dialect.backslash_escapes;
                self.eat_quoted(b'\'', true, backslash);
                Kind::Str
            }
            // MySQL: `"…"` is a string with `\` escapes.
            b'"' if self.dialect.backslash_escapes => {
                self.pos += 1;
                self.eat_quoted(b'"', true, true);
                Kind::Str
            }
            b'"' => {
                self.pos += 1;
                self.eat_quoted(b'"', true, false);
                Kind::QuotedIdent
            }
            b'[' if self.dialect.brackets && !self.follows_operand() => {
                self.pos += 1;
                self.eat_quoted(b']', true, false);
                Kind::QuotedIdent
            }
            b'`' if self.dialect.backticks => {
                self.pos += 1;
                self.eat_quoted(b'`', true, false);
                Kind::QuotedIdent
            }
            b'$' if self.dialect.dollar_quotes && self.dollar_tag_len().is_some() => {
                let len = self.dollar_tag_len().unwrap_or(1);
                let tag = &self.src[self.pos..self.pos + len];
                self.pos += len;
                let rest = &self.src[self.pos..];
                self.pos = rest
                    .windows(len)
                    .position(|w| w == tag)
                    .map_or(self.src.len(), |i| self.pos + i + len);
                Kind::Str
            }
            b'0'..=b'9' => {
                self.eat_number();
                Kind::Number
            }
            b'.' if next.is_some_and(|n| n.is_ascii_digit()) => {
                self.eat_number();
                Kind::Number
            }
            b'@' | b'#' if self.dialect.tsql_names && next.is_some_and(Self::is_word_byte) => {
                self.pos += 1;
                self.eat_while(|b| Self::is_word_byte(b) || b == b'@' || b == b'#');
                Kind::Word
            }
            b if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 => {
                self.eat_while(Self::is_word_byte);
                Kind::Word
            }
            b';' => {
                self.pos += 1;
                Kind::Semicolon
            }
            b'(' | b')' | b',' | b'.' | b'[' | b']' | b'{' | b'}' => {
                self.pos += 1;
                Kind::Punct
            }
            _ => {
                self.pos += 1;
                Kind::Op
            }
        }
    }

    fn eat_number(&mut self) {
        let mut prev = 0u8;
        while let Some(b) = self.peek(0) {
            let sign_in_exp = (b == b'+' || b == b'-')
                && (prev == b'e' || prev == b'E')
                && self.peek(1).is_some_and(|n| n.is_ascii_digit());
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || sign_in_exp {
                prev = b;
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// `arr[1]`: a `[` right after a word or `)` is a subscript, not an identifier (except in T-SQL).
    fn follows_operand(&self) -> bool {
        !self.dialect.go_separator
            && self.pos > 0
            && (matches!(self.src[self.pos - 1], b')' | b']' | b'"')
                || Self::is_word_byte(self.src[self.pos - 1]))
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        if self.pos >= self.src.len() {
            return None;
        }
        let start = self.pos;
        let kind = self.next_kind();
        debug_assert!(self.pos > start);
        Some(Token {
            kind,
            start,
            end: self.pos,
        })
    }
}

// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Normal,
    Keyword,
    Str,
    Number,
    Comment,
    Ident,
}

impl Style {
    fn of(tok: &Token, text: &str) -> Style {
        match tok.kind {
            Kind::Word if is_keyword(&text[tok.start..tok.end]) => Style::Keyword,
            Kind::Str => Style::Str,
            Kind::Number => Style::Number,
            Kind::LineComment | Kind::BlockComment => Style::Comment,
            Kind::QuotedIdent => Style::Ident,
            _ => Style::Normal,
        }
    }
}

struct Palette {
    text: Color32,
    keyword: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
    ident: Color32,
}

impl Palette {
    // Islands "Dark" scheme (DataGrip 2026)
    const DARK: Palette = Palette {
        text: Color32::from_rgb(0xBC, 0xBE, 0xC4),
        keyword: Color32::from_rgb(0xCF, 0x8E, 0x6D),
        string: Color32::from_rgb(0x6A, 0xAB, 0x73),
        number: Color32::from_rgb(0x2A, 0xAC, 0xB8),
        comment: Color32::from_rgb(0x7A, 0x7E, 0x85),
        ident: Color32::from_rgb(0xC7, 0x7D, 0xBB),
    };
    // IntelliJ Light
    const LIGHT: Palette = Palette {
        text: Color32::from_rgb(0x08, 0x08, 0x08),
        keyword: Color32::from_rgb(0x00, 0x33, 0xB3),
        string: Color32::from_rgb(0x06, 0x7D, 0x17),
        number: Color32::from_rgb(0x17, 0x50, 0xEB),
        comment: Color32::from_rgb(0x8C, 0x8C, 0x8C),
        ident: Color32::from_rgb(0x87, 0x10, 0x94),
    };

    fn format(&self, style: Style, font: &egui::FontId) -> TextFormat {
        let color = match style {
            Style::Normal => self.text,
            Style::Keyword => self.keyword,
            Style::Str => self.string,
            Style::Number => self.number,
            Style::Comment => self.comment,
            Style::Ident => self.ident,
        };
        let mut fmt = TextFormat::simple(font.clone(), color);
        fmt.italics = style == Style::Comment;
        fmt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(script: &str, driver: Driver) -> Vec<String> {
        split_statements(script, driver)
            .into_iter()
            .map(|s| s.text)
            .collect()
    }

    #[test]
    fn keywords_sorted_unique_uppercase() {
        assert!(KEYWORDS.len() >= 200);
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(KEYWORDS.iter().all(|k| k.to_ascii_uppercase() == *k));
        assert!(is_keyword("select") && is_keyword("Where") && !is_keyword("customers"));
    }

    #[test]
    fn split_basic_and_ranges() {
        let s = "  SELECT 1 ;\n\nselect 2;select 3  ";
        let st = split_statements(s, Driver::Postgres);
        assert_eq!(st.len(), 3);
        for x in &st {
            assert_eq!(&s[x.start..x.end], x.text);
        }
        assert_eq!(st[0].text, "SELECT 1");
        assert_eq!(st[0].start, 2);
        assert_eq!(st[1].text, "select 2");
        assert_eq!(st[2].text, "select 3");
        assert_eq!(st[2].end, s.len() - 2);
    }

    #[test]
    fn semicolon_in_strings_and_comments() {
        let s = "SELECT 'a;b', \"x;y\" -- c;d\nFROM t /* e;f */ WHERE x = 'it''s;'; SELECT 2";
        let t = texts(s, Driver::Postgres);
        assert_eq!(t.len(), 2);
        assert!(t[0].ends_with("'it''s;'"));
        assert_eq!(t[1], "SELECT 2");
        assert_eq!(
            texts("SELECT [a;b] FROM `c;d`; SELECT 1", Driver::Sqlite).len(),
            2
        );
        assert_eq!(texts("SELECT [a;b]; SELECT 1", Driver::MsSql).len(), 2);
    }

    #[test]
    fn nested_comments_pg() {
        let t = texts("SELECT 1 /* a /* b; */ c; */; SELECT 2", Driver::Postgres);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn dollar_quoting() {
        let s = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN; RETURN 1; END; $$ LANGUAGE plpgsql;\n\
                 DO $body$ BEGIN PERFORM 1; END $body$; SELECT $1";
        let t = texts(s, Driver::Postgres);
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[0].ends_with("LANGUAGE plpgsql"));
        assert!(t[1].starts_with("DO $body$"));
        assert_eq!(t[2], "SELECT $1");
    }

    #[test]
    fn go_separator() {
        let s = "SELECT 1\nGO\nselect 2\n  go  -- end\nSELECT 'GO'\n/*\nGO\n*/\nSELECT go_x\nGo";
        let t = texts(s, Driver::MsSql);
        assert_eq!(
            t,
            vec![
                "SELECT 1",
                "select 2",
                "SELECT 'GO'\n/*\nGO\n*/\nSELECT go_x"
            ]
        );
        // On Postgres GO doesn't separate.
        assert_eq!(texts("SELECT 1\nGO\nSELECT 2", Driver::Postgres).len(), 1);
    }

    #[test]
    fn empty_and_comment_only() {
        assert!(texts("", Driver::Postgres).is_empty());
        assert!(texts(" ;; \n ; ", Driver::Postgres).is_empty());
        assert!(texts("-- only\n/* comment */;", Driver::MsSql).is_empty());
        let t = texts("-- header\nSELECT 1; -- tail\n", Driver::Sqlite);
        assert_eq!(t, vec!["SELECT 1"]);
    }

    #[test]
    fn batches_keep_tsql_and_triggers_together() {
        let ms = split_batches("DECLARE @n int = 5;\nSELECT @n;\nGO\nSELECT 2;\n go -- end\n;", Driver::MsSql);
        let texts: Vec<_> = ms.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["DECLARE @n int = 5;\nSELECT @n;", "SELECT 2;"]);
        let trig = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE a SET x = 1; END;\nSELECT 1;";
        let lite = split_batches(trig, Driver::Sqlite);
        assert_eq!(lite.len(), 1);
        assert_eq!(lite[0].text, trig);
        assert_eq!(split_batches("SELECT 1; SELECT 2", Driver::Postgres).len(), 2);
        assert!(split_batches("  ;; -- nothing\n", Driver::MsSql).is_empty());
    }

    #[test]
    fn unclosed_constructs_do_not_panic() {
        for d in Driver::ALL.into_iter().filter(|d| !d.is_nosql()) {
            for s in [
                "SELECT 'abc",
                "SELECT \"abc",
                "SELECT /* abc",
                "SELECT $$ abc",
                "SELECT $tag$ abc $ta",
                "SELECT [abc",
                "[",
                "[x]",
                "[a] FROM t",
                "SELECT `abc",
                "SELECT E'abc\\",
                "SELECT E'\\ñ",
                "1e+",
                "'",
                "$",
                "--",
                "/*",
            ] {
                let t = texts(s, d);
                let expected = usize::from(!(s.starts_with("--") || s.starts_with("/*")));
                assert_eq!(t.len(), expected, "{s} {d:?}");
                assert_eq!(highlight_text(s), s);
                let _ = statement_at(s, 3, d);
            }
        }
        assert_eq!(texts("SELECT 'a; SELECT 2", Driver::Postgres).len(), 1);
    }

    #[test]
    fn unicode_text() {
        let s = "SELECT 'canción 🎵;' AS título; -- ñandú 🦤\nSELECT \"año\" FROM café;";
        for d in Driver::ALL.into_iter().filter(|d| !d.is_nosql()) {
            let st = split_statements(s, d);
            assert_eq!(st.len(), 2);
            assert_eq!(st[1].text, "SELECT \"año\" FROM café");
            for x in &st {
                assert_eq!(&s[x.start..x.end], x.text);
            }
            // Any position, even in the middle of a multibyte char.
            for c in 0..=s.len() + 5 {
                assert!(statement_at(s, c, d).is_some());
            }
        }
        assert_eq!(highlight_text(s), s);
    }

    #[test]
    fn statement_at_positions() {
        let s = "SELECT 1;\n\n  SELECT 2;   \n\nSELECT 3";
        let d = Driver::Postgres;
        let at = |c| statement_at(s, c, d).unwrap().text;
        assert_eq!(at(0), "SELECT 1");
        assert_eq!(at(3), "SELECT 1");
        assert_eq!(at(8), "SELECT 1"); // right before the ';'
        assert_eq!(at(9), "SELECT 1"); // after the ';'
        assert_eq!(at(10), "SELECT 1"); // blank line
        assert_eq!(at(11), "SELECT 2"); // indentation of the SELECT 2 line
        let p2 = s.find("SELECT 2").unwrap();
        assert_eq!(at(p2 + 4), "SELECT 2");
        assert_eq!(at(p2 + 12), "SELECT 2"); // spaces after the ';'
        assert_eq!(at(s.len()), "SELECT 3");
        assert_eq!(at(s.len() + 100), "SELECT 3");

        let s2 = "\n\n  -- hello\n SELECT 1; SELECT 2";
        assert_eq!(statement_at(s2, 0, d).unwrap().text, "SELECT 1");
        let st = statement_at(s2, s2.len(), d).unwrap();
        assert_eq!((st.start, st.end), (s2.len() - 8, s2.len()));
        assert_eq!(statement_at("  -- nothing\n", 3, d), None);
        assert_eq!(statement_at("", 0, d), None);
    }

    fn highlight_text(s: &str) -> String {
        let job = highlight(s, true, egui::FontId::monospace(13.0));
        job.sections
            .iter()
            .map(|sec| &job.text[sec.byte_range.start.0..sec.byte_range.end.0])
            .collect()
    }

    #[test]
    fn highlight_reconstructs_and_colors() {
        let s = "select \"Name\", [x], 42, 'a''b' -- c\nFROM t /* d */ WHERE arr[1] = $$x$$;";
        assert_eq!(highlight_text(s), s);
        assert_eq!(highlight_text(""), "");
        let job = highlight(s, false, egui::FontId::monospace(13.0));
        let color_of = |needle: &str| {
            let at = s.find(needle).unwrap();
            job.sections
                .iter()
                .find(|sec| (sec.byte_range.start.0..sec.byte_range.end.0).contains(&at))
                .unwrap()
                .format
                .clone()
        };
        assert_eq!(color_of("select").color, Palette::LIGHT.keyword);
        assert_eq!(color_of("FROM").color, Palette::LIGHT.keyword);
        assert_eq!(color_of("42").color, Palette::LIGHT.number);
        assert_eq!(color_of("'a''b'").color, Palette::LIGHT.string);
        assert_eq!(color_of("\"Name\"").color, Palette::LIGHT.ident);
        assert_eq!(color_of("[x]").color, Palette::LIGHT.ident);
        assert_eq!(color_of("[1]").color, Palette::LIGHT.text);
        assert_eq!(color_of("$$x$$").color, Palette::LIGHT.string);
        let c = color_of("-- c");
        assert!(c.italics && c.color == Palette::LIGHT.comment);
        assert_eq!(color_of(" t ").color, Palette::LIGHT.text);
    }

    // ------------------------------------------------------------------ formatter

    /// Significant tokens (words compared without case, line comments trimmed).
    fn token_texts(s: &str, d: Driver) -> Vec<(Kind, String)> {
        Lexer::new(s, Dialect::for_driver(d))
            .filter(|t| t.kind != Kind::Whitespace)
            .map(|t| {
                let text = &s[t.start..t.end];
                let text = match t.kind {
                    Kind::Word => text.to_ascii_uppercase(),
                    Kind::LineComment => text.trim_end().to_string(),
                    _ => text.to_string(),
                };
                (t.kind, text)
            })
            .collect()
    }

    /// Formats checking that only whitespace / keyword case changed and that it is stable.
    fn fmt_core(s: &str, d: Driver, idents: &[&str]) -> String {
        let is_ident = |w: &str| idents.contains(&w);
        let out = format_sql(s, d, 0, &is_ident);
        assert_eq!(token_texts(s, d), token_texts(&out, d), "tokens changed:\n{s}\n---\n{out}");
        let again = format_sql(&out, d, 0, &is_ident);
        assert_eq!(out, again, "not idempotent:\n{out}\n---\n{again}");
        out
    }

    fn fmt_with(s: &str, d: Driver, idents: &[&str]) -> String {
        let out = fmt_core(s, d, idents);
        assert!(!out.lines().any(|l| l.ends_with(' ')), "trailing spaces:\n{out}");
        out
    }

    fn fmt(s: &str, d: Driver) -> String {
        fmt_with(s, d, &[])
    }

    #[test]
    fn format_basic_select() {
        let out = fmt("select o.id, u.name, o.total from orders o join users u on u.id = o.user_id where o.total > 10 and u.name like 'a%' order by o.total desc limit 5", Driver::Postgres);
        assert_eq!(
            out,
            "SELECT o.id,\n       u.name,\n       o.total\nFROM orders o\nJOIN users u ON u.id = o.user_id\nWHERE o.total > 10\n  AND u.name LIKE 'a%'\nORDER BY o.total DESC\nLIMIT 5"
        );
        // A single item stays on the SELECT line.
        assert_eq!(fmt("SELECT *   FROM t", Driver::Sqlite), "SELECT *\nFROM t");
        assert_eq!(fmt("select count(*) from t", Driver::Sqlite), "SELECT count(*)\nFROM t");
    }

    #[test]
    fn format_joins_groups_and_set_ops() {
        let out = fmt(
            "select a, count(*) from t left outer join u on t.id=u.id cross join v group by a having count(*) > 1 or a is null union all select 1, 2",
            Driver::MySql,
        );
        assert_eq!(
            out,
            "SELECT a,\n       count(*)\nFROM t\nLEFT OUTER JOIN u ON t.id=u.id\nCROSS JOIN v\nGROUP BY a\nHAVING count(*) > 1\n   OR a IS NULL\nUNION ALL\nSELECT 1,\n       2"
        );
        // LEFT( is a function, not a join.
        assert_eq!(fmt("select left(name, 2) from t", Driver::MySql), "SELECT LEFT(name, 2)\nFROM t");
        // T-SQL APPLY.
        assert!(fmt("select * from a cross apply f(a.x) b", Driver::MsSql).contains("\nCROSS APPLY f(a.x) b"));
    }

    #[test]
    fn format_dml() {
        assert_eq!(
            fmt("insert into t (a, b) values (1, 'x'), (2, 'y')", Driver::Postgres),
            "INSERT INTO t (a, b)\nVALUES (1, 'x'),\n       (2, 'y')"
        );
        assert_eq!(
            fmt("update t set a = 1, b = 'z' where id = 3 returning id", Driver::Postgres),
            "UPDATE t\nSET a = 1,\n    b = 'z'\nWHERE id = 3\nRETURNING id"
        );
        assert_eq!(fmt("delete from t where a between 1 and 5 and b = 2", Driver::Sqlite), "DELETE FROM t\nWHERE a BETWEEN 1 AND 5\n  AND b = 2");
        // ALTER … SET is not an UPDATE clause; DEFAULT VALUES stays together.
        assert_eq!(fmt("alter table t alter column c set default 1", Driver::Postgres), "ALTER TABLE t ALTER COLUMN c SET DEFAULT 1");
        assert_eq!(fmt("insert into t default values", Driver::Postgres), "INSERT INTO t DEFAULT VALUES");
        // MySQL ON DUPLICATE KEY UPDATE: VALUES() is a function there and keeps no space.
        let out = fmt("insert into t (a) values (1) on duplicate key update a = values(a), b = 2", Driver::MySql);
        assert!(out.ends_with("ON duplicate KEY UPDATE a = VALUES(a), b = 2"), "{out}");
    }

    #[test]
    fn format_subqueries_and_ctes() {
        assert_eq!(
            fmt("select * from (select a, b from t where x = 1) s where s.a in (select id from u)", Driver::Postgres),
            "SELECT *\nFROM (SELECT a,\n             b\n      FROM t\n      WHERE x = 1) s\nWHERE s.a IN (SELECT id\n              FROM u)"
        );
        assert_eq!(
            fmt("with a as (select 1 as x), b as (select 2 as y) select * from a, b", Driver::Postgres),
            "WITH a AS (SELECT 1 AS x),\n     b AS (SELECT 2 AS y)\nSELECT *\nFROM a, b"
        );
        assert_eq!(fmt("select exists(select 1 from t)", Driver::Postgres), "SELECT EXISTS (SELECT 1\n               FROM t)");
        // Non-query parentheses stay on one line, even with clause words inside.
        assert_eq!(fmt("select extract(year from d), trim(both 'x' from s) from t", Driver::Postgres), "SELECT extract(year FROM d),\n       trim(BOTH 'x' FROM s)\nFROM t");
        assert_eq!(
            fmt("select row_number() over (partition by a order by b) from t", Driver::Postgres),
            "SELECT row_number() OVER (PARTITION BY a ORDER BY b)\nFROM t"
        );
        assert_eq!(fmt("select a is distinct from b from t", Driver::Postgres), "SELECT a IS DISTINCT FROM b\nFROM t");
    }

    #[test]
    fn format_case_and_between() {
        assert_eq!(
            fmt("select case when a > 1 and b < 2 then 'x' else 'y' end as c from t where a between 1 and 2 or b = 1", Driver::Sqlite),
            "SELECT CASE WHEN a > 1 AND b < 2 THEN 'x' ELSE 'y' END AS c\nFROM t\nWHERE a BETWEEN 1 AND 2\n   OR b = 1"
        );
    }

    #[test]
    fn format_keeps_strings_comments_and_quotes() {
        // Keywords inside strings, quoted identifiers and comments are untouched.
        let out = fmt("select 'select from where', \"from\" , [order], `group` -- select x from y\nfrom t /* where and or */ where a='and or'", Driver::Sqlite);
        assert_eq!(
            out,
            "SELECT 'select from where',\n       \"from\",\n       [order],\n       `group` -- select x from y\nFROM t /* where and or */\nWHERE a='and or'"
        );
        // Line comment on its own line keeps its line; the next token never joins it.
        let out = fmt("-- header\nselect a, -- first\n b\nfrom t", Driver::Postgres);
        assert_eq!(out, "-- header\nSELECT a, -- first\n       b\nFROM t");
        // Comment before a clause goes at the clause's indentation.
        assert_eq!(fmt("select a\n-- the table\nfrom t", Driver::Postgres), "SELECT a\n-- the table\nFROM t");
        // MySQL # comments.
        let out = fmt("select a # from x\nfrom t", Driver::MySql);
        assert_eq!(out, "SELECT a # from x\nFROM t");
    }

    #[test]
    fn format_dialect_quoting() {
        // PG dollar quotes, casts, JSON operators and parameters keep their spacing.
        let s = "create function f() returns int as $$ begin\n  select  1 from t;  return 1; end $$ language plpgsql;\nselect x::text, j->>'k', -1, $1 from t where a=$2";
        let out = fmt(s, Driver::Postgres);
        assert!(out.contains("$$ begin\n  select  1 from t;  return 1; end $$"), "{out}");
        assert!(out.ends_with("SELECT x::TEXT,\n       j->>'k',\n       -1,\n       $1\nFROM t\nWHERE a=$2"), "{out}");
        // T-SQL brackets, variables, temp tables and GO.
        let out = fmt("declare @n int = 5\nselect [Order Id], @n from #tmp where [x] = @n\ngo\nselect 1", Driver::MsSql);
        assert_eq!(out, "DECLARE @n INT = 5\nSELECT [Order Id],\n       @n\nFROM #tmp\nWHERE [x] = @n\ngo\nSELECT 1");
        // MySQL backticks and user variables.
        assert_eq!(fmt("select `from`, @x:=1 from `select`", Driver::MySql), "SELECT `from`,\n       @x:=1\nFROM `select`");
        // Prefixed literals stay glued.
        assert_eq!(fmt("select _utf8'x', N'y'", Driver::MySql), "SELECT _utf8'x',\n       N'y'");
    }

    #[test]
    fn format_statements_and_blank_lines() {
        let out = fmt("select 1;select 2;\n\n\nselect 3", Driver::Sqlite);
        assert_eq!(out, "SELECT 1;\nSELECT 2;\n\nSELECT 3");
        let out = fmt("CREATE TABLE IF NOT EXISTS t (id integer primary key, name text not null, price real);", Driver::Sqlite);
        assert_eq!(out, "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, price REAL);");
    }

    #[test]
    fn format_keyword_case_of_names() {
        // Non-reserved keywords used as names keep their case; reserved ones are uppercased.
        let out = fmt("select t.date, type from text as key where date > 1", Driver::Postgres);
        assert_eq!(out, "SELECT t.date,\n       TYPE\nFROM text AS key\nWHERE DATE > 1");
        let out = fmt_with("select t.date, type from text where date > 1", Driver::Postgres, &["type", "date"]);
        assert_eq!(out, "SELECT t.date,\n       type\nFROM text\nWHERE date > 1");
        // CAST(… AS type) is a type, not an alias.
        assert_eq!(fmt("select cast(x as int) as n", Driver::Postgres), "SELECT CAST(x AS INT) AS n");
    }

    #[test]
    fn format_indent_and_robustness() {
        let is_ident = |_: &str| false;
        // Starting at column 4: the next lines align with it.
        assert_eq!(format_sql("select a, b from t", Driver::Postgres, 4, &is_ident), "SELECT a,\n           b\n    FROM t");
        for d in Driver::ALL {
            for s in [
                "", "   ", "SELECT 'abc", "SELECT /* abc", "select (((", "))) from", "select ,,, ;;", "-- x", "a -- x\n-- y\nb",
                "SELECT $$ abc", "SELECT [abc", "go", "SELECT 1\nGO\nGO", "select case end end end", "SELECT 'canción 🎵' AS título FROM café",
            ] {
                let out = format_sql(s, d, 0, &is_ident);
                assert_eq!(token_texts(s, d), token_texts(&out, d), "{s:?} {d:?}");
                assert_eq!(format_sql(&out, d, 0, &is_ident), out, "{s:?} {d:?}");
            }
        }
    }

    #[test]
    fn format_random_token_soup() {
        // Pseudo-random scripts: only whitespace / keyword case may change, and it is stable.
        let vocab = [
            "select", "from", "where", "and", "or", "between", "case", "when", "then", "end", "group", "by", "order",
            "left", "join", "on", "union", "all", "values", "update", "set", "insert", "into", "with", "as", "(", ")",
            ",", ";", ".", "a", "t", "date", "1", "'x y'", "\"q\"", "[b]", "`c`", "-- note\n", "/* c */", "=", "-",
            "*", "::", "$1", "$$ body $$", "@v", "#t", "\n", "\n\n", "go\n", "  ", "1.5", "e", "n", "'b'", "[", "]",
            "<", ">", "!", "/", "+", "_utf8", "q", "{", "}", "exists", "distinct", "is", "having", "limit", "apply",
            "cross", "outer", "default", "returning", "'a\\'b'", "\"x\\\"y\"", "\\", "'", "q'[s; t]'", "nq'{u}'",
            "begin", "declare", "if", "loop", "procedure", "create", "is", "/\n", "\n/\n", "'p'\n'q'", "delimiter",
        ];
        let mut seed = 0x2545F491u64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..1500 {
            let len = 1 + rnd(40);
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(vocab[rnd(vocab.len())]);
                if rnd(3) > 0 {
                    s.push(' ');
                }
            }
            for d in Driver::ALL {
                fmt_core(&s, d, &[]);
                // The splitter keeps exact, ordered byte ranges.
                check_ranges(&s, &split_statements(&s, d));
                check_ranges(&s, &split_batches(&s, d));
                let _ = statement_at(&s, rnd(s.len() + 1), d);
            }
        }
    }

    #[test]
    fn explain_per_dialect() {
        assert_eq!(explain_statements("select 1;", Driver::Postgres), ["EXPLAIN select 1"]);
        assert_eq!(explain_statements(" select 1 ", Driver::MySql), ["EXPLAIN select 1"]);
        assert_eq!(explain_statements("select 1", Driver::MariaDb), ["EXPLAIN select 1"]);
        assert_eq!(explain_statements("select 1", Driver::Sqlite), ["EXPLAIN QUERY PLAN select 1"]);
        assert_eq!(
            explain_statements("select 1", Driver::MsSql),
            ["SET SHOWPLAN_TEXT ON", "select 1", "SET SHOWPLAN_TEXT OFF"]
        );
        assert_eq!(
            explain_statements("select 1 from dual", Driver::Oracle),
            ["EXPLAIN PLAN FOR select 1 from dual", "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY())"]
        );
        assert!(explain_statements("  ; ", Driver::Postgres).is_empty());
    }

    #[test]
    fn mysql_hash_comments() {
        assert_eq!(texts("SELECT 1 # a; b\n; SELECT 2", Driver::MySql), ["SELECT 1", "SELECT 2"]);
        // Elsewhere # is not a comment.
        assert_eq!(texts("SELECT #t; SELECT 2", Driver::MsSql).len(), 2);
    }

    fn check_ranges(s: &str, st: &[Statement]) {
        let mut last = 0;
        for x in st {
            assert!(x.start >= last && x.start < x.end && x.end <= s.len(), "{st:?}");
            assert_eq!(&s[x.start..x.end], x.text);
            last = x.end;
        }
    }

    #[test]
    fn snowflake_strings_and_comments() {
        let d = Driver::Snowflake;
        // Backslash escapes in '…', but "…" stays a (case-sensitive) identifier.
        assert_eq!(texts("select 'a\\';b'; select 2", d), ["select 'a\\';b'", "select 2"]);
        assert_eq!(texts("select \"a\\\"; select 2", d), ["select \"a\\\"", "select 2"]);
        // $$…$$ procedure bodies keep their semicolons.
        let proc = "create procedure p() returns int language sql as $$ begin return 1; end; $$; call p()";
        assert_eq!(texts(proc, d).len(), 2);
        // `//` comments.
        assert_eq!(texts("// a; b\nselect 1; // c;\nselect 2", d), ["select 1", "select 2"]);
        assert_eq!(texts("select 4 / 2; select 2", d).len(), 2);
        assert_eq!(split_batches("use database x; select 1", d).len(), 2);
    }

    #[test]
    fn mysql_backslash_escapes() {
        let s = "select 'a\\';b'; select 2";
        assert_eq!(texts(s, Driver::MySql), ["select 'a\\';b'", "select 2"]);
        assert_eq!(texts("select \"x\\\";y\", 'it''s;'; select 2", Driver::MariaDb).len(), 2);
        assert_eq!(texts("select N'a\\';b'; select 2", Driver::MySql).len(), 2);
        // Elsewhere a backslash is an ordinary char.
        assert_eq!(texts("select 'C:\\'; select 2", Driver::Postgres), ["select 'C:\\'", "select 2"]);
        assert_eq!(texts("select 'C:\\'; select 2", Driver::Oracle).len(), 2);
        // PostgreSQL E'…' strings escape with backslash.
        assert_eq!(texts("select E'a\\';b'; select 2", Driver::Postgres).len(), 2);
        assert_eq!(texts("select e'a\\';b'; select 2", Driver::Postgres).len(), 2);
        // The formatter leaves the literal intact.
        let out = fmt("select 'don\\'t select  from x' from t", Driver::MySql);
        assert_eq!(out, "SELECT 'don\\'t select  from x'\nFROM t");
        let out = fmt("select \"a\\\" from  b\" , 1", Driver::MySql);
        assert_eq!(out, "SELECT \"a\\\" from  b\",\n       1");
        // Highlighting with the engine's rules.
        let s = "select 'a\\';b' from t";
        let job = highlight_for(s, Driver::MySql, false, egui::FontId::monospace(13.0));
        let text: String =
            job.sections.iter().map(|sec| &job.text[sec.byte_range.start.0..sec.byte_range.end.0]).collect();
        assert_eq!(text, s);
        let str_sec = job.sections.iter().find(|sec| sec.byte_range.start.0 == 7).unwrap();
        assert_eq!(str_sec.byte_range.end.0, s.find(" from").unwrap());
        assert_eq!(str_sec.format.color, Palette::LIGHT.string);
    }

    #[test]
    fn oracle_plsql_blocks() {
        let d = Driver::Oracle;
        // Without `/`: up to the `;` of the outermost END, which is kept.
        let s = "BEGIN\n  IF x THEN y; END IF;\n  FOR i IN 1..3 LOOP z; END LOOP;\n  CASE a WHEN 1 THEN b; END CASE;\nEND;\nselect 1 from dual;";
        let st = split_statements(s, d);
        check_ranges(s, &st);
        assert_eq!(st.len(), 2, "{st:?}");
        assert!(st[0].text.starts_with("BEGIN") && st[0].text.ends_with("END;"));
        assert_eq!(st[1].text, "select 1 from dual");
        let s = "DECLARE\n  n NUMBER := 1;\nBEGIN\n  BEGIN NULL; END;\n  n := 2;\nEND my_block;\nselect 2 from dual";
        let t = texts(s, d);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END my_block;"));
        let s = "CREATE OR REPLACE EDITIONABLE PROCEDURE p(a NUMBER) IS\n  v NUMBER;\n  CURSOR c IS SELECT 1 FROM dual;\nBEGIN\n  v := CASE WHEN a > 1 THEN 1 ELSE 2 END;\nEND p;\nSELECT 3 FROM dual;";
        let t = texts(s, d);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END p;"));
        let s = "CREATE OR REPLACE PACKAGE BODY pk AS\n  PROCEDURE a IS BEGIN NULL; END a;\n  FUNCTION b RETURN NUMBER IS BEGIN RETURN 1; END;\nBEGIN\n  NULL;\nEND pk;\nSELECT 4 FROM dual";
        let t = texts(s, d);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END pk;"));
        let s = "CREATE PACKAGE pk AS\n  PROCEDURE a;\n  FUNCTION b RETURN NUMBER;\nEND pk;\nSELECT 5 FROM dual";
        assert_eq!(texts(s, d).len(), 2);
        let s = "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t REFERENCING NEW AS n FOR EACH ROW\nBEGIN\n  :n.x := 1;\nEND;\nSELECT 6 FROM dual";
        assert_eq!(texts(s, d).len(), 2);
        let s = "CREATE TYPE BODY ty AS\n  MEMBER FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;\nEND;\nSELECT 7 FROM dual";
        assert_eq!(texts(s, d).len(), 2);
        let s = "CREATE TYPE ty AS OBJECT (a NUMBER);\nSELECT 8 FROM dual";
        assert_eq!(texts(s, d), ["CREATE TYPE ty AS OBJECT (a NUMBER);", "SELECT 8 FROM dual"]);
        // With `/`: up to that line, whatever the nesting looks like.
        let s = "begin\n  x;\nend;\n/\nselect 1 from dual\n/\n  /\nDECLARE n NUMBER; BEGIN IF a THEN b; END IF; END;\n/";
        let st = split_statements(s, d);
        check_ranges(s, &st);
        let t: Vec<_> = st.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(t, ["begin\n  x;\nend;", "select 1 from dual", "DECLARE n NUMBER; BEGIN IF a THEN b; END IF; END;"]);
        // Plain SQL is still split by `;`; a `/` in an expression is a division.
        assert_eq!(texts("select 4 / 2 from dual; select 1 from dual", d).len(), 2);
        assert_eq!(texts("select 4\n/ 2 from dual", d).len(), 1);
        // The block under the cursor is the whole block.
        let s = "select 1 from dual;\nBEGIN\n  a;\n  b;\nEND;\n/\nselect 2 from dual;";
        let pos = s.find("b;").unwrap();
        let st = statement_at(s, pos, d).unwrap();
        assert_eq!(st.text, "BEGIN\n  a;\n  b;\nEND;");
        assert_eq!(&s[st.start..st.end], st.text);
        assert_eq!(split_batches(s, d).len(), 3);
        // Other engines don't treat `/` or BEGIN specially.
        assert_eq!(texts("BEGIN; SELECT 1; COMMIT", Driver::Postgres).len(), 3);
        assert_eq!(texts("BEGIN; SELECT 1; COMMIT", Driver::MySql).len(), 3);
    }

    #[test]
    fn oracle_q_quotes() {
        let d = Driver::Oracle;
        for s in [
            "select q'[it's; x]' from dual; select 2 from dual",
            "select Q'{a ' ; }' from dual; select 2 from dual",
            "select q'(a ';' )' from dual; select 2 from dual",
            "select q'<a;>' from dual; select 2 from dual",
            "select q'!a;'!' from dual; select 2 from dual",
            "select nq'[ñ;]' from dual; select 2 from dual",
            "select NQ'#x;#' from dual; select 2 from dual",
        ] {
            let st = split_statements(s, d);
            check_ranges(s, &st);
            assert_eq!(st.len(), 2, "{s}");
        }
        assert_eq!(fmt("select q'[select  ; from]' as x from dual", d), "SELECT q'[select  ; from]' AS x\nFROM dual");
        // Elsewhere `q` is a name.
        assert_eq!(texts("select q'x'; select 2", Driver::Postgres).len(), 2);
        assert_eq!(texts("select q 'a;' from t", d).len(), 1);
        // Unclosed: to the end, no panic.
        assert_eq!(texts("select q'[abc; def", d).len(), 1);
        assert_eq!(texts("select q'", d).len(), 1);
    }

    #[test]
    fn mysql_stored_routines() {
        let d = Driver::MySql;
        let s = "CREATE PROCEDURE p() BEGIN select 1; select 2; END; select 3";
        let st = split_statements(s, d);
        check_ranges(s, &st);
        let t: Vec<_> = st.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(t, ["CREATE PROCEDURE p() BEGIN select 1; select 2; END", "select 3"]);
        let s = "CREATE DEFINER=`root`@`%` PROCEDURE p(n INT)\nBEGIN\n  DECLARE i INT DEFAULT 0;\n  IF n > 0 THEN SET i = IF(n > 5, 1, 2); END IF;\n  lbl: WHILE i < n DO SET i = i + 1; END WHILE lbl;\n  REPEAT SET i = i - 1; UNTIL i = 0 END REPEAT;\n  l2: LOOP LEAVE l2; END LOOP;\n  CASE i WHEN 0 THEN SELECT REPEAT('x', 2); ELSE BEGIN END; END CASE;\nEND;\nselect 4";
        let t = texts(s, d);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END CASE;\nEND"));
        let s = "create definer = admin@localhost function f() returns int deterministic begin return 1; end;\nselect f()";
        assert_eq!(texts(s, Driver::MariaDb).len(), 2);
        // A routine without BEGIN ends at its `;`.
        assert_eq!(texts("CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET NEW.a = 1; select 1", d).len(), 2);
        assert_eq!(texts("CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN DELETE FROM t; DELETE FROM u; END; select 1", d).len(), 2);
        assert_eq!(texts("BEGIN NOT ATOMIC SELECT 1; SELECT 2; END; SELECT 3", Driver::MariaDb).len(), 2);
        // Transactions and other CREATEs are plain statements.
        assert_eq!(texts("BEGIN; INSERT INTO t VALUES (1); COMMIT;", d).len(), 3);
        assert_eq!(texts("CREATE TABLE t (begin_at INT); SELECT 1", d).len(), 2);
        // Statement under the cursor.
        let s = "select 0;\nCREATE PROCEDURE p() BEGIN select 1; select 2; END;\nselect 3;";
        let st = statement_at(s, s.find("select 2").unwrap(), d).unwrap();
        assert!(st.text.starts_with("CREATE PROCEDURE") && st.text.ends_with("END"));
    }

    #[test]
    fn mysql_delimiter_directive() {
        let d = Driver::MySql;
        let s = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND$$\nCREATE FUNCTION f() RETURNS INT RETURN 1 $$\ndelimiter ;\nSELECT 'a;b'; SELECT 2;";
        let st = split_statements(s, d);
        check_ranges(s, &st);
        let t: Vec<_> = st.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(
            t,
            ["CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND", "CREATE FUNCTION f() RETURNS INT RETURN 1", "SELECT 'a;b'", "SELECT 2"]
        );
        // `//` and a delimiter inside strings or comments.
        let s = "DELIMITER //\nSELECT '//' /* // */; SELECT 2 //\nSELECT 3//";
        assert_eq!(texts(s, d), ["SELECT '//' /* // */; SELECT 2", "SELECT 3"]);
        // A word delimiter only matches as a whole token.
        assert_eq!(texts("DELIMITER go\nSELECT category go SELECT 1", d), ["SELECT category", "SELECT 1"]);
        // `delimiter` as a column name is not a directive.
        assert_eq!(texts("SELECT a,\ndelimiter x FROM t; SELECT 2", d).len(), 2);
        // Only on MySQL.
        assert_eq!(texts("DELIMITER $$\nSELECT 1$$", Driver::Postgres).len(), 1);
    }

    #[test]
    fn format_adjacent_literals_and_slash() {
        // PostgreSQL concatenates adjacent literals only across a newline: keep it.
        let out = fmt("select 'a'\n'b', 'c' 'd' from t", Driver::Postgres);
        assert_eq!(out, "SELECT 'a'\n       'b',\n       'c' 'd'\nFROM t");
        // Oracle `/` lines stay alone; a division never ends up alone on a line.
        let out = fmt("begin null; end;\n/\nselect 4 / 2 from dual", Driver::Oracle);
        assert_eq!(out, "BEGIN NULL;\nEND;\n/\nSELECT 4 / 2\nFROM dual");
        let out = fmt("select 1;\n/ from dual", Driver::Oracle);
        assert_eq!(texts(&out, Driver::Oracle).len(), texts("select 1;\n/ from dual", Driver::Oracle).len());
    }
}
