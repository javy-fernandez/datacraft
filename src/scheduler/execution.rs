//! Isolated connections and per-file SQLite serialization for scheduled work.

use super::Job;
use anyhow::{Context, bail};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, TryLockError, Weak};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SQL_TIMEOUT: Duration = Duration::from_secs(1800);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(3);
static ACTIVE_SQL_SESSIONS: AtomicUsize = AtomicUsize::new(0);

struct SessionPermit(Option<Arc<super::control::Waiter>>);

pub(super) fn active_sessions() -> usize {
    ACTIVE_SQL_SESSIONS.load(Ordering::Acquire)
}

impl SessionPermit {
    fn acquire(waker: Option<Arc<super::control::Waiter>>) -> anyhow::Result<Self> {
        let mut count = ACTIVE_SQL_SESSIONS.load(Ordering::Acquire);
        loop {
            if count >= 4 {
                bail!(crate::i18n::tr("Earlier SQL task sessions are still stopping"));
            }
            match ACTIVE_SQL_SESSIONS.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(current) => count = current,
            }
        }
        Ok(Self(waker))
    }
}

impl Drop for SessionPermit {
    fn drop(&mut self) {
        ACTIVE_SQL_SESSIONS.fetch_sub(1, Ordering::Release);
        if let Some(waker) = &self.0
            && let Err(error) = waker.notify()
        {
            eprintln!("could not wake scheduler after SQL cleanup: {error:#}");
        }
    }
}

#[cfg(unix)]
type SqliteFileKey = (u64, u64);
#[cfg(not(unix))]
type SqliteFileKey = std::path::PathBuf;
type SqliteGate = Arc<Mutex<()>>;

static SQLITE_GATES: LazyLock<Mutex<HashMap<SqliteFileKey, Weak<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn sqlite_gate(cfg: &crate::db::ConnectionConfig) -> anyhow::Result<Option<SqliteGate>> {
    if cfg.driver != crate::db::Driver::Sqlite {
        return Ok(None);
    }
    let path = cfg.file_path.trim();
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).with_context(|| crate::i18n::trf("Could not open {path}", &[("path", &path)]))?;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let key = std::fs::canonicalize(path).with_context(|| crate::i18n::trf("Could not open {path}", &[("path", &path)]))?;
    let mut gates = SQLITE_GATES.lock().map_err(|_| anyhow::anyhow!(crate::i18n::tr("The SQLite task lock became unusable")))?;
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return Ok(Some(gate));
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    Ok(Some(gate))
}

pub(super) fn execute(job: &Job, waker: Arc<super::control::Waiter>) -> anyhow::Result<Option<String>> {
    let cfg = crate::config::load_background_connection(job.connection)?;
    let mut secrets = vec![cfg.password.clone(), cfg.ssh.password.clone(), cfg.ssh.passphrase.clone(), cfg.tls.key_passphrase.clone()];
    secrets.extend(cfg.secret_properties().into_iter().map(|(_, value)| value));
    secrets.retain(|secret| !secret.is_empty());
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let permit = if job.backup.is_none() { Some(SessionPermit::acquire(Some(waker))?) } else { None };
    execute_with_timeout(job, cfg, SQL_TIMEOUT, permit).map_err(|error| {
        let mut message = format!("{error:#}");
        for secret in secrets {
            message = message.replace(&secret, "[redacted]");
        }
        anyhow::anyhow!(message)
    })
}

#[cfg(test)]
pub(super) fn execute_with_config(job: &Job, cfg: crate::db::ConnectionConfig) -> anyhow::Result<Option<String>> {
    execute_with_timeout(job, cfg, SQL_TIMEOUT, None)
}

