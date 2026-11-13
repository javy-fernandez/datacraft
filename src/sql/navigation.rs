//! Source positions and conservative SQL navigation. All public positions are character offsets.
use super::{Dialect, Driver, Kind, Lexer, TokenClass, tokens};
use std::ops::Range;

pub fn byte(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(b, _)| b)
}

pub fn character(text: &str, byte: usize) -> usize {
    text.char_indices().take_while(|(b, _)| *b < byte).count()
}

pub fn line_column(text: &str, pos: usize) -> (usize, usize) {
    text.chars().take(pos).fold((1, 1), |(line, col), c| {
        if c == '\n' {
            (line + 1, 1)
        } else {
            (line, col + 1)
        }
    })
}

pub fn position(text: &str, input: &str) -> Result<usize, &'static str> {
    let mut parts = input.trim().split(':');
    let line = parts
        .next()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .ok_or("Enter a positive line number")?;
    let col = parts
        .next()
        .map(|s| s.trim().parse::<usize>())
        .transpose()
        .map_err(|_| "Enter a positive column number")?
        .unwrap_or(1);
    if col == 0 || parts.next().is_some() {
        return Err("Use line:column, starting at 1");
    }
    let mut start = 0;
    for (i, row) in text.split('\n').enumerate() {
        if i + 1 == line {
            return Ok(start + (col - 1).min(row.trim_end_matches('\r').chars().count()));
        }
        start += row.chars().count() + 1;
    }
    Err("Line is outside this editor")
}

