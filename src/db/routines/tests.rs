use super::*;

#[tokio::test]
async fn failed_identity_resolution_rolls_back_the_definition_change() {
    let mut config = crate::db::ConnectionConfig::new(1, Driver::Sqlite);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("datacraft-routine-rollback-{}-{stamp}.db", std::process::id()));
    drop(rusqlite::Connection::open(&path).unwrap());
    config.file_path = path.display().to_string();
    let mut session = Session::connect(&config).await.unwrap();
    let identity = RoutineRef { conn: 1, db: "main".into(), schema: "public".into(), name: "pending_routine".into(), kind: RoutineKind::Function, signature: String::new() };
    // SQLite's missing PostgreSQL resolver forces a failure after the transactional DDL.
    assert!(session.save_postgres_routine(&["CREATE TABLE pending_routine (id INTEGER)".into()], identity).await.is_err());
    let rows = session.execute("SELECT name FROM sqlite_master WHERE name = 'pending_routine'", 10).await.unwrap();
    assert!(rows.iter().all(|rs| rs.rows.is_empty()));
    drop(session);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn parsed_object_identifiers_follow_the_engines_case_rules() {
    for (driver, sql, schema, name) in [
        (Driver::Postgres, "CREATE FUNCTION Public.Total() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$", "public", "total"),
        (Driver::Oracle, "CREATE FUNCTION shop.total RETURN number AS BEGIN RETURN 1; END;", "SHOP", "TOTAL"),
        (Driver::Snowflake, "CREATE FUNCTION shop.total() RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$", "SHOP", "TOTAL"),
    ] {
        let spec = parse_definition(driver, sql).unwrap();
        assert_eq!((spec.schema.as_str(), spec.name.as_str()), (schema, name));
        let quoted = sql.replace("Public.Total", "\"shop\".\"total\"").replace("shop.total", "\"shop\".\"total\"");
        let spec = parse_definition(driver, &quoted).unwrap();
        assert_eq!((spec.schema.as_str(), spec.name.as_str()), ("shop", "total"));
    }
}

fn param(name: &str, mode: ParamMode, data_type: &str, default: &str) -> Param {
    Param { name: name.into(), mode, data_type: data_type.into(), default: default.into() }
}

fn parse(driver: Driver, text: &str) -> RoutineSpec {
    parse_definition(driver, text).unwrap_or_else(|| panic!("{driver:?} definition not understood:\n{text}"))
}

/// The definition texts below are what each server returned for routines created in tests.
#[test]
fn parses_postgres_definitions() {
    let s = parse(
        Driver::Postgres,
        "CREATE OR REPLACE FUNCTION app.add_tax(amount numeric, rate numeric DEFAULT 0.23, OUT total numeric, OUT tax numeric)\n RETURNS record\n LANGUAGE plpgsql\n STABLE\nAS $function$ begin tax := amount * rate; total := amount + tax; end; $function$\n",
    );
    assert_eq!((s.kind, s.schema.as_str(), s.name.as_str()), (RoutineKind::Function, "app", "add_tax"));
    assert_eq!(
        s.params,
        vec![
            param("amount", ParamMode::In, "numeric", ""),
            param("rate", ParamMode::In, "numeric", "0.23"),
            param("total", ParamMode::Out, "numeric", ""),
            param("tax", ParamMode::Out, "numeric", ""),
        ]
    );
    assert_eq!(s.returns.data_type, "record");
    assert_eq!((s.options.language.as_str(), s.options.volatility.as_str()), ("plpgsql", "STABLE"));
    assert_eq!(s.body, " begin tax := amount * rate; total := amount + tax; end; ");

    let s = parse(
        Driver::Postgres,
        "CREATE OR REPLACE FUNCTION app.top_orders(n integer)\n RETURNS TABLE(id integer, total numeric)\n LANGUAGE sql\n STABLE PARALLEL SAFE STRICT SECURITY DEFINER COST 50\n SET search_path TO 'app'\nAS $function$ select 1, 2.5::numeric limit n $function$\n",
    );
    assert_eq!(s.returns.kind, ReturnKind::Table);
    assert_eq!(s.returns.columns, vec![Column { name: "id".into(), data_type: "integer".into() }, Column { name: "total".into(), data_type: "numeric".into() }]);
    assert!(s.options.strict);
    assert_eq!((s.options.security.as_str(), s.options.parallel.as_str()), ("DEFINER", "SAFE"));
    assert_eq!(s.options.extra, "COST 50\nSET search_path TO 'app'");

    let s = parse(
        Driver::Postgres,
        "CREATE OR REPLACE PROCEDURE app.touch(IN p_id integer, INOUT p_note text DEFAULT 'x'::text)\n LANGUAGE plpgsql\nAS $procedure$ begin p_note := p_note || p_id; end $procedure$\n",
    );
    assert_eq!(s.kind, RoutineKind::Procedure);
    assert_eq!(s.params[1], param("p_note", ParamMode::InOut, "text", "'x'::text"));

    let s = parse(
        Driver::Postgres,
        "CREATE OR REPLACE FUNCTION app.unnamed(integer, double precision)\n RETURNS SETOF text\n LANGUAGE sql\nAS $function$select 'a'$function$\n",
    );
    assert_eq!(s.params, vec![param("", ParamMode::In, "integer", ""), param("", ParamMode::In, "double precision", "")]);
    assert_eq!((s.returns.kind, s.returns.data_type.as_str()), (ReturnKind::SetOf, "text"));

    // SQL-standard bodies and C functions stay as text.
    assert!(parse_definition(Driver::Postgres, "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END").is_none());
    assert!(parse_definition(Driver::Postgres, "CREATE FUNCTION f() RETURNS int LANGUAGE c AS 'lib', 'sym'").is_none());
}

