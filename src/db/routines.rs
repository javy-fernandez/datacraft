//! Stored procedures and functions: what the explorer lists, the definition read from the
//! server (for "Edit…" and "Generate DDL") and the statements that create, drop and call them.
//!
//! SQLite has no stored routines and Redis / MongoDB are not SQL engines, so only PostgreSQL,
//! MySQL, MariaDB, SQL Server, Oracle (standalone routines, not packages) and Snowflake have them.

mod call;
pub mod comments;
mod ddl;
mod parse;

pub use call::{ArgValue, call_sql, callable};
pub use ddl::{RoutineStatements, drop_sql, final_name, routine_statements};
pub use ddl::{create_sql, input_signature};
pub use parse::parse_definition;

use super::{Driver, Session};
use crate::edit::literal;
use crate::i18n::{tr, trf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum RoutineKind {
    #[default]
    Procedure,
    Function,
}

impl RoutineKind {
    pub fn keyword(self) -> &'static str {
        match self {
            RoutineKind::Procedure => "procedure",
            RoutineKind::Function => "function",
        }
    }
}

/// Engines with stored procedures and functions.
pub fn supported(driver: Driver) -> bool {
    matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::MsSql | Driver::Oracle | Driver::Snowflake)
}

/// A routine as the explorer lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutineInfo {
    pub schema: String,
    pub name: String,
    pub kind: RoutineKind,
    /// Input argument types that tell overloads apart (PostgreSQL, Snowflake), e.g.
    /// `integer, text`; empty on engines without overloads.
    pub signature: String,
    /// Parameters as the server lists them, for display (`a integer, OUT b text`).
    pub arguments: String,
    /// Return type (empty for procedures that return nothing).
    pub returns: String,
    pub language: String,
    /// false when the server reports it as invalid (Oracle, after a failed compilation).
    pub valid: bool,
}

impl RoutineInfo {
    /// Name with its arguments, as shown in tooltips and confirmations.
    pub fn display(&self) -> String {
        format!("{}({})", self.name, self.arguments)
    }
}

/// A routine of a connection's database.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct RoutineRef {
    pub conn: u64,
    pub db: String,
    pub schema: String,
    pub name: String,
    pub kind: RoutineKind,
    /// See [`RoutineInfo::signature`].
    pub signature: String,
}

impl RoutineRef {
    pub fn new(conn: u64, db: &str, r: &RoutineInfo) -> Self {
        Self { conn, db: db.to_string(), schema: r.schema.clone(), name: r.name.clone(), kind: r.kind, signature: r.signature.clone() }
    }
}

/// The routines of one database, loaded in the background after its schema.
#[derive(Clone, Debug, Default)]
pub struct RoutineList {
    pub items: Option<Vec<RoutineInfo>>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParamMode {
    #[default]
    In,
    Out,
    InOut,
    /// PostgreSQL: any number of trailing arguments as an array.
    Variadic,
}

impl ParamMode {
    pub fn takes_input(self) -> bool {
        !matches!(self, ParamMode::Out)
    }

    pub fn returns_output(self) -> bool {
        matches!(self, ParamMode::Out | ParamMode::InOut)
    }
}

/// Parameter modes an engine accepts for a kind of routine (the first one is the default).
pub fn param_modes(driver: Driver, kind: RoutineKind) -> &'static [ParamMode] {
    use ParamMode::*;
    match (driver, kind) {
        (Driver::Postgres, RoutineKind::Function) => &[In, Out, InOut, Variadic],
        (Driver::Postgres, RoutineKind::Procedure) => &[In, Out, InOut, Variadic],
        (Driver::MySql | Driver::MariaDb, RoutineKind::Procedure) => &[In, Out, InOut],
        // T-SQL `OUTPUT` parameters also take a value in.
        (Driver::MsSql, RoutineKind::Procedure) => &[In, Out],
        (Driver::Oracle, _) => &[In, Out, InOut],
        _ => &[In],
    }
}