pub fn paragraph(text: &str, pos: usize, forward: bool) -> usize {
    let mut boundaries = vec![0];
    let mut start = 0;
    let mut blank = true;
    for line in text.split_inclusive('\n') {
        let empty = line.trim().is_empty();
        if blank && !empty {
            boundaries.push(start);
        }
        blank = empty;
        start += line.chars().count();
    }
    boundaries.push(text.chars().count());
    if forward {
        boundaries.into_iter().find(|&p| p > pos).unwrap_or(start)
    } else {
        boundaries.into_iter().rev().find(|&p| p < pos).unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identifier {
    pub name: String,
    pub quoted: bool,
}
impl Identifier {
    pub fn matches(&self, name: &str) -> bool {
        if self.quoted {
            self.name == name
        } else {
            self.name.eq_ignore_ascii_case(name)
        }
    }
}

fn identifier(s: &str) -> Identifier {
    let first = s.chars().next().unwrap_or(' ');
    let quoted = matches!(first, '"' | '`' | '[');
    let close = if first == '[' { ']' } else { first };
    let name = if quoted && s.ends_with(close) && s.len() >= 2 {
        s[1..s.len() - 1].replace(&format!("{close}{close}"), &close.to_string())
    } else {
        s.to_string()
    };
    Identifier { name, quoted }
}

#[derive(Clone, Debug)]
pub struct Reference {
    pub parts: Vec<Identifier>,
    pub range: Range<usize>,
    pub component: usize,
}

pub fn reference_at(text: &str, pos: usize, driver: Driver) -> Option<Reference> {
    let ts: Vec<_> = tokens(text, driver)
        .into_iter()
        .filter(|(k, _)| *k != TokenClass::Blank)
        .collect();
    let b = byte(text, pos);
    let ident = |i: usize| matches!(ts[i].0, TokenClass::Word | TokenClass::QuotedIdent);
    let at = ts
        .iter()
        .position(|(_, r)| r.start <= b && b < r.end)
        .or_else(|| ts.iter().rposition(|(_, r)| r.end == b))?;
    if !ident(at) {
        return None;
    }
    let dot = |i: usize| &text[ts[i].1.clone()] == ".";
    let mut start = at;
    let mut end = at;
    while start >= 2 && dot(start - 1) && ident(start - 2) {
        start -= 2;
    }
    while end + 2 < ts.len() && dot(end + 1) && ident(end + 2) {
        end += 2;
    }
    Some(Reference {
        parts: (start..=end)
            .step_by(2)
            .map(|i| identifier(&text[ts[i].1.clone()]))
            .collect(),
        range: character(text, ts[start].1.start)..character(text, ts[end].1.end),
        component: (at - start) / 2,
    })
}

#[derive(Clone, Debug)]
pub struct TableUse {
    pub parts: Vec<Identifier>,
    pub alias: Option<Identifier>,
    pub declaration: usize,
}

/// Table bindings in one statement, ignoring literals/comments and nested SELECT bodies.
pub fn table_uses(text: &str, driver: Driver) -> Vec<TableUse> {
    let ts: Vec<_> = tokens(text, driver)
        .into_iter()
        .filter(|(k, _)| *k != TokenClass::Blank)
        .collect();
    let mut out = Vec::new();
    let word = |i: usize, s: &str| {
        ts.get(i)
            .is_some_and(|(k, r)| *k == TokenClass::Word && text[r.clone()].eq_ignore_ascii_case(s))
    };
    let ident = |i: usize| {
        ts.get(i)
            .is_some_and(|(k, _)| matches!(k, TokenClass::Word | TokenClass::QuotedIdent))
    };
    let mut depth = 0usize;
    let mut from = false;
    for i in 0..ts.len() {
        let raw = &text[ts[i].1.clone()];
        if raw == "(" {
            depth += 1;
        }
        if raw == ")" {
            depth = depth.saturating_sub(1);
        }
        if depth != 0 {
            continue;
        }
        if [
            "where",
            "group",
            "order",
            "having",
            "limit",
            "union",
            "returning",
            "set",
            "values",
        ]
        .iter()
        .any(|s| word(i, s))
        {
            from = false;
        }
        let source = ["from", "join", "update", "into", "references"]
            .iter()
            .any(|s| word(i, s));
        if word(i, "from") {
            from = true;
        }
        if !(source || (from && raw == ",")) || !ident(i + 1) {
            continue;
        }
        let mut j = i + 1;
        let declaration = character(text, ts[j].1.start);
        let mut parts = vec![identifier(&text[ts[j].1.clone()])];
        while ts.get(j + 1).is_some_and(|(_, r)| &text[r.clone()] == ".") && ident(j + 2) {
            j += 2;
            parts.push(identifier(&text[ts[j].1.clone()]));
        }
        j += 1;
        if word(j, "as") {
            j += 1;
        }
        let alias = if ident(j)
            && (ts[j].0 == TokenClass::QuotedIdent || !super::is_keyword(&text[ts[j].1.clone()]))
        {
            Some(identifier(&text[ts[j].1.clone()]))
        } else {
            None
        };
        out.push(TableUse {
            parts,
            alias,
            declaration,
        });
    }
    out
}

pub fn matching_brace(text: &str, pos: usize, driver: Driver) -> Option<usize> {
    let braces: Vec<_> = tokens(text, driver)
        .into_iter()
        .filter(|(k, r)| {
            *k == TokenClass::Other && matches!(&text[r.clone()], "(" | ")" | "[" | "]" | "{" | "}")
        })
        .collect();
    let b = byte(text, pos);
    let at = braces
        .iter()
        .position(|(_, r)| r.start == b)
        .or_else(|| braces.iter().position(|(_, r)| r.end == b))?;
    let s = &text[braces[at].1.clone()];
    let opening = matches!(s, "(" | "[" | "{");
    let close = match s {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        ")" => "(",
        "]" => "[",
        _ => "{",
    };
    let indices: Vec<usize> = if opening {
        (at + 1..braces.len()).collect()
    } else {
        (0..at).rev().collect()
    };
    let mut depth = 0;
    for i in indices {
        let t = &text[braces[i].1.clone()];
        if t == s {
            depth += 1;
        }
        if t == close {
            if depth == 0 {
                return Some(character(text, braces[i].1.start));
            }
            depth -= 1;
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub position: usize,
    pub message: &'static str,
    pub error: bool,
}

/// Lexical diagnostics only. Database syntax is validated by the server when run.
pub fn issues(text: &str, driver: Driver) -> Vec<Issue> {
    if driver.is_nosql() || driver == Driver::Kusto {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack: Vec<(&str, usize)> = Vec::new();
    let mut previous_byte = 0;
    let mut position = 0;
    for t in Lexer::new(text, Dialect::for_driver(driver)) {
        let raw = &text[t.start..t.end];
        position += text[previous_byte..t.start].chars().count();
        previous_byte = t.start;
        if t.kind == Kind::BlockComment
            && Lexer::new(&format!("{raw}\nprobe"), Dialect::for_driver(driver))
                .next()
                .is_some_and(|p| p.end > raw.len())
        {
            out.push(Issue {
                position,
                message: "Unterminated block comment",
                error: true,
            });
        }
        if matches!(t.kind, Kind::Str | Kind::QuotedIdent) {
            // The lexer consumes an unfinished quote to EOF. Appending a newline and an
            // identifier distinguishes closed literals from ones that still consume input.
            let probe = format!("{raw}\n__datacraft_quote_probe");
            if Lexer::new(&probe, Dialect::for_driver(driver))
                .next()
                .is_some_and(|p| p.end > raw.len())
            {
                out.push(Issue {
                    position,
                    message: "Unterminated quoted text",
                    error: true,
                });
            }
        }
        if t.kind != Kind::Punct {
            continue;
        }
        match raw {
            "(" | "[" | "{" => stack.push((raw, position)),
            ")" | "]" | "}" => {
                let expected = match raw {
                    ")" => "(",
                    "]" => "[",
                    _ => "{",
                };
                if stack.last().is_some_and(|(s, _)| *s == expected) {
                    stack.pop();
                } else {
                    out.push(Issue {
                        position,
                        message: "Unmatched closing bracket",
                        error: true,
                    });
                }
            }
            _ => {}
        }
    }
    out.extend(stack.into_iter().map(|(_, position)| Issue {
        position,
        message: "Unclosed bracket",
        error: true,
    }));
    out.sort_by_key(|i| i.position);
    out
}

/// CTE names are local declarations, not catalog tables. Stay within this statement.
pub fn cte_declarations(text: &str, driver: Driver) -> Vec<(Identifier, usize)> {
    let ts: Vec<_> = tokens(text, driver)
        .into_iter()
        .filter(|(k, _)| *k != TokenClass::Blank)
        .collect();
    if !ts.first().is_some_and(|(k, r)| {
        *k == TokenClass::Word && text[r.clone()].eq_ignore_ascii_case("with")
    }) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut depth = 0usize;
    for i in 1..ts.len() {
        let raw = &text[ts[i].1.clone()];
        if raw == "(" {
            depth += 1;
            continue;
        }
        if raw == ")" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0 && ts[i].0 == TokenClass::Word && raw.eq_ignore_ascii_case("select") {
            break;
        }
        if depth == 0 && matches!(ts[i].0, TokenClass::Word | TokenClass::QuotedIdent) {
            let mut j = i + 1;
            if ts.get(j).is_some_and(|(_, r)| &text[r.clone()] == "(") {
                j += 1;
                while j < ts.len() && &text[ts[j].1.clone()] != ")" {
                    j += 1;
                }
                j += 1;
            }
            if ts.get(j).is_some_and(|(k, r)| {
                *k == TokenClass::Word && text[r.clone()].eq_ignore_ascii_case("as")
            }) {
                out.push((identifier(raw), character(text, ts[i].1.start)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_spaces_in_typed_sql_literals_and_comments_are_not_issues() {
        for text in [
            "SELECT ",
            "SELECT 1;  \n",
            "SELECT 'one  \ntwo ';",
            "/* one  \ntwo */ SELECT 1;",
        ] {
            assert!(issues(text, Driver::Postgres).is_empty(), "{text}");
        }
    }
    #[test]
    fn unicode_lines_columns_and_paragraphs() {
        let s = "select 'سلام';\n\nselect 2;";
        assert_eq!(position(s, "3:8"), Ok(23));
        assert_eq!(line_column(s, 23), (3, 8));
        assert_eq!(position(s, "0:1"), Err("Enter a positive line number"));
        assert!(position(s, "8").is_err());
        assert_eq!(paragraph(s, 2, true), 16);
        assert_eq!(paragraph(s, 23, false), 16);
    }
    #[test]
    fn references_ignore_comments_and_strings_and_preserve_quoted_names() {
        let s = "select u.\"نام\" from \"sales\".\"Users\" as u where x = 'fake.table' -- bad.name";
        let r = reference_at(s, 11, Driver::Postgres).unwrap();
        assert_eq!(
            r.parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["u", "نام"]
        );
        assert!(r.parts[1].quoted);
        let uses = table_uses(s, Driver::Postgres);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].alias.as_ref().unwrap().name, "u");
        assert_eq!(uses[0].parts[1].name, "Users");
        assert!(
            reference_at(
                s,
                s.find("fake").map(|b| character(s, b)).unwrap(),
                Driver::Postgres
            )
            .is_none()
        );
    }
    #[test]
    fn diagnostics_and_braces_follow_dialect_lexing() {
        let s = "select ('ignore )', $$ ( $$, (2)); -- (";
        assert!(issues(s, Driver::Postgres).is_empty());
        assert_eq!(matching_brace(s, 7, Driver::Postgres), Some(32));
        assert!(
            issues("select 'unfinished", Driver::Postgres)
                .iter()
                .any(|i| i.message == "Unterminated quoted text")
        );
        assert!(
            issues("select (1; /* unfinished", Driver::Postgres)
                .iter()
                .any(|i| i.message == "Unclosed bracket")
        );
        assert!(
            !issues("select [odd(name)] from [table]", Driver::MsSql)
                .iter()
                .any(|i| i.error)
        );
    }
}