#[test]
fn parses_mysql_and_mariadb_definitions() {
    let s = parse(
        Driver::MariaDb,
        "CREATE DEFINER=`root`@`localhost` PROCEDURE `add_order`(in p_user int, out p_id bigint, inout p_note varchar(20))\n    MODIFIES SQL DATA\n    SQL SECURITY INVOKER\n    COMMENT 'Adds an order'\nbegin\n  set p_id = p_user * 10;\n  set p_note = concat(p_note, '!');\nend",
    );
    assert_eq!(s.name, "add_order");
    assert_eq!(s.options.definer, "`root`@`localhost`");
    assert_eq!(
        s.params,
        vec![param("p_user", ParamMode::In, "int", ""), param("p_id", ParamMode::Out, "bigint", ""), param("p_note", ParamMode::InOut, "varchar(20)", "")]
    );
    assert_eq!((s.options.data_access.as_str(), s.options.security.as_str()), ("MODIFIES SQL DATA", "INVOKER"));
    assert_eq!(s.options.comment, "Adds an order");
    assert_eq!(s.body, "begin\n  set p_id = p_user * 10;\n  set p_note = concat(p_note, '!');\nend");

    let s = parse(
        Driver::MariaDb,
        "CREATE DEFINER=`root`@`localhost` FUNCTION `total_with_tax`(amount decimal(10,2), rate double) RETURNS decimal(12,2) unsigned\n    NO SQL\n    DETERMINISTIC\nreturn amount * (1 + rate)",
    );
    assert_eq!(s.returns.data_type, "decimal(12,2) unsigned");
    assert!(s.options.deterministic);
    assert_eq!(s.options.data_access, "NO SQL");
    assert_eq!(s.body, "return amount * (1 + rate)");

    let s = parse(
        Driver::MySql,
        "CREATE DEFINER=`root`@`localhost` FUNCTION `greet`(who varchar(30)) RETURNS varchar(50) CHARSET utf8mb4 COLLATE utf8mb4_general_ci\n    READS SQL DATA\nbegin\n  return concat('hi ', who);\nend",
    );
    assert_eq!(s.returns.data_type, "varchar(50) CHARSET utf8mb4 COLLATE utf8mb4_general_ci");
    assert_eq!(s.body, "begin\n  return concat('hi ', who);\nend");

    let s = parse(Driver::MySql, "CREATE DEFINER=`root`@`localhost` PROCEDURE `add_order`(in p_user int, out p_id bigint)\n    COMMENT 'it''s'\nbegin\n  set p_id = p_user * 10;\nend");
    assert_eq!(s.options.comment, "it's");
    let s = parse(Driver::MySql, "CREATE DEFINER=`root`@`localhost` FUNCTION `f2`(a int) RETURNS double\n    DETERMINISTIC\nreturn a + 1");
    assert_eq!((s.returns.data_type.as_str(), s.options.deterministic), ("double", true));
}