/// Engines whose parameters can have default values.
pub fn has_defaults(driver: Driver) -> bool {
    !matches!(driver, Driver::MySql | Driver::MariaDb)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub mode: ParamMode,
    pub data_type: String,
    /// Default value expression ("" = none).
    pub default: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReturnKind {
    /// One value of `data_type` (PostgreSQL `void`, or nothing for procedures).
    #[default]
    Scalar,
    /// PostgreSQL `SETOF data_type`.
    SetOf,
    /// Rows with `columns`. SQL Server: an inline table-valued function when `columns` is empty.
    Table,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Returns {
    pub kind: ReturnKind,
    pub data_type: String,
    pub columns: Vec<Column>,
    /// SQL Server multi-statement table-valued functions: the table variable (`@result`).
    pub table_variable: String,
}

/// Return kinds an engine offers for a kind of routine.
pub fn return_kinds(driver: Driver, kind: RoutineKind) -> &'static [ReturnKind] {
    use ReturnKind::*;
    match (driver, kind) {
        (Driver::Postgres, RoutineKind::Function) => &[Scalar, SetOf, Table],
        (Driver::MsSql | Driver::Snowflake, RoutineKind::Function) => &[Scalar, Table],
        // Snowflake procedures always return something, possibly a table.
        (Driver::Snowflake, RoutineKind::Procedure) => &[Scalar, Table],
        (_, RoutineKind::Function) => &[Scalar],
        _ => &[],
    }
}

/// The engine-specific clauses of a routine. Each engine reads the ones it has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutineOptions {
    /// PostgreSQL (`plpgsql`, `sql`…) and Snowflake (`SQL`, `JAVASCRIPT`, `PYTHON`…).
    pub language: String,
    /// PostgreSQL `VOLATILE` / `STABLE` / `IMMUTABLE`, Snowflake `VOLATILE` / `IMMUTABLE` ("" = default).
    pub volatility: String,
    /// PostgreSQL `STRICT`, SQL Server and Snowflake `RETURNS NULL ON NULL INPUT`.
    pub strict: bool,
    /// PostgreSQL `SECURITY DEFINER|INVOKER`, MySQL `SQL SECURITY`, Oracle `AUTHID`,
    /// SQL Server and Snowflake `EXECUTE AS` ("" = default).
    pub security: String,
    /// PostgreSQL `PARALLEL SAFE|RESTRICTED|UNSAFE` ("" = default).
    pub parallel: String,
    /// MySQL / MariaDB and Oracle.
    pub deterministic: bool,
    /// MySQL / MariaDB `CONTAINS SQL`, `NO SQL`, `READS SQL DATA`, `MODIFIES SQL DATA` ("" = default).
    pub data_access: String,
    /// MySQL / MariaDB `DEFINER` (`user@host`, "" = the current user).
    pub definer: String,
    /// MySQL / MariaDB and Snowflake `COMMENT`, PostgreSQL `COMMENT ON`.
    pub comment: String,
    /// SQL Server functions.
    pub schemabinding: bool,
    /// SQL Server procedures.
    pub recompile: bool,
    /// Snowflake `SECURE`.
    pub secure: bool,
    /// Oracle functions.
    pub pipelined: bool,
    pub parallel_enable: bool,
    /// Snowflake Python / Java / Scala routines.
    pub runtime_version: String,
    pub packages: String,
    pub handler: String,
    /// Any other clauses, written as is before the body (PostgreSQL `COST 10`, `SET search_path = x`…).
    pub extra: String,
}

/// What the routine dialog generates statements from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutineSpec {
    pub kind: RoutineKind,
    pub schema: String,
    pub name: String,
    pub params: Vec<Param>,
    pub returns: Returns,
    pub options: RoutineOptions,
    /// PostgreSQL and Snowflake: the text between the quotes. MySQL: the statement (`BEGIN … END`).
    /// SQL Server and Oracle: what follows `AS`.
    pub body: String,
    /// New routines: replace an existing one with the same name.
    pub or_replace: bool,
    /// Editing: the routine as it was loaded.
    pub original: Option<Box<Original>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Original {
    /// As the server names it.
    pub name: String,
    pub signature: String,
    pub params: Vec<Param>,
    pub returns: Returns,
    pub comment: String,
}

