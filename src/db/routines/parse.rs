//! Reads a routine's `CREATE` statement back into the dialog's fields. Anything it doesn't
//! understand returns `None`, and the dialog falls back to editing the statement as text.

use super::{Column, Param, ParamMode, ReturnKind, Returns, RoutineKind, RoutineOptions, RoutineSpec};
use crate::db::Driver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Word,
    /// `"x"`, `` `x` `` or `[x]`.
    Quoted,
    /// `'…'` (with an `N`/`E` prefix).
    Str,
    /// `$tag$…$tag$`.
    Dollar,
    Num,
    Punct(u8),
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Tok,
    start: usize,
    end: usize,
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c >= 0x80
}

fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'#' | b'@') || c >= 0x80
}

fn tokenize(text: &str, driver: Driver) -> Vec<Token> {
    let b = text.as_bytes();
    let backslash = matches!(driver, Driver::MySql | Driver::MariaDb | Driver::Snowflake);
    let dollar = matches!(driver, Driver::Postgres | Driver::Snowflake);
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') || (c == b'#' && matches!(driver, Driver::MySql | Driver::MariaDb)) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        // N'…', E'…', X'…' and B'…' string literals.
        let prefixed = matches!(c, b'N' | b'n' | b'E' | b'e' | b'X' | b'x' | b'B' | b'b')
            && b.get(i + 1) == Some(&b'\'')
            && (i == 0 || !is_word_char(b[i - 1]));
        if c == b'\'' || prefixed {
            i += if prefixed { 2 } else { 1 };
            while i < b.len() {
                if backslash && b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Token { kind: Tok::Str, start, end: i.min(b.len()) });
            continue;
        }
        if dollar && c == b'$' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if b.get(j) == Some(&b'$') {
                let tag = &text[i..=j];
                if let Some(close) = text[j + 1..].find(tag) {
                    i = j + 1 + close + tag.len();
                    out.push(Token { kind: Tok::Dollar, start, end: i });
                    continue;
                }
            }
        }
        let close = match c {
            b'"' => Some(b'"'),
            b'`' if matches!(driver, Driver::MySql | Driver::MariaDb) => Some(b'`'),
            b'[' if driver == Driver::MsSql => Some(b']'),
            _ => None,
        };
        if let Some(close) = close {
            i += 1;
            while i < b.len() {
                if b[i] == close {
                    if b.get(i + 1) == Some(&close) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Token { kind: Tok::Quoted, start, end: i.min(b.len()) });
            continue;
        }
        if is_word_start(c) && !(c == b'@' && !b.get(i + 1).is_some_and(|&n| is_word_char(n))) {
            i += 1;
            while i < b.len() && is_word_char(b[i]) {
                i += 1;
            }
            out.push(Token { kind: Tok::Word, start, end: i });
            continue;
        }
        if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            out.push(Token { kind: Tok::Num, start, end: i });
            continue;
        }
        // Multi-byte characters outside words are skipped whole.
        let len = text[i..].chars().next().map_or(1, char::len_utf8);
        out.push(Token { kind: Tok::Punct(c), start, end: i + len });
        i += len;
    }
    out
}

/// Splits `text` at commas outside parentheses and quotes.
pub(crate) fn split_top_level(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for c in text.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            },
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Text of a string literal without its quotes (and prefix), with escapes undone.
fn unquote_string(raw: &str, backslash: bool) -> String {
    let inner = raw.strip_prefix(|c: char| c.is_ascii_alphabetic()).unwrap_or(raw);
    let inner = inner.strip_prefix('\'').unwrap_or(inner);
    let inner = inner.strip_suffix('\'').unwrap_or(inner);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            '\\' if backslash => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some(other) => out.push(other),
                None => {}
            },
            c => out.push(c),
        }
    }
    out
}

