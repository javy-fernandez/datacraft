//! Dedicated-session coordination for table workflows outside SQL consoles.

use super::{
    ConnectionConfig, Driver, Session,
    alter::Plan,
    table_definition::TableDefinition,
    table_export::{ExportProgress, ExportRequest},
    table_model::Model,
    table_privileges::{Grants, Principal},
};
use crate::i18n::tr;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

pub enum Operation {
    Definition {
        schema: String,
        table: String,
    },
    PreviewDuplicate {
        schema: String,
        table: String,
        target: String,
        data: bool,
    },
    Duplicate {
        schema: String,
        table: String,
        target: String,
        data: bool,
        expected: Vec<String>,
    },
    Export(ExportRequest),
    Grants {
        schema: String,
        table: String,
        principal: Option<Principal>,
    },
    ApplyGrants {
        schema: String,
        table: String,
        principal: Principal,
        old: Vec<String>,
        new: Vec<String>,
        expected: Vec<String>,
    },
    Model {
        tables: Vec<(String, String)>,
    },
}

pub enum Outcome {
    Definition(TableDefinition),
    Plan(Plan),
    Exported {
        path: std::path::PathBuf,
        rows: u64,
        print: bool,
    },
    Grants(Grants),
    Model(Model),
    Done,
}

async fn open(mut cfg: ConnectionConfig, db: &str) -> anyhow::Result<Session> {
    if !matches!(cfg.driver, Driver::Sqlite | Driver::Oracle) {
        cfg.database = db.into();
    }
    let mut session =
        tokio::time::timeout(Duration::from_secs(30), Session::connect(&cfg)).await??;
    tokio::time::timeout(Duration::from_secs(30), session.use_database(db)).await??;
    Ok(session)
}

async fn wait_cancel(progress: &ExportProgress) {
    while !progress.cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn execute(
    session: &mut Session,
    statement: &str,
    progress: &ExportProgress,
) -> anyhow::Result<()> {
    let handle = session.cancel_handle();
    cancellable(
        session.execute(statement, 0),
        handle,
        progress,
        Duration::from_secs(600),
    )
    .await
    .map(|_| ())
}

async fn cancellable<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    handle: Option<super::CancelHandle>,
    progress: &ExportProgress,
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
    progress: &ExportProgress,
) -> anyhow::Result<T> {
    cancellable(future, handle, progress, Duration::from_secs(30)).await
}