impl RoutineSpec {
    /// Template for a new routine.
    pub fn new(driver: Driver, kind: RoutineKind, schema: String) -> Self {
        let mut spec = Self { kind, schema, ..Default::default() };
        let function = kind == RoutineKind::Function;
        let (language, returns, body) = match driver {
            Driver::Postgres if function => ("plpgsql", "integer", "begin\n    return 0;\nend;"),
            Driver::Postgres => ("plpgsql", "", "begin\n    \nend;"),
            Driver::MySql | Driver::MariaDb if function => ("", "int", "begin\n    return 0;\nend"),
            Driver::MySql | Driver::MariaDb => ("", "", "begin\n    \nend"),
            Driver::MsSql if function => ("", "int", "begin\n    return 0;\nend"),
            Driver::MsSql => ("", "", "begin\n    set nocount on;\n    \nend"),
            Driver::Oracle if function => ("", "number", "begin\n    return 0;\nend;"),
            Driver::Oracle => ("", "", "begin\n    null;\nend;"),
            Driver::Snowflake if function => ("SQL", "NUMBER", "0"),
            Driver::Snowflake => ("SQL", "VARCHAR", "begin\n    return 'done';\nend;"),
            _ => ("", "", ""),
        };
        spec.options.language = language.into();
        spec.returns.data_type = returns.into();
        spec.body = body.into();
        // Binary logging makes MySQL refuse functions that don't declare what they do.
        if function && matches!(driver, Driver::MySql | Driver::MariaDb) {
            spec.options.data_access = "READS SQL DATA".into();
        }
        spec
    }

    pub fn is_edit(&self) -> bool {
        self.original.is_some()
    }

    /// Remembers the loaded routine, so saving knows what changed.
    pub fn mark_original(&mut self, signature: &str) {
        self.original = Some(Box::new(Original {
            name: self.name.clone(),
            signature: signature.to_string(),
            params: self.params.clone(),
            returns: self.returns.clone(),
            comment: self.options.comment.clone(),
        }));
    }
}

/// The definition read from the server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutineDefinition {
    /// Native metadata comment, including when the definition could not be parsed.
    pub comment: String,
    /// The complete statement that creates it, ready for a console.
    pub ddl: String,
    /// The definition split into the dialog's fields (`None` if it couldn't be understood).
    pub spec: Option<RoutineSpec>,
}

