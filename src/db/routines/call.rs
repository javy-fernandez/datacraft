//! The script "Execute…" runs: the call with the typed arguments and, for output parameters,
//! whatever the engine needs to show their values.

use super::ddl::qualified;
use super::{Param, ParamMode, ReturnKind, RoutineKind, RoutineSpec};
use crate::db::Driver;
use crate::edit::literal;

/// A value typed in the "Execute" form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArgValue {
    pub value: String,
    pub null: bool,
    /// Leave the argument out so its default value applies.
    pub use_default: bool,
    /// `value` is an SQL expression, written as is.
    pub expression: bool,
}

fn is_numeric(data_type: &str) -> bool {
    let t = data_type.trim().to_ascii_lowercase();
    [
        "int", "integer", "smallint", "bigint", "tinyint", "mediumint", "numeric", "decimal", "number", "float", "real",
        "double", "money", "smallmoney", "serial", "bit", "binary_float", "binary_double", "pls_integer", "binary_integer",
    ]
    .iter()
    .any(|n| t == *n || t.starts_with(&format!("{n}(")) || t.starts_with(&format!("{n} ")) || (*n == "int" && t.starts_with("int")))
}

fn is_boolean(data_type: &str) -> bool {
    matches!(data_type.trim().to_ascii_lowercase().as_str(), "boolean" | "bool")
}

/// The SQL for one argument.
fn argument(driver: Driver, p: &Param, v: &ArgValue) -> String {
    let ty = p.data_type.trim();
    // PostgreSQL and Snowflake pick an overload by type: typed literals avoid ambiguity.
    let cast = |s: String| match driver {
        Driver::Postgres | Driver::Snowflake if !ty.is_empty() => format!("{s}::{ty}"),
        _ => s,
    };
    let value = v.value.trim();
    if v.null {
        return cast("null".into());
    }
    if v.expression || p.mode == ParamMode::Variadic {
        return value.to_string();
    }
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let mut dot = false;
    let decimal = unsigned.bytes().all(|b| match b {
        b'0'..=b'9' => true,
        b'.' if !dot => {
            dot = true;
            true
        }
        _ => false,
    }) && unsigned.bytes().any(|b| b.is_ascii_digit());
    if is_numeric(ty) && decimal {
        return value.to_string();
    }
    if is_boolean(ty) && matches!(value.to_ascii_lowercase().as_str(), "true" | "false") {
        return value.to_ascii_lowercase();
    }
    cast(literal(driver, &Some(value.to_string())))
}

fn param_name(p: &Param) -> String {
    p.name.trim().trim_start_matches('@').to_string()
}