fn execute_with_timeout(job: &Job, mut cfg: crate::db::ConnectionConfig, timeout: Duration, permit: Option<SessionPermit>) -> anyhow::Result<Option<String>> {
    use crate::db::{Driver, Session};

    if cfg.driver == Driver::Sqlite {
        let trimmed = cfg.file_path.trim();
        if trimmed.len() != cfg.file_path.len() {
            cfg.file_path = trimmed.to_owned();
        }
    }
    if let Some(backup) = &job.backup {
        let database = if !job.database.is_empty() {
            job.database.as_str()
        } else if cfg.driver == Driver::Sqlite {
            "main"
        } else {
            cfg.database.as_str()
        };
        if cfg.driver == Driver::Oracle && database != cfg.database {
            bail!(crate::i18n::tr("Oracle backup tasks must use the saved connection service"));
        }
        if cfg.ask_password {
            bail!("Save the data source password before running a task in the background");
        }
        let request = backup.request(cfg.driver, database)?;
        let destination = match cfg.driver {
            Driver::Oracle => format!("{}/{}", request.directory, request.destination),
            Driver::Snowflake => format!("{}.{}.{}", request.db, request.directory, request.destination),
            _ => request.destination.clone(),
        };
        // Native macOS runs failed with EBADF when scheduled backups and SQL overlapped.
        let gate = sqlite_gate(&cfg)?;
        let _access = lock_sqlite(&gate, SQL_TIMEOUT)?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(crate::db::backup::run(cfg, request, Arc::new(std::sync::atomic::AtomicBool::new(false))))?;
        return Ok(Some(destination));
    }
    if cfg.driver.is_nosql() {
        bail!("This data source does not use SQL");
    }
    let batches = crate::sql::split_batches(&job.sql, cfg.driver);
    if batches.is_empty() {
        bail!("The task contains no SQL statements");
    }
    if cfg.read_only && crate::sql::split_statements(&job.sql, cfg.driver).iter().any(|statement| !crate::app::is_read_statement(cfg.driver, &statement.text)) {
        bail!("A read-only data source cannot run a statement that changes data");
    }
    if cfg.ask_password {
        bail!("Save the data source password before running a task in the background");
    }
    if cfg.driver == Driver::Sqlite && !job.database.trim().is_empty() && job.database.trim() != "main" {
        bail!("SQLite tasks can only use the main database");
    }
    let select_database = !job.database.trim().is_empty();
    if select_database {
        cfg.database.clone_from(&job.database);
    }
    let gate = sqlite_gate(&cfg)?;
    run_session(gate, permit, move |runtime| {
        runtime.block_on(async {
            let mut session = timed(Session::connect(&cfg), CONNECT_TIMEOUT, None, "Connection timed out").await?;
            let cancel = session.cancel_handle();
            if select_database {
                timed(session.use_database(&cfg.database), CONNECT_TIMEOUT, cancel.as_ref(), "Database selection timed out").await?;
            }
            for batch in batches {
                timed(session.execute(&batch.text, 0), timeout, cancel.as_ref(), "SQL task timed out").await?;
            }
            Ok(None)
        })
    })
}

async fn timed<T>(
    operation: impl std::future::Future<Output = anyhow::Result<T>>,
    timeout: Duration,
    cancel: Option<&crate::db::CancelHandle>,
    message: &'static str,
) -> anyhow::Result<T> {
    match tokio::time::timeout(timeout, operation).await {
        Ok(result) => result,
        Err(_) => {
            if let Some(cancel) = cancel {
                let _ = tokio::time::timeout(CANCEL_TIMEOUT, cancel.cancel()).await;
            }
            bail!(crate::i18n::tr(message));
        }
    }
}

