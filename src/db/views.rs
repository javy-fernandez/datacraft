//! Views: the real definition read from the server (for "Edit View…" and "Generate DDL") and
//! the statements that create, replace or rename one.

use super::{Driver, Session};
use crate::edit::literal;
use crate::i18n::trf;
use crate::ui::quote_ident_if_needed;

/// A view's definition as the server keeps it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewDefinition {
    /// The SELECT (without `CREATE VIEW … AS` and without a trailing `;`).
    pub body: String,
    /// Column list written after the name, without parentheses ("" = none).
    pub columns: String,
    /// MySQL / MariaDB: `ALGORITHM` (`UNDEFINED`, `MERGE`, `TEMPTABLE`; "" = not given).
    pub algorithm: String,
    /// MySQL / MariaDB: `SQL SECURITY` (`DEFINER`, `INVOKER`; "" = not given).
    pub security: String,
}

/// What the "New View" / "Edit View" dialog generates statements from.
#[derive(Clone, Debug, Default)]
pub struct ViewSpec {
    pub schema: String,
    pub name: String,
    pub columns: String,
    pub body: String,
    /// New views: `OR REPLACE` (SQL Server `OR ALTER`, SQLite `DROP VIEW IF EXISTS` first).
    pub or_replace: bool,
    /// Oracle: `FORCE` (create it even if the tables don't exist yet).
    pub force: bool,
    pub algorithm: String,
    pub security: String,
    /// Editing: the view's current name (same schema).
    pub original: Option<String>,
}

/// Statements to run and whether they must run in one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewStatements {
    pub statements: Vec<String>,
    pub transaction: bool,
}

/// Label of the "replace an existing view" option of new views (SQLite drops it first).
pub fn replace_label(driver: Driver) -> &'static str {
    match driver {
        Driver::MsSql => "OR ALTER",
        Driver::Sqlite => "DROP VIEW IF EXISTS",
        _ => "OR REPLACE",
    }
}

fn qualified(driver: Driver, schema: &str, name: &str) -> String {
    let q = |s: &str| quote_ident_if_needed(driver, s.trim());
    let schema = schema.trim();
    if schema.is_empty() || (driver == Driver::Sqlite && schema == "main") {
        q(name)
    } else {
        format!("{}.{}", q(schema), q(name))
    }
}

/// The SELECT without surrounding blanks or trailing semicolons.
pub fn clean_body(body: &str) -> &str {
    body.trim().trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

/// `create [or replace …] view <name> [(cols)] as <body>`.
pub fn create_view_sql(driver: Driver, spec: &ViewSpec, name: &str, replace: bool) -> String {
    let mut head = String::from("create");
    if replace {
        match driver {
            Driver::MsSql => head.push_str(" or alter"),
            Driver::Sqlite => {}
            _ => head.push_str(" or replace"),
        }
    }
    if matches!(driver, Driver::MySql | Driver::MariaDb) {
        if !spec.algorithm.trim().is_empty() {
            head.push_str(&format!(" algorithm = {}", spec.algorithm.trim().to_lowercase()));
        }
        if !spec.security.trim().is_empty() {
            head.push_str(&format!(" sql security {}", spec.security.trim().to_lowercase()));
        }
    }
    if driver == Driver::Oracle && spec.force {
        head.push_str(" force");
    }
    let columns = spec.columns.trim();
    let columns = if columns.is_empty() { String::new() } else { format!(" ({columns})") };
    format!("{head} view {}{columns} as\n{}", qualified(driver, &spec.schema, name), clean_body(&spec.body))
}

/// Statements that create the view or, when editing, replace (and maybe rename) it.
pub fn view_statements(driver: Driver, spec: &ViewSpec) -> ViewStatements {
    let name = spec.name.trim();
    let drop = |n: &str, if_exists: bool| {
        format!("drop view {}{}", if if_exists { "if exists " } else { "" }, qualified(driver, &spec.schema, n))
    };
    let (statements, transaction) = match spec.original.as_deref().map(str::trim) {
        // New view.
        None if driver == Driver::Sqlite && spec.or_replace => {
            (vec![drop(name, true), create_view_sql(driver, spec, name, false)], true)
        }
        None => (vec![create_view_sql(driver, spec, name, spec.or_replace)], false),
        // SQLite has no OR REPLACE: drop and create in one transaction.
        Some(old) if driver == Driver::Sqlite => (vec![drop(old, false), create_view_sql(driver, spec, name, false)], true),
        Some(old) if old == name => (vec![create_view_sql(driver, spec, name, true)], false),
        // PostgreSQL: replace and rename in one transaction (keeps grants and dependent views).
        Some(old) if driver == Driver::Postgres => (
            vec![
                create_view_sql(driver, spec, old, true),
                format!("alter view {} rename to {}", qualified(driver, &spec.schema, old), quote_ident_if_needed(driver, name)),
            ],
            true,
        ),
        // The rest: create the new one first, so a failure leaves the old view untouched.
        Some(old) => (vec![create_view_sql(driver, spec, name, false), drop(old, false)], false),
    };
    ViewStatements { statements, transaction }
}

/// Lexical token of a `CREATE VIEW` statement.
#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    /// Quoted identifier or string.
    Quoted,
    Open,
    Close,
    Other,
}