fn text(row: &[super::Cell], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn kind_of(code: &str) -> RoutineKind {
    if code.eq_ignore_ascii_case("P") { RoutineKind::Procedure } else { RoutineKind::Function }
}

fn string_list(driver: Driver, items: &[&str]) -> String {
    items.iter().map(|s| literal(driver, &Some((*s).to_string()))).collect::<Vec<_>>().join(", ")
}

/// `(…)` without its outer parentheses.
fn strip_parens(text: &str) -> &str {
    let t = text.trim();
    t.strip_prefix('(').and_then(|t| t.strip_suffix(')')).unwrap_or(t)
}

/// Types of a Snowflake `ARGUMENT_SIGNATURE` (`(A NUMBER, B VARCHAR)` → `NUMBER, VARCHAR`).
pub(crate) fn snowflake_signature(arguments: &str) -> String {
    parse::split_top_level(strip_parens(arguments))
        .iter()
        .map(|a| a.trim().split_once(char::is_whitespace).map_or(a.trim(), |(_, t)| t.trim()).to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Query listing the routines of the session's database.
fn list_query(driver: Driver, db: &str, oracle_maintained: bool) -> String {
    match driver {
        Driver::Postgres => "SELECT n.nspname, p.proname, CASE p.prokind WHEN 'p' THEN 'P' ELSE 'F' END, \
                    pg_get_function_identity_arguments(p.oid), pg_get_function_arguments(p.oid), \
                    CASE WHEN p.prokind = 'p' THEN '' ELSE pg_get_function_result(p.oid) END, l.lanname \
             FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace \
             JOIN pg_language l ON l.oid = p.prolang \
             WHERE p.prokind IN ('f', 'p') \
               AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
               AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp\\_%' \
               AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e') \
             ORDER BY 1, 2, 4"
            .into(),
        Driver::MySql | Driver::MariaDb => format!(
            "SELECT r.ROUTINE_SCHEMA, r.ROUTINE_NAME, LEFT(r.ROUTINE_TYPE, 1), '', \
                    (SELECT GROUP_CONCAT(CONCAT_WS(' ', CASE WHEN r.ROUTINE_TYPE = 'PROCEDURE' THEN p.PARAMETER_MODE END, \
                                                   p.PARAMETER_NAME, p.DTD_IDENTIFIER) ORDER BY p.ORDINAL_POSITION SEPARATOR ', ') \
                     FROM information_schema.PARAMETERS p \
                     WHERE p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA AND p.SPECIFIC_NAME = r.SPECIFIC_NAME \
                       AND p.ROUTINE_TYPE = r.ROUTINE_TYPE AND p.ORDINAL_POSITION > 0), \
                    COALESCE(r.DTD_IDENTIFIER, ''), LOWER(r.ROUTINE_BODY) \
             FROM information_schema.ROUTINES r \
             WHERE r.ROUTINE_SCHEMA = {} AND r.ROUTINE_TYPE IN ('PROCEDURE', 'FUNCTION') \
             ORDER BY r.ROUTINE_NAME",
            literal(driver, &Some(db.to_string()))
        ),
        Driver::MsSql => "SELECT s.name, o.name, CASE WHEN o.type = 'P' THEN 'P' ELSE 'F' END, '', \
                    ISNULL(STUFF((SELECT ', ' + p.name + ' ' + TYPE_NAME(p.user_type_id) + CASE WHEN p.is_output = 1 THEN ' OUTPUT' ELSE '' END \
                                  FROM sys.parameters p WHERE p.object_id = o.object_id AND p.parameter_id > 0 \
                                  ORDER BY p.parameter_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, ''), ''), \
                    CASE o.type WHEN 'P' THEN '' WHEN 'FN' THEN ISNULL((SELECT TYPE_NAME(p.user_type_id) FROM sys.parameters p \
                                                                       WHERE p.object_id = o.object_id AND p.parameter_id = 0), '') \
                                ELSE 'TABLE' END, '' \
             FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.type IN ('P', 'FN', 'IF', 'TF') AND o.is_ms_shipped = 0 \
             ORDER BY s.name, o.name"
            .into(),
        Driver::Oracle => {
            let owners = if oracle_maintained {
                "(o.OWNER = USER OR o.OWNER IN (SELECT USERNAME FROM ALL_USERS WHERE ORACLE_MAINTAINED = 'N'))".to_string()
            } else {
                format!("o.OWNER NOT IN ({})", string_list(driver, super::oracle::SYSTEM_SCHEMAS))
            };
            format!(
                "SELECT o.OWNER, o.OBJECT_NAME, SUBSTR(o.OBJECT_TYPE, 1, 1), '', \
                        LISTAGG(CASE WHEN a.POSITION > 0 AND a.ARGUMENT_NAME IS NOT NULL \
                                     THEN a.ARGUMENT_NAME || ' ' || DECODE(a.IN_OUT, 'IN', '', a.IN_OUT || ' ') || a.DATA_TYPE END, ', ') \
                            WITHIN GROUP (ORDER BY a.POSITION), \
                        MAX(CASE WHEN a.POSITION = 0 THEN a.DATA_TYPE END), \
                        o.STATUS \
                 FROM ALL_OBJECTS o \
                 LEFT JOIN ALL_ARGUMENTS a ON a.OWNER = o.OWNER AND a.OBJECT_NAME = o.OBJECT_NAME \
                     AND a.PACKAGE_NAME IS NULL AND a.DATA_LEVEL = 0 \
                 WHERE o.OBJECT_TYPE IN ('PROCEDURE', 'FUNCTION') AND {owners} \
                 GROUP BY o.OWNER, o.OBJECT_NAME, o.OBJECT_TYPE, o.STATUS \
                 ORDER BY 1, 2"
            )
        }
        Driver::Snowflake => "SELECT PROCEDURE_SCHEMA, PROCEDURE_NAME, 'P', ARGUMENT_SIGNATURE, ARGUMENT_SIGNATURE, DATA_TYPE, PROCEDURE_LANGUAGE \
             FROM INFORMATION_SCHEMA.PROCEDURES \
             UNION ALL \
             SELECT FUNCTION_SCHEMA, FUNCTION_NAME, 'F', ARGUMENT_SIGNATURE, ARGUMENT_SIGNATURE, DATA_TYPE, FUNCTION_LANGUAGE \
             FROM INFORMATION_SCHEMA.FUNCTIONS \
             ORDER BY 1, 2"
            .into(),
        _ => String::new(),
    }
}

/// Rows of `list_query` (schema, name, P/F, signature, arguments, returns, language or status).
fn routines_from_rows(driver: Driver, rows: Vec<Vec<super::Cell>>) -> Vec<RoutineInfo> {
    rows.into_iter()
        .map(|row| {
            let mut r = RoutineInfo {
                schema: text(&row, 0),
                name: text(&row, 1),
                kind: kind_of(&text(&row, 2)),
                signature: text(&row, 3),
                arguments: text(&row, 4),
                returns: text(&row, 5),
                language: text(&row, 6),
                valid: true,
            };
            match driver {
                Driver::Oracle => {
                    r.valid = r.language != "INVALID";
                    r.language = String::new();
                }
                Driver::Snowflake => {
                    r.signature = snowflake_signature(&r.signature);
                    r.arguments = strip_parens(&r.arguments).to_string();
                }
                Driver::MySql | Driver::MariaDb => r.language = String::new(),
                _ => {}
            }
            r
        })
        .collect()
}

impl Session {
    /// Resolve PostgreSQL's canonical overload signature before committing the edit.
    pub async fn save_postgres_routine(&mut self, statements: &[String], mut identity: RoutineRef) -> anyhow::Result<RoutineRef> {
        self.execute("BEGIN", 0).await?;
        let result = async {
            for statement in statements { self.execute(statement, 0).await?; }
            let name = format!("{}({})", Session::quote_table(Driver::Postgres, &identity.schema, &identity.name), identity.signature);
            let query = format!("SELECT pg_get_function_identity_arguments(to_regprocedure({}))", literal(Driver::Postgres, &Some(name)));
            let rows = self.execute(&query, 1).await?;
            let Some(signature) = rows.iter().flat_map(|r| &r.rows).find_map(|r| r.first().and_then(|v| v.clone())) else {
                anyhow::bail!(trf("The definition of “{name}” is not available", &[("name", &identity.name)]));
            };
            identity.signature = signature;
            self.execute("COMMIT", 0).await?;
            Ok(identity)
        }.await;
        if result.is_err() { let _ = self.execute("ROLLBACK", 0).await; }
        result
    }

    /// Procedures and functions of database `db` (the session must be on it).
    pub async fn list_routines(&mut self, driver: Driver, db: &str) -> anyhow::Result<Vec<RoutineInfo>> {
        if !supported(driver) {
            return Ok(Vec::new());
        }
        let rows = match self.execute(&list_query(driver, db, true), usize::MAX).await {
            // ORA-00904: invalid identifier (ORACLE_MAINTAINED doesn't exist before 12c).
            Err(e) if driver == Driver::Oracle && e.to_string().contains("ORA-00904") => {
                self.execute(&list_query(driver, db, false), usize::MAX).await?
            }
            other => other?,
        };
        Ok(routines_from_rows(driver, rows.into_iter().next().map(|rs| rs.rows).unwrap_or_default()))
    }

    /// The definition of routine `r` (the session must be on `r.db`).
    pub async fn routine_definition(&mut self, driver: Driver, r: &RoutineRef) -> anyhow::Result<RoutineDefinition> {
        let lit = |s: &str| literal(driver, &Some(s.to_string()));
        let kind = r.kind.keyword().to_uppercase();
        let sql = match driver {
            // `regprocedure` doesn't take the argument names the listing's signature has.
            Driver::Postgres => format!(
                "SELECT pg_get_functiondef(p.oid), obj_description(p.oid, 'pg_proc') FROM pg_proc p \
                 JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = {} AND p.proname = {} AND pg_get_function_identity_arguments(p.oid) = {}",
                lit(&r.schema),
                lit(&r.name),
                lit(&r.signature)
            ),
            Driver::MySql | Driver::MariaDb => format!("SHOW CREATE {kind} {}", Session::quote_table(driver, &r.db, &r.name)),
            Driver::MsSql => format!("SELECT OBJECT_DEFINITION(OBJECT_ID({0})), (SELECT CONVERT(nvarchar(max), value) FROM sys.extended_properties WHERE class = 1 AND major_id = OBJECT_ID({0}) AND minor_id = 0 AND name = N'MS_Description')", lit(&Session::quote_table(driver, &r.schema, &r.name))),
            Driver::Oracle => format!(
                "SELECT TEXT FROM ALL_SOURCE WHERE OWNER = {} AND NAME = {} AND TYPE = '{kind}' ORDER BY LINE",
                lit(&r.schema),
                lit(&r.name)
            ),
            Driver::Snowflake => {
                let name = format!("{}.{}({})", driver.quote_ident(&r.db), Session::quote_table(driver, &r.schema, &r.name), r.signature);
                format!("SELECT GET_DDL('{kind}', {})", lit(&name))
            }
            _ => anyhow::bail!(tr("Not available for this engine")),
        };
        let rows = self.execute(&sql, usize::MAX).await?.into_iter().next().map(|rs| rs.rows).unwrap_or_default();
        let column = match driver {
            // SHOW CREATE PROCEDURE: Procedure, sql_mode, Create Procedure, …
            Driver::MySql | Driver::MariaDb => 2,
            _ => 0,
        };
        let source: String = if driver == Driver::Oracle {
            rows.iter().map(|row| text(row, 0)).collect()
        } else {
            rows.first().map(|row| text(row, column)).unwrap_or_default()
        };
        if source.trim().is_empty() {
            anyhow::bail!(trf("The definition of “{name}” is not available", &[("name", &r.name)]));
        }
        let mut def = definition_from_text(driver, r, &source);
        // These engines keep the comment apart from the definition.
        if matches!(driver, Driver::Postgres | Driver::MsSql) {
            def.comment = rows.first().map(|row| text(row, 1)).unwrap_or_default();
            if let Some(spec) = def.spec.as_mut() {
                spec.options.comment.clone_from(&def.comment);
                spec.mark_original(&r.signature);
            }
        }
        if let Some(query) = comments::query(driver, r) {
            let result = self.execute(&query, usize::MAX).await?;
            let row = result.iter().flat_map(|rs| &rs.rows).find(|row| driver != Driver::Snowflake || snowflake_signature(&text(row, 0)) == r.signature);
            let Some(row) = row else {
                anyhow::bail!(trf("The definition of “{name}” is not available", &[("name", &r.name)]));
            };
            def.comment = text(row, 1);
            if let Some(spec) = def.spec.as_mut() {
                spec.options.comment.clone_from(&def.comment);
                spec.mark_original(&r.signature);
            }
        }
        Ok(def)
    }
}

/// `text` as the server returned it, turned into the console DDL and the dialog's fields.
pub(crate) fn definition_from_text(driver: Driver, r: &RoutineRef, text: &str) -> RoutineDefinition {
    let text = text.trim_end();
    let ddl = match driver {
        // ALL_SOURCE starts at `PROCEDURE name`; SQL*Plus style ends the block with `/`.
        Driver::Oracle => format!("create or replace {}\n/\n", text.trim_start()),
        Driver::MsSql => format!("{text}\nGO\n"),
        Driver::Snowflake => format!("{}\n", text),
        _ => format!("{};\n", text.trim_end_matches(';')),
    };
    let source = if driver == Driver::Oracle { format!("create {}", text.trim_start()) } else { text.to_string() };
    let spec = parse_definition(driver, &source).map(|mut spec| {
        spec.kind = r.kind;
        spec.schema = r.schema.clone();
        // The server's own spelling of the name (it may have folded its case).
        spec.name = r.name.clone();
        spec.mark_original(&r.signature);
        spec
    });
    let comment = spec.as_ref().map(|s| s.options.comment.clone()).unwrap_or_default();
    RoutineDefinition { ddl, spec, comment }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_tests;