/// Identifier text without its quotes.
fn unquote_ident(raw: &str) -> String {
    let b = raw.as_bytes();
    match b.first() {
        Some(b'"') => raw[1..raw.len().saturating_sub(1).max(1)].replace("\"\"", "\""),
        Some(b'`') => raw[1..raw.len().saturating_sub(1).max(1)].replace("``", "`"),
        Some(b'[') => raw[1..raw.len().saturating_sub(1).max(1)].replace("]]", "]"),
        _ => raw.to_string(),
    }
}

/// A dollar-quoted or quoted body without its delimiters and the newlines next to them.
fn body_text(text: &str, tok: Token, backslash: bool) -> String {
    let raw = &text[tok.start..tok.end];
    let inner = match tok.kind {
        Tok::Dollar => {
            let tag_len = raw[1..].find('$').map_or(1, |i| i + 2);
            raw[tag_len..raw.len() - tag_len].to_string()
        }
        _ => unquote_string(raw, backslash),
    };
    trim_body(&inner)
}

/// The body without the blank line the generator (and most servers) put around it.
fn trim_body(body: &str) -> String {
    let body = body.strip_prefix("\r\n").or_else(|| body.strip_prefix('\n')).unwrap_or(body);
    let body = body.strip_suffix("\r\n").or_else(|| body.strip_suffix('\n')).unwrap_or(body);
    body.to_string()
}

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Token>,
    i: usize,
    driver: Driver,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str, driver: Driver) -> Self {
        Self { text, toks: tokenize(text, driver), i: 0, driver }
    }

    fn tok(&self, i: usize) -> Option<Token> {
        self.toks.get(i).copied()
    }

    fn word_at(&self, i: usize) -> Option<String> {
        self.tok(i).filter(|t| t.kind == Tok::Word).map(|t| self.text[t.start..t.end].to_ascii_uppercase())
    }

    fn is(&self, kw: &str) -> bool {
        self.word_at(self.i).as_deref() == Some(kw)
    }

    fn is_seq(&self, kws: &[&str]) -> bool {
        kws.iter().enumerate().all(|(k, kw)| self.word_at(self.i + k).as_deref() == Some(*kw))
    }

    fn eat(&mut self, kw: &str) -> bool {
        let yes = self.is(kw);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn eat_seq(&mut self, kws: &[&str]) -> bool {
        let yes = self.is_seq(kws);
        if yes {
            self.i += kws.len();
        }
        yes
    }

    fn is_punct(&self, c: u8) -> bool {
        self.tok(self.i).is_some_and(|t| t.kind == Tok::Punct(c))
    }

    fn eat_punct(&mut self, c: u8) -> bool {
        let yes = self.is_punct(c);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn done(&self) -> bool {
        self.i >= self.toks.len()
    }

    /// Original text from token `from` to just before token `to`.
    fn span(&self, from: usize, to: usize) -> String {
        if from >= to || from >= self.toks.len() {
            return String::new();
        }
        let end = self.toks[to.min(self.toks.len()) - 1].end;
        self.text[self.toks[from].start..end].trim().to_string()
    }

    /// Index of the `)` that closes the `(` at token `open`.
    fn matching(&self, open: usize) -> Option<usize> {
        let mut depth = 0;
        for k in open..self.toks.len() {
            match self.toks[k].kind {
                Tok::Punct(b'(') => depth += 1,
                Tok::Punct(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(k);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Token ranges of the comma-separated items in the parentheses at the cursor (which then
    /// moves past them).
    fn group_items(&mut self) -> Option<Vec<(usize, usize)>> {
        if !self.is_punct(b'(') {
            return None;
        }
        let close = self.matching(self.i)?;
        let mut items = Vec::new();
        let mut depth = 0;
        let mut start = self.i + 1;
        for k in self.i + 1..close {
            match self.toks[k].kind {
                Tok::Punct(b'(') => depth += 1,
                Tok::Punct(b')') => depth -= 1,
                Tok::Punct(b',') if depth == 0 => {
                    items.push((start, k));
                    start = k + 1;
                }
                _ => {}
            }
        }
        if start < close {
            items.push((start, close));
        }
        self.i = close + 1;
        Some(items)
    }

    /// Moves to the first token at depth 0 that is one of `stops` (or the end); returns the span.
    fn until(&mut self, stops: &[&str]) -> String {
        let from = self.i;
        let mut depth = 0;
        while let Some(t) = self.tok(self.i) {
            match t.kind {
                Tok::Punct(b'(') => depth += 1,
                Tok::Punct(b')') => depth -= 1,
                Tok::Word if depth == 0 && stops.contains(&self.text[t.start..t.end].to_ascii_uppercase().as_str()) => break,
                Tok::Str | Tok::Dollar if depth == 0 && stops.contains(&"<STRING>") => break,
                _ => {}
            }
            self.i += 1;
        }
        self.span(from, self.i)
    }

    fn ident(&mut self) -> Option<String> {
        let t = self.tok(self.i)?;
        if !matches!(t.kind, Tok::Word | Tok::Quoted) {
            return None;
        }
        self.i += 1;
        Some(unquote_ident(&self.text[t.start..t.end]))
    }

    fn object_ident(&mut self) -> Option<String> {
        let quoted = self.tok(self.i)?.kind == Tok::Quoted;
        let mut name = self.ident()?;
        if !quoted {
            match self.driver {
                Driver::Oracle | Driver::Snowflake => name.make_ascii_uppercase(),
                Driver::Postgres => name.make_ascii_lowercase(),
                _ => {}
            }
        }
        Some(name)
    }

    fn string(&mut self) -> Option<String> {
        let t = self.tok(self.i).filter(|t| t.kind == Tok::Str)?;
        self.i += 1;
        Some(unquote_string(&self.text[t.start..t.end], matches!(self.driver, Driver::MySql | Driver::MariaDb | Driver::Snowflake)))
    }

    /// The rest of the text from the cursor.
    fn rest(&self) -> String {
        match self.tok(self.i) {
            Some(t) => self.text[t.start..].trim().to_string(),
            None => String::new(),
        }
    }

    /// `CREATE [OR REPLACE|OR ALTER] [modifiers] PROCEDURE|FUNCTION [schema.]name`.
    fn header(&mut self, spec: &mut RoutineSpec) -> Option<()> {
        if !self.eat("CREATE") && !self.eat("ALTER") {
            return None;
        }
        loop {
            if self.eat_seq(&["OR", "REPLACE"]) || self.eat_seq(&["OR", "ALTER"]) {
                continue;
            }
            if self.eat("DEFINER") {
                self.eat_punct(b'=');
                spec.options.definer = self.until(&["PROCEDURE", "FUNCTION"]);
                continue;
            }
            if self.eat("SECURE") {
                spec.options.secure = true;
                continue;
            }
            if self.eat("EDITIONABLE") || self.eat("NONEDITIONABLE") || self.eat("TEMPORARY") || self.eat("TEMP") {
                continue;
            }
            break;
        }
        spec.kind = if self.eat("PROCEDURE") || self.eat("PROC") {
            RoutineKind::Procedure
        } else if self.eat("FUNCTION") {
            RoutineKind::Function
        } else {
            return None;
        };
        let mut parts = vec![self.object_ident()?];
        while self.eat_punct(b'.') {
            parts.push(self.object_ident()?);
        }
        spec.name = parts.pop()?;
        spec.schema = parts.pop().unwrap_or_default();
        Some(())
    }

    /// The token range `(from, to)` as a parameter of this dialect.
    fn param(&self, from: usize, to: usize) -> Option<Param> {
        let word = |k: usize| if k < to { self.word_at(k) } else { None };
        let mut p = Param::default();
        let mut k = from;
        match self.driver {
            Driver::Postgres => {
                p.mode = match word(k).as_deref() {
                    Some("IN") if to - k > 1 => ParamMode::In,
                    Some("OUT") if to - k > 1 => ParamMode::Out,
                    Some("INOUT") if to - k > 1 => ParamMode::InOut,
                    Some("VARIADIC") if to - k > 1 => ParamMode::Variadic,
                    _ => {
                        k = k.wrapping_sub(1);
                        ParamMode::In
                    }
                };
                k = k.wrapping_add(1);
                let default_at = (k..to).find(|&j| word(j).as_deref() == Some("DEFAULT") || self.toks[j].kind == Tok::Punct(b'='));
                let type_end = default_at.unwrap_or(to);
                if let Some(d) = default_at {
                    p.default = self.span(d + 1, to);
                }
                if type_end - k >= 2 && pg_named(self, k) {
                    p.name = unquote_ident(&self.text[self.toks[k].start..self.toks[k].end]);
                    k += 1;
                }
                p.data_type = self.span(k, type_end);
            }
            Driver::MySql | Driver::MariaDb => {
                p.mode = match word(k).as_deref() {
                    Some("IN") => ParamMode::In,
                    Some("OUT") => ParamMode::Out,
                    Some("INOUT") => ParamMode::InOut,
                    _ => {
                        k = k.wrapping_sub(1);
                        ParamMode::In
                    }
                };
                k = k.wrapping_add(1);
                p.name = unquote_ident(&self.text[self.toks.get(k)?.start..self.toks.get(k)?.end]);
                p.data_type = self.span(k + 1, to);
            }
            Driver::MsSql => {
                p.name = self.text[self.toks.get(k)?.start..self.toks.get(k)?.end].to_string();
                k += 1;
                if word(k).as_deref() == Some("AS") {
                    k += 1;
                }
                let mut end = to;
                let mut readonly = false;
                while end > k {
                    match word(end - 1).as_deref() {
                        Some("OUT" | "OUTPUT") => p.mode = ParamMode::Out,
                        Some("READONLY") => readonly = true,
                        _ => break,
                    }
                    end -= 1;
                }
                let eq = (k..end).find(|&j| self.toks[j].kind == Tok::Punct(b'='));
                if let Some(eq) = eq {
                    p.default = self.span(eq + 1, end);
                }
                p.data_type = self.span(k, eq.unwrap_or(end));
                if readonly {
                    p.data_type.push_str(" readonly");
                }
            }
            Driver::Oracle => {
                p.name = unquote_ident(&self.text[self.toks.get(k)?.start..self.toks.get(k)?.end]);
                k += 1;
                p.mode = if word(k).as_deref() == Some("IN") && word(k + 1).as_deref() == Some("OUT") {
                    k += 2;
                    ParamMode::InOut
                } else if word(k).as_deref() == Some("OUT") {
                    k += 1;
                    ParamMode::Out
                } else {
                    if word(k).as_deref() == Some("IN") {
                        k += 1;
                    }
                    ParamMode::In
                };
                let assign = (k..to).find(|&j| {
                    word(j).as_deref() == Some("DEFAULT")
                        || (self.toks[j].kind == Tok::Punct(b':') && self.toks.get(j + 1).is_some_and(|t| t.kind == Tok::Punct(b'=')))
                });
                if let Some(a) = assign {
                    let skip = if word(a).is_some() { 1 } else { 2 };
                    p.default = self.span(a + skip, to);
                }
                p.data_type = self.span(k, assign.unwrap_or(to));
            }
            _ => {
                p.name = unquote_ident(&self.text[self.toks.get(k)?.start..self.toks.get(k)?.end]);
                k += 1;
                let default_at = (k..to).find(|&j| word(j).as_deref() == Some("DEFAULT"));
                if let Some(d) = default_at {
                    p.default = self.span(d + 1, to);
                }
                p.data_type = self.span(k, default_at.unwrap_or(to));
            }
        }
        (!p.data_type.is_empty()).then_some(p)
    }

    fn params(&mut self) -> Option<Vec<Param>> {
        let items = self.group_items()?;
        items.into_iter().map(|(a, b)| self.param(a, b)).collect()
    }

    /// `(name type, …)` of a returned table.
    fn columns(&mut self) -> Option<Vec<Column>> {
        let items = self.group_items()?;
        items
            .into_iter()
            .map(|(a, b)| {
                let name = unquote_ident(&self.text[self.toks[a].start..self.toks[a].end]);
                let data_type = self.span(a + 1, b);
                (!data_type.is_empty()).then_some(Column { name, data_type })
            })
            .collect()
    }
}

/// PostgreSQL arguments may be unnamed: `double precision` is a type, `d precision` is not.
fn pg_named(p: &Parser<'_>, k: usize) -> bool {
    if p.toks[k].kind == Tok::Quoted {
        return true;
    }
    let first = p.word_at(k).unwrap_or_default();
    let second = p.word_at(k + 1).unwrap_or_default();
    let multi_word = matches!(first.as_str(), "DOUBLE" | "CHARACTER" | "CHAR" | "BIT" | "TIME" | "TIMESTAMP" | "INTERVAL" | "NATIONAL")
        && (matches!(second.as_str(), "PRECISION" | "VARYING" | "WITH" | "WITHOUT" | "CHARACTER" | "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND")
            || p.toks.get(k + 1).is_some_and(|t| t.kind == Tok::Punct(b'(')));
    !multi_word
}

const PG_CLAUSES: &[&str] = &[
    "RETURNS", "LANGUAGE", "IMMUTABLE", "STABLE", "VOLATILE", "STRICT", "CALLED", "SECURITY", "EXTERNAL", "PARALLEL",
    "LEAKPROOF", "NOT", "COST", "ROWS", "SUPPORT", "SET", "WINDOW", "TRANSFORM", "AS", "BEGIN",
];

fn parse_postgres(p: &mut Parser<'_>, spec: &mut RoutineSpec) -> Option<()> {
    spec.params = p.params()?;
    let mut extra = Vec::new();
    loop {
        if p.done() {
            return None;
        }
        if p.eat_seq(&["RETURNS", "NULL", "ON", "NULL", "INPUT"]) || p.eat("STRICT") {
            spec.options.strict = true;
        } else if p.eat_seq(&["CALLED", "ON", "NULL", "INPUT"]) {
        } else if p.eat("RETURNS") {
            if p.eat("SETOF") {
                spec.returns.kind = ReturnKind::SetOf;
                spec.returns.data_type = p.until(PG_CLAUSES);
            } else if p.is("TABLE") && p.tok(p.i + 1).is_some_and(|t| t.kind == Tok::Punct(b'(')) {
                p.i += 1;
                spec.returns.kind = ReturnKind::Table;
                spec.returns.columns = p.columns()?;
            } else {
                spec.returns.data_type = p.until(PG_CLAUSES);
            }
        } else if p.eat("LANGUAGE") {
            spec.options.language = p.ident()?;
        } else if p.is("IMMUTABLE") || p.is("STABLE") || p.is("VOLATILE") {
            spec.options.volatility = p.word_at(p.i)?;
            p.i += 1;
        } else if p.eat_seq(&["EXTERNAL", "SECURITY"]) || p.eat("SECURITY") {
            spec.options.security = p.word_at(p.i)?;
            p.i += 1;
        } else if p.eat("PARALLEL") {
            spec.options.parallel = p.word_at(p.i)?;
            p.i += 1;
        } else if p.eat("AS") {
            let t = p.tok(p.i).filter(|t| matches!(t.kind, Tok::Str | Tok::Dollar))?;
            spec.body = body_text(p.text, t, false);
            p.i += 1;
            // `AS 'file', 'symbol'` (C functions) has no editable body.
            if p.is_punct(b',') {
                return None;
            }
        } else if p.is("BEGIN") {
            // SQL-standard bodies (`BEGIN ATOMIC … END`) stay as text.
            return None;
        } else if p.is("NOT") {
            extra.push(p.span(p.i, p.i + 2));
            p.i += 2;
        } else if p.is_punct(b';') {
            p.i += 1;
        } else {
            let from = p.i;
            p.i += 1;
            p.until(PG_CLAUSES);
            extra.push(p.span(from, p.i));
        }
        if p.done() {
            break;
        }
    }
    if spec.kind == RoutineKind::Procedure {
        spec.returns = Returns::default();
    }
    spec.options.extra = extra.join("\n");
    Some(())
}

/// A MySQL type: name, `(…)` and attributes such as `UNSIGNED` or `CHARSET x`.
fn mysql_type(p: &mut Parser<'_>) -> Option<String> {
    let from = p.i;
    p.ident()?;
    let second = match p.word_at(p.i - 1).as_deref() {
        Some("DOUBLE") => &["PRECISION"][..],
        Some("NATIONAL") => &["CHAR", "CHARACTER", "VARCHAR"][..],
        Some("LONG") => &["VARCHAR", "VARBINARY"][..],
        _ => &[][..],
    };
    if p.word_at(p.i).is_some_and(|w| second.contains(&w.as_str())) {
        p.i += 1;
    }
    if p.is_punct(b'(') {
        p.i = p.matching(p.i)? + 1;
    }
    loop {
        if p.eat("UNSIGNED") || p.eat("SIGNED") || p.eat("ZEROFILL") || p.eat("BINARY") || p.eat("ASCII") || p.eat("UNICODE") {
            continue;
        }
        if p.eat("CHARSET") || p.eat("COLLATE") || p.eat_seq(&["CHARACTER", "SET"]) {
            p.i += 1;
            continue;
        }
        break;
    }
    Some(p.span(from, p.i))
}

fn parse_mysql(p: &mut Parser<'_>, spec: &mut RoutineSpec) -> Option<()> {
    spec.params = p.params()?;
    if spec.kind == RoutineKind::Function {
        if !p.eat("RETURNS") {
            return None;
        }
        spec.returns.data_type = mysql_type(p)?;
    }
    loop {
        if p.eat("COMMENT") {
            spec.options.comment = p.string()?;
        } else if p.eat_seq(&["LANGUAGE", "SQL"]) || p.eat_seq(&["NOT", "DETERMINISTIC"]) {
        } else if p.eat("DETERMINISTIC") {
            spec.options.deterministic = true;
        } else if p.is_seq(&["CONTAINS", "SQL"]) || p.is_seq(&["NO", "SQL"]) {
            spec.options.data_access = format!("{} SQL", p.word_at(p.i)?);
            p.i += 2;
        } else if p.is_seq(&["READS", "SQL", "DATA"]) || p.is_seq(&["MODIFIES", "SQL", "DATA"]) {
            spec.options.data_access = format!("{} SQL DATA", p.word_at(p.i)?);
            p.i += 3;
        } else if p.eat_seq(&["SQL", "SECURITY"]) {
            spec.options.security = p.word_at(p.i)?;
            p.i += 1;
        } else {
            break;
        }
    }
    spec.body = p.rest();
    (!spec.body.is_empty()).then_some(())
}

fn parse_mssql(p: &mut Parser<'_>, spec: &mut RoutineSpec) -> Option<()> {
    if p.is_punct(b'(') {
        spec.params = p.params()?;
    } else if spec.kind == RoutineKind::Procedure {
        // Procedure parameters may go without parentheses, up to WITH / AS / FOR.
        let mut start = p.i;
        let mut depth = 0;
        loop {
            let t = p.tok(p.i)?;
            let at_end = depth == 0 && matches!(p.word_at(p.i).as_deref(), Some("WITH" | "AS" | "FOR"));
            match t.kind {
                Tok::Punct(b'(') => depth += 1,
                Tok::Punct(b')') => depth -= 1,
                Tok::Punct(b',') if depth == 0 => {
                    spec.params.push(p.param(start, p.i)?);
                    start = p.i + 1;
                }
                _ => {}
            }
            if at_end {
                if start < p.i {
                    spec.params.push(p.param(start, p.i)?);
                }
                break;
            }
            p.i += 1;
        }
    }
    if spec.kind == RoutineKind::Function {
        if !p.eat("RETURNS") {
            return None;
        }
        let var = p.tok(p.i).filter(|t| t.kind == Tok::Word && p.text[t.start..t.end].starts_with('@'));
        if let Some(var) = var {
            p.i += 1;
            if !p.eat("TABLE") {
                return None;
            }
            spec.returns.kind = ReturnKind::Table;
            spec.returns.table_variable = p.text[var.start..var.end].to_string();
            spec.returns.columns = p.columns()?;
        } else if p.eat("TABLE") {
            spec.returns.kind = ReturnKind::Table;
        } else {
            spec.returns.data_type = p.until(&["WITH", "AS", "BEGIN", "RETURN"]);
        }
    }
    if p.eat("WITH") {
        loop {
            if p.eat("SCHEMABINDING") {
                spec.options.schemabinding = true;
            } else if p.eat_seq(&["RETURNS", "NULL", "ON", "NULL", "INPUT"]) {
                spec.options.strict = true;
            } else if p.eat_seq(&["CALLED", "ON", "NULL", "INPUT"]) {
            } else if p.eat("RECOMPILE") {
                spec.options.recompile = true;
            } else if p.eat_seq(&["EXECUTE", "AS"]) {
                let t = p.tok(p.i)?;
                spec.options.security = p.text[t.start..t.end].to_string();
                p.i += 1;
            } else {
                return None;
            }
            if !p.eat_punct(b',') {
                break;
            }
        }
    }
    if p.is("FOR") {
        return None;
    }
    if !p.eat("AS") && spec.kind == RoutineKind::Procedure {
        return None;
    }
    spec.body = p.rest();
    (!spec.body.is_empty()).then_some(())
}

const ORACLE_CLAUSES: &[&str] = &[
    "AUTHID", "DETERMINISTIC", "PARALLEL_ENABLE", "PIPELINED", "RESULT_CACHE", "ACCESSIBLE", "SHARING", "DEFAULT", "IS", "AS",
    "AGGREGATE", "USING",
];

fn parse_oracle(p: &mut Parser<'_>, spec: &mut RoutineSpec) -> Option<()> {
    if p.is_punct(b'(') {
        spec.params = p.params()?;
    }
    if spec.kind == RoutineKind::Function {
        if !p.eat("RETURN") {
            return None;
        }
        spec.returns.data_type = p.until(ORACLE_CLAUSES);
    }
    let mut extra = Vec::new();
    loop {
        if p.eat("AUTHID") {
            spec.options.security = p.until(ORACLE_CLAUSES);
        } else if p.eat("DETERMINISTIC") {
            spec.options.deterministic = true;
        } else if p.is("PARALLEL_ENABLE") && !p.tok(p.i + 1).is_some_and(|t| t.kind == Tok::Punct(b'(')) {
            p.i += 1;
            spec.options.parallel_enable = true;
        } else if p.eat("PIPELINED") {
            spec.options.pipelined = true;
        } else if p.is("AGGREGATE") || p.is("USING") {
            return None;
        } else if p.eat("IS") || p.eat("AS") {
            break;
        } else if p.done() {
            return None;
        } else {
            let from = p.i;
            p.i += 1;
            p.until(ORACLE_CLAUSES);
            extra.push(p.span(from, p.i));
        }
    }
    spec.options.extra = extra.join("\n");
    spec.body = p.rest();
    (!spec.body.is_empty()).then_some(())
}

const SNOWFLAKE_CLAUSES: &[&str] = &[
    "RETURNS", "LANGUAGE", "RUNTIME_VERSION", "PACKAGES", "HANDLER", "IMPORTS", "CALLED", "STRICT", "VOLATILE", "IMMUTABLE",
    "COMMENT", "EXECUTE", "AS", "EXTERNAL_ACCESS_INTEGRATIONS", "SECRETS", "TARGET_PATH", "MEMOIZABLE", "COPY", "ARTIFACT_REPOSITORY",
];

fn parse_snowflake(p: &mut Parser<'_>, spec: &mut RoutineSpec) -> Option<()> {
    spec.params = p.params()?;
    let mut extra = Vec::new();
    loop {
        if p.done() {
            return None;
        }
        if p.eat_seq(&["RETURNS", "NULL", "ON", "NULL", "INPUT"]) || p.eat("STRICT") {
            spec.options.strict = true;
        } else if p.eat_seq(&["CALLED", "ON", "NULL", "INPUT"]) {
        } else if p.eat("RETURNS") {
            if p.is("TABLE") && p.tok(p.i + 1).is_some_and(|t| t.kind == Tok::Punct(b'(')) {
                p.i += 1;
                spec.returns.kind = ReturnKind::Table;
                spec.returns.columns = p.columns()?;
            } else {
                spec.returns.data_type = p.until(SNOWFLAKE_CLAUSES);
            }
        } else if p.eat("LANGUAGE") {
            spec.options.language = p.ident()?.to_uppercase();
        } else if p.eat("RUNTIME_VERSION") {
            p.eat_punct(b'=');
            spec.options.runtime_version = p.string()?;
        } else if p.eat("HANDLER") {
            p.eat_punct(b'=');
            spec.options.handler = p.string()?;
        } else if p.eat("PACKAGES") {
            p.eat_punct(b'=');
            let open = p.i;
            let close = p.matching(open)?;
            spec.options.packages = p.span(open + 1, close);
            p.i = close + 1;
        } else if p.is("VOLATILE") || p.is("IMMUTABLE") {
            spec.options.volatility = p.word_at(p.i)?;
            p.i += 1;
        } else if p.eat("COMMENT") {
            p.eat_punct(b'=');
            spec.options.comment = p.string()?;
        } else if p.eat_seq(&["EXECUTE", "AS"]) {
            spec.options.security = if p.eat_seq(&["RESTRICTED", "CALLER"]) { "RESTRICTED CALLER".into() } else { p.ident()?.to_uppercase() };
        } else if p.eat("AS") {
            let t = p.tok(p.i).filter(|t| matches!(t.kind, Tok::Str | Tok::Dollar))?;
            spec.body = body_text(p.text, t, true);
            p.i += 1;
        } else if p.is_punct(b';') {
            p.i += 1;
        } else {
            let from = p.i;
            p.i += 1;
            p.until(SNOWFLAKE_CLAUSES);
            extra.push(p.span(from, p.i));
        }
        if p.done() {
            break;
        }
    }
    spec.options.extra = extra.join("\n");
    Some(())
}

/// The fields of a `CREATE PROCEDURE|FUNCTION` statement, or `None` if it isn't understood.
pub fn parse_definition(driver: Driver, text: &str) -> Option<RoutineSpec> {
    let mut p = Parser::new(text, driver);
    let mut spec = RoutineSpec { options: RoutineOptions::default(), ..Default::default() };
    p.header(&mut spec)?;
    match driver {
        Driver::Postgres => parse_postgres(&mut p, &mut spec)?,
        Driver::MySql | Driver::MariaDb => parse_mysql(&mut p, &mut spec)?,
        Driver::MsSql => parse_mssql(&mut p, &mut spec)?,
        Driver::Oracle => parse_oracle(&mut p, &mut spec)?,
        Driver::Snowflake => parse_snowflake(&mut p, &mut spec)?,
        _ => return None,
    }
    Some(spec)
}
