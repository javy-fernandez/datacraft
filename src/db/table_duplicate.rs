//! Dedicated-session table duplication with faithful catalog definitions and SQL previews.

use super::{
    ConnectionConfig, Driver, Session,
    alter::Plan,
    table_definition::{self, TableDefinition, catalog, data_insert_options, dump_ddl, lit, optional, qualified, query, tokens, validate_names},
};
use crate::i18n::{tr, trf};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub struct Progress {
    pub cancel: AtomicBool,
}

pub enum Operation {
    Definition { schema: String, table: String },
    PreviewDuplicate { schema: String, table: String, target: String, data: bool },
    Duplicate { schema: String, table: String, target: String, data: bool, expected: Vec<String> },
}

pub enum Outcome {
    Definition(TableDefinition),
    Plan(Plan),
    Done,
}

pub async fn duplicate_plan(session: &mut Session, driver: Driver, schema: &str, source: &str, target: &str, data: bool) -> anyhow::Result<Plan> {
    validate_names(schema, source)?;
    validate_names(schema, target)?;
    anyhow::ensure!(source != target, tr("The duplicate needs a different table name"));
    let exists = match driver {
        Driver::Sqlite => format!("SELECT name FROM {}.sqlite_schema WHERE name = {} COLLATE NOCASE", driver.quote_ident(schema), lit(driver, target)),
        Driver::MsSql => format!("SELECT name FROM sys.objects WHERE schema_id = SCHEMA_ID({}) AND name = {}", lit(driver, schema), lit(driver, target)),
        Driver::Postgres => format!(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={} AND c.relname={}",
            lit(driver, schema),
            lit(driver, target)
        ),
        Driver::MySql | Driver::MariaDb | Driver::Snowflake => {
            format!("SELECT table_name FROM information_schema.tables WHERE table_schema={} AND table_name={}", lit(driver, schema), lit(driver, target))
        }
        _ => anyhow::bail!(trf("Faithful table duplication is not available for {engine}", &[("engine", &driver.label())])),
    };
    anyhow::ensure!(query(session, &exists).await?.rows.is_empty(), tr("The destination table or object already exists"));
    let def = catalog(session, driver, schema, source, target).await?;
    let from = qualified(driver, schema, source);
    let to = qualified(driver, schema, target);
    let transactional = matches!(driver, Driver::Sqlite | Driver::Postgres | Driver::MsSql);
    let mut plan = Plan { statements: def.ddl, transaction: transactional, warnings: Vec::new() };
    let mut constraints = Vec::new();
    if data && driver != Driver::Snowflake {
        let (before, after) = dump_ddl(driver, schema, target, &plan.statements)?;
        plan.statements = before;
        constraints = after;
    }
    if driver == Driver::Snowflake {
        plan.statements = vec![format!("CREATE TABLE {to} {} {from}", if data { "CLONE" } else { "LIKE" })];
    } else if data {
        anyhow::ensure!(!def.columns.is_empty(), tr("The table has no insertable columns"));
        let options = data_insert_options(session, driver, schema, source).await?;
        let mut columns = def.columns;
        if driver == Driver::Sqlite {
            let tables = query(
                session,
                &format!("SELECT wr FROM pragma_table_list WHERE schema={} AND name={} COLLATE NOCASE", lit(driver, schema), lit(driver, source)),
            )
            .await?;
            if tables.rows.first().and_then(|r| optional(r, 0)) == Some("0") {
                let fields = query(session, &format!("SELECT name FROM pragma_table_xinfo({}, {})", lit(driver, source), lit(driver, schema))).await?;
                if let Some(alias) = ["rowid", "_rowid_", "oid"]
                    .into_iter()
                    .find(|alias| !fields.rows.iter().any(|r| optional(r, 0).is_some_and(|n| n.eq_ignore_ascii_case(alias))))
                {
                    columns.push(alias.to_string());
                }
            }
        }
        let cols = columns.iter().map(|c| driver.quote_ident(c)).collect::<Vec<_>>().join(", ");
        let overriding = if options.overriding_system_value { " OVERRIDING SYSTEM VALUE" } else { "" };
        // Metadata options reference the source for dumps; duplication targets its new table.
        let before = options.before.into_iter().map(|sql| retarget_option(&sql, driver, schema, source, target)).collect::<Vec<_>>();
        let after = options.after.into_iter().map(|sql| retarget_option(&sql, driver, schema, source, target)).collect::<Vec<_>>();
        if driver == Driver::MsSql && !before.is_empty() {
            // IDENTITY_INSERT is session state, so restore it even when the INSERT raises an error.
            plan.statements.push(format!("BEGIN TRY\n{};\nINSERT INTO {to} ({cols}) SELECT {cols} FROM {from};\n{};\nEND TRY\nBEGIN CATCH\nSET IDENTITY_INSERT {to} OFF;\nTHROW;\nEND CATCH",before.join(";\n"),after.join(";\n")));
        } else {
            plan.statements.extend(before);
            plan.statements.push(format!("INSERT INTO {to} ({cols}){overriding} SELECT {cols} FROM {from}"));
            plan.statements.extend(after);
        }
        if driver == Driver::Sqlite && plan.statements.first().is_some_and(|sql| tokens(sql, driver).is_ok_and(|ts| ts.iter().any(|t| t.is("AUTOINCREMENT")))) {
            let sequence = format!("{}.sqlite_sequence", driver.quote_ident(schema));
            plan.statements.push(format!("DELETE FROM {sequence} WHERE name={}", lit(driver, target)));
            plan.statements.push(format!(
                "INSERT INTO {sequence}(name,seq) SELECT {},seq FROM {sequence} WHERE name={}",
                lit(driver, target),
                lit(driver, source)
            ));
        }
    }
    plan.statements.extend(constraints);
    if !transactional {
        plan.warnings.push(tr("This engine commits table creation immediately. If a later statement fails, the newly created table must be removed.").into());
    }
    Ok(plan)
}