#[test]
fn parses_sql_server_definitions() {
    let s = parse(
        Driver::MsSql,
        "create procedure app.add_order\n    @user_id int,\n    @note nvarchar(20) = N'none',\n    @new_id bigint output\nwith recompile\nas\nbegin\n    set nocount on;\n    set @new_id = @user_id * 10;\nend",
    );
    assert_eq!((s.schema.as_str(), s.name.as_str()), ("app", "add_order"));
    assert_eq!(
        s.params,
        vec![
            param("@user_id", ParamMode::In, "int", ""),
            param("@note", ParamMode::In, "nvarchar(20)", "N'none'"),
            param("@new_id", ParamMode::Out, "bigint", ""),
        ]
    );
    assert!(s.options.recompile);
    assert_eq!(s.body, "begin\n    set nocount on;\n    set @new_id = @user_id * 10;\nend");

    let s = parse(
        Driver::MsSql,
        "CREATE FUNCTION [app].[tax] (@amount decimal(10,2), @rate float = 0.23)\nRETURNS decimal(12,2)\nWITH SCHEMABINDING, RETURNS NULL ON NULL INPUT\nAS\nBEGIN\n    RETURN @amount * @rate;\nEND",
    );
    assert_eq!(s.params[1], param("@rate", ParamMode::In, "float", "0.23"));
    assert_eq!(s.returns.data_type, "decimal(12,2)");
    assert!(s.options.schemabinding && s.options.strict);
    assert_eq!(s.body, "BEGIN\n    RETURN @amount * @rate;\nEND");

    let s = parse(Driver::MsSql, "create function app.orders_of(@user int) returns table as return (select @user as user_id, 1 as n)");
    assert_eq!((s.returns.kind, s.returns.columns.len()), (ReturnKind::Table, 0));
    assert_eq!(s.body, "return (select @user as user_id, 1 as n)");

    let s = parse(
        Driver::MsSql,
        "create function app.multi(@n int)\nreturns @result table (id int not null, label nvarchar(10))\nas\nbegin\n    insert @result values (@n, N'x');\n    return;\nend",
    );
    assert_eq!(s.returns.table_variable, "@result");
    assert_eq!(s.returns.columns[0], Column { name: "id".into(), data_type: "int not null".into() });

    let s = parse(Driver::MsSql, "CREATE PROC dbo.p AS SELECT 1");
    assert!(s.params.is_empty() && s.body == "SELECT 1");
    let s = parse(Driver::MsSql, "CREATE PROCEDURE dbo.p (@t dbo.Ids READONLY, @x int OUT) AS SELECT 1");
    assert_eq!(s.params, vec![param("@t", ParamMode::In, "dbo.Ids readonly", ""), param("@x", ParamMode::Out, "int", "")]);
}

#[test]
fn parses_oracle_source() {
    // ALL_SOURCE text, with the `create` the loader adds.
    let s = parse(
        Driver::Oracle,
        "create procedure raise_salary (\n    p_id in number,\n    p_pct in number default 10,\n    p_new out number,\n    p_log in out nocopy varchar2\n) authid current_user is\n    v number;\nbegin\n    p_new := p_id * (1 + p_pct / 100);\nend raise_salary;\n",
    );
    assert_eq!(
        s.params,
        vec![
            param("p_id", ParamMode::In, "number", ""),
            param("p_pct", ParamMode::In, "number", "10"),
            param("p_new", ParamMode::Out, "number", ""),
            param("p_log", ParamMode::InOut, "nocopy varchar2", ""),
        ]
    );
    assert_eq!(s.options.security, "current_user");
    assert_eq!(s.body, "v number;\nbegin\n    p_new := p_id * (1 + p_pct / 100);\nend raise_salary;");

    let s = parse(
        Driver::Oracle,
        "create function full_name(p_first varchar2, p_last varchar2 := 'X') return varchar2 deterministic parallel_enable as\nbegin\n    return p_first || ' ' || p_last;\nend;\n",
    );
    assert_eq!(s.params[1], param("p_last", ParamMode::In, "varchar2", "'X'"));
    assert_eq!(s.returns.data_type, "varchar2");
    assert!(s.options.deterministic && s.options.parallel_enable);
    assert_eq!(s.body, "begin\n    return p_first || ' ' || p_last;\nend;");

    let s = parse(Driver::Oracle, "create PROCEDURE touch IS\nBEGIN\n  NULL;\nEND;");
    assert!(s.params.is_empty());
    let s = parse(Driver::Oracle, "create function f(p emp.sal%TYPE) return emp.sal%TYPE result_cache is begin return p; end;");
    assert_eq!((s.params[0].data_type.as_str(), s.returns.data_type.as_str()), ("emp.sal%TYPE", "emp.sal%TYPE"));
    assert_eq!(s.options.extra, "result_cache");
}

#[test]
fn parses_snowflake_get_ddl() {
    let s = parse(
        Driver::Snowflake,
        "CREATE OR REPLACE PROCEDURE \"ADD_ORDER\"(\"USER_ID\" NUMBER(38,0), \"NOTE\" VARCHAR(16777216) DEFAULT 'none')\nRETURNS VARCHAR(16777216)\nLANGUAGE SQL\nCOMMENT='Adds it''s order'\nEXECUTE AS CALLER\nAS '\nbegin\n  return ''ok'';\nend\n';",
    );
    assert_eq!(s.name, "ADD_ORDER");
    assert_eq!(s.params, vec![param("USER_ID", ParamMode::In, "NUMBER(38,0)", ""), param("NOTE", ParamMode::In, "VARCHAR(16777216)", "'none'")]);
    assert_eq!(s.returns.data_type, "VARCHAR(16777216)");
    assert_eq!((s.options.language.as_str(), s.options.security.as_str()), ("SQL", "CALLER"));
    assert_eq!(s.options.comment, "Adds it's order");
    assert_eq!(s.body, "begin\n  return 'ok';\nend");

    let s = parse(
        Driver::Snowflake,
        "CREATE OR REPLACE SECURE FUNCTION \"TOP\"(\"N\" NUMBER(38,0))\nRETURNS TABLE (\"ID\" NUMBER(38,0), \"NAME\" VARCHAR)\nLANGUAGE PYTHON\nRUNTIME_VERSION = '3.11'\nPACKAGES = ('snowflake-snowpark-python')\nHANDLER = 'Top'\nIMMUTABLE\nAS $$\nclass Top:\n    pass\n$$;",
    );
    assert!(s.options.secure);
    assert_eq!(s.returns.columns.len(), 2);
    assert_eq!((s.options.runtime_version.as_str(), s.options.handler.as_str()), ("3.11", "Top"));
    assert_eq!(s.options.packages, "'snowflake-snowpark-python'");
    assert_eq!(s.options.volatility, "IMMUTABLE");
    assert_eq!(s.body, "class Top:\n    pass");
}

