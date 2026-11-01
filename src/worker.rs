//! Background thread with a tokio runtime. The UI sends `Request`s and collects `Response`s
//! with `Worker::poll` every frame; the worker requests a repaint when each task finishes.
//!
//! Each connection can have several databases. SQL Server and SQLite use a single
//! session (with `USE [db]` before each operation on SQL Server); PostgreSQL opens one
//! session per database, because a PG connection cannot switch databases.

use crate::db::{CancelHandle, ConnectionConfig, DatabaseInfo, Driver, ResultSet, SchemaInfo, Session};
use crate::i18n::{tr, trf};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;
use tokio::sync::Mutex;

pub type ConnId = u64;
pub type ConsoleId = u64;

pub enum Request {
    TableShortcut { path: std::path::PathBuf, target: crate::table_shortcut::Target },
    TableDuplicate { conn: ConnId, db: String, request_id: u64, operation: Box<crate::db::table_duplicate::Operation>, progress: Arc<crate::db::table_duplicate::Progress> },
    ExportTable { conn: ConnId, db: String, request_id: u64, request: Box<crate::db::table_export::ExportRequest>, progress: Arc<crate::db::table_export::ExportProgress> },
    OpenTableReport { request_id: u64, path: std::path::PathBuf },
    /// "Run SQL Script…": the files on a connection of their own; progress goes to `run`.
    RunScript { conn: ConnId, db: Option<String>, files: Vec<std::path::PathBuf>, run: Arc<crate::db::script::ScriptRun> },
    Backup { conn: ConnId, id: uuid::Uuid, request: Box<crate::db::backup::BackupRequest>, cancel: Arc<std::sync::atomic::AtomicBool> },
    InspectTable { conn: ConnId, db: String, schema: String, table: String, console: ConsoleId, generation: u64, count_rows: bool, page_filter: Option<String> },
    SchemaSearch {
        conn: ConnId,
        request_id: u64,
        request: crate::db::schema_search::SearchRequest,
        cancel: Arc<tokio::sync::Notify>,
    },
    /// "Copy Table To…": copies rows from `source` to `target` on sessions of their own.
    CopyTable {
        source: ConnId,
        target: ConnId,
        request_id: u64,
        request: crate::db::copy::CopyRequest,
        cancel: Arc<tokio::sync::Notify>,
    },
    /// Opens (or reopens) the connection, lists its databases and loads the default database's schema.
    Connect(ConnectionConfig),
    /// Only tests the connection and closes it.
    Test(ConnectionConfig),
    Disconnect(ConnId),
    /// Lists the databases again and reloads the default one.
    Refresh(ConnId),
    /// Loads (or reloads) the schema of a specific database.
    LoadDatabase {
        conn: ConnId,
        db: String,
    },
    ObjectInfo {
        conn: ConnId,
        db: String,
        schema: String,
        table: String,
        request_id: u64,
        count_rows: bool,
    },
    ObjectCatalog {
        conn: ConnId,
        db: String,
        schema: Option<String>,
        request_id: u64,
    },
    /// The real definition of a view (for "Edit View…" and "Generate DDL").
    ViewDefinition { conn: ConnId, db: String, schema: String, view: String, request_id: u64 },
    /// The procedures and functions of a database.
    LoadRoutines { conn: ConnId, db: String },
    /// A routine's definition (for "Edit…", "Execute…" and "Generate DDL").
    RoutineDefinition { routine: crate::db::routines::RoutineRef, request_id: u64 },
    /// "New Database": the server's character sets, collations, roles… for the dialog.
    DatabaseChoices { conn: ConnId },
    /// "Modify Table…": the table's full structure (defaults, primary key, indexes).
    TableDetails { conn: ConnId, db: String, schema: String, table: String, request_id: u64 },
    /// Runs statements in order; stops at the first error.
    /// `db`: database to run them in (`None` = the connection's default database).
    Execute {
        conn: ConnId,
        db: Option<String>,
        console: ConsoleId,
        statements: Vec<String>,
        max_rows: usize,
    },
    /// Saves data editor changes (statements from `edit::change_sql`) in one transaction.
    ApplyChanges {
        conn: ConnId,
        db: Option<String>,
        console: ConsoleId,
        statements: Vec<String>,
    },
    /// Runs DDL outside any console (create or drop a database or table). `db`: as in
    /// `Execute`. `close_db`: database whose own session (PostgreSQL) is closed first, so
    /// it can be dropped.
    /// `transaction`: all statements in one transaction (imports); DDL runs without one.
    RunDdl { conn: ConnId, db: Option<String>, statements: Vec<String>, close_db: Option<String>, transaction: bool },
    SaveRoutine { conn: ConnId, db: String, console: ConsoleId, statements: Vec<String>, transaction: bool, identity: Option<crate::db::routines::RoutineRef> },
    /// Cancels what a console's `Execute` is running. Engines without a native cancel
    /// abandon the query and close the session (it reconnects on the next run).
    Cancel { console: ConsoleId },
    /// Users / roles: the list or one user's privileges, on the metadata session.
    Users { conn: ConnId, request: crate::db::users::UsersRequest },
}

pub enum Response {
    TableShortcut { result: Result<std::path::PathBuf, String> },
    TableDuplicate { conn: ConnId, request_id: u64, result: Result<crate::db::table_duplicate::Outcome, String> },
    TableExported { conn: ConnId, request_id: u64, result: Result<(std::path::PathBuf, u64), String> },
    TableReportOpened { request_id: u64, result: Result<std::path::PathBuf, String> },
    Backup { conn: ConnId, id: uuid::Uuid, request: Box<crate::db::backup::BackupRequest>, result: Result<Option<u64>, String> },
    InspectTable { console: ConsoleId, generation: u64, page_rows: Option<u64>, result: Result<Box<crate::db::object_info::ObjectInfo>, String> },
    SchemaSearch { request_id: u64, result: Result<crate::db::schema_search::SearchReport, String> },
    /// Result of `CopyTable`: rows copied to each target table.
    TableCopied { request_id: u64, result: Result<Vec<(String, u64)>, String> },
    /// `databases` in order; the default one has `current = true` and its schema loaded.
    Connected { conn: ConnId, databases: Vec<DatabaseInfo> },
    ConnectFailed { conn: ConnId, error: String },
    /// `Ok` carries the server's product and version when it could be read.
    TestResult { conn: ConnId, result: Result<Option<String>, String> },
    Disconnected(ConnId),
    /// Reply to `Refresh`: same shape as `Connected`.
    Refreshed {
        conn: ConnId,
        databases: Vec<DatabaseInfo>,
    },
    DatabaseLoaded {
        conn: ConnId,
        db: String,
        result: Result<Vec<SchemaInfo>, String>,
    },
    ObjectInfo {
        conn: ConnId,
        request_id: u64,
        result: Result<Box<crate::db::object_info::ObjectInfo>, String>,
    },
    ObjectCatalog {
        conn: ConnId,
        request_id: u64,
        result: Result<Vec<crate::db::object_info::ObjectInfoRow>, String>,
    },
    ViewDefinition { conn: ConnId, request_id: u64, result: Result<crate::db::views::ViewDefinition, String> },
    RoutinesLoaded { conn: ConnId, db: String, result: Result<Vec<crate::db::routines::RoutineInfo>, String> },
    RoutineDefinition { conn: ConnId, request_id: u64, result: Result<Box<crate::db::routines::RoutineDefinition>, String> },
    /// `results` holds what ran before the error (if there was one).
    Executed {
        console: ConsoleId,
        results: Vec<ResultSet>,
        error: Option<String>,
        elapsed: std::time::Duration,
    },
    /// Result of `ApplyChanges`: modified rows or an error (in which case nothing was saved).
    ChangesApplied { console: ConsoleId, result: Result<u64, String> },
    /// Result of `RunDdl`.
    DdlDone { conn: ConnId, result: Result<(), String> },
    RoutineSaved { console: ConsoleId, result: Result<(), String>, identity: Option<crate::db::routines::RoutineRef> },
    TableDetails { request_id: u64, result: Result<Box<crate::db::alter::TableDetails>, String> },
    DatabaseChoices { conn: ConnId, result: Result<crate::db::create_db::DatabaseChoices, String> },
    /// Result of `Users`.
    Users { conn: ConnId, result: Result<crate::db::users::UsersReply, String> },
}

pub struct Worker {
    tx: tokio::sync::mpsc::UnboundedSender<Request>,
    rx: mpsc::Receiver<Response>,
}

impl Worker {
    pub fn start(ctx: egui::Context) -> Self {
        let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (resp_tx, resp_rx) = mpsc::channel::<Response>();

        std::thread::Builder::new()
            .name("datacraft-worker".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async move {
                    let state = Arc::new(State::default());
                    while let Some(req) = req_rx.recv().await {
                        // The epoch is stamped here, in arrival order: a Disconnect after a
                        // Connect invalidates that Connect even if it finishes later.
                        let epoch = state.bump_if_lifecycle(&req);
                        match &req {
                            Request::ObjectInfo { conn, request_id, .. } => state.meta_started(*conn, false, *request_id),
                            Request::ObjectCatalog { conn, request_id, .. } => state.meta_started(*conn, true, *request_id),
                            _ => {}
                        }
                        let state = state.clone();
                        let out = resp_tx.clone();
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            for resp in handle(req, epoch, &state).await {
                                let _ = out.send(resp);
                            }
                            ctx.request_repaint();
                        });
                    }
                });
            })
            .expect("worker thread");

        Self { tx: req_tx, rx: resp_rx }
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    pub fn poll(&self) -> Vec<Response> {
        self.rx.try_iter().collect()
    }
}

type SharedSession = Arc<Mutex<Session>>;

/// Open connection: settings (with password, in memory only) and its sessions.
struct Live {
    cfg: ConnectionConfig,
    /// Default database (that of the main session).
    default_db: String,
    /// Key "" = main session. On PostgreSQL, also one session per extra database.
    sessions: HashMap<String, SharedSession>,
    /// Object browser metadata sessions, per database: their queries neither wait behind a
    /// console nor take the console's session down when they time out.
    meta: HashMap<String, SharedSession>,
}

/// A console's `Execute` in progress. Registered before waiting for the session, so a
/// cancel that arrives meanwhile is not lost.
#[derive(Default)]
struct Running {
    /// Native cancel of the session's running statement (once the session is held).
    native: std::sync::Mutex<Option<CancelHandle>>,
    /// true while a statement of THIS execution is running on the server (a native cancel
    /// outside that window could hit another console's query on a shared session).
    in_flight: std::sync::atomic::AtomicBool,
    /// Abandons the query (engines without native cancel, or if it failed).
    abandon: tokio::sync::Notify,
    cancelled: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct State {
    live: Mutex<HashMap<ConnId, Live>>,
    running: std::sync::Mutex<HashMap<ConsoleId, Arc<Running>>>,
    /// Epoch per connection: changes with every Connect/Disconnect.
    epochs: std::sync::Mutex<HashMap<ConnId, u64>>,
    /// Newest object browser request per (connection, catalog?): older ones still queued for
    /// the metadata session are skipped (their reply would be discarded anyway).
    meta_latest: std::sync::Mutex<HashMap<(ConnId, bool), u64>>,
}

impl State {
    fn bump_if_lifecycle(&self, req: &Request) -> u64 {
        let conn = match req {
            Request::Connect(cfg) => cfg.id,
            Request::Disconnect(conn) => *conn,
            _ => return 0,
        };
        let mut epochs = self.epochs.lock().unwrap_or_else(|e| e.into_inner());
        let e = epochs.entry(conn).or_default();
        *e += 1;
        *e
    }

    fn current_epoch(&self, conn: ConnId) -> u64 {
        let epochs = self.epochs.lock().unwrap_or_else(|e| e.into_inner());
        epochs.get(&conn).copied().unwrap_or_default()
    }

    /// Session to operate on `db` with, and the database name to pass to the driver.
    /// On PostgreSQL it opens (and keeps) a new session if the database isn't the default one.
    async fn session_for(&self, conn: ConnId, db: Option<&str>) -> anyhow::Result<(SharedSession, String)> {
        let (cfg, default_db, main, existing) = {
            let live = self.live.lock().await;
            let l = live.get(&conn).ok_or_else(|| anyhow::anyhow!(tr("The connection is not open")))?;
            let target = db.filter(|d| !d.is_empty()).unwrap_or(&l.default_db).to_string();
            let main = l.sessions.get("").cloned().ok_or_else(|| anyhow::anyhow!(tr("The connection is not open")))?;
            let existing = l.sessions.get(&target).cloned();
            (l.cfg.clone(), l.default_db.clone(), main, existing)
        };
        let target = db.filter(|d| !d.is_empty()).unwrap_or(&default_db).to_string();
        if cfg.driver != Driver::Postgres || target == default_db {
            return Ok((main, target));
        }
        if let Some(s) = existing {
            return Ok((s, target));
        }
        let mut extra_cfg = cfg;
        extra_cfg.database = target.clone();
        let epoch = self.current_epoch(conn);
        let session = Arc::new(Mutex::new(Session::connect(&extra_cfg).await?));
        let mut live = self.live.lock().await;
        // If it was disconnected meanwhile, don't keep it (the session closes when dropped).
        if self.current_epoch(conn) == epoch
            && let Some(l) = live.get_mut(&conn)
        {
            let s = l.sessions.entry(target.clone()).or_insert(session);
            return Ok((s.clone(), target));
        }
        Ok((session, target))
    }

