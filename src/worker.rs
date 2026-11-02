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
    TableModel { conn: ConnId, db: String, request_id: u64, tables: Vec<(String, String)>, cancel: Arc<std::sync::atomic::AtomicBool> },
    SaveModel { path: std::path::PathBuf, contents: String },
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
    /// "Compare Schemas…": reads both schemas on sessions of their own.
    SchemaCompare {
        source: ConnId,
        target: ConnId,
        request_id: u64,
        sides: Box<(crate::db::schema_compare::read::Side, crate::db::schema_compare::read::Side)>,
        progress: Arc<crate::db::schema_compare::read::Progress>,
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
    TableGrants {
        conn: ConnId, db: String, schema: String, table: String, request_id: u64,
        request: crate::db::table_privileges::Request, cancel: Arc<std::sync::atomic::AtomicBool>,
    },
    /// Runs statements in order; stops at the first error.
    /// `db`: database to run them in (`None` = the connection's default database).
    /// `manual`: on the console's own session, inside the transaction it keeps open.
    Execute {
        conn: ConnId,
        db: Option<String>,
        console: ConsoleId,
        statements: Vec<String>,
        max_rows: usize,
        manual: bool,
    },
    /// Commits (or rolls back) a console's Manual transaction.
    EndTransaction { console: ConsoleId, commit: bool },
    /// Closes a console's own Manual session (tab closed, back to Auto…): an open transaction
    /// is rolled back.
    ReleaseConsole { console: ConsoleId },
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
    /// Session Monitor: the server's sessions, on the connection's monitor session.
    Sessions { conn: ConnId, request_id: u64 },
    /// Session Monitor: cancels a query or terminates a session, on the monitor session.
    /// `epoch`: that of the list the session was picked from (refused after a reconnect).
    SessionAction { conn: ConnId, request_id: u64, epoch: u64, action: crate::db::sessions::Action },
    /// The last Session Monitor window of the connection closed: its session is closed too.
    CloseSessionMonitor(ConnId),
}

pub enum Response {
    TableShortcut { result: Result<std::path::PathBuf, String> },
    TableModel { request_id: u64, result: Result<crate::db::table_model::Model, String> },
    ModelSaved { result: Result<std::path::PathBuf, String> },
    TableDuplicate { conn: ConnId, request_id: u64, result: Result<crate::db::table_duplicate::Outcome, String> },
    TableExported { conn: ConnId, request_id: u64, result: Result<(std::path::PathBuf, u64), String> },
    TableReportOpened { request_id: u64, result: Result<std::path::PathBuf, String> },
    Backup { conn: ConnId, id: uuid::Uuid, request: Box<crate::db::backup::BackupRequest>, result: Result<Option<u64>, String> },
    InspectTable { console: ConsoleId, generation: u64, page_rows: Option<u64>, result: Result<Box<crate::db::object_info::ObjectInfo>, String> },
    SchemaSearch { request_id: u64, result: Result<crate::db::schema_search::SearchReport, String> },
    /// Result of `CopyTable`: rows copied to each target table.
    TableCopied { request_id: u64, result: Result<Vec<(String, u64)>, String> },
    /// Result of `SchemaCompare`: the source and target models.
    SchemaCompared { request_id: u64, result: Result<Box<(crate::db::schema_compare::SchemaModel, crate::db::schema_compare::SchemaModel)>, String> },
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
    /// After the `Executed` of a Manual `Execute`: the console's transaction now, and why it
    /// ended if it ended by itself.
    Transaction { console: ConsoleId, tx: Option<TxInfo>, note: Option<TxNote> },
    /// Result of `EndTransaction`; `tx` is the transaction if it is still open (a commit that failed).
    TransactionEnded { console: ConsoleId, commit: bool, result: Result<(), String>, tx: Option<TxInfo>, elapsed: std::time::Duration },
    /// Result of `ApplyChanges`: modified rows or an error (in which case nothing was saved).
    ChangesApplied { console: ConsoleId, result: Result<u64, String> },
    /// Result of `RunDdl`.
    DdlDone { conn: ConnId, result: Result<(), String> },
    RoutineSaved { console: ConsoleId, result: Result<(), String>, identity: Option<crate::db::routines::RoutineRef> },
    TableDetails { request_id: u64, result: Result<Box<crate::db::alter::TableDetails>, String> },
    TableGrants { conn: ConnId, request_id: u64, result: Result<crate::db::table_privileges::Grants, String> },
    DatabaseChoices { conn: ConnId, result: Result<crate::db::create_db::DatabaseChoices, String> },
    /// Result of `Users`.
    Users { conn: ConnId, result: Result<crate::db::users::UsersReply, String> },
    /// `epoch`: the connection's when the list was read, to send back with an action.
    Sessions { conn: ConnId, request_id: u64, epoch: u64, result: Result<crate::db::sessions::SessionList, String> },
    SessionAction { conn: ConnId, request_id: u64, result: Result<(), String> },
}

/// A console's open Manual transaction, as the worker last saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxInfo {
    pub began: Instant,
    /// Statements that ran in it since it began.
    pub statements: usize,
    /// A statement failed and the server aborted the transaction (PostgreSQL, a doomed SQL
    /// Server transaction): only a rollback is possible.
    pub aborted: bool,
}

/// Why a console's Manual transaction ended by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxNote {
    /// Its session was lost or closed (a cancel that had to abandon the query): rolled back.
    Lost,
    /// A DDL statement committed it (MySQL, MariaDB, Oracle, Snowflake).
    ImplicitCommit,
    /// The server rolled it back after an error (or answered a COMMIT of a failed PostgreSQL
    /// transaction with a rollback).
    RolledBack,
    /// A DDL statement failed, but Oracle or Snowflake may have committed the transaction before
    /// running it.
    MaybeCommitted,
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
                // `handle` dispatches every request from one async fn: unoptimised builds keep all
                // its arms' temporaries in one poll frame, under the composed driver futures. 8 MB
                // instead of Tokio's 2 MB keeps adding a request kind from overflowing the stack.
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_stack_size(8 * 1024 * 1024)
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
    /// Session Monitor session: refreshes don't queue behind a console or the object browser,
    /// and a cancel or terminate never runs on (or takes down) a console's session.
    monitor: Option<SharedSession>,
    /// Times the monitor was closed: a session that finishes connecting after a close is used
    /// for its one request and not kept.
    monitor_closes: u64,
}

/// A console's own session in Manual transaction mode: its transaction stays open between runs,
/// which the connection's shared sessions cannot do. A separate connection, tied to the data
/// source and database it was opened for: losing the shared sessions doesn't end it, only a
/// Disconnect of the data source (`generation`) or its own failure does.
#[derive(Clone)]
struct TxSession {
    conn: ConnId,
    db: String,
    generation: u64,
    session: Arc<Mutex<TxConn>>,
}

struct TxConn {
    session: Session,
    driver: Driver,
    /// The console's database: selected again before each run, so a typed `USE` lasts one run
    /// as in Auto mode.
    db: String,
    tx: Option<TxInfo>,
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
    /// Consoles' own sessions for Manual transactions.
    tx: std::sync::Mutex<HashMap<ConsoleId, TxSession>>,
    /// Disconnects per connection, counted as they arrive: the generation of Manual sessions.
    disconnects: std::sync::Mutex<HashMap<ConnId, u64>>,
}