/// Tokens with their byte ranges; comments and blanks are skipped.
fn tokens(sql: &str) -> Vec<(Tok<'_>, usize, usize)> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let close = match c {
            b'\'' => Some(b'\''),
            b'"' => Some(b'"'),
            b'`' => Some(b'`'),
            b'[' => Some(b']'),
            _ => None,
        };
        if let Some(close) = close {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == close {
                    // Doubled quote = escaped quote.
                    if bytes.get(i + 1) == Some(&close) && close != b']' {
                        i += 2;
                        continue;
                    }
                    if close == b']' && bytes.get(i + 1) == Some(&b']') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            out.push((Tok::Quoted, start, i));
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'@' || c >= 0x80 {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$' | b'@' | b'#') || bytes[i] >= 0x80) {
                i += 1;
            }
            out.push((Tok::Word(&sql[start..i]), start, i));
            continue;
        }
        i += 1;
        let tok = match c {
            b'(' => Tok::Open,
            b')' => Tok::Close,
            _ => Tok::Other,
        };
        out.push((tok, start, i));
    }
    out
}

/// Splits a full `CREATE … VIEW name [(cols)] … AS body` statement (SQLite's `sqlite_master.sql`,
/// MySQL's `SHOW CREATE VIEW`, SQL Server's `OBJECT_DEFINITION`, Snowflake's definitions).
/// `None` if it isn't one.
pub fn parse_create_view(sql: &str) -> Option<ViewDefinition> {
    let toks = tokens(sql);
    let is = |t: &Tok, w: &str| matches!(t, Tok::Word(x) if x.eq_ignore_ascii_case(w));
    let create = toks.iter().position(|(t, ..)| is(t, "create"))?;
    let view = create + toks[create..].iter().position(|(t, ..)| is(t, "view"))?;
    let mut def = ViewDefinition::default();
    // MySQL options before VIEW: ALGORITHM=…, DEFINER=…, SQL SECURITY ….
    let header = &toks[create..view];
    for (i, (t, ..)) in header.iter().enumerate() {
        if is(t, "algorithm")
            && let Some((Tok::Word(v), ..)) = header.get(i + 2)
        {
            def.algorithm = v.to_uppercase();
        }
        if is(t, "security")
            && i > 0
            && is(&header[i - 1].0, "sql")
            && let Some((Tok::Word(v), ..)) = header.get(i + 1)
        {
            def.security = v.to_uppercase();
        }
    }
    let mut depth = 0usize;
    let mut columns_open = None;
    for (t, start, end) in &toks[view + 1..] {
        match t {
            Tok::Open => {
                if depth == 0 && columns_open.is_none() && def.columns.is_empty() {
                    columns_open = Some(*end);
                }
                depth += 1;
            }
            Tok::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(open) = columns_open.take()
                {
                    def.columns = sql[open..*start].split_whitespace().collect::<Vec<_>>().join(" ");
                }
            }
            Tok::Word(w) if depth == 0 && w.eq_ignore_ascii_case("as") => {
                def.body = clean_body(&sql[*end..]).to_string();
                return Some(def);
            }
            _ => {}
        }
    }
    None
}

