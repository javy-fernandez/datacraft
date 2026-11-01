//! Statements that create, replace, rename and drop routines.

use super::{Param, ParamMode, ReturnKind, Returns, RoutineKind, RoutineSpec};
use crate::db::Driver;
use crate::edit::literal;
use crate::ui::drop_dialog::effective_name;
use crate::ui::quote_ident_if_needed;

/// Statements to run and whether they must run in one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutineStatements {
    pub statements: Vec<String>,
    pub transaction: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Create,
    /// `CREATE OR REPLACE` (SQL Server `CREATE OR ALTER`).
    Replace,
    /// SQL Server `ALTER`.
    Alter,
}

/// `schema.name` of a routine whose name is already final (as the server keeps it).
/// MySQL / MariaDB: the schema is the database.
pub(crate) fn qualified(driver: Driver, schema: &str, name: &str) -> String {
    let name = quote_ident_if_needed(driver, name.trim());
    let schema = schema.trim();
    if schema.is_empty() { name } else { format!("{}.{name}", quote_ident_if_needed(driver, schema)) }
}

/// The name the routine will have on the server. Oracle and Snowflake fold a simple name
/// typed in lowercase to uppercase, except the unchanged name of a routine being edited.
pub fn final_name(driver: Driver, spec: &RoutineSpec) -> String {
    let name = spec.name.trim();
    match &spec.original {
        Some(o) if o.name == name => name.to_string(),
        _ => effective_name(driver, name),
    }
}

/// A parameter or column name. Oracle and Snowflake fold simple names, so they are not quoted.
fn ident(driver: Driver, name: &str) -> String {
    let name = name.trim();
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    match driver {
        Driver::Oracle | Driver::Snowflake if simple && !crate::sql::KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(name)) => name.to_string(),
        _ => quote_ident_if_needed(driver, name),
    }
}

/// SQL Server parameter names start with `@`.
fn mssql_param(name: &str) -> String {
    let name = name.trim();
    if name.starts_with('@') { name.to_string() } else { format!("@{name}") }
}