/// A spec that uses the options of `driver`.
fn sample(driver: Driver, kind: RoutineKind) -> RoutineSpec {
    // Oracle and Snowflake keep unquoted names in uppercase.
    let schema = if matches!(driver, Driver::Oracle | Driver::Snowflake) { "APP" } else { "app" };
    let mut s = RoutineSpec::new(driver, kind, schema.into());
    s.name = "order_total".into();
    let function = kind == RoutineKind::Function;
    let (a, b) = match driver {
        Driver::MsSql => ("@order_id", "@rate"),
        _ => ("order_id", "rate"),
    };
    s.params.push(param(a, ParamMode::In, "integer", ""));
    s.params.push(param(b, ParamMode::In, "numeric(5,2)", if has_defaults(driver) { "0.23" } else { "" }));
    if !function && param_modes(driver, kind).contains(&ParamMode::Out) {
        s.params.push(param(if driver == Driver::MsSql { "@total" } else { "total" }, ParamMode::Out, "numeric(12,2)", ""));
    }
    let o = &mut s.options;
    match driver {
        Driver::Postgres => {
            o.security = "DEFINER".into();
            if function {
                o.volatility = "STABLE".into();
                o.strict = true;
                o.parallel = "SAFE".into();
            }
            o.extra = "SET search_path TO 'app'".into();
        }
        Driver::MySql | Driver::MariaDb => {
            o.comment = "Order's total".into();
            o.deterministic = function;
            o.security = "INVOKER".into();
            o.definer = "`root`@`localhost`".into();
        }
        Driver::MsSql => {
            o.security = "OWNER".into();
            o.schemabinding = function;
            o.strict = function;
            o.recompile = !function;
        }
        Driver::Oracle => {
            o.security = "CURRENT_USER".into();
            o.deterministic = function;
        }
        Driver::Snowflake => {
            o.comment = "Order's total".into();
            o.volatility = "IMMUTABLE".into();
            o.strict = true;
            o.secure = true;
            if !function {
                o.security = "CALLER".into();
            }
        }
        _ => {}
    }
    s
}

const ENGINES: [Driver; 6] = [Driver::Postgres, Driver::MySql, Driver::MariaDb, Driver::MsSql, Driver::Oracle, Driver::Snowflake];

/// What the parser can't know (it comes from the catalog) or normalizes.
fn comparable(driver: Driver, mut s: RoutineSpec) -> RoutineSpec {
    s.or_replace = false;
    s.original = None;
    s.schema = s.schema.to_lowercase();
    s.name = s.name.to_lowercase();
    if driver == Driver::Postgres {
        s.options.comment.clear();
    }
    for p in &mut s.params {
        p.name = p.name.to_lowercase();
    }
    s.options.volatility = s.options.volatility.to_uppercase();
    s.options.security = s.options.security.to_uppercase();
    s.options.parallel = s.options.parallel.to_uppercase();
    s.options.data_access = s.options.data_access.to_uppercase();
    s.options.language = s.options.language.to_lowercase();
    s
}

#[test]
fn generated_statements_parse_back_to_the_same_fields() {
    for driver in ENGINES {
        for kind in [RoutineKind::Procedure, RoutineKind::Function] {
            let spec = sample(driver, kind);
            let sql = create_sql(driver, &spec, true);
            let parsed = parse_definition(driver, &sql).unwrap_or_else(|| panic!("{driver:?} {kind:?}:\n{sql}"));
            assert_eq!(comparable(driver, parsed), comparable(driver, spec.clone()), "{driver:?} {kind:?}:\n{sql}");
        }
    }
}

#[test]
fn table_results_round_trip() {
    let cols = vec![Column { name: "id".into(), data_type: "integer".into() }, Column { name: "label".into(), data_type: "varchar(20)".into() }];
    for driver in [Driver::Postgres, Driver::MsSql, Driver::Snowflake] {
        let mut spec = sample(driver, RoutineKind::Function);
        spec.options.strict = false;
        spec.options.schemabinding = false;
        spec.options.security.clear();
        spec.returns = Returns { kind: ReturnKind::Table, columns: cols.clone(), table_variable: if driver == Driver::MsSql { "@result".into() } else { String::new() }, ..Default::default() };
        let sql = create_sql(driver, &spec, false);
        let parsed = parse_definition(driver, &sql).unwrap_or_else(|| panic!("{driver:?}:\n{sql}"));
        assert_eq!(parsed.returns, spec.returns, "{driver:?}:\n{sql}");
    }
}