fn retarget_option(sql: &str, driver: Driver, schema: &str, source: &str, target: &str) -> String {
    let from = qualified(driver, schema, source);
    let to = qualified(driver, schema, target);
    // Only generated option statements pass through here, never catalog/user SQL.
    sql.replace(&lit(driver, &from), &lit(driver, &to)).replace(&from, &to)
}

async fn open(mut cfg: ConnectionConfig, db: &str) -> anyhow::Result<Session> {
    if !matches!(cfg.driver, Driver::Sqlite | Driver::Oracle) {
        cfg.database = db.into();
    }
    let mut session = tokio::time::timeout(Duration::from_secs(30), Session::connect(&cfg)).await??;
    tokio::time::timeout(Duration::from_secs(30), session.use_database(db)).await??;
    Ok(session)
}

async fn wait_cancel(progress: &Progress) {
    while !progress.cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn execute(session: &mut Session, statement: &str, progress: &Progress) -> anyhow::Result<()> {
    let handle = session.cancel_handle();
    cancellable(session.execute(statement, 0), handle, progress, Duration::from_secs(600)).await.map(|_| ())
}

async fn cancellable<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    handle: Option<super::CancelHandle>,
    progress: &Progress,
    timeout: Duration,
) -> anyhow::Result<T> {
    anyhow::ensure!(!progress.cancel.load(Ordering::Relaxed), tr("Cancelled"));
    tokio::pin!(future);
    let (result, abandoned) = tokio::select! {
        result = &mut future => (result, false),
        _ = tokio::time::sleep(timeout) => (Err(anyhow::anyhow!(tr("Table operation timed out"))), true),
        _ = wait_cancel(progress) => (Err(anyhow::anyhow!(tr("Cancelled"))), true),
    };
    if abandoned && let Some(handle) = handle {
        let _ = tokio::time::timeout(Duration::from_secs(3), handle.cancel()).await;
        // SQLite's blocking worker owns the connection until the interrupted query returns.
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut future).await;
    }
    result
}