/// Types of the input parameters, which identify a PostgreSQL or Snowflake overload.
pub fn input_signature(driver: Driver, params: &[Param]) -> String {
    params
        .iter()
        .filter(|p| match driver {
            Driver::Postgres => p.mode != ParamMode::Out,
            _ => true,
        })
        .map(|p| match p.mode {
            ParamMode::Variadic => format!("variadic {}", p.data_type.trim()),
            _ => p.data_type.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn param_sql(driver: Driver, kind: RoutineKind, p: &Param) -> String {
    let ty = p.data_type.trim();
    let default = p.default.trim();
    match driver {
        Driver::Postgres => {
            let mode = match p.mode {
                ParamMode::In => "",
                ParamMode::Out => "out ",
                ParamMode::InOut => "inout ",
                ParamMode::Variadic => "variadic ",
            };
            let name = if p.name.trim().is_empty() { String::new() } else { format!("{} ", ident(driver, &p.name)) };
            let default = if default.is_empty() { String::new() } else { format!(" default {default}") };
            format!("{mode}{name}{ty}{default}")
        }
        Driver::MySql | Driver::MariaDb => {
            let mode = match (kind, p.mode) {
                (RoutineKind::Function, _) | (_, ParamMode::In | ParamMode::Variadic) => "",
                (_, ParamMode::Out) => "out ",
                (_, ParamMode::InOut) => "inout ",
            };
            format!("{mode}{} {ty}", ident(driver, &p.name))
        }
        Driver::MsSql => {
            let default = if default.is_empty() { String::new() } else { format!(" = {default}") };
            let output = if p.mode.returns_output() { " output" } else { "" };
            format!("{} {ty}{default}{output}", mssql_param(&p.name))
        }
        Driver::Oracle => {
            let mode = match p.mode {
                ParamMode::In | ParamMode::Variadic => "",
                ParamMode::Out => "out ",
                ParamMode::InOut => "in out ",
            };
            let default = if default.is_empty() { String::new() } else { format!(" default {default}") };
            format!("{} {mode}{ty}{default}", ident(driver, &p.name))
        }
        _ => {
            let default = if default.is_empty() { String::new() } else { format!(" default {default}") };
            format!("{} {ty}{default}", ident(driver, &p.name))
        }
    }
}

/// `(\n    a int,\n    b int\n)`, or `()` without parameters.
fn param_block(driver: Driver, spec: &RoutineSpec) -> String {
    if spec.params.is_empty() {
        return "()".into();
    }
    let lines: Vec<String> = spec.params.iter().map(|p| format!("    {}", param_sql(driver, spec.kind, p))).collect();
    format!("(\n{}\n)", lines.join(",\n"))
}

fn table_columns(driver: Driver, r: &Returns) -> String {
    let cols: Vec<String> = r.columns.iter().map(|c| format!("    {} {}", ident(driver, &c.name), c.data_type.trim())).collect();
    format!("(\n{}\n)", cols.join(",\n"))
}

/// PostgreSQL `$$` (or `$body$`, `$body1$`… if the body contains it).
fn dollar_quote(body: &str) -> String {
    let mut tag = "$$".to_string();
    let mut n = 0;
    while body.contains(&tag) {
        tag = if n == 0 { "$body$".into() } else { format!("$body{n}$") };
        n += 1;
    }
    format!("{tag}\n{}\n{tag}", body.trim_matches('\n'))
}

/// Snowflake body: `$$…$$`, or a quoted string if it contains `$$`.
fn snowflake_body(body: &str) -> String {
    let body = body.trim_matches('\n');
    if body.contains("$$") {
        format!("'\n{}\n'", body.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("$$\n{body}\n$$")
    }
}

/// Snowflake `PACKAGES = (…)`: items typed without quotes get them.
fn snowflake_packages(packages: &str) -> String {
    if packages.contains('\'') {
        return packages.trim().to_string();
    }
    packages.split(',').map(str::trim).filter(|p| !p.is_empty()).map(|p| format!("'{p}'")).collect::<Vec<_>>().join(", ")
}

/// Option keywords in the generated statements' case; quoted names (`'user'`) stay as they are.
fn keyword(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') { s.to_lowercase() } else { s.to_string() }
}

fn push_extra(lines: &mut Vec<String>, extra: &str) {
    lines.extend(extra.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).map(str::to_string));
}

fn returns_line(driver: Driver, spec: &RoutineSpec) -> Option<String> {
    let r = &spec.returns;
    let keyword = if driver == Driver::Oracle { "return" } else { "returns" };
    match r.kind {
        ReturnKind::Table if driver == Driver::MsSql && r.columns.is_empty() => Some("returns table".into()),
        ReturnKind::Table if driver == Driver::MsSql => {
            let var = if r.table_variable.trim().is_empty() { "@result".to_string() } else { mssql_param(&r.table_variable) };
            Some(format!("returns {var} table {}", table_columns(driver, r)))
        }
        ReturnKind::Table => Some(format!("{keyword} table {}", table_columns(driver, r))),
        ReturnKind::SetOf => Some(format!("returns setof {}", r.data_type.trim())),
        ReturnKind::Scalar => {
            let ty = r.data_type.trim();
            if !ty.is_empty() {
                Some(format!("{keyword} {ty}"))
            } else if driver == Driver::Postgres && !spec.params.iter().any(|p| p.mode.returns_output()) {
                Some("returns void".into())
            } else {
                None
            }
        }
    }
}

fn create(driver: Driver, spec: &RoutineSpec, verb: Verb) -> String {
    let o = &spec.options;
    let function = spec.kind == RoutineKind::Function;
    let name = qualified(driver, &spec.schema, &final_name(driver, spec));
    let kind = spec.kind.keyword();
    let verb_sql = match (verb, driver) {
        (Verb::Create, _) => "create",
        (Verb::Replace, Driver::MsSql) => "create or alter",
        (Verb::Replace, _) => "create or replace",
        (Verb::Alter, _) => "alter",
    };
    let mut lines: Vec<String> = Vec::new();
    match driver {
        Driver::Postgres => {
            lines.push(format!("{verb_sql} {kind} {name}{}", param_block(driver, spec)));
            if function && let Some(r) = returns_line(driver, spec) {
                lines.push(r);
            }
            if !o.language.trim().is_empty() {
                lines.push(format!("language {}", o.language.trim()));
            }
            let mut attrs = Vec::new();
            if function {
                if !o.volatility.is_empty() {
                    attrs.push(keyword(&o.volatility));
                }
                if o.strict {
                    attrs.push("strict".into());
                }
            }
            if !o.security.is_empty() {
                attrs.push(format!("security {}", keyword(&o.security)));
            }
            if function && !o.parallel.is_empty() {
                attrs.push(format!("parallel {}", keyword(&o.parallel)));
            }
            if !attrs.is_empty() {
                lines.push(attrs.join(" "));
            }
            push_extra(&mut lines, &o.extra);
            lines.push(format!("as {}", dollar_quote(&spec.body)));
        }
        Driver::MySql | Driver::MariaDb => {
            let definer = if o.definer.trim().is_empty() { String::new() } else { format!(" definer = {}", o.definer.trim()) };
            lines.push(format!("{verb_sql}{definer} {kind} {name}{}", param_block(driver, spec)));
            if function && let Some(r) = returns_line(driver, spec) {
                lines.push(r);
            }
            if !o.comment.is_empty() {
                lines.push(format!("comment {}", literal(driver, &Some(o.comment.clone()))));
            }
            if o.deterministic {
                lines.push("deterministic".into());
            }
            if !o.data_access.is_empty() {
                lines.push(keyword(&o.data_access));
            }
            if !o.security.is_empty() {
                lines.push(format!("sql security {}", keyword(&o.security)));
            }
            push_extra(&mut lines, &o.extra);
            lines.push(spec.body.trim().trim_end_matches(';').trim_end().to_string());
        }
        Driver::MsSql => {
            let r = &spec.returns;
            let inline = function && r.kind == ReturnKind::Table && r.columns.is_empty();
            let scalar = function && r.kind == ReturnKind::Scalar;
            if function {
                lines.push(format!("{verb_sql} {kind} {name}{}", param_block(driver, spec)));
                if let Some(r) = returns_line(driver, spec) {
                    lines.push(r);
                }
            } else {
                lines.push(format!("{verb_sql} {kind} {name}"));
                let params: Vec<String> = spec.params.iter().map(|p| format!("    {}", param_sql(driver, spec.kind, p))).collect();
                if !params.is_empty() {
                    lines.push(params.join(",\n"));
                }
            }
            let mut with = Vec::new();
            if function && o.schemabinding {
                with.push("schemabinding".to_string());
            }
            if scalar && o.strict {
                with.push("returns null on null input".into());
            }
            if !function && o.recompile {
                with.push("recompile".into());
            }
            if !inline && !o.security.is_empty() {
                with.push(format!("execute as {}", keyword(&o.security)));
            }
            if !with.is_empty() {
                lines.push(format!("with {}", with.join(", ")));
            }
            push_extra(&mut lines, &o.extra);
            lines.push("as".into());
            lines.push(spec.body.trim().to_string());
        }
        Driver::Oracle => {
            let params = if spec.params.is_empty() { String::new() } else { param_block(driver, spec) };
            lines.push(format!("{verb_sql} {kind} {name}{params}"));
            if function && let Some(r) = returns_line(driver, spec) {
                lines.push(r);
            }
            if !o.security.is_empty() {
                lines.push(format!("authid {}", keyword(&o.security)));
            }
            if function {
                for (on, word) in [(o.deterministic, "deterministic"), (o.parallel_enable, "parallel_enable"), (o.pipelined, "pipelined")] {
                    if on {
                        lines.push(word.into());
                    }
                }
            }
            push_extra(&mut lines, &o.extra);
            lines.push("as".into());
            let body = spec.body.trim();
            lines.push(if body.ends_with(';') { body.to_string() } else { format!("{body};") });
        }
        Driver::Snowflake => {
            let secure = if o.secure { "secure " } else { "" };
            lines.push(format!("{verb_sql} {secure}{kind} {name}{}", param_block(driver, spec)));
            if let Some(r) = returns_line(driver, spec) {
                lines.push(r);
            }
            if !o.language.trim().is_empty() {
                lines.push(format!("language {}", o.language.trim().to_lowercase()));
            }
            if !o.runtime_version.trim().is_empty() {
                lines.push(format!("runtime_version = {}", literal(driver, &Some(o.runtime_version.trim().to_string()))));
            }
            if !o.packages.trim().is_empty() {
                lines.push(format!("packages = ({})", snowflake_packages(&o.packages)));
            }
            if !o.handler.trim().is_empty() {
                lines.push(format!("handler = {}", literal(driver, &Some(o.handler.trim().to_string()))));
            }
            if o.strict {
                lines.push("returns null on null input".into());
            }
            if !o.volatility.is_empty() {
                lines.push(keyword(&o.volatility));
            }
            if !o.comment.is_empty() {
                lines.push(format!("comment = {}", literal(driver, &Some(o.comment.clone()))));
            }
            if !function && !o.security.is_empty() {
                lines.push(format!("execute as {}", keyword(&o.security)));
            }
            push_extra(&mut lines, &o.extra);
            lines.push(format!("as\n{}", snowflake_body(&spec.body)));
        }
        _ => {}
    }
    lines.join("\n")
}

/// The statement that creates the routine (with `OR REPLACE` / `OR ALTER` if `replace`).
pub fn create_sql(driver: Driver, spec: &RoutineSpec, replace: bool) -> String {
    create(driver, spec, if replace { Verb::Replace } else { Verb::Create })
}

/// Drops routine `schema.name` as the server names it (PostgreSQL and Snowflake also need the
/// argument types).
pub fn drop_sql(driver: Driver, kind: RoutineKind, schema: &str, name: &str, signature: &str) -> String {
    let object = qualified(driver, schema, name);
    match driver {
        Driver::Postgres | Driver::Snowflake => format!("drop {} {object}({signature})", kind.keyword()),
        _ => format!("drop {} {object}", kind.keyword()),
    }
}

/// Oracle reports compilation errors without failing `CREATE`: this block turns them into one.
fn oracle_compile_check(spec: &RoutineSpec, cleanup: bool) -> String {
    let owner = spec.schema.replace('\'', "''");
    let name = final_name(Driver::Oracle, spec).replace('\'', "''");
    let kind = spec.kind.keyword().to_uppercase();
    let cleanup = if cleanup {
        let drop = drop_sql(Driver::Oracle, spec.kind, &spec.schema, &final_name(Driver::Oracle, spec), "");
        format!("    execute immediate {};\n", literal(Driver::Oracle, &Some(drop)))
    } else {
        String::new()
    };
    format!(
        "declare\n    msg varchar2(4000);\nbegin\n    for e in (select line, position, text from all_errors\n              \
         where owner = '{owner}' and name = '{name}' and type = '{kind}' and attribute = 'ERROR'\n              \
         order by sequence) loop\n        msg := substr(msg || 'Line ' || e.line || ':' || e.position || ' ' || e.text || chr(10), 1, 4000);\n    \
         end loop;\n{cleanup}    if msg is not null then\n        raise_application_error(-20000, rtrim(msg, chr(10)));\n    end if;\nend;"
    )
}

/// PostgreSQL `COMMENT ON` for the routine as `spec` creates it.
fn pg_comment(spec: &RoutineSpec) -> String {
    let comment = if spec.options.comment.is_empty() { "null".to_string() } else { literal(Driver::Postgres, &Some(spec.options.comment.clone())) };
    format!(
        "comment on {} {}({}) is {comment}",
        spec.kind.keyword(),
        qualified(Driver::Postgres, &spec.schema, &final_name(Driver::Postgres, spec)),
        input_signature(Driver::Postgres, &spec.params)
    )
}

/// What the dialog runs: a new routine, or the changes to the one being edited.
pub fn routine_statements(driver: Driver, spec: &RoutineSpec) -> RoutineStatements {
    let mut statements = Vec::new();
    let mut transaction = false;
    let Some(original) = spec.original.as_deref() else {
        match driver {
            // MySQL has no CREATE OR REPLACE for routines.
            Driver::MySql if spec.or_replace => {
                let mut check = spec.clone();
                check.name = scratch_name(&spec.name);
                statements.push(create(driver, &check, Verb::Create));
                statements.push(drop_sql(driver, spec.kind, &spec.schema, &check.name, ""));
                statements.push(format!("drop {} if exists {}", spec.kind.keyword(), qualified(driver, &spec.schema, &spec.name)));
                statements.push(create(driver, spec, Verb::Create));
            }
            Driver::Oracle if spec.or_replace => {
                oracle_scratch_check(spec, &mut statements);
                statements.push(create(driver, spec, Verb::Replace));
            }
            _ => statements.push(create(driver, spec, if spec.or_replace { Verb::Replace } else { Verb::Create })),
        }
        match driver {
            Driver::Oracle => statements.push(oracle_compile_check(spec, false)),
            Driver::Postgres if !spec.options.comment.is_empty() => {
                statements.push(pg_comment(spec));
                transaction = true;
            }
            _ => {}
        }
        return RoutineStatements { statements, transaction };
    };
    let same_name = final_name(driver, spec) == original.name;
    let same_shape = spec.params == original.params && spec.returns == original.returns;
    let types = |params: &[Param]| params.iter().filter(|p| p.mode.takes_input()).map(|p| p.data_type.trim().to_lowercase()).collect::<Vec<_>>();
    let same_inputs = types(&spec.params) == types(&original.params);
    let drop_old = drop_sql(driver, spec.kind, &spec.schema, &original.name, &original.signature);
    match driver {
        Driver::Postgres => {
            transaction = true;
            if !same_shape {
                // Parameter names, types, defaults or the return type can't change in place.
                statements.push(drop_old);
                statements.push(create(driver, spec, Verb::Create));
            } else {
                if !same_name {
                    statements.push(format!(
                        "alter {} {}({}) rename to {}",
                        spec.kind.keyword(),
                        qualified(driver, &spec.schema, &original.name),
                        original.signature,
                        quote_ident_if_needed(driver, spec.name.trim())
                    ));
                }
                statements.push(create(driver, spec, Verb::Replace));
            }
            // Dropping the routine also drops its comment.
            let comment = if same_shape { spec.options.comment != original.comment } else { !spec.options.comment.is_empty() };
            if comment {
                statements.push(pg_comment(spec));
            }
        }
        Driver::MySql if same_name => {
            // No CREATE OR REPLACE: a copy under a scratch name proves the new text compiles
            // before the routine is dropped.
            let mut check = spec.clone();
            check.name = scratch_name(&spec.name);
            check.original = None;
            statements.push(create(driver, &check, Verb::Create));
            statements.push(drop_sql(driver, spec.kind, &spec.schema, &check.name, ""));
            statements.push(drop_old);
            statements.push(create(driver, spec, Verb::Create));
        }
        Driver::MariaDb if same_name => statements.push(create(driver, spec, Verb::Replace)),
        Driver::MsSql if same_name && spec.returns.kind == original.returns.kind && (spec.returns.columns.is_empty() == original.returns.columns.is_empty()) => {
            statements.push(create(driver, spec, Verb::Alter));
        }
        Driver::MsSql if same_name => {
            // A scalar function can't be altered into a table-valued one (or back).
            transaction = true;
            statements.push(drop_old);
            statements.push(create(driver, spec, Verb::Create));
        }
        Driver::Oracle => {
            oracle_scratch_check(spec, &mut statements);
            statements.push(create(driver, spec, if same_name { Verb::Replace } else { Verb::Create }));
            statements.push(oracle_compile_check(spec, false));
            if !same_name {
                statements.push(drop_old);
            }
        }
        // Same name and argument types: the same overload, replaced in place.
        Driver::Snowflake if same_name && same_inputs => statements.push(create(driver, spec, Verb::Replace)),
        // Renamed (or, on Snowflake, new argument types): create the new one, then drop the old.
        _ => {
            statements.push(create(driver, spec, if driver == Driver::Snowflake { Verb::Replace } else { Verb::Create }));
            statements.push(drop_old);
        }
    }
    RoutineStatements { statements, transaction }
}

fn oracle_scratch_check(spec: &RoutineSpec, statements: &mut Vec<String>) {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    spec.schema.hash(&mut hash);
    spec.name.hash(&mut hash);
    let mut check = spec.clone();
    check.name = format!("DC_CHECK_{:016X}", hash.finish());
    check.original = None;
    statements.push(create(Driver::Oracle, &check, Verb::Create));
    statements.push(oracle_compile_check(&check, true));
}

/// MySQL scratch name for the compile check (64 characters at most).
fn scratch_name(name: &str) -> String {
    let base: String = name.trim().chars().take(52).collect();
    format!("{base}_dc_check")
}