#[test]
fn creates_routines_per_engine() {
    let pg = create_sql(Driver::Postgres, &sample(Driver::Postgres, RoutineKind::Function), true);
    assert_eq!(
        pg,
        "create or replace function app.order_total(\n    order_id integer,\n    rate numeric(5,2) default 0.23\n)\nreturns integer\nlanguage plpgsql\n\
         stable strict security definer parallel safe\nSET search_path TO 'app'\nas $$\nbegin\n    return 0;\nend;\n$$"
    );
    let my = create_sql(Driver::MySql, &sample(Driver::MySql, RoutineKind::Procedure), false);
    assert_eq!(
        my,
        "create definer = `root`@`localhost` procedure app.order_total(\n    order_id integer,\n    rate numeric(5,2),\n    out total numeric(12,2)\n)\n\
         comment 'Order''s total'\nsql security invoker\nbegin\n    \nend"
    );
    let ms = create_sql(Driver::MsSql, &sample(Driver::MsSql, RoutineKind::Procedure), true);
    assert_eq!(
        ms,
        "create or alter procedure app.order_total\n    @order_id integer,\n    @rate numeric(5,2) = 0.23,\n    @total numeric(12,2) output\n\
         with recompile, execute as owner\nas\nbegin\n    set nocount on;\n    \nend"
    );
    let ora = create_sql(Driver::Oracle, &sample(Driver::Oracle, RoutineKind::Function), true);
    assert_eq!(
        ora,
        "create or replace function APP.ORDER_TOTAL(\n    order_id integer,\n    rate numeric(5,2) default 0.23\n)\nreturn number\nauthid current_user\n\
         deterministic\nas\nbegin\n    return 0;\nend;"
    );
    let sf = create_sql(Driver::Snowflake, &sample(Driver::Snowflake, RoutineKind::Procedure), true);
    assert_eq!(
        sf,
        "create or replace secure procedure APP.ORDER_TOTAL(\n    order_id integer,\n    rate numeric(5,2) default 0.23\n)\nreturns VARCHAR\nlanguage sql\n\
         returns null on null input\nimmutable\ncomment = 'Order''s total'\nexecute as caller\nas\n$$\nbegin\n    return 'done';\nend;\n$$"
    );
}

#[test]
fn bodies_with_their_own_delimiters_get_other_quotes() {
    let mut s = RoutineSpec::new(Driver::Postgres, RoutineKind::Function, "public".into());
    s.name = "f".into();
    s.body = "select $$x$$".into();
    assert!(create_sql(Driver::Postgres, &s, false).ends_with("as $body$\nselect $$x$$\n$body$"));
    let mut s = RoutineSpec::new(Driver::Snowflake, RoutineKind::Function, "PUBLIC".into());
    s.name = "F".into();
    s.body = "'$$' || 'it\\'s'".into();
    let sql = create_sql(Driver::Snowflake, &s, false);
    assert!(sql.ends_with("as\n'\n''$$'' || ''it\\\\''s''\n'"), "{sql}");
    assert_eq!(parse_definition(Driver::Snowflake, &sql).unwrap().body, s.body);
}

#[test]
fn new_routine_statements() {
    let mut s = sample(Driver::MySql, RoutineKind::Function);
    s.or_replace = true;
    let st = routine_statements(Driver::MySql, &s);
    assert_eq!(st.statements[2], "drop function if exists app.order_total");
    assert!(st.statements[3].starts_with("create definer"));
    assert!(routine_statements(Driver::MariaDb, &s).statements[0].starts_with("create or replace definer"));

    let s = sample(Driver::Oracle, RoutineKind::Procedure);
    let st = routine_statements(Driver::Oracle, &s);
    assert_eq!(st.statements.len(), 2);
    assert!(st.statements[1].contains("name = 'ORDER_TOTAL' and type = 'PROCEDURE'"), "{}", st.statements[1]);

    let mut s = sample(Driver::Postgres, RoutineKind::Function);
    s.options.comment = "Total".into();
    let st = routine_statements(Driver::Postgres, &s);
    assert!(st.transaction);
    assert_eq!(st.statements[1], "comment on function app.order_total(integer, numeric(5,2)) is 'Total'");
}

fn edited(driver: Driver, kind: RoutineKind, signature: &str) -> RoutineSpec {
    let mut s = sample(driver, kind);
    s.name = final_name(driver, &s);
    s.mark_original(signature);
    s
}