impl State {
    fn bump_if_lifecycle(&self, req: &Request) -> u64 {
        let conn = match req {
            Request::Connect(cfg) => cfg.id,
            Request::Disconnect(conn) => *conn,
            // Stamp queued grant writes so they cannot cross a disconnect/reconnect.
            Request::TableGrants { conn, .. } => return self.current_epoch(*conn),
            _ => return 0,
        };
        if matches!(req, Request::Disconnect(_)) {
            *self.disconnects.lock().unwrap_or_else(|e| e.into_inner()).entry(conn).or_default() += 1;
        }
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

    /// The Session Monitor session (opened on first use and kept).
    async fn monitor_session(&self, conn: ConnId) -> anyhow::Result<SharedSession> {
        let (cfg, existing, closes) = {
            let live = self.live.lock().await;
            let l = live.get(&conn).ok_or_else(|| anyhow::anyhow!(tr("The connection is not open")))?;
            (l.cfg.clone(), l.monitor.clone(), l.monitor_closes)
        };
        if let Some(s) = existing {
            return Ok(s);
        }
        let epoch = self.current_epoch(conn);
        let session = Arc::new(Mutex::new(Session::connect(&cfg).await?));
        Ok(self.keep_monitor(conn, epoch, closes, session).await)
    }

    /// Keeps a new monitor session unless the connection was reopened or the monitor closed while
    /// it connected (then it closes after this request).
    async fn keep_monitor(&self, conn: ConnId, epoch: u64, closes: u64, session: SharedSession) -> SharedSession {
        let mut live = self.live.lock().await;
        match live.get_mut(&conn) {
            Some(l) if self.current_epoch(conn) == epoch && l.monitor_closes == closes => l.monitor.get_or_insert(session).clone(),
            _ => session,
        }
    }

    /// Drops the Session Monitor session if it is still `session` (timed out or broken).
    async fn forget_monitor(&self, conn: ConnId, session: &SharedSession) {
        if let Some(l) = self.live.lock().await.get_mut(&conn)
            && l.monitor.as_ref().is_some_and(|s| Arc::ptr_eq(s, session))
        {
            l.monitor = None;
        }
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

    fn disconnect_gen(&self, conn: ConnId) -> u64 {
        self.disconnects.lock().unwrap_or_else(|e| e.into_inner()).get(&conn).copied().unwrap_or_default()
    }

    /// The console's own Manual session for `db` (opened on first use and kept). One left from
    /// another connection, database or generation is dropped: closing it rolls its transaction
    /// back.
    async fn tx_session_for(&self, console: ConsoleId, conn: ConnId, db: Option<&str>) -> anyhow::Result<Arc<Mutex<TxConn>>> {
        // Read before the connection: a Disconnect arriving after this changes it, so a session
        // opened meanwhile is never kept (and one kept before it is dropped by its handler).
        let generation = self.disconnect_gen(conn);
        let (mut cfg, target) = {
            let live = self.live.lock().await;
            let l = live.get(&conn).ok_or_else(|| anyhow::anyhow!(tr("The connection is not open")))?;
            (l.cfg.clone(), db.filter(|d| !d.is_empty()).unwrap_or(&l.default_db).to_string())
        };
        let stale = {
            let mut map = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            match map.get(&console) {
                Some(t) if t.conn == conn && t.db == target && t.generation == generation => return Ok(t.session.clone()),
                Some(_) => map.remove(&console),
                None => None,
            }
        };
        drop(stale);
        anyhow::ensure!(cfg.driver.has_transactions(), tr("Manual transactions are not available for this engine"));
        if cfg.driver == Driver::Postgres {
            cfg.database = target.clone();
        }
        let mut session = Session::connect(&cfg).await?;
        session.use_database(&target).await?;
        session.manual_session().await?;
        let session = Arc::new(Mutex::new(TxConn { session, driver: cfg.driver, db: target.clone(), tx: None }));
        let mut map = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        // Disconnected meanwhile: it is not kept (it closes when dropped). Checked under the map's
        // lock, so a Disconnect after the check finds it there and drops it.
        if self.disconnect_gen(conn) != generation {
            drop(map);
            anyhow::bail!(tr("The connection is not open"));
        }
        let old = map.insert(console, TxSession { conn, db: target, generation, session: session.clone() });
        drop(map);
        drop(old);
        Ok(session)
    }

    /// Drops the Manual sessions of a disconnected connection: their transactions roll back as
    /// they close.
    fn forget_tx(&self, conn: ConnId) {
        let dropped: Vec<_> = self.tx.lock().unwrap_or_else(|e| e.into_inner()).extract_if(|_, t| t.conn == conn).collect();
        drop(dropped);
    }

    /// Drops a console's Manual session if it is still `session` (it was lost).
    fn forget_console_tx(&self, console: ConsoleId, session: &Arc<Mutex<TxConn>>) {
        let mut map = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        let old = if map.get(&console).is_some_and(|t| Arc::ptr_eq(&t.session, session)) { map.remove(&console) } else { None };
        drop(map);
        drop(old);
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

type MonitorJob<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// Runs `job` on the connection's Session Monitor session, 15 s at most; a query that can't be
/// cancelled in time drops only that session.
async fn on_monitor<T>(state: &State, conn: ConnId, job: impl for<'a> FnOnce(&'a mut Session, Driver) -> MonitorJob<'a, T>) -> Result<T, String> {
    let Some(cfg) = state.live.lock().await.get(&conn).map(|l| l.cfg.clone()) else {
        return Err(tr("The connection is not open").into());
    };
    if !crate::db::sessions::supported(cfg.driver) {
        return Err(tr("Session monitoring is not available for this engine").into());
    }
    let redact = |e: anyhow::Error| {
        let message = format!("{e:#}");
        if cfg.password.is_empty() { message } else { message.replace(&cfg.password, "[redacted]") }
    };
    let session = state.monitor_session(conn).await.map_err(redact)?;
    let mut s = session.lock().await;
    let cancel = s.cancel_handle();
    let (result, broken) = match tokio::time::timeout(std::time::Duration::from_secs(15), job(&mut s, cfg.driver)).await {
        Ok(result) => (result.map_err(redact), s.is_closed()),
        Err(_) => {
            let cancelled = match cancel {
                Some(h) => tokio::time::timeout(std::time::Duration::from_secs(2), h.cancel()).await.is_ok_and(|r| r.is_ok()),
                None => false,
            };
            (Err(tr("The session monitor request timed out").into()), !cancelled)
        }
    };
    drop(s);
    if broken {
        state.forget_monitor(conn, &session).await;
    }
    result
}

fn save_model_file(path: std::path::PathBuf, contents: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    let parent = path.parent().filter(|p| p.is_dir()).ok_or_else(|| anyhow::anyhow!(tr("Choose an existing destination directory")))?;
    let temporary = parent.join(format!(".datacraft-model-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(contents.as_bytes())?; file.sync_all()?; drop(file);
        std::fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result?; Ok(path)
}

type CompareFuture = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<(crate::db::schema_compare::SchemaModel, crate::db::schema_compare::SchemaModel)>> + Send>>;

/// The comparison's large future is built in this frame and boxed, not in `handle`'s poll
/// frame, which holds every arm's temporaries.
#[inline(never)]
fn compare_schemas(
    source: ConnectionConfig,
    target: ConnectionConfig,
    src: crate::db::schema_compare::read::Side,
    tgt: crate::db::schema_compare::read::Side,
    progress: Arc<crate::db::schema_compare::read::Progress>,
) -> CompareFuture {
    Box::pin(crate::db::schema_compare::read::run(source, target, src, tgt, progress))
}

async fn handle(req: Request, epoch: u64, state: &State) -> Vec<Response> {
    match req {
        Request::TableShortcut { path, target } => {
            let result = tokio::task::spawn_blocking(move || crate::table_shortcut::create(&path, &target).map(|_| path)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
            vec![Response::TableShortcut { result }]
        }
        Request::TableModel { conn, db, request_id, tables, cancel } => {
            let cfg = state.live.lock().await.get(&conn).map(|l| l.cfg.clone());
            let result = match cfg {
                Some(cfg) => Box::pin(crate::db::table_model::read(cfg, &db, &tables, &cancel)).await.map_err(|e| format!("{e:#}")),
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::TableModel { request_id, result }]
        }
        Request::SaveModel { path, contents } => {
            let result = tokio::task::spawn_blocking(move || save_model_file(path, &contents)).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
            vec![Response::ModelSaved { result }]
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
                    // Streaming has no total deadline; setup/cleanup bounds and cancellation live in the exporter.
                    let result = Box::pin(crate::db::table_export::export(cfg, &db, *request, progress)).await;
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
        Request::TableGrants { conn, db, schema, table, request_id, request, cancel } => {
            let cfg = state.live.lock().await.get(&conn).map(|live| live.cfg.clone());
            let result = match cfg {
                Some(cfg) if state.current_epoch(conn) == epoch => {
                    let password = cfg.password.clone();
                    // Composed driver futures must stay off the Tokio worker's stack.
                    let mut task = Box::pin(crate::db::table_privileges::run(cfg, &db, &schema, &table, request, cancel.clone()));
                    let result = tokio::select! {
                        result = &mut task => result,
                        _ = async {
                            while state.current_epoch(conn) == epoch { tokio::time::sleep(std::time::Duration::from_millis(50)).await; }
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        } => task.await,
                    };
                    result.map_err(|error| {
                        let message = format!("{error:#}");
                        if password.is_empty() { message } else { message.replace(&password, "[redacted]") }
                    })
                }
                _ => Err(tr("The connection is not open").into()),
            };
            vec![Response::TableGrants { conn, request_id, result }]
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
        Request::Sessions { conn, request_id } => {
            let epoch = state.current_epoch(conn);
            let result = on_monitor(state, conn, |s, driver| Box::pin(crate::db::sessions::list(s, driver))).await;
            vec![Response::Sessions { conn, request_id, epoch, result }]
        }
        Request::SessionAction { conn, request_id, epoch, action } => {
            let read_only = state.live.lock().await.get(&conn).is_none_or(|l| l.cfg.read_only);
            // The ids of a list read before a reconnect may name other sessions by now.
            let result = if state.current_epoch(conn) != epoch {
                Err(tr("The list is from an earlier connection; refresh and try again").into())
            } else if read_only {
                Err(tr("Read-only connection").into())
            } else {
                on_monitor(state, conn, move |s, driver| Box::pin(async move { crate::db::sessions::act(s, driver, &action).await })).await
            };
            vec![Response::SessionAction { conn, request_id, result }]
        }
        Request::CloseSessionMonitor(conn) => {
            if let Some(l) = state.live.lock().await.get_mut(&conn) {
                l.monitor = None;
                l.monitor_closes += 1;
            }
            Vec::new()
        }
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
        Request::SchemaCompare { source, target, request_id, sides, progress } => {
            let cfgs = {
                let live = state.live.lock().await;
                live.get(&source).map(|l| l.cfg.clone()).zip(live.get(&target).map(|l| l.cfg.clone()))
            };
            let result = match cfgs {
                Some((source, target)) => {
                    let (src, tgt) = *sides;
                    compare_schemas(source, target, src, tgt, progress).await.map(Box::new).map_err(|e| format!("{e:#}"))
                }
                None => Err(tr("The connection is not open").into()),
            };
            vec![Response::SchemaCompared { request_id, result }]
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
                    live.insert(conn, Live { cfg, default_db, sessions, meta: HashMap::new(), monitor: None, monitor_closes: 0 });
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
            state.forget_tx(conn);
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
        Request::EndTransaction { console, commit } => vec![end_transaction(state, console, commit).await],
        Request::ReleaseConsole { console } => {
            let old = state.tx.lock().unwrap_or_else(|e| e.into_inner()).remove(&console);
            // Roll back explicitly when the session is idle; a busy one rolls back as it closes.
            if let Some(t) = old
                && let Ok(mut t) = t.session.try_lock()
                && t.tx.take().is_some()
            {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), t.session.end_transaction(false)).await;
            }
            vec![]
        }
        Request::Execute { conn, db, console, statements, max_rows, manual: true } => {
            Box::pin(execute_manual(state, conn, db, console, statements, max_rows)).await
        }
        Request::Execute { conn, db, console, statements, max_rows, manual: false } => {
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

/// `Execute` in Manual mode: on the console's own session, inside the transaction it keeps open.
/// The transaction begins before the first statement that doesn't itself control it.
async fn execute_manual(state: &State, conn: ConnId, db: Option<String>, console: ConsoleId, statements: Vec<String>, max_rows: usize) -> Vec<Response> {
    use crate::db::transaction::{self, Control};
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
    let shared = match state.tx_session_for(console, conn, db.as_deref()).await {
        Ok(s) => s,
        Err(e) => {
            unregister(state);
            let error = Some(format!("{e:#}"));
            return vec![Response::Executed { console, results: vec![], error, elapsed: start.elapsed() }, Response::Transaction { console, tx: None, note: None }];
        }
    };
    let mut guard = shared.lock().await;
    let t = &mut *guard;
    *running.native.lock().unwrap_or_else(|e| e.into_inner()) = t.session.cancel_handle();
    let driver = t.driver;
    let statements_tail = statements.last().cloned();
    let (mut results, mut error, mut abandoned, mut note) = (Vec::new(), None, false, None);
    let mut broken = false;
    let db = t.db.clone();
    if let Err(e) = t.session.use_database(&db).await {
        error = Some(format!("{e:#}"));
    }
    let statements = if error.is_some() { Vec::new() } else { statements };
    for stmt in statements {
        // As in `Execute`: `in_flight` first, so either side sees the other.
        running.in_flight.store(true, SeqCst);
        if running.cancelled.load(SeqCst) {
            running.in_flight.store(false, SeqCst);
            break;
        }
        let control = transaction::control(driver, &stmt);
        let mut result = Ok(Vec::new());
        if t.tx.is_none() && control.is_none() {
            result = tokio::select! {
                r = t.session.begin() => r.map(|()| Vec::new()),
                _ = running.abandon.notified() => {
                    abandoned = true;
                    Err(anyhow::anyhow!(tr("Query cancelled")))
                }
            };
            if result.is_ok() {
                t.tx = Some(TxInfo { began: Instant::now(), statements: 0, aborted: false });
            }
        }
        let was_open = t.tx.is_some();
        let was_aborted = t.tx.is_some_and(|tx| tx.aborted);
        if result.is_ok() {
            result = tokio::select! {
                r = t.session.execute(&stmt, max_rows) => r,
                _ = running.abandon.notified() => {
                    abandoned = true;
                    Err(anyhow::anyhow!(tr("Query cancelled")))
                }
            };
        }
        running.in_flight.store(false, SeqCst);
        let failed = match result {
            Ok(rs) => {
                results.extend(rs);
                None
            }
            Err(e) => Some(format!("{e:#}")),
        };
        if abandoned {
            error = failed;
            break;
        }
        // The user turned autocommit back on: the session goes back to never committing by itself.
        if control == Some(Control::Autocommit)
            && !t.session.is_closed()
            && let Err(e) = t.session.manual_session().await
        {
            broken = true;
            error = Some(format!("{e:#}"));
            break;
        }
        // Where the transaction stands now: asked where the engine can tell, else followed. When
        // unsure it stays open: the session never commits by itself, so later statements are
        // still uncommitted and the guards keep asking.
        let server = if t.session.is_closed() { None } else { t.session.transaction_state().await };
        if let (Some((_, doomed)), Some(tx)) = (server, t.tx.as_mut()) {
            tx.aborted = doomed;
        }
        let after = transaction::after(driver, &stmt, failed.as_deref(), was_open, was_aborted, server.map(|(open, _)| open));
        if after.maybe_committed {
            note = Some(TxNote::MaybeCommitted);
        }
        match (after.open, t.tx.as_mut()) {
            (true, Some(tx)) if after.implicit => {
                *tx = TxInfo { began: Instant::now(), statements: 0, aborted: false };
                note = Some(TxNote::ImplicitCommit);
            }
            (true, Some(tx)) => {
                if failed.is_none() && control.is_none() {
                    tx.statements += 1;
                }
                // Any error aborts a PostgreSQL transaction until it rolls back (or to a savepoint).
                if driver == Driver::Postgres {
                    if failed.is_some() {
                        tx.aborted = true;
                    } else if control == Some(Control::RollbackTo) {
                        tx.aborted = false;
                    }
                }
            }
            (true, None) => t.tx = Some(TxInfo { began: Instant::now(), statements: 0, aborted: false }),
            (false, _) => {
                if after.rolled_back {
                    note = Some(TxNote::RolledBack);
                }
                t.tx = None;
            }
        }
        if failed.is_some() {
            error = failed;
            break;
        }
    }
    if error.is_some()
        && !abandoned
        && let Some(last) = statements_tail.filter(|s| s.trim().eq_ignore_ascii_case("SET SHOWPLAN_TEXT OFF"))
    {
        let _ = t.session.execute(&last, 0).await;
    }
    unregister(state);
    if running.cancelled.load(SeqCst) {
        error = Some(tr("Query cancelled").to_string());
    }
    // A lost session (or an abandoned query, which closes it) takes its transaction with it.
    let lost = abandoned || broken || t.session.is_closed();
    if lost && t.tx.take().is_some() {
        note = Some(TxNote::Lost);
    }
    // Released meanwhile (tab closed, disconnected…): the app already reported the rollback, so
    // the transaction ends here instead of coming back.
    let released = !lost && !state.tx.lock().unwrap_or_else(|e| e.into_inner()).get(&console).is_some_and(|x| Arc::ptr_eq(&x.session, &shared));
    if released && t.tx.take().is_some() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), t.session.end_transaction(false)).await;
        note = None;
    }
    let tx = t.tx;
    drop(guard);
    if lost {
        state.forget_console_tx(console, &shared);
    }
    vec![Response::Executed { console, results, error, elapsed: start.elapsed() }, Response::Transaction { console, tx, note }]
}

/// Longest wait for a console's COMMIT or ROLLBACK before its session is closed.
const END_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `EndTransaction`: COMMIT or ROLLBACK on the console's own session.
async fn end_transaction(state: &State, console: ConsoleId, commit: bool) -> Response {
    let start = Instant::now();
    let shared = state.tx.lock().unwrap_or_else(|e| e.into_inner()).get(&console).map(|t| t.session.clone());
    let Some(shared) = shared else {
        return Response::TransactionEnded { console, commit, result: Err(tr("No transaction is open").into()), tx: None, elapsed: start.elapsed() };
    };
    // Stop (Request::Cancel) and a time limit reach it like a running statement.
    let running = Arc::new(Running::default());
    state.running.lock().unwrap_or_else(|e| e.into_inner()).insert(console, running.clone());
    let mut guard = shared.lock().await;
    let t = &mut *guard;
    *running.native.lock().unwrap_or_else(|e| e.into_inner()) = t.session.cancel_handle();
    running.in_flight.store(true, std::sync::atomic::Ordering::SeqCst);
    let rollback_failed = commit && t.tx.is_some_and(|tx| tx.aborted);
    let end = async {
        match t.tx {
            None => Err(tr("No transaction is open").to_string()),
            // PostgreSQL answers COMMIT with a rollback here: say so instead.
            Some(_) if rollback_failed => match t.session.end_transaction(false).await {
                Ok(()) => Err(tr("The transaction had failed, so it was rolled back; nothing was committed").to_string()),
                Err(e) => Err(format!("{e:#}")),
            },
            Some(_) => t.session.end_transaction(commit).await.map_err(|e| format!("{e:#}")),
        }
    };
    let (result, stopped) = tokio::select! {
        r = tokio::time::timeout(END_TIMEOUT, end) => match r {
            Ok(r) => (r, false),
            Err(_) => (Ok(()), true),
        },
        _ = running.abandon.notified() => (Ok(()), true),
    };
    running.in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
    {
        let mut map = state.running.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&console).is_some_and(|r| Arc::ptr_eq(r, &running)) {
            map.remove(&console);
        }
    }
    // Stopped or too slow: the session is closed, which rolls back what didn't commit.
    let result = match stopped {
        true if commit => Err(tr("The commit was stopped before the server answered: the console's session was closed, so whether it committed is unknown").to_string()),
        true => Err(tr("The rollback was stopped: the console's session was closed, which rolls the transaction back").to_string()),
        false => result,
    };
    // A failed COMMIT can leave it open: where the engine says so (SQLite: database busy), and
    // when unsure except on PostgreSQL and Oracle, which roll back on a failed COMMIT.
    let lost = stopped || t.session.is_closed();
    let server = if lost { None } else { t.session.transaction_state().await };
    t.tx = match server {
        Some((true, doomed)) => t.tx.map(|tx| TxInfo { aborted: doomed, ..tx }),
        None if result.is_err() && !lost && !matches!(t.driver, Driver::Postgres | Driver::Oracle) => t.tx,
        _ => None,
    };
    let tx = t.tx;
    drop(guard);
    if lost {
        state.forget_console_tx(console, &shared);
    }
    Response::TransactionEnded { console, commit, result, tx, elapsed: start.elapsed() }
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

    #[tokio::test]
    async fn session_actions_need_the_list_epoch_and_closing_drops_the_monitor_session() {
        use crate::db::sessions::{Action, ActionKind};
        let state = State::default();
        let path = std::env::temp_dir().join(format!("datacraft-monitor-{}.db", uuid::Uuid::new_v4()));
        rusqlite::Connection::open(&path).unwrap();
        let mut cfg = ConnectionConfig::new(8, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        let monitor = Arc::new(Mutex::new(Session::connect(&cfg).await.unwrap()));
        let late = Arc::new(Mutex::new(Session::connect(&cfg).await.unwrap()));
        state.live.lock().await.insert(8, Live { cfg, default_db: "main".into(), sessions: HashMap::new(), meta: HashMap::new(), monitor: Some(monitor), monitor_closes: 0 });
        state.bump_if_lifecycle(&Request::Disconnect(8));
        let action = Action::new(ActionKind::Terminate, &crate::db::sessions::SessionRow { id: "1".into(), ..Default::default() });
        let stale = Request::SessionAction { conn: 8, request_id: 1, epoch: 0, action };
        let responses = Box::pin(handle(stale, 0, &state)).await;
        let [Response::SessionAction { result: Err(error), .. }] = responses.as_slice() else { panic!("expected a refusal") };
        assert_eq!(error, tr("The list is from an earlier connection; refresh and try again"));
        let (epoch, closes) = (state.current_epoch(8), 0);
        assert!(Box::pin(handle(Request::CloseSessionMonitor(8), 0, &state)).await.is_empty());
        assert!(state.live.lock().await.get(&8).is_some_and(|l| l.monitor.is_none()));
        // A session that finished connecting after the window closed serves its request only.
        let used = state.keep_monitor(8, epoch, closes, late.clone()).await;
        assert!(Arc::ptr_eq(&used, &late));
        assert!(state.live.lock().await.get(&8).is_some_and(|l| l.monitor.is_none()), "not kept after a close");
        let kept = state.keep_monitor(8, epoch, 1, late.clone()).await;
        assert!(Arc::ptr_eq(&kept, &late) && state.live.lock().await.get(&8).is_some_and(|l| l.monitor.is_some()));
        drop((used, kept, late));
        state.live.lock().await.clear();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn grants_worker_rejects_read_only_and_invalidated_connection_requests() {
        use crate::db::table_privileges::{Edit, Principal, Request as GrantsRequest};
        let state = State::default();
        let mut cfg = ConnectionConfig::new(7, Driver::Sqlite);
        cfg.read_only = true;
        state.live.lock().await.insert(7, Live { cfg, default_db: "main".into(), sessions: HashMap::new(), meta: HashMap::new(), monitor: None, monitor_closes: 0 });
        let request = |id| Request::TableGrants {
            conn: 7,
            db: "main".into(),
            schema: "main".into(),
            table: "notes".into(),
            request_id: id,
            request: GrantsRequest::Apply(Edit { principal: Principal { name: "reader".into(), host: "".into() }, old: vec![], new: vec![], expected: vec![] }),
            cancel: Arc::default(),
        };
        let responses = Box::pin(handle(request(9), 0, &state)).await;
        let [Response::TableGrants { conn: 7, request_id: 9, result: Err(error) }] = responses.as_slice() else {
            panic!("expected typed grants refusal");
        };
        assert_eq!(error, tr("Read-only connection"));
        let pending = request(10);
        let epoch = state.bump_if_lifecycle(&pending);
        state.bump_if_lifecycle(&Request::Disconnect(7));
        let responses = Box::pin(handle(pending, epoch, &state)).await;
        let [Response::TableGrants { request_id: 10, result: Err(error), .. }] = responses.as_slice() else {
            panic!("expected invalidated request");
        };
        assert_eq!(error, tr("The connection is not open"));
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
    fn schema_compare_reads_both_sides_on_background_sessions() {
        use crate::db::schema_compare::read::{Progress, Side};
        let directory = std::env::temp_dir().join(format!("datacraft-compare-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let worker = Worker::start(egui::Context::default());
        for (id, file, sql) in [(91, "a.db", "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"), (92, "b.db", "CREATE TABLE t (id INTEGER PRIMARY KEY)")] {
            let path = directory.join(file);
            rusqlite::Connection::open(&path).unwrap().execute_batch(sql).unwrap();
            let mut cfg = ConnectionConfig::new(id, Driver::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            cfg.read_only = true;
            worker.send(Request::Connect(cfg));
            wait_for(&worker, |r| matches!(r, Response::Connected { conn, .. } if *conn == id));
        }
        let side = || Side { db: "main".into(), schema: "main".into() };
        let send = |request_id, progress: Arc<Progress>| {
            worker.send(Request::SchemaCompare { source: 91, target: 92, request_id, sides: Box::new((side(), side())), progress });
        };
        let progress = Arc::new(Progress::default());
        send(1, progress.clone());
        let Response::SchemaCompared { result, .. } = wait_for(&worker, |r| matches!(r, Response::SchemaCompared { request_id: 1, .. })) else { unreachable!() };
        let models = result.unwrap();
        assert_eq!((models.0.tables[0].details.columns.len(), models.1.tables[0].details.columns.len()), (2, 1));
        assert_eq!((progress.done.load(std::sync::atomic::Ordering::Relaxed), progress.total.load(std::sync::atomic::Ordering::Relaxed)), (2, 2));
        let cancelled = Arc::new(Progress::default());
        cancelled.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        send(2, cancelled);
        let Response::SchemaCompared { result, .. } = wait_for(&worker, |r| matches!(r, Response::SchemaCompared { request_id: 2, .. })) else { unreachable!() };
        assert_eq!(result.err().as_deref(), Some(tr("Cancelled")));
        for id in [91, 92] {
            worker.send(Request::Disconnect(id));
            wait_for(&worker, |r| matches!(r, Response::Disconnected(c) if *c == id));
        }
        drop(worker);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn table_model_uses_a_background_session_and_saves_atomically() {
        let directory = std::env::temp_dir().join(format!("datacraft-model-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("source.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE parent(a INTEGER,b INTEGER,computed TEXT GENERATED ALWAYS AS (a||':'||b),PRIMARY KEY(b,a)); CREATE TABLE child(x INTEGER,y INTEGER,FOREIGN KEY(x,y) REFERENCES parent);").unwrap();
        drop(db);
        let worker = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(73, Driver::Sqlite); cfg.file_path = path.to_string_lossy().into_owned(); cfg.read_only = true;
        worker.send(Request::Connect(cfg));
        wait_for(&worker, |r| matches!(r, Response::Connected { conn: 73, .. }));
        worker.send(Request::TableModel { conn: 73, db: "main".into(), request_id: 1, tables: vec![("main".into(),"child".into())], cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)) });
        let Response::TableModel { result, .. } = wait_for(&worker, |r| matches!(r, Response::TableModel { request_id: 1, .. })) else { unreachable!() };
        let model = result.unwrap(); assert_eq!(model.relations[0].ref_columns, ["b","a"]); assert!(model.tables.iter().any(|t| t.columns.iter().any(|c| c.name=="computed")));
        let output = directory.join("model.json"); let contents = serde_json::to_string(&model).unwrap();
        worker.send(Request::SaveModel { path: output.clone(), contents: contents.clone() });
        assert!(matches!(wait_for(&worker, |r| matches!(r, Response::ModelSaved { .. })), Response::ModelSaved { result: Ok(_) }));
        assert_eq!(std::fs::read_to_string(&output).unwrap(), contents);
        let preserved = directory.join("preserved"); std::fs::create_dir(&preserved).unwrap(); let keep=preserved.join("keep"); std::fs::write(&keep,"unchanged").unwrap();
        assert!(save_model_file(preserved.clone(),"replacement").is_err()); assert_eq!(std::fs::read_to_string(&keep).unwrap(),"unchanged");
        assert!(!std::fs::read_dir(&directory).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(".datacraft-model-")));
        worker.send(Request::Disconnect(73)); wait_for(&worker, |r| matches!(r, Response::Disconnected(73)));
        std::fs::remove_file(keep).unwrap(); std::fs::remove_dir(preserved).unwrap(); std::fs::remove_file(output).unwrap(); std::fs::remove_file(path).unwrap(); std::fs::remove_dir(directory).unwrap();
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
        state.live.lock().await.insert(71, Live { cfg, default_db: "main".into(), sessions: HashMap::new(), meta: HashMap::new(), monitor: None, monitor_closes: 0 });
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

    /// The Session Monitor on a real PostgreSQL (DATACRAFT_PG_HOST, as `db::postgres`'s tests): the
    /// list doesn't wait behind a console's running query, cancelling that query keeps the
    /// console's session, and a read-only connection lists but refuses actions.
    #[test]
    #[ignore]
    fn postgres_session_monitor_through_the_worker() {
        use crate::db::sessions::{Action, ActionKind};
        let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else { return };
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let mut cfg = ConnectionConfig::new(33, Driver::Postgres);
        cfg.host = host;
        cfg.port = var("DATACRAFT_PG_PORT", "5432").parse().unwrap();
        cfg.user = var("DATACRAFT_PG_USER", "postgres");
        cfg.password = var("DATACRAFT_PG_PASSWORD", "");
        cfg.database = var("DATACRAFT_PG_DB", "postgres");
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg.clone()));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 33, .. }));
        w.send(Request::Execute { conn: 33, db: None, console: 330, statements: vec!["select pg_sleep(30) /* dc monitor test */".into()], max_rows: 10, manual: false });
        let started = Instant::now();
        let (mut row, mut epoch, mut monitor_pid) = (None, 0, String::new());
        for request_id in 1..=50 {
            w.send(Request::Sessions { conn: 33, request_id });
            let Response::Sessions { result, epoch: listed, .. } = wait_for(&w, |r| matches!(r, Response::Sessions { request_id: id, .. } if *id == request_id)) else { unreachable!() };
            let rows = result.unwrap().rows;
            (epoch, monitor_pid) = (listed, rows.iter().find(|r| r.own).map(|r| r.id.clone()).unwrap_or_default());
            row = rows.into_iter().find(|r| r.query.contains("dc monitor test") && r.state == "active" && !r.own);
            if row.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let row = row.expect("the console's query is listed while it runs");
        assert!(started.elapsed() < Duration::from_secs(10), "the list did not wait for the console");
        w.send(Request::SessionAction { conn: 33, request_id: 900, epoch, action: Action::new(ActionKind::Cancel, &row) });
        // Both replies, in whichever order they arrive.
        let (mut cancelled, mut executed) = (None, None);
        let deadline = Instant::now() + Duration::from_secs(20);
        while (cancelled.is_none() || executed.is_none()) && Instant::now() < deadline {
            for r in w.poll() {
                match r {
                    Response::SessionAction { request_id: 900, result, .. } => cancelled = Some(result),
                    Response::Executed { console: 330, error, .. } => executed = Some(error),
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(cancelled, Some(Ok(())));
        assert!(executed.expect("the console's query ended").is_some(), "the console's query was cancelled");
        w.send(Request::Execute { conn: 33, db: None, console: 330, statements: vec!["select 1".into()], max_rows: 10, manual: false });
        let Response::Executed { results, error, .. } = wait_for(&w, |r| matches!(r, Response::Executed { console: 330, .. })) else { unreachable!() };
        assert_eq!((error, results[0].rows[0][0].as_deref()), (None, Some("1")), "the console's session survived");

        // Closing the last monitor window closes its session on the server.
        w.send(Request::CloseSessionMonitor(33));
        let mut probe = cfg.clone();
        probe.id = 37;
        w.send(Request::Connect(probe));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 37, .. }));
        let mut gone = false;
        for request_id in 100..150 {
            w.send(Request::Sessions { conn: 37, request_id });
            let Response::Sessions { result, .. } = wait_for(&w, |r| matches!(r, Response::Sessions { request_id: id, .. } if *id == request_id)) else { unreachable!() };
            gone = !result.unwrap().rows.iter().any(|r| r.id == monitor_pid);
            if gone {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(gone && !monitor_pid.is_empty(), "the monitor session {monitor_pid} was closed");

        // After a reconnect, an action from the earlier list is refused.
        w.send(Request::Disconnect(33));
        wait_for(&w, |r| matches!(r, Response::Disconnected(33)));
        w.send(Request::Connect(cfg.clone()));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 33, .. }));
        w.send(Request::SessionAction { conn: 33, request_id: 901, epoch, action: Action::new(ActionKind::Terminate, &row) });
        let Response::SessionAction { result, .. } = wait_for(&w, |r| matches!(r, Response::SessionAction { request_id: 901, .. })) else { unreachable!() };
        assert_eq!(result, Err(tr("The list is from an earlier connection; refresh and try again").into()));

        cfg.id = 34;
        cfg.read_only = true;
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 34, .. }));
        w.send(Request::Sessions { conn: 34, request_id: 1 });
        let Response::Sessions { result, epoch, .. } = wait_for(&w, |r| matches!(r, Response::Sessions { conn: 34, .. })) else { unreachable!() };
        let own = result.unwrap().rows.into_iter().find(|r| r.own).expect("listed on a read-only connection");
        w.send(Request::SessionAction { conn: 34, request_id: 2, epoch, action: Action::new(ActionKind::Terminate, &own) });
        let Response::SessionAction { result, .. } = wait_for(&w, |r| matches!(r, Response::SessionAction { conn: 34, .. })) else { unreachable!() };
        assert_eq!(result, Err(tr("Read-only connection").into()));
    }

    #[test]
    fn session_monitor_is_refused_on_engines_without_sessions() {
        let path = std::env::temp_dir().join(format!("datacraft-sessions-{}.db", uuid::Uuid::new_v4()));
        rusqlite::Connection::open(&path).unwrap();
        let w = Worker::start(egui::Context::default());
        let mut cfg = ConnectionConfig::new(35, Driver::Sqlite);
        cfg.file_path = path.display().to_string();
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 35, .. }));
        w.send(Request::Sessions { conn: 35, request_id: 4 });
        let Response::Sessions { result, .. } = wait_for(&w, |r| matches!(r, Response::Sessions { request_id: 4, .. })) else { unreachable!() };
        assert_eq!(result.unwrap_err(), tr("Session monitoring is not available for this engine"));
        w.send(Request::Sessions { conn: 36, request_id: 5 });
        let Response::Sessions { result, .. } = wait_for(&w, |r| matches!(r, Response::Sessions { request_id: 5, .. })) else { unreachable!() };
        assert_eq!(result.unwrap_err(), tr("The connection is not open"));
        w.send(Request::Disconnect(35));
        wait_for(&w, |r| matches!(r, Response::Disconnected(35)));
        let _ = std::fs::remove_file(&path);
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
        w.send(Request::Execute { conn: 7, db: None, console: 1, statements: vec![endless], max_rows: 10, manual: false });
        std::thread::sleep(Duration::from_millis(300));
        w.send(Request::Cancel { console: 1 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 1, .. }));
        let Response::Executed { error, .. } = r else { unreachable!() };
        assert_eq!(error.as_deref(), Some(tr("Query cancelled")));
        // The session is still usable.
        w.send(Request::Execute { conn: 7, db: None, console: 2, statements: vec!["select 1".into()], max_rows: 10, manual: false });
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
        w.send(Request::Execute { conn, db: None, console: 3, statements: vec![write.into()], max_rows: 10, manual: false });
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
        w.send(Request::Execute { conn, db: None, console: 4, statements: vec![format!("select count(*) from {table}")], max_rows: 10, manual: false });
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
        w.send(Request::Execute { conn, db: None, console: 4, statements: vec!["select sleep(4)".into()], max_rows: 10, manual: false });
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
        w.send(Request::Execute { conn: 8, db: None, console: 1, statements: vec!["select sleep(30)".into()], max_rows: 10, manual: false });
        std::thread::sleep(Duration::from_millis(500));
        w.send(Request::Cancel { console: 1 });
        let r = wait_for(&w, |r| matches!(r, Response::Executed { console: 1, .. }));
        let Response::Executed { error, .. } = r else { unreachable!() };
        assert_eq!(error.as_deref(), Some(tr("Query cancelled")));
        assert!(start.elapsed() < Duration::from_secs(10));
        w.send(Request::Execute { conn: 8, db: None, console: 2, statements: vec!["select 1".into()], max_rows: 10, manual: false });
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
        w.send(Request::Execute { conn, db: None, console, statements: statements.iter().map(|s| s.to_string()).collect(), max_rows: 100, manual: false });
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
        w.send(Request::Execute { conn, db: None, console: 5, statements: vec!["select id, title, price from dc_goods".into()], max_rows: 10, manual: false });
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
        w.send(Request::Execute { conn: 14, db: None, console: 6, statements: vec!["select n from dc_view".into()], max_rows: 10, manual: false });
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

    /// A Manual `Execute`: (results, error, the transaction after it, why it ended).
    fn manual_run(w: &Worker, conn: ConnId, console: ConsoleId, statements: &[&str]) -> (Vec<ResultSet>, Option<String>, Option<TxInfo>, Option<TxNote>) {
        w.send(Request::Execute { conn, db: None, console, statements: statements.iter().map(|s| s.to_string()).collect(), max_rows: 100, manual: true });
        manual_reply(w, console)
    }

    /// The `Executed` and `Transaction` of a console's Manual run (`wait_for` would drop the second).
    fn manual_reply(w: &Worker, console: ConsoleId) -> (Vec<ResultSet>, Option<String>, Option<TxInfo>, Option<TxNote>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut executed = None;
        while Instant::now() < deadline {
            for r in w.poll() {
                match r {
                    Response::Executed { console: c, results, error, .. } if c == console => executed = Some((results, error)),
                    Response::Transaction { console: c, tx, note } if c == console => {
                        let (results, error) = executed.take().expect("Executed comes first");
                        return (results, error, tx, note);
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for the Manual run");
    }

    fn end_tx(w: &Worker, console: ConsoleId, commit: bool) -> (Result<(), String>, Option<TxInfo>) {
        w.send(Request::EndTransaction { console, commit });
        let r = wait_for(w, |r| matches!(r, Response::TransactionEnded { console: c, .. } if *c == console));
        let Response::TransactionEnded { result, tx, commit: c, .. } = r else { unreachable!() };
        assert_eq!(c, commit);
        (result, tx)
    }

    /// `select count(*)` on the connection's shared session (another console in Auto mode).
    fn shared_count(w: &Worker, conn: ConnId, console: ConsoleId, table: &str) -> String {
        let rs = first_rows(w, conn, console, &[&format!("select count(*) from {table}")]);
        rs[0].rows[0][0].clone().unwrap_or_default()
    }

    fn sqlite_tx_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("datacraft-tx-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        rusqlite::Connection::open(&path).unwrap().execute_batch("create table t(x)").unwrap();
        (dir, path)
    }

    /// Rows of `t` and whether the file takes a write, read straight from the file.
    fn sqlite_file_state(path: &std::path::Path) -> (i64, bool) {
        let db = rusqlite::Connection::open(path).unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        let rows = db.query_row("select count(*) from t", [], |r| r.get(0)).unwrap();
        let writable = db.execute_batch("begin immediate; rollback").is_ok();
        (rows, writable)
    }

    #[test]
    fn sqlite_manual_transactions_roll_back_and_commit() {
        let (dir, path) = sqlite_tx_db("commit");
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(30, &path)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 30, .. }));

        let (_, error, tx, note) = manual_run(&w, 30, 1, &["insert into t values (1)"]);
        assert!(error.is_none(), "{error:?}");
        let tx = tx.expect("the transaction stays open");
        assert_eq!((tx.statements, tx.aborted, note), (1, false, None));
        // Another console reads on the shared session: not blocked, and it doesn't see the row.
        assert_eq!(shared_count(&w, 30, 2, "t"), "0");
        assert_eq!(sqlite_file_state(&path), (0, false), "the console's session holds the write lock");

        let (result, tx) = end_tx(&w, 1, false);
        assert!(result.is_ok() && tx.is_none(), "{result:?}");
        assert_eq!(shared_count(&w, 30, 2, "t"), "0");
        assert_eq!(sqlite_file_state(&path), (0, true), "rolled back: nothing persisted");

        // The transaction spans runs and counts their statements.
        manual_run(&w, 30, 1, &["insert into t values (2)"]);
        let (_, _, tx, _) = manual_run(&w, 30, 1, &["insert into t values (3)", "select * from t"]);
        assert_eq!(tx.map(|t| t.statements), Some(3));
        assert_eq!(shared_count(&w, 30, 2, "t"), "0");
        let (result, tx) = end_tx(&w, 1, true);
        assert!(result.is_ok() && tx.is_none(), "{result:?}");
        assert_eq!(shared_count(&w, 30, 2, "t"), "2");
        assert_eq!(sqlite_file_state(&path), (2, true), "committed");

        // A COMMIT typed in the console ends it too; nothing is left to commit.
        let (_, error, tx, _) = manual_run(&w, 30, 1, &["insert into t values (4)", "commit"]);
        assert!(error.is_none() && tx.is_none(), "{error:?} {tx:?}");
        assert_eq!(end_tx(&w, 1, true).0, Err(tr("No transaction is open").to_string()));
        assert_eq!(sqlite_file_state(&path), (3, true));

        // Leaving Manual mode releases the session, rolling back what it held.
        manual_run(&w, 30, 1, &["insert into t values (5)"]);
        w.send(Request::ReleaseConsole { console: 1 });
        let deadline = Instant::now() + Duration::from_secs(10);
        while sqlite_file_state(&path) != (3, true) {
            assert!(Instant::now() < deadline, "the released session still holds its transaction");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_manual_cancel_keeps_the_transaction_and_disconnect_drops_it() {
        let (dir, path) = sqlite_tx_db("cancel");
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(31, &path)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 31, .. }));
        manual_run(&w, 31, 1, &["insert into t values (1)"]);
        // Endless recursive CTE on the console's own session: only a cancel stops it.
        let endless = "with recursive c(n) as (select 1 union all select n + 1 from c) select count(*) from c";
        w.send(Request::Execute { conn: 31, db: None, console: 1, statements: vec![endless.into()], max_rows: 10, manual: true });
        std::thread::sleep(Duration::from_millis(300));
        w.send(Request::Cancel { console: 1 });
        let (_, error, tx, note) = manual_reply(&w, 1);
        assert_eq!(error.as_deref(), Some(tr("Query cancelled")));
        // An interrupted read leaves SQLite's transaction (and the insert) in place.
        assert_eq!((tx.map(|t| t.statements), note), (Some(1), None));
        let (rs, error, tx, _) = manual_run(&w, 31, 1, &["select count(*) from t"]);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(rs[0].rows[0][0].as_deref(), Some("1"));
        assert!(tx.is_some());

        w.send(Request::Disconnect(31));
        wait_for(&w, |r| matches!(r, Response::Disconnected(31)));
        assert_eq!(sqlite_file_state(&path), (0, true), "disconnecting rolled the transaction back");
        w.send(Request::Connect(sqlite_cfg(31, &path)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 31, .. }));
        assert_eq!(end_tx(&w, 1, true).0, Err(tr("No transaction is open").to_string()));
        // The next Manual run opens a new session and transaction.
        let (_, error, tx, _) = manual_run(&w, 31, 1, &["insert into t values (2)"]);
        assert!(error.is_none() && tx.is_some_and(|t| t.statements == 1), "{error:?}");
        assert_eq!(end_tx(&w, 1, true).0, Ok(()));
        assert_eq!(sqlite_file_state(&path), (1, true));
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_manual_transaction_survives_a_reconnect_and_a_released_run_ends_it() {
        let (dir, path) = sqlite_tx_db("reconnect");
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(sqlite_cfg(35, &path)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 35, .. }));
        manual_run(&w, 35, 1, &["insert into t values (1)"]);
        // Reconnecting the data source (as after a lost shared session) keeps the Manual session.
        w.send(Request::Connect(sqlite_cfg(35, &path)));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 35, .. }));
        let (_, error, tx, _) = manual_run(&w, 35, 1, &["insert into t values (2)"]);
        assert!(error.is_none() && tx.is_some_and(|t| t.statements == 2), "{error:?}");
        assert_eq!(end_tx(&w, 1, true), (Ok(()), None));
        assert_eq!(sqlite_file_state(&path), (2, true));

        // Released while a statement runs (tab closed, disconnected…): the reply doesn't bring
        // the transaction back, and it is rolled back.
        manual_run(&w, 35, 2, &["insert into t values (3)"]);
        let endless = "with recursive c(n) as (select 1 union all select n + 1 from c) select count(*) from c";
        w.send(Request::Execute { conn: 35, db: None, console: 2, statements: vec![endless.into()], max_rows: 10, manual: true });
        std::thread::sleep(Duration::from_millis(300));
        w.send(Request::ReleaseConsole { console: 2 });
        std::thread::sleep(Duration::from_millis(100));
        w.send(Request::Cancel { console: 2 });
        let (_, error, tx, note) = manual_reply(&w, 2);
        assert_eq!((error.as_deref(), tx, note), (Some(tr("Query cancelled")), None, None));
        assert_eq!(sqlite_file_state(&path), (2, true), "rolled back");
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shared main session's loss leaves the Manual sessions alone; only a Disconnect
    /// (counted when it arrives) retires them, even one opened while it was on its way.
    #[tokio::test]
    async fn manual_sessions_outlive_the_shared_session_until_a_disconnect() {
        let (dir, path) = sqlite_tx_db("generation");
        let state = State::default();
        let cfg = sqlite_cfg(36, &path);
        let live = |main: &SharedSession| Live { cfg: cfg.clone(), default_db: "main".into(), sessions: HashMap::from([(String::new(), main.clone())]), meta: HashMap::new(), monitor: None, monitor_closes: 0 };
        let main: SharedSession = Arc::new(Mutex::new(Session::connect(&cfg).await.unwrap()));
        state.live.lock().await.insert(36, live(&main));
        let first = state.tx_session_for(1, 36, None).await.unwrap();
        state.forget(36, &main).await;
        assert!(state.live.lock().await.get(&36).is_none(), "the connection lost its shared session");
        assert!(state.tx.lock().unwrap().get(&1).is_some_and(|t| Arc::ptr_eq(&t.session, &first)), "the Manual session stays");
        state.live.lock().await.insert(36, live(&main));
        assert!(Arc::ptr_eq(&state.tx_session_for(1, 36, None).await.unwrap(), &first), "reused after the reconnect");
        // A Disconnect counted on arrival retires it, before its handler even runs.
        state.bump_if_lifecycle(&Request::Disconnect(36));
        let second = state.tx_session_for(1, 36, None).await.unwrap();
        assert!(!Arc::ptr_eq(&second, &first));
        state.forget_tx(36);
        assert!(state.tx.lock().unwrap().is_empty());
        drop((first, second, main));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_manual_mode_keeps_read_only_and_engine_limits() {
        let (dir, path) = sqlite_tx_db("ro");
        let w = Worker::start(egui::Context::default());
        let mut cfg = sqlite_cfg(32, &path);
        cfg.read_only = true;
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 32, .. }));
        // Reads run in the transaction; the console's own session is opened read-only too.
        let (_, error, tx, _) = manual_run(&w, 32, 1, &["select count(*) from t"]);
        assert!(error.is_none() && tx.is_some(), "{error:?}");
        let (_, error, _, _) = manual_run(&w, 32, 1, &["insert into t values (1)"]);
        assert!(error.is_some_and(|e| e.contains("readonly")), "a read-only data source took a write");
        assert_eq!(end_tx(&w, 1, false).0, Ok(()));
        assert_eq!(sqlite_file_state(&path).0, 0);
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
        // Engines without transactions refuse Manual mode.
        assert!(!Driver::Redis.has_transactions() && !Driver::ClickHouse.has_transactions() && Driver::Oracle.has_transactions());
    }

    /// Manual transactions on a real PostgreSQL (DATACRAFT_PG_HOST, as `db::postgres`'s tests):
    /// rollback, commit, the aborted state after an error, savepoints, cancel, read-only and
    /// disconnect.
    #[test]
    #[ignore]
    fn postgres_manual_transactions() {
        let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else { return };
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let mut cfg = ConnectionConfig::new(33, Driver::Postgres);
        cfg.host = host;
        cfg.port = var("DATACRAFT_PG_PORT", "5432").parse().unwrap();
        cfg.user = var("DATACRAFT_PG_USER", "postgres");
        cfg.password = var("DATACRAFT_PG_PASSWORD", "");
        cfg.database = var("DATACRAFT_PG_DB", "postgres");
        let table = format!("dc_manual_tx_{}", uuid::Uuid::new_v4().simple());
        let w = Worker::start(egui::Context::default());
        w.send(Request::Connect(cfg.clone()));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 33, .. }));
        first_rows(&w, 33, 10, &[&format!("create table {table} (id int primary key)")]);
        let insert = |id: i32| format!("insert into {table} values ({id})");

        // Rollback: never visible, nothing persisted.
        let (_, error, tx, _) = manual_run(&w, 33, 11, &[&insert(1)]);
        assert!(error.is_none() && tx.is_some_and(|t| t.statements == 1 && !t.aborted), "{error:?}");
        assert_eq!(shared_count(&w, 33, 12, &table), "0");
        assert_eq!(end_tx(&w, 11, false), (Ok(()), None));
        assert_eq!(shared_count(&w, 33, 12, &table), "0");

        // Commit: visible to the other console only afterwards.
        manual_run(&w, 33, 11, &[&insert(2), &insert(3)]);
        assert_eq!(shared_count(&w, 33, 12, &table), "0");
        assert_eq!(end_tx(&w, 11, true), (Ok(()), None));
        assert_eq!(shared_count(&w, 33, 12, &table), "2");

        // An error aborts the transaction: it stays open, Commit only rolls it back.
        manual_run(&w, 33, 11, &[&insert(4)]);
        let (_, error, tx, note) = manual_run(&w, 33, 11, &["select 1 / 0"]);
        // (Server messages may be localized: the state is checked, not the text.)
        assert!(error.is_some());
        assert_eq!((tx.map(|t| (t.statements, t.aborted)), note), (Some((1, true)), None));
        let (_, error, tx, _) = manual_run(&w, 33, 11, &["select 1"]);
        assert!(error.is_some(), "PostgreSQL refuses statements until the rollback");
        assert!(tx.is_some_and(|t| t.aborted));
        let (result, tx) = end_tx(&w, 11, true);
        assert_eq!(result, Err(tr("The transaction had failed, so it was rolled back; nothing was committed").to_string()));
        assert!(tx.is_none());
        assert_eq!(shared_count(&w, 33, 12, &table), "2");

        // ROLLBACK TO SAVEPOINT recovers the transaction.
        manual_run(&w, 33, 11, &["savepoint s1"]);
        let (_, _, tx, _) = manual_run(&w, 33, 11, &[&insert(2)]);
        assert!(tx.is_some_and(|t| t.aborted), "duplicate key aborts it");
        let (_, error, tx, _) = manual_run(&w, 33, 11, &["rollback to savepoint s1", &insert(5)]);
        assert!(error.is_none() && tx.is_some_and(|t| !t.aborted), "{error:?}");
        assert_eq!(end_tx(&w, 11, true), (Ok(()), None));
        assert_eq!(shared_count(&w, 33, 12, &table), "3");

        // A typed COMMIT of a failed transaction is PostgreSQL's silent ROLLBACK: said so.
        manual_run(&w, 33, 11, &[&insert(12)]);
        manual_run(&w, 33, 11, &["select 1 / 0"]);
        let (_, error, tx, note) = manual_run(&w, 33, 11, &["commit"]);
        assert_eq!((error, tx, note), (None, None, Some(TxNote::RolledBack)));
        assert_eq!(shared_count(&w, 33, 12, &table), "3");

        // A COMMIT that hangs (a deferred trigger here) is stopped like a statement.
        let slow = format!("{table}_slow");
        first_rows(&w, 33, 10, &[
            &format!("create function {slow}() returns trigger language plpgsql as $$ begin perform pg_sleep(30); return null; end $$"),
            &format!("create constraint trigger {slow} after insert on {table} deferrable initially deferred for each row execute function {slow}()"),
        ]);
        manual_run(&w, 33, 11, &[&insert(13)]);
        let started = Instant::now();
        w.send(Request::EndTransaction { console: 11, commit: true });
        std::thread::sleep(Duration::from_millis(500));
        w.send(Request::Cancel { console: 11 });
        let r = wait_for(&w, |r| matches!(r, Response::TransactionEnded { console: 11, .. }));
        let Response::TransactionEnded { result, tx, .. } = r else { unreachable!() };
        assert!(started.elapsed() < Duration::from_secs(10) && result.is_err() && tx.is_none(), "{result:?}");
        first_rows(&w, 33, 10, &[&format!("drop trigger {slow} on {table}"), &format!("drop function {slow}()")]);
        assert_eq!(shared_count(&w, 33, 12, &table), "3");

        // Cancel reaches the console's own session (and PostgreSQL aborts the transaction).
        manual_run(&w, 33, 11, &[&insert(6)]);
        w.send(Request::Execute { conn: 33, db: None, console: 11, statements: vec!["select pg_sleep(30)".into()], max_rows: 10, manual: true });
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        w.send(Request::Cancel { console: 11 });
        let (_, error, tx, _) = manual_reply(&w, 11);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(error.as_deref(), Some(tr("Query cancelled")));
        assert!(tx.is_some_and(|t| t.aborted));
        assert_eq!(end_tx(&w, 11, false), (Ok(()), None));
        assert_eq!(shared_count(&w, 33, 12, &table), "3");

        // Roll Back while a statement runs (the confirmation stops it first): the rollback
        // waits behind the cancelled statement instead of hanging.
        manual_run(&w, 33, 11, &[&insert(9)]);
        w.send(Request::Execute { conn: 33, db: None, console: 11, statements: vec!["select pg_sleep(30)".into()], max_rows: 10, manual: true });
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        w.send(Request::Cancel { console: 11 });
        w.send(Request::EndTransaction { console: 11, commit: false });
        let r = wait_for(&w, |r| matches!(r, Response::TransactionEnded { console: 11, .. }));
        let Response::TransactionEnded { result, tx, .. } = r else { unreachable!() };
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!((result, tx), (Ok(()), None));
        assert_eq!(shared_count(&w, 33, 12, &table), "3");

        // Losing the shared session (here: killed by the server) doesn't end the Manual
        // transaction, which runs on a connection of its own; it goes on after the reconnect.
        manual_run(&w, 33, 11, &[&insert(10)]);
        let pid = first_rows(&w, 33, 12, &["select pg_backend_pid()"])[0].rows[0][0].clone().unwrap();
        let (_, error, tx, _) = manual_run(&w, 33, 11, &[&format!("select pg_terminate_backend({pid})")]);
        assert!(error.is_none() && tx.is_some_and(|t| t.statements == 2), "{error:?}");
        w.send(Request::Execute { conn: 33, db: None, console: 12, statements: vec!["select 1".into()], max_rows: 10, manual: false });
        wait_for(&w, |r| matches!(r, Response::ConnectFailed { conn: 33, .. }));
        w.send(Request::Connect(cfg.clone()));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 33, .. }));
        let (_, error, tx, _) = manual_run(&w, 33, 11, &[&insert(11)]);
        assert!(error.is_none() && tx.is_some_and(|t| t.statements == 3), "the same transaction goes on: {error:?}");
        assert_eq!(shared_count(&w, 33, 12, &table), "3");
        assert_eq!(end_tx(&w, 11, true), (Ok(()), None));
        assert_eq!(shared_count(&w, 33, 12, &table), "5");

        // Read-only data sources stay read-only on the console's own session.
        let mut ro = cfg.clone();
        ro.id = 34;
        ro.read_only = true;
        w.send(Request::Connect(ro));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 34, .. }));
        let (_, error, tx, _) = manual_run(&w, 34, 13, &[&format!("select count(*) from {table}")]);
        assert!(error.is_none() && tx.is_some(), "{error:?}");
        let (_, error, _, _) = manual_run(&w, 34, 13, &[&insert(7)]);
        assert!(error.is_some(), "a read-only data source took a write");
        w.send(Request::Disconnect(34));
        wait_for(&w, |r| matches!(r, Response::Disconnected(34)));

        // Disconnecting drops the transaction and its locks.
        manual_run(&w, 33, 11, &[&insert(8)]);
        w.send(Request::Disconnect(33));
        wait_for(&w, |r| matches!(r, Response::Disconnected(33)));
        w.send(Request::Connect(cfg));
        wait_for(&w, |r| matches!(r, Response::Connected { conn: 33, .. }));
        let locks = format!("select count(*) from pg_locks l join pg_class c on c.oid = l.relation where c.relname = '{table}' and l.pid <> pg_backend_pid()");
        let deadline = Instant::now() + Duration::from_secs(10);
        while first_rows(&w, 33, 12, &[&locks])[0].rows[0][0].as_deref() != Some("0") {
            assert!(Instant::now() < deadline, "the dropped session still holds its transaction");
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(shared_count(&w, 33, 12, &table), "5");
        assert_eq!(end_tx(&w, 11, true).0, Err(tr("No transaction is open").to_string()));
        first_rows(&w, 33, 12, &[&format!("drop table {table}")]);
    }
}