/// Query that reads the definition of `schema.view` (`db` for the engines that need it).
fn definition_query(driver: Driver, db: &str, schema: &str, view: &str) -> String {
    let lit = |s: &str| literal(driver, &Some(s.to_string()));
    match driver {
        // Plain views only: saving a materialized view from the dialog would turn it into a view.
        Driver::Postgres => format!(
            "SELECT pg_catalog.pg_get_viewdef(c.oid, true) FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {} AND c.relname = {} AND c.relkind = 'v'",
            lit(schema),
            lit(view)
        ),
        Driver::MySql | Driver::MariaDb => format!("SHOW CREATE VIEW {}", Session::quote_table(driver, schema, view)),
        // The CREATE VIEW statement (same shape MySQL's SHOW returns).
        Driver::ClickHouse => format!(
            "SELECT create_table_query FROM system.tables WHERE database = {} AND name = {} AND engine = 'View'",
            lit(schema),
            lit(view)
        ),
        Driver::MsSql => format!("SELECT OBJECT_DEFINITION(OBJECT_ID({}))", lit(&Session::quote_table(driver, schema, view))),
        // TEXT is a LONG: fetched as text, one row.
        Driver::Oracle => format!("SELECT text FROM all_views WHERE owner = {} AND view_name = {}", lit(schema), lit(view)),
        Driver::Sqlite => format!(
            "SELECT sql FROM {}.sqlite_master WHERE type = 'view' AND name = {}",
            driver.quote_ident(if schema.is_empty() { "main" } else { schema }),
            lit(view)
        ),
        Driver::Snowflake => format!(
            "SELECT VIEW_DEFINITION FROM {}.INFORMATION_SCHEMA.VIEWS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
            driver.quote_ident(db),
            lit(schema),
            lit(view)
        ),
        Driver::Redis | Driver::MongoDb => String::new(),
    }
}

/// The definition text returned by `definition_query` turned into a `ViewDefinition`.
pub fn parse_definition(driver: Driver, text: &str) -> ViewDefinition {
    match driver {
        // Only the SELECT.
        Driver::Postgres | Driver::Oracle => ViewDefinition { body: clean_body(text).to_string(), ..Default::default() },
        _ => parse_create_view(text).unwrap_or_else(|| ViewDefinition { body: clean_body(text).to_string(), ..Default::default() }),
    }
}