/// `a => 1` arguments when some argument is left to its default (and isn't the last ones),
/// positional ones otherwise. `args` holds `None` for arguments left out.
fn arg_list(params: &[&Param], args: &[Option<String>]) -> String {
    let last_given = args.iter().rposition(Option::is_some);
    let gaps = last_given.is_some_and(|last| args[..last].iter().any(Option::is_none));
    let named = gaps && params.iter().all(|p| !p.name.trim().is_empty());
    params
        .iter()
        .zip(args)
        .take(last_given.map_or(0, |l| l + 1))
        .filter_map(|(p, a)| match (a, named) {
            (Some(a), true) => Some(format!("{} => {a}", param_name(p))),
            (Some(a), false) => Some(a.clone()),
            // A gap without names: the default expression is written out.
            (None, false) => Some(p.default.trim().to_string()),
            (None, true) => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Oracle variable declaration for an output parameter: unconstrained text types need a length.
fn plsql_type(data_type: &str) -> String {
    let t = data_type.trim();
    let upper = t.to_ascii_uppercase();
    let base = upper.trim_start_matches("NOCOPY ").trim();
    if matches!(base, "VARCHAR2" | "NVARCHAR2" | "VARCHAR" | "CHAR" | "NCHAR" | "RAW") {
        format!("{base}(32767)")
    } else {
        t.trim_start_matches("NOCOPY ").trim_start_matches("nocopy ").to_string()
    }
}

fn oracle_print(name: &str, var: &str, data_type: &str) -> String {
    let label = name.replace('\'', "''");
    if is_boolean(data_type) {
        format!("dbms_output.put_line('{label} = ' || case when {var} then 'TRUE' when not {var} then 'FALSE' end);")
    } else {
        format!("dbms_output.put_line('{label} = ' || {var});")
    }
}

/// The script that calls `spec` with `values` (one per parameter, in order).
pub fn call_sql(driver: Driver, spec: &RoutineSpec, values: &[ArgValue]) -> String {
    let name = qualified(driver, &spec.schema, &spec.name);
    let function = spec.kind == RoutineKind::Function;
    let blank = ArgValue::default();
    let value = |i: usize| values.get(i).unwrap_or(&blank);
    let input = |i: usize, p: &Param| -> Option<String> {
        let v = value(i);
        (!v.use_default || p.default.trim().is_empty()).then(|| argument(driver, p, v))
    };
    let table = matches!(spec.returns.kind, ReturnKind::Table | ReturnKind::SetOf);
    match driver {
        Driver::Postgres => {
            let (params, args): (Vec<&Param>, Vec<Option<String>>) = spec
                .params
                .iter()
                .enumerate()
                // Functions take only input arguments; procedures also get NULL for OUT ones.
                .filter(|(_, p)| !function || p.mode != ParamMode::Out)
                .map(|(i, p)| (p, if p.mode == ParamMode::Out { Some("null".into()) } else { input(i, p) }))
                .unzip();
            let args = arg_list(&params, &args);
            if !function {
                format!("call {name}({args});")
            } else if table || spec.params.iter().any(|p| p.mode.returns_output()) {
                format!("select * from {name}({args});")
            } else {
                format!("select {name}({args});")
            }
        }
        Driver::MySql | Driver::MariaDb => {
            if function {
                let args: Vec<String> = spec.params.iter().enumerate().map(|(i, p)| argument(driver, p, value(i))).collect();
                return format!("select {name}({});", args.join(", "));
            }
            let var = |p: &Param| format!("@{}", param_name(p));
            let mut lines = Vec::new();
            let mut args = Vec::new();
            for (i, p) in spec.params.iter().enumerate() {
                match p.mode {
                    ParamMode::Out => args.push(var(p)),
                    ParamMode::InOut => {
                        lines.push(format!("set {} = {};", var(p), argument(driver, p, value(i))));
                        args.push(var(p));
                    }
                    _ => args.push(argument(driver, p, value(i))),
                }
            }
            lines.push(format!("call {name}({});", args.join(", ")));
            let outs: Vec<String> = spec
                .params
                .iter()
                .filter(|p| p.mode.returns_output())
                .map(|p| format!("{} as {}", var(p), crate::ui::quote_ident_if_needed(driver, &param_name(p))))
                .collect();
            if !outs.is_empty() {
                lines.push(format!("select {};", outs.join(", ")));
            }
            lines.join("\n")
        }
        Driver::MsSql => {
            if function {
                // Function arguments are positional; `default` takes a parameter's default.
                let args: Vec<String> = spec
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| input(i, p).unwrap_or_else(|| "default".into()))
                    .collect();
                return if table {
                    format!("select * from {name}({});", args.join(", "))
                } else {
                    format!("select {name}({}) as [result];", args.join(", "))
                };
            }
            let mut lines = Vec::new();
            let mut args = Vec::new();
            for (i, p) in spec.params.iter().enumerate() {
                let pname = format!("@{}", param_name(p));
                if p.mode.returns_output() {
                    let ty = p.data_type.trim();
                    let v = value(i);
                    let init = if v.value.trim().is_empty() && !v.null && !v.expression { String::new() } else { format!(" = {}", argument(driver, p, v)) };
                    lines.push(format!("declare {pname} {ty}{init};"));
                    args.push(format!("{pname} = {pname} output"));
                } else if let Some(a) = input(i, p) {
                    args.push(format!("{pname} = {a}"));
                }
            }
            lines.push("declare @dc_return int;".into());
            let args = if args.is_empty() { String::new() } else { format!(" {}", args.join(", ")) };
            lines.push(format!("exec @dc_return = {name}{args};"));
            let mut outs: Vec<String> = spec
                .params
                .iter()
                .filter(|p| p.mode.returns_output())
                .map(|p| format!("@{0} as [{0}]", param_name(p).replace(']', "]]")))
                .collect();
            outs.push("@dc_return as [return_value]".into());
            lines.push(format!("select {};", outs.join(", ")));
            lines.join("\n")
        }
        Driver::Oracle => {
            let outputs = spec.params.iter().any(|p| p.mode.returns_output());
            let boolean_result = function && is_boolean(&spec.returns.data_type);
            let (params, args): (Vec<&Param>, Vec<Option<String>>) = spec
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| (p, if p.mode.returns_output() { Some(format!("v_{}", param_name(p))) } else { input(i, p) }))
                .unzip();
            let args = arg_list(&params, &args);
            let call = if args.is_empty() { name.clone() } else { format!("{name}({args})") };
            if function && !outputs && !boolean_result {
                return if spec.options.pipelined {
                    format!("select * from table({call});")
                } else {
                    format!("select {call} as result from dual;")
                };
            }
            if !function && !outputs {
                return format!("begin\n    {call};\nend;");
            }
            let mut decl = Vec::new();
            let mut body = vec!["    dbms_output.enable(null);".to_string()];
            for (i, p) in spec.params.iter().enumerate().filter(|(_, p)| p.mode.returns_output()) {
                let var = format!("v_{}", param_name(p));
                decl.push(format!("    {var} {};", plsql_type(&p.data_type)));
                let v = value(i);
                if p.mode == ParamMode::InOut && (v.null || v.expression || !v.value.trim().is_empty()) {
                    body.push(format!("    {var} := {};", argument(driver, p, v)));
                }
            }
            if function {
                decl.push(format!("    v_result {};", plsql_type(&spec.returns.data_type)));
                body.push(format!("    v_result := {call};"));
                body.push(format!("    {}", oracle_print("result", "v_result", &spec.returns.data_type)));
            } else {
                body.push(format!("    {call};"));
            }
            for p in spec.params.iter().filter(|p| p.mode.returns_output()) {
                body.push(format!("    {}", oracle_print(&param_name(p), &format!("v_{}", param_name(p)), &p.data_type)));
            }
            format!("declare\n{}\nbegin\n{}\nend;", decl.join("\n"), body.join("\n"))
        }
        Driver::Snowflake => {
            let (params, args): (Vec<&Param>, Vec<Option<String>>) = spec.params.iter().enumerate().map(|(i, p)| (p, input(i, p))).unzip();
            let args = arg_list(&params, &args);
            if !function {
                format!("call {name}({args});")
            } else if table {
                format!("select * from table({name}({args}));")
            } else {
                format!("select {name}({args});")
            }
        }
        _ => String::new(),
    }
}

/// Routines "Execute…" can't call (PostgreSQL trigger functions run only from their trigger).
pub fn callable(spec: &RoutineSpec) -> bool {
    !matches!(spec.returns.data_type.trim().to_ascii_lowercase().as_str(), "trigger" | "event_trigger")
}