#[test]
fn editing_replaces_in_place_when_possible() {
    let pg = edited(Driver::Postgres, RoutineKind::Function, "order_id integer, rate numeric");
    let st = routine_statements(Driver::Postgres, &pg);
    assert_eq!(st.statements.len(), 1);
    assert!(st.statements[0].starts_with("create or replace function app.order_total("));

    let mut renamed = pg.clone();
    renamed.name = "grand_total".into();
    let st = routine_statements(Driver::Postgres, &renamed);
    assert_eq!(st.statements[0], "alter function app.order_total(order_id integer, rate numeric) rename to grand_total");
    assert!(st.statements[1].starts_with("create or replace function app.grand_total("));

    let mut retyped = pg.clone();
    retyped.params[0].data_type = "bigint".into();
    retyped.options.comment = "kept".into();
    let st = routine_statements(Driver::Postgres, &retyped);
    assert!(st.transaction);
    assert_eq!(st.statements[0], "drop function app.order_total(order_id integer, rate numeric)");
    assert!(st.statements[1].starts_with("create function app.order_total("));
    assert_eq!(st.statements[2], "comment on function app.order_total(bigint, numeric(5,2)) is 'kept'");

    assert!(routine_statements(Driver::MariaDb, &edited(Driver::MariaDb, RoutineKind::Procedure, "")).statements[0].starts_with("create or replace definer"));
    let ms = routine_statements(Driver::MsSql, &edited(Driver::MsSql, RoutineKind::Procedure, ""));
    assert_eq!(ms.statements.len(), 1);
    assert!(ms.statements[0].starts_with("alter procedure app.order_total"));
    let mut ms_kind = edited(Driver::MsSql, RoutineKind::Function, "");
    ms_kind.returns = Returns { kind: ReturnKind::Table, ..Default::default() };
    let st = routine_statements(Driver::MsSql, &ms_kind);
    assert!(st.transaction);
    assert_eq!(st.statements[0], "drop function app.order_total");

    let ora = routine_statements(Driver::Oracle, &edited(Driver::Oracle, RoutineKind::Function, ""));
    assert!(ora.statements[2].starts_with("create or replace function APP.ORDER_TOTAL("));
    assert_eq!(ora.statements.len(), 4);

    let sf = edited(Driver::Snowflake, RoutineKind::Procedure, "NUMBER, NUMBER");
    assert_eq!(routine_statements(Driver::Snowflake, &sf).statements.len(), 1);
    let mut sf_retyped = sf.clone();
    sf_retyped.params[0].data_type = "varchar".into();
    let st = routine_statements(Driver::Snowflake, &sf_retyped);
    assert!(st.statements[0].starts_with("create or replace secure procedure APP.ORDER_TOTAL("));
    assert_eq!(st.statements[1], "drop procedure APP.ORDER_TOTAL(NUMBER, NUMBER)");
}

#[test]
fn mysql_checks_the_new_text_before_dropping() {
    let st = routine_statements(Driver::MySql, &edited(Driver::MySql, RoutineKind::Procedure, ""));
    assert_eq!(st.statements.len(), 4);
    assert!(st.statements[0].contains("procedure app.order_total_dc_check("));
    assert_eq!(st.statements[1], "drop procedure app.order_total_dc_check");
    assert_eq!(st.statements[2], "drop procedure app.order_total");
    assert!(st.statements[3].starts_with("create definer = `root`@`localhost` procedure app.order_total("));

    let mut renamed = edited(Driver::MySql, RoutineKind::Procedure, "");
    renamed.name = "other".into();
    let st = routine_statements(Driver::MySql, &renamed);
    assert!(st.statements[0].contains("procedure app.other("));
    assert_eq!(st.statements[1], "drop procedure app.order_total");
}

#[test]
fn replacement_checks_a_scratch_routine_before_touching_the_original() {
    for driver in [Driver::MySql, Driver::Oracle] {
        for edit in [false, true] {
            let mut s = if edit { edited(driver, RoutineKind::Function, "") } else { sample(driver, RoutineKind::Function) };
            s.or_replace = true;
            let statements = routine_statements(driver, &s).statements;
            let target = final_name(driver, &s);
            assert!(statements[0].to_ascii_lowercase().contains("dc_check"), "{driver:?}: {statements:?}");
            let target_action = statements.iter().position(|sql| {
                let sql = sql.to_ascii_lowercase();
                !sql.contains("dc_check") && sql.contains(&target.to_ascii_lowercase()) && (sql.starts_with("drop ") || sql.starts_with("create or replace"))
            }).unwrap();
            assert!(target_action >= 2, "{driver:?}: {statements:?}");
            if driver == Driver::Oracle {
                assert!(statements[1].contains("all_errors"));
                assert!(statements[1].to_ascii_lowercase().contains("execute immediate"));
            }
        }
    }
}