async fn metadata<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    handle: Option<super::CancelHandle>,
    progress: &Progress,
) -> anyhow::Result<T> {
    cancellable(future, handle, progress, Duration::from_secs(30)).await
}

pub async fn run(cfg: ConnectionConfig, db: &str, operation: Operation, progress: Arc<Progress>) -> anyhow::Result<Outcome> {
    anyhow::ensure!(table_definition::supported(cfg.driver), tr("Not available for this engine"));
    anyhow::ensure!(!progress.cancel.load(Ordering::Relaxed), tr("Cancelled"));
    let driver = cfg.driver;
    let mut session = metadata(open(cfg.clone(), db), None, &progress).await?;
    macro_rules! read {
        ($future:expr) => {{
            let handle = session.cancel_handle();
            metadata($future, handle, &progress).await?
        }};
    }
    match operation {
        Operation::Definition { schema, table } => Ok(Outcome::Definition(read!(super::table_definition::definition(&mut session, driver, &schema, &table)))),
        Operation::PreviewDuplicate { schema, table, target, data } => {
            anyhow::ensure!(!cfg.read_only, tr("Read-only connection"));
            Ok(Outcome::Plan(read!(duplicate_plan(&mut session, driver, &schema, &table, &target, data))))
        }
        Operation::Duplicate { schema, table, target, data, expected } => {
            anyhow::ensure!(!cfg.read_only, tr("Read-only connection"));
            let plan = read!(duplicate_plan(&mut session, driver, &schema, &table, &target, data));
            anyhow::ensure!(plan.statements == expected, tr("The table definition changed. Preview the operation again."));
            let begin = if driver == Driver::MsSql { "BEGIN TRANSACTION" } else { "BEGIN" };
            if plan.transaction {
                execute(&mut session, begin, &progress).await?;
            }
            let mut created = false;
            let mut result = Ok(());
            for statement in &plan.statements {
                if let Err(error) = execute(&mut session, statement, &progress).await {
                    result = Err(error);
                    break;
                }
                if statement.trim_start().to_ascii_lowercase().starts_with("create table ") {
                    created = true;
                }
            }
            if plan.transaction {
                if result.is_ok() {
                    result = execute(&mut session, "COMMIT", &progress).await;
                }
                if result.is_err() {
                    let _ = tokio::time::timeout(Duration::from_secs(10), session.execute("ROLLBACK", 0)).await;
                }
            } else if created && result.is_err() {
                // Only a successfully observed CREATE authorizes cleanup of this new target.
                let drop_sql = format!("DROP TABLE {}", Session::quote_table(driver, &schema, &target));
                let cleanup = match open(cfg, db).await {
                    Ok(mut cleanup) => tokio::time::timeout(Duration::from_secs(15), cleanup.execute(&drop_sql, 0))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r.map(|_| ())),
                    Err(error) => Err(error),
                };
                if let Err(error) = cleanup {
                    return Err(anyhow::anyhow!(
                        "{}\n{}: {error:#}",
                        result.err().map(|e| format!("{e:#}")).unwrap_or_default(),
                        tr("Could not remove the incomplete duplicate")
                    ));
                }
            }
            result?;
            Ok(Outcome::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ConnectionConfig;
    use crate::db::table_definition::definition;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        async fn open(name: &str) -> (Self, Session) {
            let path = std::env::temp_dir().join(format!("datacraft_definition_{}_{name}.db", std::process::id()));
            let _ = std::fs::remove_file(&path);
            std::fs::File::create(&path).unwrap();
            let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            let session = Session::connect(&cfg).await.unwrap();
            (Self(path), session)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn scalar(s: &mut Session, sql: &str) -> Option<String> {
        s.execute(sql, 10).await.unwrap()[0].rows[0][0].clone()
    }

    #[tokio::test]
    async fn sqlite_duplicate_preserves_schema_binary_defaults_and_identity() {
        let (_fixture, mut s) = Fixture::open("faithful").await;
        s.execute(
            "PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); INSERT INTO parent VALUES(1); \
            CREATE TABLE \"source\" (id INTEGER PRIMARY KEY AUTOINCREMENT, parent_id INTEGER REFERENCES parent(id), \
            label TEXT NOT NULL DEFAULT 'source', payload BLOB, derived TEXT GENERATED ALWAYS AS (label || '_source') STORED, \
            CONSTRAINT positive CHECK(id > 0), UNIQUE(label)); \
            CREATE INDEX \"source_idx\" ON source(lower(label)) WHERE label <> 'source'; \
            INSERT INTO source(id,parent_id,label,payload) VALUES(1,1,'first',zeroblob(512)); \
            INSERT INTO source(id,label) VALUES(900,'deleted'); DELETE FROM source WHERE id=900",
            0,
        )
        .await
        .unwrap();
        let def = definition(&mut s, Driver::Sqlite, "main", "source").await.unwrap();
        assert_eq!(def.columns, ["id", "parent_id", "label", "payload"]);
        assert_eq!(def.ddl.len(), 2);
        let plan = duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "copy\"source", true).await.unwrap();
        assert!(plan.transaction);
        s.run_transaction(&plan.statements).await.unwrap();
        assert_eq!(
            scalar(&mut s, "SELECT hex(payload)=hex((SELECT payload FROM source)) AND length(payload)=512 FROM \"copy\"\"source\"").await.as_deref(),
            Some("1")
        );
        assert_eq!(scalar(&mut s, "SELECT derived FROM \"copy\"\"source\"").await.as_deref(), Some("first_source"));
        s.execute("INSERT INTO \"copy\"\"source\"(parent_id,payload) VALUES(1,X'01')", 0).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT id FROM \"copy\"\"source\" WHERE label='source'").await.as_deref(), Some("901"));
        assert!(s.execute("INSERT INTO \"copy\"\"source\"(parent_id,label) VALUES(999,'bad')", 0).await.is_err());
        assert!(s.execute("INSERT INTO \"copy\"\"source\"(label) VALUES('first')", 0).await.is_err());
        assert_eq!(scalar(&mut s, "SELECT count(*) FROM sqlite_schema WHERE type='index' AND tbl_name='copy\"source'").await.as_deref(), Some("2"));
    }
    #[tokio::test]
    async fn sqlite_structure_only_self_reference_and_definition_restore() {
        let (_fixture, mut s) = Fixture::open("self").await;
        s.execute(
            "CREATE TABLE \"source source\" (id INTEGER PRIMARY KEY, parent INT REFERENCES \"source source\"(id), \
            \"source source\" TEXT DEFAULT 'source source'); INSERT INTO \"source source\"(id) VALUES(1); \
            CREATE INDEX idx ON \"source source\"(\"source source\")",
            0,
        )
        .await
        .unwrap();
        let def = definition(&mut s, Driver::Sqlite, "main", "source source").await.unwrap();
        let plan = duplicate_plan(&mut s, Driver::Sqlite, "main", "source source", "target", false).await.unwrap();
        s.run_transaction(&plan.statements).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT count(*) FROM target").await.as_deref(), Some("0"));
        assert_eq!(scalar(&mut s, "SELECT \"table\" FROM pragma_foreign_key_list('target')").await.as_deref(), Some("target"));
        assert_eq!(scalar(&mut s, "SELECT dflt_value FROM pragma_table_info('target') WHERE name='source source'").await.as_deref(), Some("'source source'"));
        s.execute("DROP TABLE \"source source\"", 0).await.unwrap();
        s.run_transaction(&def.ddl).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT count(*) FROM pragma_index_list('source source')").await.as_deref(), Some("1"));
    }
    #[tokio::test]
    async fn sqlite_conflicts_and_failed_plans_do_not_damage_existing_tables() {
        let (_fixture, mut s) = Fixture::open("rollback").await;
        s.execute("CREATE TABLE source(id INTEGER PRIMARY KEY); INSERT INTO source VALUES(42); CREATE TABLE existing(id INT)", 0).await.unwrap();
        assert!(duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "source", true).await.is_err());
        assert!(duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "existing", true).await.is_err());
        let mut plan = duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "target", true).await.unwrap();
        plan.statements.push("INSERT INTO target VALUES(42)".into());
        assert!(s.run_transaction(&plan.statements).await.is_err());
        assert_eq!(scalar(&mut s, "SELECT count(*) FROM sqlite_schema WHERE name='target'").await.as_deref(), Some("0"));
        assert_eq!(scalar(&mut s, "SELECT id FROM source").await.as_deref(), Some("42"));
    }
    #[tokio::test]
    async fn sqlite_preserves_observable_rowids_and_attached_schema_isolation() {
        let (_fixture, mut s) = Fixture::open("rowids").await;
        s.execute("ATTACH ':memory:' AS other; CREATE TABLE main.source(v TEXT); CREATE TABLE other.source(v TEXT); INSERT INTO main.source(rowid,v) VALUES(99,'main'); INSERT INTO other.source(rowid,v) VALUES(700,'attached')",0).await.unwrap();
        let plan = duplicate_plan(&mut s, Driver::Sqlite, "other", "source", "target", true).await.unwrap();
        s.run_transaction(&plan.statements).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT rowid || ':' || v FROM other.target").await.as_deref(), Some("700:attached"));
        assert_eq!(scalar(&mut s, "SELECT count(*) FROM main.sqlite_schema WHERE name='target'").await.as_deref(), Some("0"));
    }
    #[test]
    fn snowflake_native_clone_rejects_source_modifiers_that_would_be_lost() {
        for modifier in ["TRANSIENT", "TEMPORARY", "TEMP", "HYBRID", "ICEBERG", "DYNAMIC", "EXTERNAL", "EVENT"] {
            let sql = format!("CREATE OR REPLACE {modifier} TABLE source (id NUMBER NOT NULL)");
            assert!(table_definition::validate_snowflake_source(&sql, true).is_err(), "{modifier}");
        }
    }
    fn fixture() -> (ConnectionConfig, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("datacraft-tools-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO notes VALUES(1, 'kept');").unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        (cfg, path)
    }

    #[tokio::test]
    async fn duplicate_requires_the_preview_and_read_only_mode_prevents_writes() {
        let (mut cfg, path) = fixture();
        let operation = |expected| Operation::Duplicate { schema: "main".into(), table: "notes".into(), target: "copy".into(), data: true, expected };
        let result = run(cfg.clone(), "main", operation(vec!["unapproved".into()]), Arc::default()).await;
        assert!(result.is_err());
        let preview = run(
            cfg.clone(),
            "main",
            Operation::PreviewDuplicate { schema: "main".into(), table: "notes".into(), target: "copy".into(), data: true },
            Arc::default(),
        )
        .await
        .unwrap();
        let Outcome::Plan(plan) = preview else { panic!("expected plan") };
        cfg.read_only = true;
        assert!(run(cfg.clone(), "main", operation(plan.statements.clone()), Arc::default()).await.is_err());
        cfg.read_only = false;
        run(cfg, "main", operation(plan.statements), Arc::default()).await.unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(db.query_row("SELECT value FROM copy", [], |r| r.get::<_, String>(0)).unwrap(), "kept");
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancelled_table_query_interrupts_native_sqlite_work() {
        let (cfg, path) = fixture();
        let mut session = open(cfg, "main").await.unwrap();
        let progress = Arc::new(Progress::default());
        let cancellation = Arc::clone(&progress);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancellation.cancel.store(true, Ordering::Relaxed);
        });
        let query = "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n) SELECT max(x) FROM n";
        let result = tokio::time::timeout(Duration::from_secs(5), execute(&mut session, query, &progress)).await.unwrap();
        assert!(result.is_err());
        task.await.unwrap();
        drop(session);
        std::fs::remove_file(path).unwrap();
    }
}