impl Session {
    /// The definition of `schema.view` as the server keeps it. The session must be on `db`.
    pub async fn view_definition(&mut self, driver: Driver, db: &str, schema: &str, view: &str) -> anyhow::Result<ViewDefinition> {
        if driver.is_nosql() {
            anyhow::bail!(crate::i18n::tr("Not available for this engine"));
        }
        let first_text = |rs: Vec<super::ResultSet>, column: usize| -> Option<String> {
            rs.into_iter().next()?.rows.into_iter().next()?.into_iter().nth(column)?.filter(|s| !s.trim().is_empty())
        };
        // SHOW CREATE VIEW: View, Create View, character_set_client, collation_connection.
        let column = if matches!(driver, Driver::MySql | Driver::MariaDb) { 1 } else { 0 };
        let mut text = first_text(self.execute(&definition_query(driver, db, schema, view), 1).await?, column);
        // Snowflake hides the text of secure views from non-owners in INFORMATION_SCHEMA.
        if text.is_none() && driver == Driver::Snowflake {
            let name = format!("{}.{}", driver.quote_ident(db), Session::quote_table(driver, schema, view));
            let sql = format!("SELECT GET_DDL('VIEW', {})", literal(driver, &Some(name)));
            text = first_text(self.execute(&sql, 1).await?, 0);
        }
        let Some(text) = text else {
            anyhow::bail!(trf("The definition of view “{name}” is not available", &[("name", &view)]));
        };
        Ok(parse_definition(driver, &text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, original: Option<&str>) -> ViewSpec {
        ViewSpec {
            schema: "app".into(),
            name: name.into(),
            body: "  select id, name from users where active;  \n".into(),
            original: original.map(str::to_string),
            ..Default::default()
        }
    }

    fn run(driver: Driver, s: &ViewSpec) -> (Vec<String>, bool) {
        let v = view_statements(driver, s);
        (v.statements, v.transaction)
    }

    #[test]
    fn new_view_per_engine() {
        let body = "select id, name from users where active";
        let mut s = spec("active_users", None);
        assert_eq!(run(Driver::Postgres, &s), (vec![format!("create view app.active_users as\n{body}")], false));
        s.or_replace = true;
        assert_eq!(run(Driver::Postgres, &s).0, vec![format!("create or replace view app.active_users as\n{body}")]);
        assert_eq!(run(Driver::MsSql, &s).0, vec![format!("create or alter view app.active_users as\n{body}")]);
        // Snowflake and Oracle fold unquoted names to uppercase: lowercase ones are quoted.
        assert_eq!(run(Driver::Snowflake, &s).0, vec![format!("create or replace view \"app\".\"active_users\" as\n{body}")]);
        s.force = true;
        assert_eq!(run(Driver::Oracle, &s).0, vec![format!("create or replace force view \"app\".\"active_users\" as\n{body}")]);
        s.algorithm = "MERGE".into();
        s.security = "INVOKER".into();
        assert_eq!(
            run(Driver::MariaDb, &s).0,
            vec![format!("create or replace algorithm = merge sql security invoker view app.active_users as\n{body}")]
        );
        // SQLite: no OR REPLACE, so drop it first in the same transaction.
        s.schema = "main".into();
        assert_eq!(
            run(Driver::Sqlite, &s),
            (vec!["drop view if exists active_users".into(), format!("create view active_users as\n{body}")], true)
        );
        s.or_replace = false;
        assert_eq!(run(Driver::Sqlite, &s), (vec![format!("create view active_users as\n{body}")], false));
    }

    #[test]
    fn quotes_names_and_keeps_the_column_list() {
        let mut s = spec("Active Users", None);
        s.columns = "a, b".into();
        assert!(create_view_sql(Driver::MsSql, &s, "Active Users", false).starts_with("create view app.[Active Users] (a, b) as\n"));
        assert!(create_view_sql(Driver::Postgres, &s, "Active Users", false).starts_with("create view app.\"Active Users\" (a, b) as\n"));
    }

    #[test]
    fn editing_replaces_and_renames() {
        let same = spec("v", Some("v"));
        assert_eq!(run(Driver::Postgres, &same).0, vec!["create or replace view app.v as\nselect id, name from users where active"]);
        assert!(run(Driver::MsSql, &same).0[0].starts_with("create or alter view app.v as"));
        assert!(run(Driver::MySql, &same).0[0].starts_with("create or replace view app.v as"));
        assert!(run(Driver::Oracle, &same).0[0].starts_with("create or replace view \"app\".\"v\" as"));
        let sqlite = run(Driver::Sqlite, &same);
        assert!(sqlite.1 && sqlite.0[0] == "drop view app.v" && sqlite.0[1].starts_with("create view app.v as"));

        let renamed = spec("w", Some("v"));
        let pg = run(Driver::Postgres, &renamed);
        assert!(pg.1);
        assert!(pg.0[0].starts_with("create or replace view app.v as"));
        assert_eq!(pg.0[1], "alter view app.v rename to w");
        let my = run(Driver::MariaDb, &renamed);
        assert!(!my.1);
        assert!(my.0[0].starts_with("create view app.w as"));
        assert_eq!(my.0[1], "drop view app.v");
        let lite = run(Driver::Sqlite, &renamed);
        assert_eq!(lite.0[0], "drop view app.v");
        assert!(lite.0[1].starts_with("create view app.w as"));
    }

    #[test]
    fn parses_sqlite_definitions() {
        let d = parse_create_view("CREATE VIEW \"big orders\"(id, total) AS\n  SELECT id, total FROM orders WHERE total > 100;").unwrap();
        assert_eq!(d.columns, "id, total");
        assert_eq!(d.body, "SELECT id, total FROM orders WHERE total > 100");
        let d = parse_create_view("create view if not exists v as select (1) as one").unwrap();
        assert_eq!((d.columns.as_str(), d.body.as_str()), ("", "select (1) as one"));
    }

    #[test]
    fn parses_show_create_view() {
        let text = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY INVOKER VIEW `shop`.`v as` AS select `t`.`a` AS `a` from `t`";
        let d = parse_definition(Driver::MariaDb, text);
        assert_eq!(d.algorithm, "UNDEFINED");
        assert_eq!(d.security, "INVOKER");
        assert_eq!(d.columns, "");
        assert_eq!(d.body, "select `t`.`a` AS `a` from `t`");
    }

    #[test]
    fn parses_sql_server_and_snowflake_definitions() {
        let text = "-- active users\n/* AS */ CREATE   VIEW [dbo].[Active AS Users]\nWITH SCHEMABINDING\nAS\nSELECT id FROM dbo.users WHERE note = 'as'\nWITH CHECK OPTION";
        let d = parse_definition(Driver::MsSql, text);
        assert_eq!(d.body, "SELECT id FROM dbo.users WHERE note = 'as'\nWITH CHECK OPTION");
        let d = parse_definition(Driver::Snowflake, "create or replace view DB.S.V(\n\tID,\n\tNAME\n) as select id, name from t;");
        assert_eq!(d.columns, "ID, NAME");
        assert_eq!(d.body, "select id, name from t");
        // Engines that only return the SELECT.
        assert_eq!(parse_definition(Driver::Postgres, " SELECT users.id\n   FROM users;").body, "SELECT users.id\n   FROM users");
        assert_eq!(parse_definition(Driver::Oracle, "select 1 from dual ").body, "select 1 from dual");
    }

    #[test]
    fn definition_queries_escape_names() {
        assert!(definition_query(Driver::Postgres, "db", "public", "o'v").contains("c.relname = 'o''v'"));
        assert_eq!(definition_query(Driver::MySql, "shop", "shop", "v"), "SHOW CREATE VIEW `shop`.`v`");
        assert_eq!(definition_query(Driver::MsSql, "db", "dbo", "v"), "SELECT OBJECT_DEFINITION(OBJECT_ID(N'[dbo].[v]'))");
        assert!(definition_query(Driver::Sqlite, "main", "main", "v").starts_with("SELECT sql FROM \"main\".sqlite_master"));
    }

    mod live {
        use super::super::*;
        use crate::db::{ConnectionConfig, ObjectKind};
        use crate::worker::{ConnId, Request, Response, Worker};
        use std::time::{Duration, Instant};

        fn wait_for(w: &Worker, pred: impl Fn(&Response) -> bool) -> Response {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                for r in w.poll() {
                    if pred(&r) {
                        return r;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("timed out waiting for the worker");
        }

        fn run(w: &Worker, conn: ConnId, db: &str, driver: Driver, spec: &ViewSpec) {
            let v = view_statements(driver, spec);
            w.send(Request::RunDdl { conn, db: Some(db.into()), statements: v.statements, close_db: None, transaction: v.transaction });
            let Response::DdlDone { result, .. } = wait_for(w, |r| matches!(r, Response::DdlDone { .. })) else { unreachable!() };
            result.unwrap();
        }

        fn definition(w: &Worker, conn: ConnId, db: &str, schema: &str, view: &str, request_id: u64) -> Result<ViewDefinition, String> {
            w.send(Request::ViewDefinition { conn, db: db.into(), schema: schema.into(), view: view.into(), request_id });
            let r = wait_for(w, |r| matches!(r, Response::ViewDefinition { request_id: id, .. } if *id == request_id));
            let Response::ViewDefinition { result, .. } = r else { unreachable!() };
            result
        }

        fn views_in(w: &Worker, conn: ConnId, db: &str) -> Vec<String> {
            w.send(Request::LoadDatabase { conn, db: db.into() });
            let r = wait_for(w, |r| matches!(r, Response::DatabaseLoaded { .. }));
            let Response::DatabaseLoaded { result, .. } = r else { unreachable!() };
            let mut names: Vec<String> =
                result.unwrap().into_iter().flat_map(|s| s.tables).filter(|t| t.kind == ObjectKind::View && t.name.starts_with("dc_")).map(|t| t.name).collect();
            names.sort();
            names
        }

        /// Creates a view, reads its definition, replaces it and renames it.
        fn create_load_replace(cfg: ConnectionConfig, db: &str, schema: &str) {
            let (conn, driver) = (cfg.id, cfg.driver);
            let w = Worker::start(egui::Context::default());
            w.send(Request::Connect(cfg));
            wait_for(&w, |r| matches!(r, Response::Connected { .. }));
            let mut spec = ViewSpec {
                schema: schema.into(),
                name: "dc_big_orders".into(),
                body: "select id, total from dc_orders where total > 10;".into(),
                or_replace: true,
                ..Default::default()
            };
            run(&w, conn, db, driver, &spec);
            let def = definition(&w, conn, db, schema, "dc_big_orders", 1).unwrap();
            assert!(def.body.to_lowercase().contains("dc_orders") && def.body.contains("10"), "{def:?}");
            assert!(!def.body.trim_end().ends_with(';'));

            // Edit: new body, same name.
            spec.original = Some(spec.name.clone());
            spec.body = "select id from dc_orders where total > 50".into();
            run(&w, conn, db, driver, &spec);
            assert!(definition(&w, conn, db, schema, "dc_big_orders", 2).unwrap().body.contains("50"));

            // Edit and rename.
            spec.name = "dc_huge_orders".into();
            spec.body = "select id from dc_orders where total > 99".into();
            run(&w, conn, db, driver, &spec);
            assert_eq!(views_in(&w, conn, db), vec!["dc_huge_orders".to_string()]);
            assert!(definition(&w, conn, db, schema, "dc_huge_orders", 3).unwrap().body.contains("99"));
            assert!(definition(&w, conn, db, schema, "dc_big_orders", 4).is_err());

            spec.original = None;
            spec.name = "dc_huge_orders".into();
            w.send(Request::RunDdl { conn, db: Some(db.into()), statements: vec!["drop view dc_huge_orders".into()], close_db: None, transaction: false });
            wait_for(&w, |r| matches!(r, Response::DdlDone { .. }));
            w.send(Request::Disconnect(conn));
            wait_for(&w, |r| matches!(r, Response::Disconnected(_)));
        }

        #[test]
        fn sqlite_views_are_created_loaded_and_replaced() {
            let path = std::env::temp_dir().join(format!("datacraft-views-{}.db", uuid::Uuid::new_v4()));
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch("create table dc_orders(id integer primary key, total real); insert into dc_orders values (1, 5), (2, 100);")
                .unwrap();
            let mut cfg = ConnectionConfig::new(41, Driver::Sqlite);
            cfg.file_path = path.display().to_string();
            create_load_replace(cfg, "main", "main");
            let _ = std::fs::remove_file(path);
        }

        /// Live: `DATACRAFT_MY_HOST/PORT/USER/PASSWORD/DB` (see `db/mysql.rs`).
        #[test]
        #[ignore]
        fn mariadb_views_are_created_loaded_and_replaced() {
            let var = |k: &str| std::env::var(k).unwrap_or_default();
            let mut cfg = ConnectionConfig::new(42, Driver::MariaDb);
            cfg.host = var("DATACRAFT_MY_HOST");
            cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
            cfg.user = var("DATACRAFT_MY_USER");
            cfg.password = var("DATACRAFT_MY_PASSWORD");
            cfg.database = var("DATACRAFT_MY_DB");
            let db = cfg.database.clone();
            {
                let w = Worker::start(egui::Context::default());
                w.send(Request::Connect(cfg.clone()));
                wait_for(&w, |r| matches!(r, Response::Connected { .. }));
                let setup = [
                    "drop view if exists dc_big_orders",
                    "drop view if exists dc_huge_orders",
                    "drop table if exists dc_orders",
                    "create table dc_orders(id int primary key, total decimal(10,2))",
                    "insert into dc_orders values (1, 5), (2, 100)",
                ];
                w.send(Request::RunDdl { conn: 42, db: Some(db.clone()), statements: setup.map(String::from).to_vec(), close_db: None, transaction: false });
                let Response::DdlDone { result, .. } = wait_for(&w, |r| matches!(r, Response::DdlDone { .. })) else { unreachable!() };
                result.unwrap();
            }
            create_load_replace(cfg, &db, &db);
        }
    }
}