#[test]
fn numeric_arguments_only_emit_plain_decimal_literals_unquoted() {
    let spec = parse(Driver::MySql, "CREATE PROCEDURE p(in n double) begin end");
    for value in ["NaN", "inf", "-inf", "1e5"] {
        assert!(call_sql(Driver::MySql, &spec, &values(&[value])).contains(&format!("'{value}'")), "{value}");
    }
    for value in ["-12.5", "+4", ".5", "5."] {
        assert!(call_sql(Driver::MySql, &spec, &values(&[value])).contains(&format!("({value})")), "{value}");
    }
}

#[test]
fn existing_names_keep_their_case() {
    // A lowercase (quoted) Oracle name read from the server is not folded to uppercase.
    let mut s = sample(Driver::Oracle, RoutineKind::Procedure);
    s.name = "lower_case".into();
    s.mark_original("");
    assert!(create_sql(Driver::Oracle, &s, true).starts_with("create or replace procedure APP.\"lower_case\""));
    assert_eq!(drop_sql(Driver::Oracle, RoutineKind::Procedure, "HR", "lower_case", ""), "drop procedure HR.\"lower_case\"");
    assert_eq!(drop_sql(Driver::Postgres, RoutineKind::Function, "public", "f", "a integer"), "drop function public.f(a integer)");
}

fn values(v: &[&str]) -> Vec<ArgValue> {
    v.iter().map(|s| ArgValue { value: (*s).into(), ..Default::default() }).collect()
}

#[test]
fn calls_per_engine() {
    let pg = parse(
        Driver::Postgres,
        "CREATE OR REPLACE FUNCTION app.add_tax(amount numeric, rate numeric DEFAULT 0.23, OUT total numeric, OUT tax numeric)\n RETURNS record\n LANGUAGE plpgsql\nAS $f$ begin end $f$",
    );
    assert_eq!(call_sql(Driver::Postgres, &pg, &values(&["10", "0.5"])), "select * from app.add_tax(10, 0.5);");
    let mut v = values(&["10", ""]);
    v[1].use_default = true;
    assert_eq!(call_sql(Driver::Postgres, &pg, &v), "select * from app.add_tax(10);");

    let touch = parse(Driver::Postgres, "CREATE PROCEDURE app.touch(IN p_id integer, INOUT p_note text DEFAULT 'x'::text, OUT o int)\n LANGUAGE plpgsql\nAS $$ begin end $$");
    let mut v = values(&["7", "", ""]);
    v[1].use_default = true;
    // A gap before the OUT argument: named notation.
    assert_eq!(call_sql(Driver::Postgres, &touch, &v), "call app.touch(p_id => 7, o => null);");
    v[1] = ArgValue { value: "hello".into(), ..Default::default() };
    assert_eq!(call_sql(Driver::Postgres, &touch, &v), "call app.touch(7, 'hello'::text, null);");
    v[1] = ArgValue { null: true, ..Default::default() };
    assert_eq!(call_sql(Driver::Postgres, &touch, &v), "call app.touch(7, null::text, null);");

    let my = parse(Driver::MySql, "CREATE PROCEDURE `add_order`(in p_user int, out p_id bigint, inout p_note varchar(20))\nbegin end");
    let my = RoutineSpec { schema: "shop".into(), ..my };
    assert_eq!(
        call_sql(Driver::MySql, &my, &values(&["3", "", "it's"])),
        "set @p_note = 'it''s';\ncall shop.add_order(3, @p_id, @p_note);\nselect @p_id as p_id, @p_note as p_note;"
    );

    let ms = parse(Driver::MsSql, "create procedure app.add_order @user_id int, @note nvarchar(20) = N'none', @new_id bigint output as select 1");
    let mut v = values(&["5", "", ""]);
    v[1].use_default = true;
    assert_eq!(
        call_sql(Driver::MsSql, &ms, &v),
        "declare @new_id bigint;\ndeclare @dc_return int;\nexec @dc_return = app.add_order @user_id = 5, @new_id = @new_id output;\nselect @new_id as [new_id], @dc_return as [return_value];"
    );
    let tax = parse(Driver::MsSql, "CREATE FUNCTION [app].[tax] (@amount decimal(10,2), @rate float = 0.23) RETURNS decimal(12,2) AS BEGIN RETURN 1; END");
    let mut v = values(&["100", ""]);
    v[1].use_default = true;
    assert_eq!(call_sql(Driver::MsSql, &tax, &v), "select app.tax(100, default) as [result];");

    let ora = parse(Driver::Oracle, "create procedure raise_salary (p_id in number, p_pct in number default 10, p_new out number, p_log in out nocopy varchar2) is begin null; end;");
    let ora = RoutineSpec { schema: "HR".into(), name: "RAISE_SALARY".into(), ..ora };
    let mut v = values(&["1", "", "", "start"]);
    v[1].use_default = true;
    assert_eq!(
        call_sql(Driver::Oracle, &ora, &v),
        "declare\n    v_p_new number;\n    v_p_log VARCHAR2(32767);\nbegin\n    dbms_output.enable(null);\n    v_p_log := 'start';\n    \
         HR.RAISE_SALARY(p_id => 1, p_new => v_p_new, p_log => v_p_log);\n    dbms_output.put_line('p_new = ' || v_p_new);\n    \
         dbms_output.put_line('p_log = ' || v_p_log);\nend;"
    );
    let full = parse(Driver::Oracle, "create function full_name(p_first varchar2, p_last varchar2 := 'X') return varchar2 as begin return 1; end;");
    let full = RoutineSpec { schema: "HR".into(), name: "FULL_NAME".into(), ..full };
    assert_eq!(call_sql(Driver::Oracle, &full, &values(&["Ada", "Lovelace"])), "select HR.FULL_NAME('Ada', 'Lovelace') as result from dual;");
    let touch = RoutineSpec { schema: "HR".into(), name: "TOUCH".into(), ..parse(Driver::Oracle, "create procedure touch is begin null; end;") };
    assert_eq!(call_sql(Driver::Oracle, &touch, &[]), "begin\n    HR.TOUCH;\nend;");

    let sf = parse(Driver::Snowflake, "CREATE PROCEDURE \"P\"(\"A\" NUMBER(38,0), \"B\" VARCHAR DEFAULT 'x') RETURNS VARCHAR LANGUAGE SQL AS $$ begin return 'a'; end $$");
    let sf = RoutineSpec { schema: "PUBLIC".into(), ..sf };
    let mut v = values(&["1", ""]);
    v[1].use_default = true;
    assert_eq!(call_sql(Driver::Snowflake, &sf, &v), "call PUBLIC.P(1);");
    v[1] = ArgValue { value: "y".into(), ..Default::default() };
    assert_eq!(call_sql(Driver::Snowflake, &sf, &v), "call PUBLIC.P(1, 'y'::VARCHAR);");
    v[0] = ArgValue { value: "a + 1".into(), expression: true, ..Default::default() };
    assert_eq!(call_sql(Driver::Snowflake, &sf, &v), "call PUBLIC.P(a + 1, 'y'::VARCHAR);");
}