pub async fn run(
    cfg: ConnectionConfig,
    db: &str,
    operation: Operation,
    progress: Arc<ExportProgress>,
) -> anyhow::Result<Outcome> {
    anyhow::ensure!(!cfg.driver.is_nosql(), tr("Not available for this engine"));
    anyhow::ensure!(!progress.cancel.load(Ordering::Relaxed), tr("Cancelled"));
    let driver = cfg.driver;
    if let Operation::Export(request) = operation {
        let path = request.path.clone();
        let print = matches!(request.kind, super::table_export::ExportKind::Print { .. });
        let rows = super::table_export::export(cfg, db, request, progress).await?;
        return Ok(Outcome::Exported { path, rows, print });
    }
    let mut session = metadata(open(cfg.clone(), db), None, &progress).await?;
    macro_rules! read {
        ($future:expr) => {{
            let handle = session.cancel_handle();
            metadata($future, handle, &progress).await?
        }};
    }
    match operation {
        Operation::Definition { schema, table } => Ok(Outcome::Definition(read!(
            super::table_definition::definition(&mut session, driver, &schema, &table)
        ))),
        Operation::PreviewDuplicate {
            schema,
            table,
            target,
            data,
        } => {
            anyhow::ensure!(!cfg.read_only, tr("Read-only connection"));
            Ok(Outcome::Plan(read!(
                super::table_definition::duplicate_plan(
                    &mut session,
                    driver,
                    &schema,
                    &table,
                    &target,
                    data
                )
            )))
        }
        Operation::Duplicate {
            schema,
            table,
            target,
            data,
            expected,
        } => {
            anyhow::ensure!(!cfg.read_only, tr("Read-only connection"));
            let plan = read!(super::table_definition::duplicate_plan(
                &mut session,
                driver,
                &schema,
                &table,
                &target,
                data
            ));
            anyhow::ensure!(
                plan.statements == expected,
                tr("The table definition changed. Preview the operation again.")
            );
            let begin = if driver == Driver::MsSql {
                "BEGIN TRANSACTION"
            } else {
                "BEGIN"
            };
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
                if statement
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("create table ")
                {
                    created = true;
                }
            }
            if plan.transaction {
                if result.is_ok() {
                    result = execute(&mut session, "COMMIT", &progress).await;
                }
                if result.is_err() {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(10),
                        session.execute("ROLLBACK", 0),
                    )
                    .await;
                }
            } else if created && result.is_err() {
                // Only a successfully observed CREATE authorizes cleanup of this new target.
                let drop_sql = format!(
                    "DROP TABLE {}",
                    Session::quote_table(driver, &schema, &target)
                );
                let cleanup = match open(cfg, db).await {
                    Ok(mut cleanup) => {
                        tokio::time::timeout(Duration::from_secs(15), cleanup.execute(&drop_sql, 0))
                            .await
                            .map_err(anyhow::Error::from)
                            .and_then(|r| r.map(|_| ()))
                    }
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
        Operation::Grants {
            schema,
            table,
            principal,
        } => Ok(Outcome::Grants(read!(super::table_privileges::load(
            &mut session,
            driver,
            &schema,
            &table,
            principal.as_ref()
        )))),
        Operation::ApplyGrants {
            schema,
            table,
            principal,
            old,
            new,
            expected,
        } => {
            anyhow::ensure!(!cfg.read_only, tr("Read-only connection"));
            let current = read!(super::table_privileges::load(
                &mut session,
                driver,
                &schema,
                &table,
                Some(&principal)
            ));
            let mut current_grants = current.granted;
            let mut old_grants = old.clone();
            current_grants.sort();
            old_grants.sort();
            anyhow::ensure!(
                current_grants == old_grants,
                tr("The privileges changed. Reload them before applying.")
            );
            let statements = super::table_privileges::statements(
                driver, &schema, &table, &principal, &old, &new,
            )?;
            anyhow::ensure!(
                statements == expected,
                tr("Preview the privileges again before applying.")
            );
            // These catalogs can be nontransactional; always reload after a failure.
            for statement in statements {
                execute(&mut session, &statement, &progress).await?;
            }
            Ok(Outcome::Grants(read!(super::table_privileges::load(
                &mut session,
                driver,
                &schema,
                &table,
                Some(&principal)
            ))))
        }
        Operation::Model { tables } => Ok(Outcome::Model(read!(super::table_model::load(
            &mut session,
            driver,
            db,
            &tables
        )))),
        Operation::Export(_) => anyhow::bail!(tr("Invalid table operation")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (ConnectionConfig, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("datacraft-tools-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO notes VALUES(1, 'kept');").unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        (cfg, path)
    }

    #[tokio::test]
    async fn duplicate_requires_the_preview_and_read_only_mode_prevents_writes() {
        let (mut cfg, path) = fixture();
        let operation = |expected| Operation::Duplicate {
            schema: "main".into(),
            table: "notes".into(),
            target: "copy".into(),
            data: true,
            expected,
        };
        let result = run(
            cfg.clone(),
            "main",
            operation(vec!["unapproved".into()]),
            Arc::default(),
        )
        .await;
        assert!(result.is_err());
        let preview = run(
            cfg.clone(),
            "main",
            Operation::PreviewDuplicate {
                schema: "main".into(),
                table: "notes".into(),
                target: "copy".into(),
                data: true,
            },
            Arc::default(),
        )
        .await
        .unwrap();
        let Outcome::Plan(plan) = preview else {
            panic!("expected plan")
        };
        cfg.read_only = true;
        assert!(
            run(
                cfg.clone(),
                "main",
                operation(plan.statements.clone()),
                Arc::default()
            )
            .await
            .is_err()
        );
        cfg.read_only = false;
        run(cfg, "main", operation(plan.statements), Arc::default())
            .await
            .unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM copy", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "kept"
        );
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancelled_table_query_interrupts_native_sqlite_work() {
        let (cfg, path) = fixture();
        let mut session = open(cfg, "main").await.unwrap();
        let progress = Arc::new(ExportProgress::default());
        let cancellation = Arc::clone(&progress);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancellation.cancel.store(true, Ordering::Relaxed);
        });
        let query =
            "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n) SELECT max(x) FROM n";
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            execute(&mut session, query, &progress),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        task.await.unwrap();
        drop(session);
        std::fs::remove_file(path).unwrap();
    }
}