fn lock_sqlite(gate: &Option<SqliteGate>, timeout: Duration) -> anyhow::Result<Option<MutexGuard<'_, ()>>> {
    let Some(gate) = gate else {
        return Ok(None);
    };
    let start = Instant::now();
    loop {
        match gate.try_lock() {
            Ok(access) => return Ok(Some(access)),
            Err(TryLockError::Poisoned(_)) => bail!(crate::i18n::tr("The SQLite task lock became unusable")),
            Err(TryLockError::WouldBlock) => {
                if start.elapsed() >= timeout {
                    bail!(crate::i18n::tr("Timed out waiting for another SQLite task"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn run_session(
    gate: Option<SqliteGate>,
    permit: Option<SessionPermit>,
    run: impl FnOnce(&tokio::runtime::Runtime) -> anyhow::Result<Option<String>> + Send + 'static,
) -> anyhow::Result<Option<String>> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("scheduled-sql-session".into())
        .spawn(move || {
            let _permit = permit;
            let access = match lock_sqlite(&gate, SQL_TIMEOUT) {
                Ok(access) => access,
                Err(error) => {
                    let _ = send.send(Err(error));
                    return;
                }
            };
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = send.send(Err(error.into()));
                    return;
                }
            };
            let result = run(&runtime);
            if result.is_ok() {
                drop(runtime);
                drop(access);
                drop(_permit);
                let _ = send.send(result);
            } else {
                // Runtime drop waits for blocking drivers; retain their file gate and bounded permit until they stop.
                let _ = send.send(result);
                drop(runtime);
            }
        })
        .context("start scheduled SQL session")?;
    receive.recv().context("scheduled SQL session stopped unexpectedly")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ConnectionConfig, Driver};

    #[test]
    fn sql_timeout_cancels_the_query_skips_later_statements_and_releases_the_file() {
        let path = std::env::temp_dir().join(format!("datacraft-scheduler-timeout-{}.sqlite", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE records(value INTEGER); INSERT INTO records VALUES(1)").unwrap();
        drop(db);
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let task = Job {
            id: 0,
            name: "timeout".into(),
            connection: 1,
            database: "main".into(),
            sql: "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 1000000000) SELECT sum(x) FROM n; UPDATE records SET value = 99;"
                .into(),
            backup: None,
            interval_seconds: 10,
            enabled: true,
            next_run: 0,
        };
        let (send, receive) = std::sync::mpsc::channel();
        let next_cfg = cfg.clone();
        let mut next_task = task.clone();
        std::thread::spawn(move || {
            let _ = send.send(execute_with_timeout(&task, cfg, Duration::from_millis(100), None));
        });
        let error = receive.recv_timeout(Duration::from_secs(5)).expect("the timed out task must release its worker").unwrap_err();
        assert!(format!("{error:#}").contains("SQL task timed out"), "{error:#}");
        next_task.sql = "UPDATE records SET value = value + 1".into();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(execute_with_config(&next_task, next_cfg));
        });
        receive.recv_timeout(Duration::from_secs(5)).expect("the next task must acquire the same file").unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(db.query_row("SELECT value FROM records", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_session_keeps_capacity_and_the_file_until_cleanup_then_wakes_the_scheduler() {
        let path = std::env::temp_dir().join(format!("datacraft-cleanup-wakeup-{}.sqlite", uuid::Uuid::new_v4()));
        let waker = Arc::new(super::super::control::Waiter::new(&path).unwrap());
        let permits: Vec<_> = (0..3).map(|_| SessionPermit::acquire(None).unwrap()).collect();
        let permit = Some(SessionPermit::acquire(Some(Arc::clone(&waker))).unwrap());
        assert_eq!(active_sessions(), 4);
        assert!(SessionPermit::acquire(None).is_err());
        let gate = Arc::new(Mutex::new(()));
        let worker_gate = Some(Arc::clone(&gate));
        let (release, blocked) = std::sync::mpsc::channel();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = run_session(worker_gate, permit, move |runtime| {
                let (started, running) = std::sync::mpsc::channel();
                runtime.spawn_blocking(move || {
                    let _ = started.send(());
                    let _ = blocked.recv();
                });
                running.recv_timeout(Duration::from_secs(5)).unwrap();
                bail!("simulated SQL timeout");
            });
            let _ = send.send(result);
        });
        assert!(receive.recv_timeout(Duration::from_secs(5)).expect("cleanup must not retain the scheduler worker").is_err());
        assert_eq!(active_sessions(), 4, "a reported failure must remain active until its blocking cleanup ends");
        assert!(SessionPermit::acquire(None).is_err());
        assert!(matches!(gate.try_lock(), Err(TryLockError::WouldBlock)));
        assert!(lock_sqlite(&Some(Arc::clone(&gate)), Duration::from_millis(20)).unwrap_err().to_string().contains("Timed out waiting"));
        let (awake, awoken) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&waker);
        let wake_worker = std::thread::spawn(move || {
            let _ = awake.send(waiting.wait(Some(Duration::from_secs(10))));
        });
        release.send(()).unwrap();
        awoken.recv_timeout(Duration::from_secs(5)).expect("completed cleanup must wake the idle scheduler").unwrap();
        wake_worker.join().unwrap();
        assert_eq!(active_sessions(), 3);
        let gate = Some(gate);
        let access = lock_sqlite(&gate, Duration::from_secs(5)).expect("cleanup must eventually release the file");
        drop(access);
        let replacement = SessionPermit::acquire(None).unwrap();
        assert!(SessionPermit::acquire(None).is_err());
        drop(replacement);
        drop(permits);
        assert_eq!(active_sessions(), 0);
        drop(waker);
        super::super::control::Waiter::clear(&path).unwrap();
    }
}