#[test]
fn trigger_functions_are_not_callable() {
    let spec = parse(Driver::Postgres, "CREATE FUNCTION public.t() RETURNS trigger LANGUAGE plpgsql AS $$ begin return new; end $$");
    assert!(!callable(&spec));
    assert!(callable(&sample(Driver::Postgres, RoutineKind::Function)));
}


fn cells(v: &[&str]) -> Vec<crate::db::Cell> {
    v.iter().map(|s| Some((*s).to_string())).collect()
}

#[test]
fn rows_become_routines() {
    let pg = routines_from_rows(
        Driver::Postgres,
        vec![cells(&["public", "add", "F", "a integer, b integer", "a integer, b integer DEFAULT 1", "integer", "sql"])],
    );
    assert_eq!(pg[0].kind, RoutineKind::Function);
    assert_eq!(pg[0].signature, "a integer, b integer");
    assert_eq!(pg[0].display(), "add(a integer, b integer DEFAULT 1)");

    let sf = routines_from_rows(Driver::Snowflake, vec![cells(&["PUBLIC", "P", "P", "(A NUMBER, B VARCHAR)", "(A NUMBER, B VARCHAR)", "VARCHAR", "SQL"])]);
    assert_eq!((sf[0].kind, sf[0].signature.as_str(), sf[0].arguments.as_str()), (RoutineKind::Procedure, "NUMBER, VARCHAR", "A NUMBER, B VARCHAR"));

    let ora = routines_from_rows(Driver::Oracle, vec![cells(&["HR", "RAISE", "P", "", "PCT NUMBER", "", "INVALID"])]);
    assert!(!ora[0].valid && ora[0].language.is_empty());
}

#[test]
fn snowflake_signatures_keep_only_types() {
    assert_eq!(snowflake_signature("()"), "");
    assert_eq!(snowflake_signature("(X NUMBER(38,0), Y TIMESTAMP_NTZ(9))"), "NUMBER(38,0), TIMESTAMP_NTZ(9)");
}

#[test]
fn oracle_definitions_get_create_and_a_slash() {
    let r = RoutineRef { conn: 1, db: "FREEPDB1".into(), schema: "HR".into(), name: "TOUCH".into(), kind: RoutineKind::Procedure, signature: String::new() };
    let def = definition_from_text(Driver::Oracle, &r, "PROCEDURE touch IS\nBEGIN\n  NULL;\nEND;\n");
    assert_eq!(def.ddl, "create or replace PROCEDURE touch IS\nBEGIN\n  NULL;\nEND;\n/\n");
    let spec = def.spec.unwrap();
    assert_eq!((spec.schema.as_str(), spec.name.as_str()), ("HR", "TOUCH"));
    assert_eq!(spec.body, "BEGIN\n  NULL;\nEND;");
    assert!(spec.is_edit());
}