    /// Metadata session for `db` (opened on first use and kept), and the database name to pass to
    /// the driver.
    async fn meta_session_for(&self, conn: ConnId, db: &str) -> anyhow::Result<(SharedSession, String)> {
        let (mut cfg, target, existing) = {
            let live = self.live.lock().await;
            let l = live.get(&conn).ok_or_else(|| anyhow::anyhow!(tr("The connection is not open")))?;
            let target = if db.is_empty() { l.default_db.clone() } else { db.to_string() };
            (l.cfg.clone(), target.clone(), l.meta.get(&target).cloned())
        };
        if let Some(s) = existing {
            return Ok((s, target));
        }
        if cfg.driver == Driver::Postgres {
            cfg.database = target.clone();
        }
        let epoch = self.current_epoch(conn);
        let session = Arc::new(Mutex::new(Session::connect(&cfg).await?));
        let mut live = self.live.lock().await;
        if self.current_epoch(conn) == epoch
            && let Some(l) = live.get_mut(&conn)
        {
            let s = l.meta.entry(target.clone()).or_insert(session);
            return Ok((s.clone(), target));
        }
        Ok((session, target))
    }

    /// Drops a metadata session (timed out or broken); the next request opens a new one.
    async fn forget_meta(&self, conn: ConnId, session: &SharedSession) {
        if let Some(l) = self.live.lock().await.get_mut(&conn) {
            l.meta.retain(|_, s| !Arc::ptr_eq(s, session));
        }
    }

    /// true if a newer object browser request of the same kind has arrived.
    fn meta_superseded(&self, conn: ConnId, catalog: bool, request_id: u64) -> bool {
        let latest = self.meta_latest.lock().unwrap_or_else(|e| e.into_inner());
        latest.get(&(conn, catalog)).is_some_and(|&newest| newest != request_id)
    }

    fn meta_started(&self, conn: ConnId, catalog: bool, request_id: u64) {
        self.meta_latest.lock().unwrap_or_else(|e| e.into_inner()).insert((conn, catalog), request_id);
    }

    /// Removes the session if it is still the same one (the connection was lost).
    async fn forget(&self, conn: ConnId, session: &SharedSession) {
        let mut live = self.live.lock().await;
        let Some(l) = live.get_mut(&conn) else { return };
        let key = l.sessions.iter().find(|(_, s)| Arc::ptr_eq(s, session)).map(|(k, _)| k.clone());
        match key.as_deref() {
            // Without the main session the whole connection is closed.
            Some("") => {
                live.remove(&conn);
            }
            Some(k) => {
                let k = k.to_string();
                l.sessions.remove(&k);
            }
            None => {}
        }
    }
}

/// Object browser metadata, on the connection's metadata session for `db`. A query that takes
/// over 15 s is cancelled; if it can't be, only the metadata session is dropped.
async fn fetch_object_info(
    state: &State,
    conn: ConnId,
    db: &str,
    schema: Option<&str>,
    table: Option<&str>,
    count_rows: bool,
    request_id: u64,
) -> Result<Vec<crate::db::object_info::ObjectInfoRow>, String> {
    let catalog = table.is_none();
    let Some(driver) = state.live.lock().await.get(&conn).map(|l| l.cfg.driver) else {
        return Err(tr("Connection lost").into());
    };
    let (session, target) = state.meta_session_for(conn, db).await.map_err(|e| format!("{e:#}"))?;
    let mut s = session.lock().await;
    // Arrowing through tables queues requests: only the newest one runs.
    if state.meta_superseded(conn, catalog, request_id) {
        return Err(tr("Object information request superseded").into());
    }
    let cancel = s.cancel_handle();
    let query = async {
        s.use_database(&target).await?;
        match (schema, table) {
            (Some(schema), Some(table)) => s
                .object_info(driver, schema, table, count_rows)
                .await
                .map(|info| {
                    vec![crate::db::object_info::ObjectInfoRow {
                        schema: schema.into(),
                        name: table.into(),
                        info,
                    }]
                }),
            _ => s.object_list_info(driver, schema).await,
        }
    };
    let (result, broken) = match tokio::time::timeout(std::time::Duration::from_secs(15), query).await {
        Ok(result) => (result.map_err(|e| format!("{e:#}")), s.is_closed()),
        Err(_) => {
            let cancelled = match cancel {
                Some(h) => h.cancel().await.is_ok(),
                None => false,
            };
            (Err(tr("Object information request timed out").into()), !cancelled)
        }
    };
    drop(s);
    if broken {
        state.forget_meta(conn, &session).await;
    }
    result
}

/// A view's definition, on the metadata session for `db` (15 s at most, like `fetch_object_info`).
async fn fetch_view_definition(state: &State, conn: ConnId, db: &str, schema: &str, view: &str) -> Result<crate::db::views::ViewDefinition, String> {
    let Some(driver) = state.live.lock().await.get(&conn).map(|l| l.cfg.driver) else {
        return Err(tr("Connection lost").into());
    };
    let (session, target) = state.meta_session_for(conn, db).await.map_err(|e| format!("{e:#}"))?;
    let mut s = session.lock().await;
    let cancel = s.cancel_handle();
    let query = async {
        s.use_database(&target).await?;
        s.view_definition(driver, &target, schema, view).await
    };
    let (result, broken) = match tokio::time::timeout(std::time::Duration::from_secs(15), query).await {
        Ok(result) => (result.map_err(|e| format!("{e:#}")), s.is_closed()),
        Err(_) => {
            let cancelled = match cancel {
                Some(h) => h.cancel().await.is_ok(),
                None => false,
            };
            (Err(tr("The view definition request timed out").into()), !cancelled)
        }
    };
    drop(s);
    if broken {
        state.forget_meta(conn, &session).await;
    }
    result
}

enum RoutineQuery<'a> {
    List,
    Definition(&'a crate::db::routines::RoutineRef),
}

enum RoutineReply {
    List(Vec<crate::db::routines::RoutineInfo>),
    Definition(Box<crate::db::routines::RoutineDefinition>),
}

/// Routines of `db`, or one's definition, on the metadata session (15 s at most, like
/// `fetch_object_info`).
async fn fetch_routines(state: &State, conn: ConnId, db: &str, query: RoutineQuery<'_>) -> Result<RoutineReply, String> {
    let Some(driver) = state.live.lock().await.get(&conn).map(|l| l.cfg.driver) else {
        return Err(tr("Connection lost").into());
    };
    let (session, target) = state.meta_session_for(conn, db).await.map_err(|e| format!("{e:#}"))?;
    let mut s = session.lock().await;
    let cancel = s.cancel_handle();
    let run = async {
        s.use_database(&target).await?;
        match query {
            RoutineQuery::List => s.list_routines(driver, &target).await.map(RoutineReply::List),
            RoutineQuery::Definition(r) => s.routine_definition(driver, r).await.map(|d| RoutineReply::Definition(Box::new(d))),
        }
    };
    let (result, broken) = match tokio::time::timeout(std::time::Duration::from_secs(15), run).await {
        Ok(result) => (result.map_err(|e| format!("{e:#}")), s.is_closed()),
        Err(_) => {
            let cancelled = match cancel {
                Some(h) => h.cancel().await.is_ok(),
                None => false,
            };
            (Err(tr("The routine request timed out").into()), !cancelled)
        }
    };
    drop(s);
    if broken {
        state.forget_meta(conn, &session).await;
    }
    result
}

/// Users / roles, on the connection's metadata session for its default database (so the list
/// does not wait behind a console).
async fn fetch_users(state: &State, conn: ConnId, request: &crate::db::users::UsersRequest) -> Result<crate::db::users::UsersReply, String> {
    let Some(driver) = state.live.lock().await.get(&conn).map(|l| l.cfg.driver) else {
        return Err(tr("The connection is not open").into());
    };
    let (session, _) = state.meta_session_for(conn, "").await.map_err(|e| format!("{e:#}"))?;
    let mut s = session.lock().await;
    let result = crate::db::users::load(&mut s, driver, request).await.map_err(|e| format!("{e:#}"));
    let broken = s.is_closed();
    drop(s);
    if broken {
        state.forget_meta(conn, &session).await;
    }
    result
}

async fn handle(req: Request, epoch: u64, state: &State) -> Vec<Response> {
    match req {
        Request::TableShortcut { path, target } => {
            let result = tokio::task::spawn_blocking(move || crate::table_shortcut::create(&path, &target).map(|_| path)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
            vec![Response::TableShortcut { result }]
        }
        Request::TableDuplicate { conn, db, request_id, operation, progress } => {
            let cfg = state.live.lock().await.get(&conn).map(|live| live.cfg.clone());
            let result = match cfg {
                Some(cfg) => {
                    let password = cfg.password.clone();
                    // Composed driver futures otherwise exhaust the Tokio worker's stack.
                    // Per-statement bounds own cancellation and cleanup; never drop the whole operation on a second timer.
                    let future = Box::pin(crate::db::table_duplicate::run(cfg, &db, *operation, Arc::clone(&progress)));
                    future.await.map_err(|error| {
                        let message = format!("{error:#}");
                        if password.is_empty() { message } else { message.replace(&password, "[redacted]") }
                    })
                }
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::TableDuplicate { conn, request_id, result }]
        }
        Request::ExportTable { conn, db, request_id, request, progress } => {
            let cfg = state.live.lock().await.get(&conn).map(|l| l.cfg.clone());
            let path = request.path.clone();
            let result = match cfg {
                Some(cfg) => {
                    let password = cfg.password.clone();
                    // Composed native driver futures otherwise exhaust the Tokio worker's stack.
                    let mut operation = Box::pin(crate::db::table_export::export(cfg, &db, *request, Arc::clone(&progress)));
                    let result = match tokio::time::timeout(std::time::Duration::from_secs(1800), &mut operation).await {
                        Ok(result) => result,
                        Err(_) => {
                            progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut operation).await;
                            Err(anyhow::anyhow!(tr("Table operation timed out")))
                        }
                    };
                    result.map(|rows| (path, rows)).map_err(|error| {
                        let message = format!("{error:#}");
                        if password.is_empty() { message } else { message.replace(&password, "[redacted]") }
                    })
                }
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::TableExported { conn, request_id, result }]
        }
        Request::OpenTableReport { request_id, path } => {
            let result =
                tokio::task::spawn_blocking(move || open_table_report(&path)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
            vec![Response::TableReportOpened { request_id, result }]
        }
        Request::DatabaseChoices { conn } => {
            let result = async {
                let driver = state.live.lock().await.get(&conn).map(|l| l.cfg.driver).ok_or_else(|| anyhow::anyhow!(tr("Connection lost")))?;
                let (session, _) = state.meta_session_for(conn, "").await?;
                let mut s = session.lock().await;
                s.database_choices(driver).await
            }
            .await
            .map_err(|e| format!("{e:#}"));
            vec![Response::DatabaseChoices { conn, result }]
        }
        Request::Users { conn, request } => vec![Response::Users { conn, result: fetch_users(state, conn, &request).await }],
        Request::RunScript { conn, db, files, run } => {
            let cfg = state.live.lock().await.get(&conn).map(|l| l.cfg.clone());
            match cfg {
                Some(cfg) => crate::db::script::run(cfg, db, files, run).await,
                None => {
                    let mut p = run.progress.lock().unwrap_or_else(|e| e.into_inner());
                    p.finished = true;
                    p.stopped = Some(tr("The connection is not open").into());
                }
            }
            Vec::new()
        }
        Request::Backup { conn, id, request, cancel } => {
            let cfg = state.live.lock().await.get(&conn).map(|l| l.cfg.clone());
            let result = match cfg {
                Some(cfg) => {
                    let password = cfg.password.clone();
                    crate::db::backup::run(cfg, *request.clone(), cancel).await.map_err(|error| {
                        let message = format!("{error:#}");
                        if password.is_empty() { message } else { message.replace(&password, "[redacted]") }
                    })
                }
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::Backup { conn, id, request, result }]
        }
        Request::InspectTable { conn, db, schema, table, console, generation, count_rows, page_filter } => {
            let cfg = state.live.lock().await.get(&conn).map(|l| l.cfg.clone());
            let result = match cfg {
                Some(mut cfg) if !cfg.driver.is_nosql() => {
                    if cfg.driver != Driver::Oracle { cfg.database.clone_from(&db); }
                    let query = async {
                        let mut session = Session::connect(&cfg).await?;
                        session.use_database(&db).await?;
                        let cancel = session.cancel_handle();
                        let read = async {
                            let info = session.object_info(cfg.driver, &schema, &table, count_rows && page_filter.is_none()).await?;
                            let page_rows = if let Some(filter) = &page_filter { Some(session.table_row_count(cfg.driver, &schema, &table, filter).await?) } else { None };
                            anyhow::Ok((info, page_rows))
                        };
                        match tokio::time::timeout(std::time::Duration::from_secs(15), read).await {
                            Ok(result) => result,
                            Err(_) => {
                                if let Some(cancel) = cancel { let _ = tokio::time::timeout(std::time::Duration::from_secs(2), cancel.cancel()).await; }
                                anyhow::bail!(tr("Object information request timed out"))
                            }
                        }
                    };
                    match tokio::time::timeout(std::time::Duration::from_secs(35), query).await {
                        Ok(result) => result.map_err(|e| format!("{e:#}")),
                        Err(_) => Err(tr("Object information request timed out").into()),
                    }
                }
                _ => Err(tr("The connection is not open").into()),
            };
            let (result, page_rows) = match result { Ok((info, rows)) => (Ok(Box::new(info)), rows), Err(error) => (Err(error), None) };
            vec![Response::InspectTable { console, generation, page_rows, result }]
        }
        Request::SchemaSearch { conn, request_id, request, cancel } => {
            let cfg = state.live.lock().await.get(&conn).map(|l| l.cfg.clone());
            let result = match cfg {
                Some(cfg) => crate::db::schema_search::search(cfg, request, cancel).await.map_err(|e| format!("{e:#}")),
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::SchemaSearch { request_id, result }]
        }
        Request::CopyTable { source, target, request_id, request, cancel } => {
            let cfgs = {
                let live = state.live.lock().await;
                live.get(&source).map(|l| l.cfg.clone()).zip(live.get(&target).map(|l| l.cfg.clone()))
            };
            let result = match cfgs {
                Some((source, target)) => {
                    crate::db::copy::copy(source, target, request, cancel).await.map_err(|e| format!("{e:#}"))
                }
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::TableCopied { request_id, result }]
        }
        Request::Connect(cfg) => {
            let conn = cfg.id;
            match connect_and_load(&cfg).await {
                Ok((session, default_db, databases)) => {
                    let mut live = state.live.lock().await;
                    // Disconnected or reconnected meanwhile: this session is not needed.
                    if state.current_epoch(conn) != epoch {
                        return vec![];
                    }
                    let mut sessions = HashMap::new();
                    sessions.insert(String::new(), Arc::new(Mutex::new(session)));
                    live.insert(conn, Live { cfg, default_db, sessions, meta: HashMap::new() });
                    vec![Response::Connected { conn, databases }]
                }
                Err(e) if state.current_epoch(conn) == epoch => {
                    vec![Response::ConnectFailed { conn, error: format!("{e:#}") }]
                }
                Err(_) => vec![],
            }
        }
        Request::Test(cfg) => {
            let result = Session::test(&cfg).await.map_err(|e| format!("{e:#}"));
            vec![Response::TestResult { conn: cfg.id, result }]
        }
        Request::Disconnect(conn) => {
            state.live.lock().await.remove(&conn);
            vec![Response::Disconnected(conn)]
        }
        Request::Refresh(conn) => {
            let (session, default_db) = match state.session_for(conn, None).await {
                Ok(s) => s,
                Err(e) => return vec![Response::ConnectFailed { conn, error: format!("{e:#}") }],
            };
            let mut s = session.lock().await;
            match load_databases(&mut s, &default_db).await {
                Ok(databases) => vec![Response::Refreshed { conn, databases }],
                Err(e) => {
                    let lost = s.is_closed();
                    drop(s);
                    if lost {
                        state.forget(conn, &session).await;
                    }
                    vec![Response::ConnectFailed { conn, error: format!("{e:#}") }]
                }
            }
        }
        Request::LoadDatabase { conn, db } => {
            let result = match state.session_for(conn, Some(&db)).await {
                Ok((session, target)) => {
                    let mut s = session.lock().await;
                    s.introspect_database(&target).await.map_err(|e| format!("{e:#}"))
                }
                Err(e) => Err(format!("{e:#}")),
            };
            vec![Response::DatabaseLoaded { conn, db, result }]
        }
        Request::ObjectInfo {
            conn,
            db,
            schema,
            table,
            request_id,
            count_rows,
        } => {
            let result =
                fetch_object_info(state, conn, &db, Some(&schema), Some(&table), count_rows, request_id).await;
            let result = result.and_then(|rows| {
                rows.into_iter()
                    .next()
                    .map(|row| Box::new(row.info))
                    .ok_or_else(|| tr("Object information is not available").into())
            });
            vec![Response::ObjectInfo { conn, request_id, result }]
        }
        Request::ObjectCatalog {
            conn,
            db,
            schema,
            request_id,
        } => {
            let result = fetch_object_info(state, conn, &db, schema.as_deref(), None, false, request_id).await;
            vec![Response::ObjectCatalog { conn, request_id, result }]
        }
        Request::LoadRoutines { conn, db } => {
            let result = fetch_routines(state, conn, &db, RoutineQuery::List).await.map(|r| match r {
                RoutineReply::List(list) => list,
                RoutineReply::Definition(_) => Vec::new(),
            });
            vec![Response::RoutinesLoaded { conn, db, result }]
        }
        Request::RoutineDefinition { routine, request_id } => {
            let conn = routine.conn;
            let result = fetch_routines(state, conn, &routine.db, RoutineQuery::Definition(&routine)).await.and_then(|r| match r {
                RoutineReply::Definition(d) => Ok(d),
                RoutineReply::List(_) => Err(tr("Not available for this engine").into()),
            });
            vec![Response::RoutineDefinition { conn, request_id, result }]
        }
        Request::ViewDefinition { conn, db, schema, view, request_id } => {
            let result = fetch_view_definition(state, conn, &db, &schema, &view).await;
            vec![Response::ViewDefinition { conn, request_id, result }]
        }
        Request::TableDetails { conn, db, schema, table, request_id } => {
            // On the metadata session: it doesn't wait behind a console's query.
            let result = async {
                let driver = state.live.lock().await.get(&conn).map(|l| l.cfg.driver).ok_or_else(|| anyhow::anyhow!(tr("Connection lost")))?;
                let (session, target) = state.meta_session_for(conn, &db).await?;
                let mut s = session.lock().await;
                s.use_database(&target).await?;
                let details = crate::db::alter::load_details(&mut s, driver, &schema, &table).await;
                let broken = s.is_closed();
                drop(s);
                if broken {
                    state.forget_meta(conn, &session).await;
                }
                details
            }
            .await;
            vec![Response::TableDetails { request_id, result: result.map(Box::new).map_err(|e| format!("{e:#}")) }]
        }
        Request::ApplyChanges { conn, db, console, statements } => {
            let (session, target) = match state.session_for(conn, db.as_deref()).await {
                Ok(s) => s,
                Err(e) => return vec![Response::ChangesApplied { console, result: Err(format!("{e:#}")) }],
            };
            let mut s = session.lock().await;
            let result = match s.use_database(&target).await {
                Ok(()) => s.apply_changes(&statements).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            };
            let lost = s.is_closed();
            drop(s);
            let mut out = vec![Response::ChangesApplied { console, result }];
            if lost {
                state.forget(conn, &session).await;
                out.push(Response::ConnectFailed { conn, error: tr("Connection lost").into() });
            }
            out
        }
        request @ (Request::RunDdl { .. } | Request::SaveRoutine { .. }) => {
            let (conn, db, statements, close_db, transaction, console, identity) = match request {
                Request::RunDdl { conn, db, statements, close_db, transaction } => (conn, db, statements, close_db, transaction, None, None),
                Request::SaveRoutine { conn, db, console, statements, transaction, identity } => (conn, Some(db), statements, None, transaction, Some(console), identity),
                _ => unreachable!(),
            };
            let response = |result, identity| match console {
                Some(console) => Response::RoutineSaved { console, result, identity },
                None => Response::DdlDone { conn, result },
            };
            if let Some(name) = close_db.filter(|n| !n.is_empty())
                && let Some(l) = state.live.lock().await.get_mut(&conn)
            {
                l.sessions.remove(&name);
                l.meta.remove(&name);
            }
            let (session, target) = match state.session_for(conn, db.as_deref()).await {
                Ok(s) => s,
                Err(e) => return vec![response(Err(format!("{e:#}")), None)],
            };
            let mut s = session.lock().await;
            let mut result = s.use_database(&target).await.map_err(|e| format!("{e:#}"));
            let mut resolved = None;
            if result.is_ok() && let Some(identity) = identity {
                match s.save_postgres_routine(&statements, identity).await {
                    Ok(identity) => resolved = Some(identity),
                    Err(error) => result = Err(format!("{error:#}")),
                }
            } else if result.is_ok() && transaction {
                result = s.run_transaction(&statements).await.map(|_| ()).map_err(|e| format!("{e:#}"));
            } else if result.is_ok() {
                for stmt in &statements {
                    if let Err(e) = s.execute(stmt, 0).await {
                        result = Err(format!("{e:#}"));
                        break;
                    }
                }
            }
            let lost = s.is_closed();
            drop(s);
            let mut out = vec![response(result, resolved)];
            if lost {
                state.forget(conn, &session).await;
                out.push(Response::ConnectFailed { conn, error: tr("Connection lost").into() });
            }
            out
        }
        Request::Cancel { console } => {
            use std::sync::atomic::Ordering::SeqCst;
            let running = state.running.lock().unwrap_or_else(|e| e.into_inner()).get(&console).cloned();
            if let Some(r) = running {
                r.cancelled.store(true, SeqCst);
                let native = r.native.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // The statement may not have reached the server yet when the first cancel
                // arrives: retry while this same execution is still running a statement.
                let still_running = || {
                    r.in_flight.load(SeqCst)
                        && state.running.lock().unwrap_or_else(|e| e.into_inner()).get(&console).is_some_and(|x| Arc::ptr_eq(x, &r))
                };
                for _ in 0..25 {
                    if !still_running() {
                        break;
                    }
                    match &native {
                        Some(h) if h.cancel().await.is_ok() => {}
                        _ => {
                            r.abandon.notify_one();
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                // Still running after ~5 s of native cancels: abandon it.
                if still_running() {
                    r.abandon.notify_one();
                }
            }
            vec![]
        }
        Request::Execute { conn, db, console, statements, max_rows } => {
            use std::sync::atomic::Ordering::SeqCst;
            let start = Instant::now();
            let running = Arc::new(Running::default());
            state.running.lock().unwrap_or_else(|e| e.into_inner()).insert(console, running.clone());
            let unregister = |state: &State| {
                let mut map = state.running.lock().unwrap_or_else(|e| e.into_inner());
                if map.get(&console).is_some_and(|r| Arc::ptr_eq(r, &running)) {
                    map.remove(&console);
                }
            };
            let (session, target) = match state.session_for(conn, db.as_deref()).await {
                Ok(s) => s,
                Err(e) => {
                    unregister(state);
                    return vec![Response::Executed {
                        console,
                        results: vec![],
                        error: Some(format!("{e:#}")),
                        elapsed: start.elapsed(),
                    }];
                }
            };
            let mut s = session.lock().await;
            *running.native.lock().unwrap_or_else(|e| e.into_inner()) = s.cancel_handle();
            let statements_tail = statements.last().cloned();
            let mut results = Vec::new();
            let mut error = None;
            let mut abandoned = false;
            if let Err(e) = s.use_database(&target).await {
                error = Some(format!("{e:#}"));
            } else {
                for stmt in statements {
                    // `in_flight` first: either this sees `cancelled` or the canceller sees
                    // `in_flight` (and retries until the statement stops).
                    running.in_flight.store(true, SeqCst);
                    if running.cancelled.load(SeqCst) {
                        running.in_flight.store(false, SeqCst);
                        break;
                    }
                    let result = tokio::select! {
                        r = s.execute(&stmt, max_rows) => r,
                        _ = running.abandon.notified() => {
                            abandoned = true;
                            Err(anyhow::anyhow!(tr("Query cancelled")))
                        }
                    };
                    running.in_flight.store(false, SeqCst);
                    match result {
                        Ok(rs) => results.extend(rs),
                        Err(e) => {
                            error = Some(format!("{e:#}"));
                            break;
                        }
                    }
                    if abandoned {
                        break;
                    }
                }
            }
            // SQL Server explain: `SET SHOWPLAN_TEXT OFF` must run even if the statement failed,
            // or the session would keep returning plans instead of running queries.
            if error.is_some()
                && !abandoned
                && let Some(last) = statements_tail.filter(|s| s.trim().eq_ignore_ascii_case("SET SHOWPLAN_TEXT OFF"))
            {
                let _ = s.execute(&last, 0).await;
            }
            unregister(state);
            if running.cancelled.load(SeqCst) {
                error = Some(tr("Query cancelled").to_string());
            }
            // An abandoned query leaves the session in an unknown state: it is closed.
            let lost = abandoned || s.is_closed();
            drop(s);
            let mut out = vec![Response::Executed { console, results, error: error.clone(), elapsed: start.elapsed() }];
            if abandoned {
                state.forget(conn, &session).await;
                out.push(Response::ConnectFailed { conn, error: tr("Query cancelled; the connection was closed and will reopen on the next run").to_string() });
            } else if lost {
                // Forget the broken session; the UI will reconnect on the next run.
                state.forget(conn, &session).await;
                let detail = error.unwrap_or_default();
                out.push(Response::ConnectFailed { conn, error: trf("Connection lost. {detail}", &[("detail", &detail)]) });
            }
            out
        }
    }
}

fn report_path(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(
        path.extension().is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm")),
        tr("Choose an HTML file for the print report")
    );
    let path = path.canonicalize()?;
    anyhow::ensure!(path.is_file(), tr("Choose an HTML file for the print report"));
    Ok(path)
}

fn open_table_report(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let path = report_path(path)?;
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(&path).spawn()?;
    Ok(path)
}

async fn connect_and_load(cfg: &ConnectionConfig) -> anyhow::Result<(Session, String, Vec<DatabaseInfo>)> {
    let mut session = Session::connect(cfg).await?;
    let default_db = session.current_database().await?;
    let databases = load_databases(&mut session, &default_db).await?;
    Ok((session, default_db, databases))
}

/// List of databases with the default one's schema already loaded.
async fn load_databases(session: &mut Session, default_db: &str) -> anyhow::Result<Vec<DatabaseInfo>> {
    let mut names = session.list_databases().await?;
    if !names.iter().any(|n| n == default_db) {
        names.push(default_db.to_string());
        names.sort_by_key(|n| n.to_lowercase());
    }
    let schemas = session.introspect_database(default_db).await?;
    let mut schemas = Some(schemas);
    Ok(names
        .into_iter()
        .map(|name| {
            let current = name == default_db;
            DatabaseInfo { schemas: if current { schemas.take() } else { None }, current, name }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn report_paths_are_validated_without_launching_an_application() {
        let dir = std::env::temp_dir().join(format!("dc-report-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("sample & report.HTML");
        std::fs::write(&file, "<!doctype html><p>sample</p>").unwrap();
        assert_eq!(report_path(&file).unwrap(), file.canonicalize().unwrap());
        assert!(report_path(&dir.join("bad.exe")).is_err());
        assert!(report_path(&dir.join("missing.html")).is_err());
        let directory = dir.join("folder.html");
        std::fs::create_dir(&directory).unwrap();
        assert!(report_path(&directory).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_worker_reads_complete_1200_row_table_and_restores_sql_without_opening_apps() {
        use crate::db::table_export::{ExportKind, ExportProgress, ExportRequest};
        let dir = std::env::temp_dir().join(format!("dc-export-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source.db");
        let db = rusqlite::Connection::open(&source).unwrap();
        db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY,note TEXT,bytes BLOB,computed TEXT GENERATED ALWAYS AS (note||'!') VIRTUAL);").unwrap();
        for id in 1..=1200 {
            db.execute(
                "INSERT INTO notes(id,note,bytes) VALUES(?1,?2,?3)",
                rusqlite::params![id, format!("<sample> {id} {}", "long text".repeat(1000)), vec![0xFFu8; 256]],
            )
            .unwrap();
        }
        drop(db);
        let w = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(171, Driver::Sqlite);
        cfg.file_path = source.display().to_string();
        cfg.read_only = true;
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 171, .. }));
        for (id, kind, filename) in [
            (1, ExportKind::Json, "notes.json"),
            (2, ExportKind::Sql { structure: true, data: true }, "notes.sql"),
            (3, ExportKind::Print { data: true }, "notes.html"),
        ] {
            let path = dir.join(filename);
            let progress = Arc::new(ExportProgress::default());
            w.send(Request::ExportTable {
                conn: 171,
                db: "main".into(),
                request_id: id,
                request: Box::new(ExportRequest { schema: "main".into(), table: "notes".into(), path: path.clone(), kind, overwrite: false }),
                progress: progress.clone(),
            });
            let Response::TableExported { conn, request_id, result } = wait_for(&w, |r| matches!(r,Response::TableExported{request_id,..} if *request_id==id))
            else {
                panic!("expected export result")
            };
            assert_eq!((conn, request_id), (171, id));
            assert_eq!(result.unwrap(), (path.clone(), 1200));
            assert_eq!(progress.rows.load(std::sync::atomic::Ordering::Relaxed), 1200);
            match kind {
                ExportKind::Json => {
                    let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    assert_eq!(rows.as_array().unwrap().len(), 1200);
                    assert_eq!(rows[1199]["id"], 1200);
                    assert_eq!(rows[0]["bytes"].as_str().unwrap().len(), 514);
                }
                ExportKind::Sql { .. } => {
                    let target = rusqlite::Connection::open_in_memory().unwrap();
                    target.execute_batch(&std::fs::read_to_string(&path).unwrap()).unwrap();
                    assert_eq!(
                        target.query_row("SELECT count(*) FROM notes WHERE length(bytes)=256 AND computed=note||'!'", [], |r| r.get::<_, i64>(0)).unwrap(),
                        1200
                    );
                }
                ExportKind::Print { .. } => {
                    let html = std::fs::read_to_string(&path).unwrap();
                    assert!(html.contains("&lt;sample&gt; 1200"));
                    assert!(!html.contains("<sample>"));
                }
                _ => unreachable!(),
            }
        }
        let keep = dir.join("keep.json");
        std::fs::write(&keep, b"existing output").unwrap();
        let progress = Arc::new(ExportProgress::default());
        progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        w.send(Request::ExportTable {
            conn: 171,
            db: "main".into(),
            request_id: 4,
            request: Box::new(ExportRequest { schema: "main".into(), table: "notes".into(), path: keep.clone(), kind: ExportKind::Json, overwrite: true }),
            progress,
        });
        let Response::TableExported { result, .. } = wait_for(&w, |r| matches!(r, Response::TableExported { request_id: 4, .. })) else {
            panic!("expected cancellation result")
        };
        assert!(result.is_err());
        assert_eq!(std::fs::read(&keep).unwrap(), b"existing output");
        w.send(Request::Disconnect(171));
        wait_for(&w, |r| matches!(r, Response::Disconnected(171)));
        drop(w);
        std::fs::remove_dir_all(dir).unwrap();
    }
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

    #[test]
    fn duplicate_requests_complete_on_the_background_worker() {
        let path = std::env::temp_dir().join(format!("datacraft-table-worker-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, value TEXT, computed TEXT GENERATED ALWAYS AS ('Note '||id) VIRTUAL); INSERT INTO notes(id,value) VALUES(1,'preserved');").unwrap();
        drop(db);
        let w = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(71, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 71, .. }));
        let operation =
            crate::db::table_duplicate::Operation::PreviewDuplicate { schema: "main".into(), table: "notes".into(), target: "notes_copy".into(), data: true };
        w.send(Request::TableDuplicate { conn: 71, db: "main".into(), request_id: 2, operation: Box::new(operation), progress: Arc::default() });
        let Response::TableDuplicate { result, .. } = wait_for(&w, |r| matches!(r, Response::TableDuplicate { request_id: 2, .. })) else { unreachable!() };
        let crate::db::table_duplicate::Outcome::Plan(plan) = result.unwrap() else { panic!("expected duplicate preview") };
        let operation = crate::db::table_duplicate::Operation::Duplicate {
            schema: "main".into(),
            table: "notes".into(),
            target: "notes_copy".into(),
            data: true,
            expected: plan.statements,
        };
        w.send(Request::TableDuplicate { conn: 71, db: "main".into(), request_id: 3, operation: Box::new(operation), progress: Arc::default() });
        let Response::TableDuplicate { result, .. } = wait_for(&w, |r| matches!(r, Response::TableDuplicate { request_id: 3, .. })) else { unreachable!() };
        assert!(matches!(result.unwrap(), crate::db::table_duplicate::Outcome::Done));
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(db.query_row("SELECT value FROM notes_copy", [], |r| r.get::<_, String>(0)).unwrap(), "preserved");
        drop(db);
        w.send(Request::Disconnect(71));
        wait_for(&w, |r| matches!(r, Response::Disconnected(71)));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn duplicate_worker_redacts_passwords_from_connection_errors() {
        let secret = "unit-test-password-only";
        let state = State::default();
        let mut cfg = ConnectionConfig::new(71, Driver::Sqlite);
        cfg.password = secret.into();
        cfg.file_path = std::env::temp_dir().join(format!("datacraft-missing-{secret}-{}.db", uuid::Uuid::new_v4())).to_string_lossy().into_owned();
        state.live.lock().await.insert(71, Live { cfg, default_db: "main".into(), sessions: HashMap::new(), meta: HashMap::new() });
        let request = Request::TableDuplicate {
            conn: 71,
            db: "main".into(),
            request_id: 9,
            operation: Box::new(crate::db::table_duplicate::Operation::Definition { schema: "main".into(), table: "notes".into() }),
            progress: Arc::default(),
        };
        let response = Box::pin(handle(request, 0, &state)).await.pop().unwrap();
        let Response::TableDuplicate { result: Err(error), .. } = response else { panic!("expected a connection error") };
        assert!(!error.contains(secret));
        assert!(error.contains("[redacted]"));
    }

    /// SQLite has no stored routines: the request answers with an empty list.
    #[test]
    fn sqlite_lists_no_routines() {
        let path = std::env::temp_dir().join(format!("datacraft-routines-{}.db", uuid::Uuid::new_v4()));
        rusqlite::Connection::open(&path).unwrap();
        let w = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(31, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 31, .. }));
        w.send(Request::LoadRoutines { conn: 31, db: "main".into() });
        let Response::RoutinesLoaded { result, .. } = wait_for(&w, |r| matches!(r, Response::RoutinesLoaded { conn: 31, .. })) else { unreachable!() };
        assert_eq!(result, Ok(Vec::new()));
        let _ = std::fs::remove_file(&path);
    }

    /// The routine requests on a real PostgreSQL (DATACRAFT_PG_HOST, as `db::postgres`'s tests).
    #[test]
    #[ignore]
    fn postgres_routines_through_the_worker() {
        let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else { return };
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let mut cfg = ConnectionConfig::new(32, Driver::Postgres);
        cfg.host = host;
        cfg.port = var("DATACRAFT_PG_PORT", "5432").parse().unwrap();
        cfg.user = var("DATACRAFT_PG_USER", "postgres");
        cfg.password = var("DATACRAFT_PG_PASSWORD", "");
        cfg.database = var("DATACRAFT_PG_DB", "postgres");
        let db = cfg.database.clone();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 32, .. }));
        let create = "create or replace function public.dc_worker_twice(n integer) returns integer language sql immutable as $$ select n * 2 $$";
        w.send(Request::RunDdl { conn: 32, db: None, statements: vec![create.into()], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { conn: 32, result: Ok(_) }));
        w.send(Request::LoadRoutines { conn: 32, db: db.clone() });
        let Response::RoutinesLoaded { result, .. } = wait_for(&w, |r| matches!(r, Response::RoutinesLoaded { conn: 32, .. })) else { unreachable!() };
        let info = result.unwrap().into_iter().find(|r| r.name == "dc_worker_twice").expect("listed");
        w.send(Request::RoutineDefinition { routine: crate::db::routines::RoutineRef::new(32, &db, &info), request_id: 9 });
        let Response::RoutineDefinition { result, .. } = wait_for(&w, |r| matches!(r, Response::RoutineDefinition { request_id: 9, .. })) else { unreachable!() };
        let def = result.unwrap();
        assert!(def.ddl.contains("dc_worker_twice"));
        assert_eq!(def.spec.unwrap().body.trim(), "select n * 2");
        w.send(Request::RunDdl { conn: 32, db: None, statements: vec!["drop function public.dc_worker_twice(integer)".into()], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { conn: 32, result: Ok(_) }));
    }

    #[test]
    fn object_metadata_is_async_and_exact_counts_are_explicit() {
        let path =
            std::env::temp_dir().join(format!("datacraft-object-info-{}.db", uuid::Uuid::new_v4()));
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("create table notes(id integer); insert into notes values (1),(2);")
            .unwrap();
        let w = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(17, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        cfg.read_only = true;
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 17, .. }));
        for (request_id, count_rows) in [(100, false), (101, true)] {
            w.send(Request::ObjectInfo {
                conn: 17,
                db: "main".into(),
                schema: "main".into(),
                table: "notes".into(),
                request_id,
                count_rows,
            });
            let response = wait_for(
                &w,
                |r| matches!(r, Response::ObjectInfo { request_id: id, .. } if *id == request_id),
            );
            let Response::ObjectInfo { conn, result, .. } = response else {
                unreachable!()
            };
            assert_eq!(conn, 17);
            let info = result.unwrap();
            assert_eq!(info.rows, if count_rows { Some(2) } else { None });
            assert_eq!(info.rows_exact, count_rows);
        }
        w.send(Request::ObjectCatalog {
            conn: 17,
            db: "main".into(),
            schema: Some("main".into()),
            request_id: 102,
        });
        let response = wait_for(&w, |r| {
            matches!(
                r,
                Response::ObjectCatalog {
                    request_id: 102,
                    ..
                }
            )
        });
        let Response::ObjectCatalog { result, .. } = response else {
            unreachable!()
        };
        let catalog = result.unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "notes");
        w.send(Request::Disconnect(17));
        wait_for(&w, |r| matches!(r, Response::Disconnected(17)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancels_a_running_sqlite_query() {
        let dir = std::env::temp_dir().join(format!("datacraft-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        rusqlite::Connection::open(&path).unwrap().execute_batch("create table t(x)").unwrap();
        let w = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(7, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        // Endless recursive CTE: only a cancel stops it.
        let endless = "with recursive c(n) as (select 1 union all select n + 1 from c) select count(*) from c".to_string();
        w.send(Request::Execute { conn: 7, db: None, console: 1, statements: vec![endless], max_rows: 10 });
        std::thread::sleep(Duration::from_millis(300));
        w.send(Request::Cancel { console: 1 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 1, .. }));
        let Response::Executed { error, .. } = r else { unreachable!() };
        assert_eq!(error.as_deref(), Some(tr("Query cancelled")));
        // The session is still usable.
        w.send(Request::Execute { conn: 7, db: None, console: 2, statements: vec!["select 1".into()], max_rows: 10 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 2, .. }));
        let Response::Executed { error, results, .. } = r else { unreachable!() };
        assert!(error.is_none() && results.len() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_only_rejects_writes(cfg: ConnectionConfig, write: &str) {
        let conn = cfg.id;
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        w.send(Request::Execute { conn, db: None, console: 3, statements: vec![write.into()], max_rows: 10 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 3, .. }));
        let Response::Executed { error, .. } = r else { unreachable!() };
        assert!(error.is_some(), "the server accepted a write on a read-only connection");
    }

    #[test]
    fn sqlite_read_only_is_enforced_by_the_engine() {
        let dir = std::env::temp_dir().join(format!("datacraft-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        rusqlite::Connection::open(&path).unwrap().execute_batch("create table t(x); insert into t values (1)").unwrap();
        let mut cfg = ConnectionConfig::new(9, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        cfg.read_only = true;
        read_only_rejects_writes(cfg, "delete from t");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An import (transactional RunDdl) whose second batch fails leaves no rows behind.
    fn transactional_import_rolls_back(cfg: ConnectionConfig, setup: &str, table: &str) {
        let conn = cfg.id;
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        w.send(Request::RunDdl { conn, db: None, statements: vec![setup.into()], close_db: None, transaction: false });
        let Response::DdlDone { result, .. } = wait_for(&w, |r| matches!(r, Response::DdlDone { .. })) else { unreachable!() };
        result.unwrap();
        let statements = vec![format!("insert into {table} values (1), (2)"), format!("insert into {table} values (2)")];
        w.send(Request::RunDdl { conn, db: None, statements, close_db: None, transaction: true });
        let Response::DdlDone { result, .. } = wait_for(&w, |r| matches!(r, Response::DdlDone { .. })) else { unreachable!() };
        assert!(result.is_err());
        w.send(Request::Execute { conn, db: None, console: 4, statements: vec![format!("select count(*) from {table}")], max_rows: 10 });
        let Response::Executed { results, .. } = wait_for(&w, |r| matches!(r, Response::Executed { console: 4, .. })) else { unreachable!() };
        assert_eq!(results[0].rows[0][0].as_deref(), Some("0"));
        w.send(Request::RunDdl { conn, db: None, statements: vec![format!("drop table {table}")], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { .. }));
    }

    #[test]
    fn sqlite_import_is_all_or_nothing() {
        let dir = std::env::temp_dir().join(format!("datacraft-tx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        rusqlite::Connection::open(&path).unwrap();
        let mut cfg = ConnectionConfig::new(11, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        transactional_import_rolls_back(cfg, "create table imp(x integer primary key)", "imp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn mysql_import_is_all_or_nothing() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(12, Driver::MariaDb);
        cfg.host = var("DATACRAFT_MY_HOST");
        cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        cfg.user = var("DATACRAFT_MY_USER");
        cfg.password = var("DATACRAFT_MY_PASSWORD");
        cfg.database = var("DATACRAFT_MY_DB");
        transactional_import_rolls_back(cfg, "create table dc_imp(x int primary key)", "dc_imp");
    }

    /// Live, like `cancels_a_running_mysql_query`.
    #[test]
    #[ignore]
    fn mysql_read_only_is_enforced_by_the_server() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(10, Driver::MariaDb);
        cfg.host = var("DATACRAFT_MY_HOST");
        cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        cfg.user = var("DATACRAFT_MY_USER");
        cfg.password = var("DATACRAFT_MY_PASSWORD");
        cfg.database = var("DATACRAFT_MY_DB");
        cfg.read_only = true;
        read_only_rejects_writes(cfg, "create table ro_probe(x int)");
    }

    /// Live, like `cancels_a_running_mysql_query`: object info doesn't wait behind a console's
    /// query, and Find in Schema searches the database.
    #[test]
    #[ignore]
    fn mysql_object_info_runs_beside_a_busy_console() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(13, Driver::MariaDb);
        cfg.host = var("DATACRAFT_MY_HOST");
        cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        cfg.user = var("DATACRAFT_MY_USER");
        cfg.password = var("DATACRAFT_MY_PASSWORD");
        cfg.database = var("DATACRAFT_MY_DB");
        let (conn, db) = (cfg.id, cfg.database.clone());
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        w.send(Request::RunDdl {
            conn,
            db: None,
            statements: vec![
                "drop table if exists dc_info".into(),
                "create table dc_info(id int primary key, note varchar(50))".into(),
                "insert into dc_info values (1, 'needle_50%'), (2, 'hay')".into(),
            ],
            close_db: None,
            transaction: false,
        });
        wait_for(&w, |r| matches!(r, Response::DdlDone { result: Ok(_), .. }));
        w.send(Request::Execute { conn, db: None, console: 4, statements: vec!["select sleep(4)".into()], max_rows: 10 });
        std::thread::sleep(Duration::from_millis(300));
        let start = Instant::now();
        for request_id in 1..=3 {
            w.send(Request::ObjectInfo { conn, db: db.clone(), schema: db.clone(), table: "dc_info".into(), request_id, count_rows: true });
        }
        let r = wait_for(&w, |r| matches!(r, Response::ObjectInfo { request_id: 3, .. }));
        assert!(start.elapsed() < Duration::from_secs(3), "object info waited for the console: {:?}", start.elapsed());
        let Response::ObjectInfo { result, .. } = r else { unreachable!() };
        assert!(result.is_ok(), "{:?}", result.err());

        let cancel = Arc::new(tokio::sync::Notify::new());
        let request = crate::db::schema_search::SearchRequest { db: db.clone(), schema: Some(db.to_uppercase()), value: "LE_50%".into(), exact: false, case_sensitive: false };
        w.send(Request::SchemaSearch { conn, request_id: 9, request, cancel });
        let r = wait_for(&w, |r| matches!(r, Response::SchemaSearch { request_id: 9, .. }));
        let Response::SchemaSearch { result: Ok(report), .. } = r else { panic!("search failed") };
        assert!(report.matches.iter().any(|m| m.table == "dc_info" && m.result.rows.len() == 1), "errors: {:?}", report.errors);
        w.send(Request::RunDdl { conn, db: None, statements: vec!["drop table dc_info".into()], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { .. }));
    }

    /// Live, like `cancels_a_running_mysql_query`: creates a user with the user editor's
    /// statements, changes its privileges, reads it back (no differences left) and drops it.
    #[test]
    #[ignore]
    fn mysql_user_editor_round_trip() {
        use crate::db::users::{self, Account, PrivSet, PrivTarget, UserState, UsersReply, UsersRequest};
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(14, Driver::MariaDb);
        cfg.host = var("DATACRAFT_MY_HOST");
        cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        cfg.user = var("DATACRAFT_MY_USER");
        cfg.password = var("DATACRAFT_MY_PASSWORD");
        cfg.database = var("DATACRAFT_MY_DB");
        let (conn, db) = (cfg.id, cfg.database.clone());
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        let run = |statements: Vec<String>| {
            w.send(Request::RunDdl { conn, db: None, statements, close_db: None, transaction: false });
            let Response::DdlDone { result, .. } = wait_for(&w, |r| matches!(r, Response::DdlDone { .. })) else { unreachable!() };
            result
        };
        let list = || {
            w.send(Request::Users { conn, request: UsersRequest::List });
            match wait_for(&w, |r| matches!(r, Response::Users { .. })) {
                Response::Users { result: Ok(UsersReply::List(l)), .. } => l,
                _ => panic!("list failed"),
            }
        };
        let detail = |a: &Account| {
            w.send(Request::Users { conn, request: UsersRequest::Detail(a.clone()) });
            match wait_for(&w, |r| matches!(r, Response::Users { .. })) {
                Response::Users { result: Ok(UsersReply::Detail(_, u)), .. } => *u,
                Response::Users { result: Err(e), .. } => panic!("{e}"),
                _ => unreachable!(),
            }
        };
        let account = Account::new("dc_live'user", "%");
        let _ = run(vec![users::drop_statement(Driver::MariaDb, &account)]);
        let available = list();
        assert!(available.accounts.iter().any(|a| a.account.name == var("DATACRAFT_MY_USER")));
        assert!(available.global_privileges.iter().any(|p| p == "PROCESS"));

        let mut new = UserState::new_user(Driver::MariaDb);
        new.account = account.clone();
        new.limits = [0, 0, 0, 3];
        new.global.privs.insert("PROCESS".into());
        new.grants.insert(PrivTarget::Database(db.clone()), PrivSet { privs: ["SELECT".into(), "INSERT".into()].into(), grant_option: false });
        let statements = users::statements(Driver::MariaDb, None, &new, Some("pw'1"), true);
        run(statements).expect("create");
        assert!(list().accounts.iter().any(|a| a.account == account));
        let loaded = detail(&account);
        assert_eq!(loaded.limits, [0, 0, 0, 3]);
        assert!(users::statements(Driver::MariaDb, Some(&loaded), &new, None, true).is_empty(), "{loaded:?}");

        let mut edited = loaded.clone();
        edited.locked = true;
        edited.global = PrivSet { privs: ["RELOAD".into()].into(), grant_option: true };
        edited.grants.insert(PrivTarget::Database(db.clone()), PrivSet { privs: ["SELECT".into(), "UPDATE".into()].into(), grant_option: false });
        run(users::statements(Driver::MariaDb, Some(&loaded), &edited, Some("pw2"), true)).expect("alter");
        let reloaded = detail(&account);
        assert!(reloaded.locked);
        assert!(users::statements(Driver::MariaDb, Some(&reloaded), &edited, None, true).is_empty(), "{reloaded:?}");

        run(vec![users::drop_statement(Driver::MariaDb, &account)]).expect("drop");
        assert!(!list().accounts.iter().any(|a| a.account == account));
    }

    /// Live: `DATACRAFT_MY_HOST/PORT/USER/PASSWORD` (see `db/mysql.rs`).
    #[test]
    #[ignore]
    fn cancels_a_running_mysql_query() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(8, Driver::MariaDb);
        cfg.host = var("DATACRAFT_MY_HOST");
        cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        cfg.user = var("DATACRAFT_MY_USER");
        cfg.password = var("DATACRAFT_MY_PASSWORD");
        cfg.database = var("DATACRAFT_MY_DB");
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        let start = Instant::now();
        w.send(Request::Execute { conn: 8, db: None, console: 1, statements: vec!["select sleep(30)".into()], max_rows: 10 });
        std::thread::sleep(Duration::from_millis(500));
        w.send(Request::Cancel { console: 1 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 1, .. }));
        let Response::Executed { error, .. } = r else { unreachable!() };
        assert_eq!(error.as_deref(), Some(tr("Query cancelled")));
        assert!(start.elapsed() < Duration::from_secs(10));
        w.send(Request::Execute { conn: 8, db: None, console: 2, statements: vec!["select 1".into()], max_rows: 10 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 2, .. }));
        let Response::Executed { error, .. } = r else { unreachable!() };
        assert!(error.is_none(), "{error:?}");
    }

    fn sqlite_cfg(id: ConnId, path: &std::path::Path) -> ConnectionConfig {
        let mut cfg = ConnectionConfig::new(id, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        cfg
    }

    fn copy_request(target_db: &str, target_table: &str, filter: &str, percent: u8, create: bool, clear: bool) -> crate::db::copy::CopyRequest {
        let col = |name: &str, ty: &str, pk: bool| crate::db::ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk };
        let columns = vec![col("id", "INTEGER", true), col("name", "TEXT", false), col("ok", "BOOLEAN", false)];
        crate::db::copy::CopyRequest {
            source_db: "main".into(),
            source_schema: "main".into(),
            source_table: "people".into(),
            target_names: columns.iter().map(|c| c.name.clone()).collect(),
            columns,
            filter: filter.into(),
            percent,
            target_db: target_db.into(),
            target_schema: target_db.into(),
            target_table: target_table.into(),
            create,
            clear,
            masks: vec![crate::db::copy::Mask::Keep; 3],
            mask_key: "test-key".into(),
            parents: false,
            children: false,
            mask_related: false,
        }
    }

    fn run_copy(w: &Worker, source: ConnId, target: ConnId, request_id: u64, request: crate::db::copy::CopyRequest) -> Result<u64, String> {
        copy_report(w, source, target, request_id, request).map(|r| r.iter().map(|(_, n)| n).sum())
    }

    fn copy_report(w: &Worker, source: ConnId, target: ConnId, request_id: u64, request: crate::db::copy::CopyRequest) -> Result<Vec<(String, u64)>, String> {
        let cancel = Arc::new(tokio::sync::Notify::new());
        w.send(Request::CopyTable { source, target, request_id, request, cancel });
        let r = wait_for(w, |r| matches!(r, Response::TableCopied { request_id: id, .. } if *id == request_id));
        let Response::TableCopied { result, .. } = r else { unreachable!() };
        result
    }

    fn people_db(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("source.db");
        let mut sql = String::from("create table people(id integer primary key, name text, ok boolean, photo blob);");
        for i in 1..=40 {
            sql.push_str(&format!("insert into people values ({i}, 'O''Brien {i}', {}, x'00ff');", i % 2));
        }
        rusqlite::Connection::open(&path).unwrap().execute_batch(&sql).unwrap();
        path
    }

    /// Copies part of a table to another SQLite file, creating it, then replaces its rows.
    #[test]
    fn copies_a_table_between_connections() {
        let dir = std::env::temp_dir().join(format!("datacraft-copy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = people_db(&dir);
        let target = dir.join("target.db");
        rusqlite::Connection::open(&target).unwrap();
        let w = Worker::start(egui::Context::default());
        let mut src = sqlite_cfg(21, &source);
        src.read_only = true;
        w.send(Request::Connect(src));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 21, .. }));
        w.send(Request::Connect(sqlite_cfg(22, &target)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 22, .. }));

        // Half of the 20 rows with id > 20.
        assert_eq!(run_copy(&w, 21, 22, 1, copy_request("main", "people_copy", "id > 20", 50, true, false)), Ok(10));
        // Again into the existing table, replacing its rows.
        assert_eq!(run_copy(&w, 21, 22, 2, copy_request("main", "people_copy", "", 100, false, true)), Ok(40));
        // A filter that writes is refused by the read-only source session.
        assert!(run_copy(&w, 21, 22, 3, copy_request("main", "people_copy", "1=1; delete from people", 100, false, false)).is_err());

        // Masked names: fake and the same for the same value on every copy.
        let mut masked = copy_request("main", "people_masked", "id <= 2", 100, true, false);
        masked.masks[1] = crate::db::copy::Mask::Name;
        assert_eq!(run_copy(&w, 21, 22, 4, masked.clone()), Ok(2));
        masked.target_table = "people_masked2".into();
        assert_eq!(run_copy(&w, 21, 22, 5, masked), Ok(2));

        let db = rusqlite::Connection::open(&target).unwrap();
        let (fakes, same): (i64, i64) = db
            .query_row(
                "select count(*), sum(a.name = b.name) from people_masked a join people_masked2 b using (id) where a.name not like 'O''Brien%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((fakes, same), (2, 2));
        let (n, name): (i64, String) =
            db.query_row("select count(*), max(name) from people_copy where ok = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((n, name.as_str()), (20, "O'Brien 9"));
        let total: i64 = db.query_row("select count(*) from people_copy", [], |r| r.get(0)).unwrap();
        assert_eq!(total, 40);
        let left: i64 = rusqlite::Connection::open(&source).unwrap().query_row("select count(*) from people", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SQLite → MariaDB (creating the table) → SQLite. Needs the same `DATACRAFT_MY_*` variables.
    #[test]
    #[ignore]
    fn copies_between_sqlite_and_mariadb() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut my = ConnectionConfig::new(23, Driver::MariaDb);
        my.host = var("DATACRAFT_MY_HOST");
        my.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        my.user = var("DATACRAFT_MY_USER");
        my.password = var("DATACRAFT_MY_PASSWORD");
        my.database = var("DATACRAFT_MY_DB");
        let db = my.database.clone();
        let dir = std::env::temp_dir().join(format!("datacraft-copy-my-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = people_db(&dir);
        let back = dir.join("back.db");
        rusqlite::Connection::open(&back).unwrap();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(24, &source)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 24, .. }));
        w.send(Request::Connect(my));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 23, .. }));
        w.send(Request::Connect(sqlite_cfg(25, &back)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 25, .. }));
        w.send(Request::RunDdl { conn: 23, db: None, statements: vec!["drop table if exists dc_people".into()], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { result: Ok(_), .. }));

        assert_eq!(run_copy(&w, 24, 23, 1, copy_request(&db, "dc_people", "", 100, true, false)), Ok(40));
        let mut back_req = copy_request("main", "people", "ok = 1", 100, true, false);
        back_req.source_db.clone_from(&db);
        back_req.source_schema.clone_from(&db);
        back_req.source_table = "dc_people".into();
        back_req.columns[2].data_type = "tinyint(1)".into();
        assert_eq!(run_copy(&w, 23, 25, 2, back_req), Ok(20));
        let name: String = rusqlite::Connection::open(&back).unwrap().query_row("select name from people where id = 3", [], |r| r.get(0)).unwrap();
        assert_eq!(name, "O'Brien 3");
        w.send(Request::RunDdl { conn: 23, db: None, statements: vec!["drop table dc_people".into()], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// customers ← orders ← lines → products, and customers → staff (whose boss is staff).
    const SHOP: &str = "create table staff(id integer primary key, name text, boss integer references staff(id));
        create table customers(id integer primary key, name text, email text, manager_id integer references staff(id));
        create table products(id integer primary key, title text);
        create table orders(id integer primary key, customer_id integer references customers(id), note text);
        create table lines(id integer primary key, order_id integer references orders(id), product_id integer references products(id), qty integer);
        insert into staff values (1, 'Big Boss', null), (2, 'Middle', 1), (3, 'Ann Staff', 2), (4, 'Other', null);
        insert into customers values (1, 'Ana Real', 'ana@corp.com', 3), (2, 'Luis', 'luis@corp.com', null), (3, 'Eva', 'eva@corp.com', 4);
        insert into products values (1, 'Pen'), (2, 'Ink'), (3, 'Pad'), (4, 'Box');
        insert into orders values (1, 1, 'a'), (2, 1, 'b'), (3, 2, 'c'), (4, 3, 'd');
        insert into lines values (1, 1, 1, 5), (2, 1, 2, 1), (3, 3, 3, 2);";

    fn customers_request(target_db: &str) -> crate::db::copy::CopyRequest {
        let col = |name: &str, ty: &str, pk: bool| crate::db::ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk };
        let columns = vec![col("id", "integer", true), col("name", "text", false), col("email", "text", false), col("manager_id", "integer", false)];
        crate::db::copy::CopyRequest {
            source_db: "main".into(),
            source_schema: "main".into(),
            source_table: "customers".into(),
            target_names: columns.iter().map(|c| c.name.clone()).collect(),
            masks: vec![crate::db::copy::Mask::Keep, crate::db::copy::Mask::Name, crate::db::copy::Mask::Email, crate::db::copy::Mask::Keep],
            columns,
            filter: "id = 1".into(),
            percent: 100,
            target_db: target_db.into(),
            target_schema: target_db.into(),
            target_table: "customers".into(),
            create: true,
            clear: false,
            mask_key: "test-key".into(),
            parents: true,
            children: true,
            mask_related: true,
        }
    }

    /// A customer with its orders, their lines and products, and its manager's chain of bosses.
    #[test]
    fn copies_a_subset_that_keeps_foreign_keys() {
        let dir = std::env::temp_dir().join(format!("datacraft-subset-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (source, target) = (dir.join("shop.db"), dir.join("dev.db"));
        rusqlite::Connection::open(&source).unwrap().execute_batch(SHOP).unwrap();
        rusqlite::Connection::open(&target).unwrap();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(31, &source)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 31, .. }));
        w.send(Request::Connect(sqlite_cfg(32, &target)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 32, .. }));

        let mut report = copy_report(&w, 31, 32, 1, customers_request("main")).unwrap();
        report.sort();
        let expect = [("customers", 1), ("lines", 2), ("orders", 2), ("products", 2), ("staff", 3)];
        assert_eq!(report, expect.map(|(t, n)| (t.to_string(), n)).to_vec());

        let db = rusqlite::Connection::open(&target).unwrap();
        let q = |sql: &str| db.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!(q("select group_concat(id) from (select id from staff order by rowid)"), "1,2,3");
        assert_eq!(q("select group_concat(title) from (select title from products order by id)"), "Pen,Ink");
        // The customer's name and email are masked; the related staff names too (by column name).
        assert_ne!(q("select name from customers"), "Ana Real");
        assert!(q("select email from customers").ends_with("@example.com"));
        assert_ne!(q("select name from staff where id = 3"), "Ann Staff");
        assert_eq!(q("select cast(sum(qty) as text) from lines"), "6");

        // Again: what is already there is skipped, so nothing new is added.
        let mut again = customers_request("main");
        again.create = false;
        let report = copy_report(&w, 31, 32, 2, again).unwrap();
        assert!(report.iter().all(|(_, n)| *n == 0), "{report:?}");

        // Without the key column the foreign keys need, it refuses.
        let mut narrow = customers_request("main");
        narrow.columns.truncate(3);
        narrow.target_names.truncate(3);
        narrow.masks.truncate(3);
        narrow.create = false;
        assert!(run_copy(&w, 31, 32, 3, narrow).unwrap_err().contains("manager_id"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SQLite → MariaDB into tables with enforced foreign keys: the insert order must be right.
    #[test]
    #[ignore]
    fn copies_a_subset_into_mariadb_with_foreign_keys() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut my = ConnectionConfig::new(33, Driver::MariaDb);
        my.host = var("DATACRAFT_MY_HOST");
        my.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        my.user = var("DATACRAFT_MY_USER");
        my.password = var("DATACRAFT_MY_PASSWORD");
        my.database = var("DATACRAFT_MY_DB");
        let db = my.database.clone();
        let dir = std::env::temp_dir().join(format!("datacraft-subset-my-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("shop.db");
        rusqlite::Connection::open(&source).unwrap().execute_batch(SHOP).unwrap();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(34, &source)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 34, .. }));
        w.send(Request::Connect(my));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 33, .. }));
        let ddl = [
            "drop table if exists `lines`, orders, products, customers, staff",
            "create table staff(id int primary key, name varchar(50), boss int, foreign key (boss) references staff(id))",
            "create table customers(id int primary key, name varchar(50), email varchar(80), manager_id int, foreign key (manager_id) references staff(id))",
            "create table products(id int primary key, title varchar(50))",
            "create table orders(id int primary key, customer_id int, note varchar(10), foreign key (customer_id) references customers(id))",
            "create table `lines`(id int primary key, order_id int, product_id int, qty int, foreign key (order_id) references orders(id), foreign key (product_id) references products(id))",
        ];
        w.send(Request::RunDdl { conn: 33, db: None, statements: ddl.map(String::from).to_vec(), close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { result: Ok(_), .. }));

        let mut req = customers_request(&db);
        req.create = false;
        assert_eq!(run_copy(&w, 34, 33, 1, req.clone()), Ok(10));
        assert_eq!(run_copy(&w, 34, 33, 2, req), Ok(0));

        // And back: MariaDB's foreign keys are read from information_schema.
        let back = dir.join("back.db");
        rusqlite::Connection::open(&back).unwrap();
        w.send(Request::Connect(sqlite_cfg(35, &back)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 35, .. }));
        let mut back_req = customers_request("main");
        back_req.source_db.clone_from(&db);
        back_req.source_schema.clone_from(&db);
        back_req.filter.clear();
        back_req.masks = vec![crate::db::copy::Mask::Keep; 4];
        back_req.mask_related = false;
        let mut report = copy_report(&w, 33, 35, 3, back_req).unwrap();
        report.sort();
        assert_eq!(report, [("customers", 1), ("lines", 2), ("orders", 2), ("products", 2), ("staff", 3)].map(|(t, n)| (t.to_string(), n)).to_vec());
        w.send(Request::RunDdl { conn: 33, db: None, statements: vec![ddl[0].into()], close_db: None, transaction: false });
        wait_for(&w, |r| matches!(r, Response::DdlDone { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn first_rows(w: &Worker, conn: ConnId, console: ConsoleId, statements: &[&str]) -> Vec<ResultSet> {
        w.send(Request::Execute { conn, db: None, console, statements: statements.iter().map(|s| s.to_string()).collect(), max_rows: 100 });
        let r = wait_for(w, |r| matches!(r, Response::Executed { console: c, .. } if *c == console));
        let Response::Executed { results, error, .. } = r else { unreachable!() };
        assert!(error.is_none(), "{error:?}");
        results
    }

    /// Needs a Redis or Valkey server: `DATACRAFT_REDIS_PORT` (on 127.0.0.1, no password).
    #[test]
    #[ignore]
    fn redis_commands_and_key_groups() {
        let mut cfg = ConnectionConfig::new(41, Driver::Redis);
        cfg.host = "127.0.0.1".into();
        cfg.port = std::env::var("DATACRAFT_REDIS_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(6379);
        cfg.database = "5".into();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        let r = wait_for(&w, |r| matches!(r, Response::Connected { conn: 41, .. } | Response::ConnectFailed { conn: 41, .. }));
        assert!(matches!(r, Response::Connected { .. }), "could not connect");
        let rs = first_rows(&w, 41, 1, &["FLUSHDB\nHSET dc:user:1 name \"Ana P\" age 3\nSET dc:greeting 'hola mundo' EX 60\nRPUSH dc:q a b"]);
        assert_eq!(rs.len(), 4);
        let rs = first_rows(&w, 41, 2, &["HGETALL dc:user:1", "SCAN 0 MATCH dc:* COUNT 1000", "GET missing", "INFO server"]);
        assert_eq!(rs[0].columns, vec!["field", "value"]);
        assert_eq!(rs[0].rows[0], vec![Some("name".into()), Some("Ana P".into())]);
        let keys: Vec<_> = rs[1].rows.iter().map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap())).collect();
        assert_eq!(keys, [("dc:greeting", "string"), ("dc:q", "list"), ("dc:user:1", "hash")].map(|(a, b)| (a.to_string(), b.to_string())));
        assert!(rs[1].rows[0][2].as_deref().is_some_and(|t| t.parse::<i64>().unwrap() > 0));
        assert_eq!(rs[1].rows[1][3].as_deref(), Some(r#"["a","b"]"#));
        assert_eq!(rs[2].rows, vec![vec![None]]);
        assert!(rs[3].rows.iter().any(|r| r[1].as_deref() == Some("tcp_port")));
        w.send(Request::LoadDatabase { conn: 41, db: "db5".into() });
        let r = wait_for(&w, |r| matches!(r, Response::DatabaseLoaded { conn: 41, .. }));
        let Response::DatabaseLoaded { result: Ok(schemas), .. } = r else { panic!("introspection failed") };
        let names: Vec<_> = schemas[0].tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["*", "dc:*"]);

        // Grid edits on the `dc:*` group, as the data editor builds them.
        let group = &schemas[0].tables[1];
        let rs = first_rows(&w, 41, 4, &["SCAN 0 MATCH dc:* COUNT 1000"]).remove(0);
        let target = crate::ui::editing::target_for(&rs.columns, "db5", group.kind, &group.name, &group.columns).unwrap();
        let types: std::collections::HashMap<String, String> =
            rs.rows.iter().map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap())).collect();
        let row = |k: &str| crate::ui::editing::RowRef::Existing(rs.rows.iter().position(|r| r[0].as_deref() == Some(k)).unwrap());
        let mut st = crate::ui::editing::EditState::default();
        st.set_value(&rs, row("dc:greeting"), 3, Some("adiós \"mundo\"".into()));
        st.set_value(&rs, row("dc:greeting"), 2, None);
        let n = st.add_row(4);
        st.set_value(&rs, crate::ui::editing::RowRef::New(n), 0, Some("dc:new key".into()));
        st.set_value(&rs, crate::ui::editing::RowRef::New(n), 2, Some("100".into()));
        st.set_value(&rs, crate::ui::editing::RowRef::New(n), 3, Some("x y".into()));
        st.delete_rows(&rs, &target, &[row("dc:q")]);
        let changes = st.build_changes(&rs, &target).unwrap();
        let statements = crate::edit::change_statements(Driver::Redis, "db5", "dc:*", &changes, &types).unwrap();
        assert_eq!(apply(&w, 41, 5, statements), Ok(4));
        let rs = first_rows(&w, 41, 6, &["GET dc:greeting", "TTL dc:greeting", "GET \"dc:new key\"", "TTL \"dc:new key\"", "EXISTS dc:q"]);
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("adiós \"mundo\""));
        assert_eq!(rs[1].rows[0][0].as_deref(), Some("-1"));
        assert_eq!(rs[2].rows[0][0].as_deref(), Some("x y"));
        assert!(rs[3].rows[0][0].as_deref().is_some_and(|t| t.parse::<i64>().unwrap() > 0));
        assert_eq!(rs[4].rows[0][0].as_deref(), Some("0"));
        // A failed precondition (missing key, existing key) saves nothing.
        let err = apply(&w, 41, 7, vec!["DEL dc:greeting".into(), "EXPIRE dc:missing 10".into()]).unwrap_err();
        assert!(err.contains("EXPIRE dc:missing 10"), "{err}");
        assert!(apply(&w, 41, 8, vec!["SET dc:greeting x NX".into()]).unwrap_err().contains("dc:greeting"));
        assert!(apply(&w, 41, 9, vec!["SET dc:user:1 x XX KEEPTTL".into()]).unwrap_err().contains("hash"));
        let rs = first_rows(&w, 41, 10, &["GET dc:greeting"]);
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("adiós \"mundo\""));
        first_rows(&w, 41, 3, &["FLUSHDB"]);
    }

    /// Saves data editor statements through the worker.
    fn apply(w: &Worker, conn: ConnId, console: ConsoleId, statements: Vec<String>) -> Result<u64, String> {
        w.send(Request::ApplyChanges { conn, db: None, console, statements });
        let r = wait_for(w, |r| matches!(r, Response::ChangesApplied { console: c, .. } if *c == console));
        let Response::ChangesApplied { result, .. } = r else { unreachable!() };
        result
    }

    /// Needs a MongoDB server: `DATACRAFT_MONGO_PORT` (on 127.0.0.1, no authentication).
    #[test]
    #[ignore]
    fn mongodb_commands_and_collections() {
        let mut cfg = ConnectionConfig::new(42, Driver::MongoDb);
        cfg.host = "127.0.0.1".into();
        cfg.port = std::env::var("DATACRAFT_MONGO_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(27017);
        cfg.database = "dc_test".into();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        let r = wait_for(&w, |r| matches!(r, Response::Connected { conn: 42, .. } | Response::ConnectFailed { conn: 42, .. }));
        assert!(matches!(r, Response::Connected { .. }), "could not connect");
        let script = "db.users.drop()\ndb.users.insertMany([{_id: 1, name: 'Ana', tags: ['a'], at: ISODate('2026-01-02')}, {_id: 2, name: 'Luis', age: 41}])\ndb.users.updateOne({_id: 2}, {$inc: {age: 1}})";
        let stmts: Vec<String> = crate::sql::split_statements(script, Driver::MongoDb).into_iter().map(|s| s.text).collect();
        assert_eq!(stmts.len(), 3);
        let rs = first_rows(&w, 42, 1, &stmts.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(rs[1].affected, Some(2));
        assert_eq!(rs[2].affected, Some(1));
        let rs = first_rows(&w, 42, 2, &[
            "db.users.find({}, {name: 1, age: 1}).sort({_id: -1}).limit(1)",
            "db.users.aggregate([{$group: {_id: null, n: {$sum: 1}}}])",
            "db.users.countDocuments({name: /^a/i})",
            "db.getCollection(\"users\").find({_id: 1})",
        ]);
        assert_eq!(rs[0].columns, vec!["_id", "name", "age"]);
        assert_eq!(rs[0].rows, vec![vec![Some("2".into()), Some("Luis".into()), Some("42".into())]]);
        assert_eq!(rs[1].rows[0][1].as_deref(), Some("2"));
        assert_eq!(rs[2].rows[0][0].as_deref(), Some("1"));
        assert_eq!(rs[3].rows[0][2].as_deref(), Some(r#"["a"]"#));
        assert_eq!(rs[3].rows[0][3].as_deref(), Some("2026-01-02T00:00:00Z"));
        w.send(Request::LoadDatabase { conn: 42, db: "dc_test".into() });
        let r = wait_for(&w, |r| matches!(r, Response::DatabaseLoaded { conn: 42, .. }));
        let Response::DatabaseLoaded { result: Ok(schemas), .. } = r else { panic!("introspection failed") };
        let users = schemas[0].tables.iter().find(|t| t.name == "users").expect("users");
        let cols: Vec<_> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, vec!["_id", "name", "tags", "at", "age"]);

        // Grid edits on the collection, as the data editor builds them.
        let rs = first_rows(&w, 42, 4, &[&crate::app::table_query(Driver::MongoDb, "dc_test", "users", "", "{_id: 1}", 100)]).remove(0);
        let target = crate::ui::editing::document_target(&rs.columns, "dc_test", users.kind, &users.name, &users.columns).unwrap();
        let types: std::collections::HashMap<String, String> = users.columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect();
        let col = |n: &str| rs.columns.iter().position(|c| c == n).unwrap();
        use crate::ui::editing::RowRef;
        let mut st = crate::ui::editing::EditState::default();
        st.set_value(&rs, RowRef::Existing(1), col("name"), Some("Luis \"M\"".into()));
        st.set_value(&rs, RowRef::Existing(1), col("age"), Some("50".into()));
        st.set_value(&rs, RowRef::Existing(0), col("tags"), Some(r#"["a",{"k":1}]"#.into()));
        let n = st.add_row(rs.columns.len());
        st.set_value(&rs, RowRef::New(n), col("_id"), Some("3".into()));
        st.set_value(&rs, RowRef::New(n), col("name"), Some("Eva".into()));
        st.set_value(&rs, RowRef::New(n), col("at"), Some("2026-03-04T05:06:07Z".into()));
        st.delete_rows(&rs, &target, &[RowRef::Existing(0)]);
        let changes = st.build_changes(&rs, &target).unwrap();
        let statements = crate::edit::change_statements(Driver::MongoDb, "dc_test", "users", &changes, &types).unwrap();
        assert_eq!(apply(&w, 42, 5, statements), Ok(3));
        let rs = first_rows(&w, 42, 6, &["db.users.find({}).sort({_id: 1})", "db.users.find({at: {$type: 'date'}, _id: 3})", "db.users.find({age: {$type: 'int'}})"]);
        let ids: Vec<_> = rs[0].rows.iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(ids, vec!["2", "3"]);
        assert_eq!(rs[1].rows.len(), 1);
        assert_eq!(rs[2].rows.len(), 1);
        // A change that matches no document fails (and, on a replica set, rolls the rest back).
        let err = apply(&w, 42, 7, vec![r#"db.getCollection("users").deleteOne({"_id": 99})"#.into()]).unwrap_err();
        assert!(err.contains("deleteOne"), "{err}");
        assert!(apply(&w, 42, 8, vec!["db.users.drop()".into()]).is_err());
        first_rows(&w, 42, 3, &["db.users.drop()"]);
    }

    /// SQLite → MongoDB (masking the names), MongoDB → MongoDB (again: duplicates skipped) and
    /// MongoDB → SQLite (creating the table). Needs `DATACRAFT_MONGO_PORT` like the test above.
    #[test]
    #[ignore]
    fn copies_between_sqlite_and_mongodb() {
        use crate::db::copy::Mask;
        let dir = std::env::temp_dir().join(format!("datacraft-copy-mongo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = people_db(&dir);
        let target = dir.join("target.db");
        rusqlite::Connection::open(&target).unwrap();
        let mut cfg = ConnectionConfig::new(43, Driver::MongoDb);
        cfg.host = "127.0.0.1".into();
        cfg.port = std::env::var("DATACRAFT_MONGO_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(27017);
        cfg.database = "dc_test".into();
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(44, &source)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 44, .. }));
        w.send(Request::Connect(sqlite_cfg(45, &target)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 45, .. }));
        w.send(Request::Connect(cfg));
        let r = wait_for(&w, |r| matches!(r, Response::Connected { conn: 43, .. } | Response::ConnectFailed { conn: 43, .. }));
        assert!(matches!(r, Response::Connected { .. }), "could not connect");
        first_rows(&w, 43, 1, &["db.dc_people.drop()", "db.dc_people2.drop()"]);

        // SQLite → MongoDB: typed fields, fake names.
        let mut req = copy_request("dc_test", "dc_people", "", 100, true, false);
        req.masks[1] = Mask::Name;
        assert_eq!(run_copy(&w, 44, 43, 1, req), Ok(40));
        let rs = first_rows(&w, 43, 2, &[
            "db.dc_people.countDocuments({id: {$type: 'int'}, ok: {$type: 'bool'}})",
            "db.dc_people.countDocuments({ok: true})",
            "db.dc_people.countDocuments({name: /^O'Brien/})",
        ]);
        let count = |i: usize| rs[i].rows[0][0].clone().unwrap();
        assert_eq!((count(0), count(1), count(2)), ("40".into(), "20".into(), "0".into()));

        // MongoDB → MongoDB, twice: the second time every `_id` is already there.
        let col = |name: &str, ty: &str| crate::db::ColumnInfo { name: name.into(), data_type: ty.into(), nullable: name != "_id", primary_key: name == "_id" };
        let columns = vec![col("_id", "objectId"), col("id", "int"), col("name", "string"), col("ok", "bool")];
        let mongo_req = |target_table: &str, filter: &str, create: bool| crate::db::copy::CopyRequest {
            source_db: "dc_test".into(),
            source_schema: "dc_test".into(),
            source_table: "dc_people".into(),
            target_names: columns.iter().map(|c| c.name.clone()).collect(),
            columns: columns.clone(),
            filter: filter.into(),
            percent: 100,
            target_db: if target_table.starts_with("dc_") { "dc_test".into() } else { "main".into() },
            target_schema: if target_table.starts_with("dc_") { "dc_test".into() } else { "main".into() },
            target_table: target_table.into(),
            create,
            clear: false,
            masks: vec![Mask::Keep; 4],
            mask_key: "test-key".into(),
            parents: false,
            children: false,
            mask_related: false,
        };
        assert_eq!(run_copy(&w, 43, 43, 2, mongo_req("dc_people2", "", true)), Ok(40));
        assert_eq!(run_copy(&w, 43, 43, 3, mongo_req("dc_people2", "", false)), Ok(0));
        let rs = first_rows(&w, 43, 3, &["db.dc_people2.countDocuments({_id: {$type: 'objectId'}, id: {$type: 'int'}})"]);
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("40"));

        // MongoDB → SQLite with a filter (braces optional), creating the table.
        assert_eq!(run_copy(&w, 43, 45, 4, mongo_req("from_mongo", "ok: true", true)), Ok(20));
        let db = rusqlite::Connection::open(&target).unwrap();
        let (n, ids, oks, keys, real): (i64, i64, i64, i64, i64) = db
            .query_row(
                "select count(*), sum(typeof(id) = 'integer'), sum(ok = 1), sum(length(_id) = 24), sum(name like 'O''Brien%') from from_mongo",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((n, ids, oks, keys, real), (20, 20, 20, 20, 0));
        first_rows(&w, 43, 4, &["db.dc_people.drop()", "db.dc_people2.drop()"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    // ---- "Modify Table…" ------------------------------------------------------------------

    fn table_details(w: &Worker, conn: ConnId, db: &str, schema: &str, table: &str, request_id: u64) -> crate::db::alter::TableDetails {
        w.send(Request::TableDetails { conn, db: db.into(), schema: schema.into(), table: table.into(), request_id });
        let r = wait_for(w, |r| matches!(r, Response::TableDetails { request_id: id, .. } if *id == request_id));
        let Response::TableDetails { result, .. } = r else { unreachable!() };
        *result.unwrap()
    }

    fn run_ddl(w: &Worker, conn: ConnId, statements: Vec<String>, transaction: bool) -> Result<(), String> {
        w.send(Request::RunDdl { conn, db: None, statements, close_db: None, transaction });
        let Response::DdlDone { result, .. } = wait_for(w, |r| matches!(r, Response::DdlDone { .. })) else { unreachable!() };
        result
    }

    /// Renames, retypes, makes NOT NULL with a new default, drops and adds columns, changes the
    /// primary key, drops and adds indexes and renames the table, then reads the structure back.
    fn modify_table_roundtrip(cfg: ConnectionConfig, schema: &str, setup: &[&str]) {
        use crate::db::alter::{self, EditColumn, EditIndex, TableEdit};
        use crate::ui::create_dialog::NewColumn;
        let (conn, driver, db) = (cfg.id, cfg.driver, cfg.database.clone());
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        let r = wait_for(&w, |r| matches!(r, Response::Connected { .. } | Response::ConnectFailed { .. }));
        assert!(matches!(r, Response::Connected { .. }), "could not connect");
        let _ = run_ddl(&w, conn, vec!["drop table if exists dc_goods".into(), "drop table if exists dc_mod".into()], false);
        run_ddl(&w, conn, setup.iter().map(|s| s.to_string()).collect(), false).unwrap();

        let d = table_details(&w, conn, &db, schema, "dc_mod", 1);
        let names: Vec<_> = d.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "price", "note"]);
        assert_eq!(d.primary_key, ["id"]);
        assert_eq!(d.indexes.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["dc_mod_name"]);
        assert!(!d.columns[1].nullable && d.columns[2].nullable);
        assert_eq!(d.columns[2].default.as_deref(), Some("0"));

        let mut cols = alter::initial_columns(&d);
        cols[1].column.name = "title".into();
        cols[1].column.data_type = "varchar(100)".into();
        cols[1].column.primary_key = true;
        cols[2].column.not_null = true;
        cols[2].column.default = "1".into();
        cols.remove(3);
        cols.push(EditColumn {
            original: None,
            column: NewColumn { name: "email".into(), data_type: "varchar(255)".into(), not_null: false, primary_key: false, default: String::new() },
        });
        let indexes = vec![EditIndex { original: None, name: "dc_goods_email".into(), columns: "email".into(), unique: true }];
        let edit = TableEdit { schema, table: "dc_mod", new_name: "dc_goods", columns: &cols, indexes: &indexes };
        let plan = alter::plan(driver, &d, &edit).unwrap();
        run_ddl(&w, conn, plan.statements.clone(), plan.transaction).unwrap_or_else(|e| panic!("{e}\n{:#?}", plan.statements));

        let d = table_details(&w, conn, &db, schema, "dc_goods", 2);
        let names: Vec<_> = d.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "title", "price", "email"]);
        assert_eq!(d.primary_key, ["id", "title"]);
        assert_eq!(d.indexes.iter().map(|x| (x.name.as_str(), x.unique)).collect::<Vec<_>>(), [("dc_goods_email", true)]);
        assert!(d.columns[1].data_type.contains("100"), "{}", d.columns[1].data_type);
        assert!(!d.columns[2].nullable);
        assert_eq!(d.columns[2].default.as_deref(), Some("1"));
        // The rows survive, under the new names.
        w.send(Request::Execute { conn, db: None, console: 5, statements: vec!["select id, title, price from dc_goods".into()], max_rows: 10 });
        let Response::Executed { results, error, .. } = wait_for(&w, |r| matches!(r, Response::Executed { console: 5, .. })) else { unreachable!() };
        assert!(error.is_none(), "{error:?}");
        assert_eq!(results[0].rows, vec![vec![Some("1".into()), Some("pen".into()), Some("3".into())]]);

        // A rename alone; then dropping the column an index uses also drops the index.
        let mut cols = alter::initial_columns(&d);
        cols[2].column.name = "cost".into();
        cols.remove(3);
        let indexes = alter::initial_indexes(&d);
        let edit = TableEdit { schema, table: "dc_goods", new_name: "dc_goods", columns: &cols, indexes: &indexes };
        let plan = alter::plan(driver, &d, &edit).unwrap();
        run_ddl(&w, conn, plan.statements.clone(), plan.transaction).unwrap_or_else(|e| panic!("{e}\n{:#?}", plan.statements));
        let d = table_details(&w, conn, &db, schema, "dc_goods", 3);
        assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "title", "cost"]);
        assert!(d.indexes.is_empty());
        run_ddl(&w, conn, vec!["drop table dc_goods".into()], false).unwrap();
    }

    #[test]
    fn sqlite_modify_table() {
        let dir = std::env::temp_dir().join(format!("datacraft-modify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        rusqlite::Connection::open(&path).unwrap();
        let mut cfg = ConnectionConfig::new(14, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        modify_table_roundtrip(
            cfg.clone(),
            "main",
            &[
                "create table dc_mod (id integer primary key autoincrement, name varchar(50) not null, price integer default 0, note text)",
                "create index dc_mod_name on dc_mod (name)",
                "insert into dc_mod (name, price, note) values ('pen', 3, 'x')",
            ],
        );
        // A rebuild keeps AUTOINCREMENT and works with a view on the table; a CHECK constraint
        // refuses it.
        use crate::db::alter::{self, TableEdit};
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { .. }));
        run_ddl(
            &w,
            14,
            vec![
                // Apple's SQLite defaults to legacy renames; the bundled one (Windows, Linux) doesn't.
                "pragma legacy_alter_table = off".into(),
                "create table dc_auto (id integer primary key autoincrement, n int)".into(),
                "create view dc_view as select id, n from dc_auto".into(),
                "insert into dc_auto (n) values (1)".into(),
            ],
            false,
        )
        .unwrap();
        let d = table_details(&w, 14, "main", "main", "dc_auto", 4);
        assert!(d.autoincrement && d.rebuild_blocker.is_none());
        let mut cols = alter::initial_columns(&d);
        cols[1].column.data_type = "bigint".into();
        cols[1].column.not_null = true;
        let plan = alter::plan(Driver::Sqlite, &d, &TableEdit { schema: "main", table: "dc_auto", new_name: "dc_auto", columns: &cols, indexes: &[] }).unwrap();
        run_ddl(&w, 14, plan.statements.clone(), plan.transaction).unwrap_or_else(|e| panic!("{e}\n{:#?}", plan.statements));
        let d = table_details(&w, 14, "main", "main", "dc_auto", 5);
        assert!(d.autoincrement);
        assert_eq!(d.columns[1].data_type, "bigint");
        assert!(!d.columns[1].nullable);
        w.send(Request::Execute { conn: 14, db: None, console: 6, statements: vec!["select n from dc_view".into()], max_rows: 10 });
        let Response::Executed { results, error, .. } = wait_for(&w, |r| matches!(r, Response::Executed { console: 6, .. })) else { unreachable!() };
        assert!(error.is_none(), "{error:?}");
        assert_eq!(results[0].rows, vec![vec![Some("1".into())]]);
        run_ddl(&w, 14, vec!["create table dc_check (id integer primary key, n int check (n > 0))".into()], false).unwrap();
        let d = table_details(&w, 14, "main", "main", "dc_check", 6);
        let mut cols = alter::initial_columns(&d);
        cols[1].column.not_null = true;
        assert!(alter::plan(Driver::Sqlite, &d, &TableEdit { schema: "main", table: "dc_check", new_name: "dc_check", columns: &cols, indexes: &[] }).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live: `DATACRAFT_MY_HOST/PORT/USER/PASSWORD/DB`.
    #[test]
    #[ignore]
    fn mysql_modify_table() {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(15, Driver::MariaDb);
        cfg.host = var("DATACRAFT_MY_HOST");
        cfg.port = var("DATACRAFT_MY_PORT").parse().unwrap_or(3306);
        cfg.user = var("DATACRAFT_MY_USER");
        cfg.password = var("DATACRAFT_MY_PASSWORD");
        cfg.database = var("DATACRAFT_MY_DB");
        let schema = cfg.database.clone();
        modify_table_roundtrip(
            cfg,
            &schema,
            &[
                "create table dc_mod (id int auto_increment primary key, name varchar(50) not null, price int default 0, note text)",
                "create index dc_mod_name on dc_mod (name)",
                "insert into dc_mod (name, price, note) values ('pen', 3, 'x')",
            ],
        );
    }

    /// A grid save whose second change fails: on a replica set (`DATACRAFT_MONGO_RS=1`) nothing
    /// is kept; on a standalone server the first change stays and the error says so.
    #[test]
    #[ignore]
    fn mongodb_partial_save() {
        let mut cfg = ConnectionConfig::new(46, Driver::MongoDb);
        cfg.host = "127.0.0.1".into();
        cfg.port = std::env::var("DATACRAFT_MONGO_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(27017);
        cfg.database = "dc_test".into();
        let replica_set = std::env::var("DATACRAFT_MONGO_RS").is_ok_and(|v| v == "1");
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg));
        let r = wait_for(&w, |r| matches!(r, Response::Connected { conn: 46, .. } | Response::ConnectFailed { conn: 46, .. }));
        assert!(matches!(r, Response::Connected { .. }), "could not connect");
        first_rows(&w, 46, 1, &["db.partial.drop()", "db.partial.insertOne({_id: 1, n: 1})"]);
        let statements = vec![
            "db.getCollection(\"partial\").updateOne({\"_id\": 1}, {\"$set\": {\"n\": 2}})".to_string(),
            "db.getCollection(\"partial\").updateOne({\"_id\": 99}, {\"$set\": {\"n\": 3}})".to_string(),
        ];
        w.send(Request::ApplyChanges { conn: 46, db: None, console: 2, statements });
        let r = wait_for(&w, |r| matches!(r, Response::ChangesApplied { console: 2, .. }));
        let Response::ChangesApplied { result, .. } = r else { unreachable!() };
        let error = result.expect_err("the second change matches no document");
        let rs = first_rows(&w, 46, 3, &["db.partial.find({_id: 1})"]);
        let n = rs[0].rows[0][1].clone();
        if replica_set {
            assert_eq!(n.as_deref(), Some("1"), "rolled back: {error}");
        } else {
            assert_eq!(n.as_deref(), Some("2"), "kept: {error}");
        }
        first_rows(&w, 46, 4, &["db.partial.drop()"]);
    }
}
