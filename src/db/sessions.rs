//! Session Monitor: the server's sessions and running queries, which session blocks which, and
//! cancelling a query or terminating a session.
//!
//! One query per dialect, each returning the same `s_*` columns, read into `SessionRow`. The
//! statements that cancel or terminate only take ids read from those rows, checked to be numbers
//! (or escaped, for a ClickHouse query id) before they are written into SQL.

use super::{Driver, ResultSet, Session};
use crate::i18n::{tr, trf};
use anyhow::Context;
use std::collections::HashSet;

/// At most this many sessions are read.
const MAX_ROWS: usize = 10_000;

/// Tags the monitor's own list query (ClickHouse lists queries, not sessions).
const MARKER: &str = "DataCraft session monitor";

/// Engines whose sessions can be listed (SQLite, Snowflake and the NoSQL engines cannot).
pub fn supported(driver: Driver) -> bool {
    matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::MsSql | Driver::Oracle | Driver::ClickHouse)
}

/// Why the engine can't cancel another session's query (None when it can).
pub fn cancel_unsupported(driver: Driver) -> Option<&'static str> {
    match driver {
        Driver::MsSql => Some(tr("SQL Server cannot cancel another session's query; terminate the session instead")),
        d if !supported(d) => Some(tr("Session monitoring is not available for this engine")),
        _ => None,
    }
}

/// Why the engine can't terminate a session (None when it can).
pub fn terminate_unsupported(driver: Driver) -> Option<&'static str> {
    match driver {
        Driver::ClickHouse => Some(tr("ClickHouse lists queries, not sessions: a query can only be cancelled")),
        d if !supported(d) => Some(tr("Session monitoring is not available for this engine")),
        _ => None,
    }
}

/// One server session (ClickHouse: one running query).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionRow {
    /// PID, SPID, thread id, SID or ClickHouse query id: what `blocked_by` refers to.
    pub id: String,
    /// Oracle: the SERIAL# that `ALTER SYSTEM` needs with the SID.
    pub serial: Option<String>,
    /// PostgreSQL: when the backend started (µs since the epoch), so an action can't reach a
    /// new session that reused the PID.
    pub started: Option<String>,
    pub user: String,
    pub database: String,
    pub client: String,
    pub application: String,
    pub state: String,
    /// Time in the current state (running query or open transaction); None when idle.
    pub duration_ms: Option<u64>,
    pub wait: String,
    /// Sessions this one waits for.
    pub blocked_by: Vec<String>,
    /// Running query, or the last one when idle.
    pub query: String,
    /// Neither running a query nor holding a transaction open.
    pub idle: bool,
    /// Idle, but with a transaction open (where the state doesn't say it).
    pub in_transaction: bool,
    /// The monitor's own session.
    pub own: bool,
    /// A DataCraft session (by its application name).
    pub datacraft: bool,
}

impl SessionRow {
    /// The id as the server writes it: `sid,serial#` on Oracle.
    pub fn label(&self) -> String {
        match &self.serial {
            Some(serial) => format!("{},{serial}", self.id),
            None => self.id.clone(),
        }
    }

    /// Whether the row matches the text filter (already lowercase).
    pub fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || [&self.label(), &self.user, &self.database, &self.client, &self.application, &self.state, &self.wait, &self.query, &self.blocked_by.join(",")]
                .iter()
                .any(|v| v.to_lowercase().contains(filter))
    }
}

/// The sessions, and why part of the picture is missing (if it is).
#[derive(Clone, Debug, Default)]
pub struct SessionList {
    pub rows: Vec<SessionRow>,
    pub notice: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// Cancel the running query; the session stays.
    Cancel,
    /// End the session (its transaction is rolled back).
    Terminate,
}

/// Cancel or terminate one session, identified as its row was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub kind: ActionKind,
    pub id: String,
    pub serial: Option<String>,
    pub started: Option<String>,
}

impl Action {
    pub fn new(kind: ActionKind, row: &SessionRow) -> Self {
        Self { kind, id: row.id.clone(), serial: row.serial.clone(), started: row.started.clone() }
    }

    pub fn label(&self) -> String {
        match &self.serial {
            Some(serial) => format!("{},{serial}", self.id),
            None => self.id.clone(),
        }
    }
}

/// Sessions that others wait for.
pub fn blockers(rows: &[SessionRow]) -> HashSet<&str> {
    rows.iter().flat_map(|r| r.blocked_by.iter().map(String::as_str)).collect()
}

/// How a row is highlighted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Highlight {
    None,
    /// Waits for another session.
    Blocked,
    /// Others wait for it, and it waits for nobody: the head of a chain.
    Blocker,
}

pub fn highlight(row: &SessionRow, blockers: &HashSet<&str>) -> Highlight {
    if !row.blocked_by.is_empty() {
        Highlight::Blocked
    } else if blockers.contains(row.id.as_str()) {
        Highlight::Blocker
    } else {
        Highlight::None
    }
}

/// Whether the row is shown: idle sessions can be hidden, except those that block others.
pub fn visible(row: &SessionRow, filter: &str, hide_idle: bool, blockers: &HashSet<&str>) -> bool {
    !(hide_idle && row.idle && !blockers.contains(row.id.as_str())) && row.matches(filter)
}

/// `0.4 s`, `12.0 s`, `3 min 05 s`, `2 h 10 min`.
pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        trf("{n} s", &[("n", &format!("{:.1}", ms as f64 / 1000.0))])
    } else if s < 3600 {
        trf("{m} min {s} s", &[("m", &(s / 60)), ("s", &format!("{:02}", s % 60))])
    } else {
        trf("{h} h {m} min", &[("h", &(s / 3600)), ("m", &format!("{:02}", s % 3600 / 60))])
    }
}

/// Milliseconds since `start`, without DATEDIFF's int overflow after 24 days.
fn mssql_ms(start: &str) -> String {
    format!(
        "CASE WHEN {start} IS NULL THEN NULL WHEN {start} < DATEADD(day, -20, GETDATE()) THEN CAST(CAST(DATEDIFF(second, {start}, GETDATE()) AS bigint) * 1000 AS varchar(20)) ELSE CAST(DATEDIFF(millisecond, {start}, GETDATE()) AS varchar(20)) END"
    )
}

/// The dialect's list query; `nonce` tells the monitor's own query apart (ClickHouse).
pub fn list_sql(driver: Driver, nonce: &str) -> Option<String> {
    let sql = match driver {
        Driver::Postgres => format!(
            "SELECT /* {MARKER} */ pid::text AS s_id, (extract(epoch FROM backend_start) * 1000000)::bigint::text AS s_started, \
             usename::text AS s_user, datname::text AS s_db, \
             CASE WHEN client_addr IS NOT NULL THEN host(client_addr) || ':' || client_port WHEN client_port = -1 THEN 'local' END AS s_client, \
             application_name AS s_app, COALESCE(state, backend_type) AS s_state, \
             CASE WHEN state IS DISTINCT FROM 'idle' THEN (extract(epoch FROM clock_timestamp() - CASE WHEN state = 'active' THEN query_start ELSE COALESCE(state_change, backend_start) END) * 1000)::bigint::text END AS s_ms, \
             CASE WHEN wait_event IS NOT NULL AND state IS DISTINCT FROM 'idle' THEN wait_event_type || ': ' || wait_event END AS s_wait, \
             array_to_string(pg_blocking_pids(pid), ',') AS s_blocked, query AS s_query, \
             CASE WHEN state = 'idle' THEN '1' ELSE '0' END AS s_idle, \
             CASE WHEN pid = pg_backend_pid() THEN '1' ELSE '0' END AS s_own, \
             CASE WHEN application_name = 'DataCraft' THEN '1' ELSE '0' END AS s_dc \
             FROM pg_stat_activity WHERE backend_type IN ('client backend', 'autovacuum worker') ORDER BY pid"
        ),
        Driver::MySql | Driver::MariaDb => format!(
            "SELECT /* {MARKER} */ CAST(ID AS CHAR) AS s_id, USER AS s_user, DB AS s_db, HOST AS s_client, COMMAND AS s_state, \
             CASE WHEN COMMAND = 'Sleep' THEN NULL ELSE CAST(TIME * 1000 AS CHAR) END AS s_ms, CAST(TIME * 1000 AS CHAR) AS s_state_ms, \
             STATE AS s_wait, INFO AS s_query, CASE WHEN COMMAND = 'Sleep' THEN '1' ELSE '0' END AS s_idle, \
             CASE WHEN ID = CONNECTION_ID() THEN '1' ELSE '0' END AS s_own \
             FROM information_schema.PROCESSLIST ORDER BY ID"
        ),
        Driver::MsSql => format!(
            "SELECT /* {MARKER} */ CAST(s.session_id AS varchar(10)) AS s_id, s.login_name AS s_user, \
             DB_NAME(COALESCE(r.database_id, s.database_id)) AS s_db, c.client_net_address AS s_client, s.program_name AS s_app, \
             COALESCE(r.status, s.status) AS s_state, \
             CASE WHEN r.session_id IS NOT NULL THEN {} WHEN s.open_transaction_count > 0 THEN {} END AS s_ms, \
             r.wait_type AS s_wait, CAST(NULLIF(r.blocking_session_id, 0) AS varchar(10)) AS s_blocked, t.text AS s_query, \
             CASE WHEN r.session_id IS NULL AND s.open_transaction_count = 0 THEN '1' ELSE '0' END AS s_idle, \
             CASE WHEN r.session_id IS NULL AND s.open_transaction_count > 0 THEN '1' ELSE '0' END AS s_tx, \
             CASE WHEN s.session_id = @@SPID THEN '1' ELSE '0' END AS s_own, \
             CASE WHEN s.program_name = 'DataCraft' THEN '1' ELSE '0' END AS s_dc \
             FROM sys.dm_exec_sessions s \
             LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             OUTER APPLY (SELECT TOP 1 client_net_address, most_recent_sql_handle FROM sys.dm_exec_connections WHERE session_id = s.session_id) c \
             OUTER APPLY sys.dm_exec_sql_text(COALESCE(r.sql_handle, c.most_recent_sql_handle)) t \
             WHERE s.is_user_process = 1 ORDER BY s.session_id",
            mssql_ms("r.start_time"),
            mssql_ms("s.last_request_end_time")
        ),
        Driver::Oracle => format!(
            "SELECT /* {MARKER} */ TO_CHAR(s.sid) AS s_id, TO_CHAR(s.serial#) AS s_serial, s.username AS s_user, s.schemaname AS s_db, \
             s.machine AS s_client, s.program AS s_app, s.status AS s_state, \
             CASE WHEN s.status = 'ACTIVE' OR s.taddr IS NOT NULL THEN TO_CHAR(s.last_call_et * 1000) END AS s_ms, \
             CASE WHEN s.wait_class <> 'Idle' THEN s.event END AS s_wait, TO_CHAR(s.blocking_session) AS s_blocked, q.sql_text AS s_query, \
             CASE WHEN s.status <> 'ACTIVE' AND s.taddr IS NULL THEN '1' ELSE '0' END AS s_idle, \
             CASE WHEN s.status <> 'ACTIVE' AND s.taddr IS NOT NULL THEN '1' ELSE '0' END AS s_tx, \
             CASE WHEN s.sid = SYS_CONTEXT('USERENV', 'SID') THEN '1' ELSE '0' END AS s_own, \
             CASE WHEN s.module = 'DataCraft' THEN '1' ELSE '0' END AS s_dc \
             FROM v$session s LEFT JOIN v$sql q ON q.sql_id = NVL(s.sql_id, s.prev_sql_id) \
             AND q.child_number = CASE WHEN s.sql_id IS NULL THEN s.prev_child_number ELSE s.sql_child_number END \
             WHERE s.type = 'USER' ORDER BY s.sid"
        ),
        Driver::ClickHouse => format!(
            "SELECT /* {MARKER} {nonce} */ query_id AS s_id, user AS s_user, toString(address) AS s_client, client_name AS s_app, \
             if(is_cancelled, 'cancelling', 'running') AS s_state, toString(toUInt64(elapsed * 1000)) AS s_ms, query AS s_query \
             FROM system.processes ORDER BY elapsed DESC"
        ),
        Driver::Sqlite | Driver::Snowflake | Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto => return None,
    };
    Some(sql)
}

fn column(rs: &ResultSet, name: &str) -> Option<usize> {
    rs.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
}

/// Reads the rows of a `list_sql` result. Duplicates (SQL Server MARS requests) keep the first.
pub fn parse(driver: Driver, rs: &ResultSet, nonce: &str) -> anyhow::Result<Vec<SessionRow>> {
    let id = column(rs, "s_id").context(tr("Incomplete session metadata"))?;
    let index: Vec<Option<usize>> =
        ["s_serial", "s_started", "s_user", "s_db", "s_client", "s_app", "s_state", "s_ms", "s_wait", "s_blocked", "s_query", "s_idle", "s_tx", "s_own", "s_dc"]
            .iter()
            .map(|name| column(rs, name))
            .collect();
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for row in &rs.rows {
        let Some(id) = row.get(id).cloned().flatten().filter(|v| !v.is_empty()) else { continue };
        let get = |i: usize| index[i].and_then(|c| row.get(c)).cloned().flatten().unwrap_or_default();
        let flag = |i: usize| get(i).trim() == "1";
        let query = get(10);
        let mut session = SessionRow {
            serial: Some(get(0)).filter(|v| !v.is_empty()),
            started: Some(get(1)).filter(|v| !v.is_empty()),
            user: get(2),
            database: get(3),
            client: get(4),
            application: get(5),
            state: get(6),
            duration_ms: get(7).trim().parse::<f64>().ok().map(|ms| ms.max(0.0) as u64),
            wait: get(8),
            blocked_by: get(9).split(',').map(str::trim).filter(|v| is_number(v) && *v != id).map(String::from).collect(),
            idle: flag(11),
            in_transaction: flag(12),
            own: flag(13),
            datacraft: flag(14),
            id,
            query,
        };
        if driver == Driver::ClickHouse {
            session.own = !nonce.is_empty() && session.query.contains(nonce);
            session.datacraft = session.query.contains(MARKER);
        }
        session.datacraft |= session.own;
        if seen.insert(session.label()) {
            rows.push(session);
        }
    }
    Ok(rows)
}

fn is_number(v: &str) -> bool {
    !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit())
}

/// The id checked to be a number before it goes into a statement.
fn number(v: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(is_number(v), trf("Invalid session id: {id}", &[("id", &v)]));
    Ok(v)
}

/// A ClickHouse string literal.
fn ch_literal(v: &str) -> anyhow::Result<String> {
    anyhow::ensure!(!v.is_empty() && !v.contains('\0'), trf("Invalid session id: {id}", &[("id", &v)]));
    Ok(format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'")))
}

/// The statement that carries out `action`.
pub fn action_sql(driver: Driver, action: &Action) -> anyhow::Result<String> {
    let id = &action.id;
    Ok(match (driver, action.kind) {
        (Driver::Postgres, kind) => {
            let function = if kind == ActionKind::Cancel { "pg_cancel_backend" } else { "pg_terminate_backend" };
            // Only the backend that was listed, never the monitor's own.
            let started = match &action.started {
                Some(started) => format!(" AND (extract(epoch FROM backend_start) * 1000000)::bigint = {}", number(started)?),
                None => String::new(),
            };
            format!("SELECT {function}(pid)::text FROM pg_stat_activity WHERE pid = {} AND pid <> pg_backend_pid(){started}", number(id)?)
        }
        (Driver::MySql | Driver::MariaDb, ActionKind::Cancel) => format!("KILL QUERY {}", number(id)?),
        (Driver::MySql | Driver::MariaDb, ActionKind::Terminate) => format!("KILL {}", number(id)?),
        (Driver::MsSql, ActionKind::Terminate) => format!("KILL {}", number(id)?),
        (Driver::Oracle, kind) => {
            let serial = action.serial.as_deref().map(number).transpose()?.context(tr("Incomplete session metadata"))?;
            match kind {
                ActionKind::Cancel => format!("ALTER SYSTEM CANCEL SQL '{},{serial}'", number(id)?),
                ActionKind::Terminate => format!("ALTER SYSTEM KILL SESSION '{},{serial}' IMMEDIATE", number(id)?),
            }
        }
        (Driver::ClickHouse, ActionKind::Cancel) => format!("KILL QUERY WHERE query_id = {} ASYNC", ch_literal(id)?),
        (d, ActionKind::Cancel) => anyhow::bail!(cancel_unsupported(d).unwrap_or(tr("Session monitoring is not available for this engine"))),
        (d, ActionKind::Terminate) => anyhow::bail!(terminate_unsupported(d).unwrap_or(tr("Session monitoring is not available for this engine"))),
    })
}

/// First result set of `sql` (empty if it returned none).
async fn query(session: &mut Session, sql: &str, max_rows: usize) -> anyhow::Result<ResultSet> {
    Ok(session.execute(sql, max_rows).await?.into_iter().next().unwrap_or_default())
}

/// The server's sessions.
pub async fn list(session: &mut Session, driver: Driver) -> anyhow::Result<SessionList> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let sql = list_sql(driver, &nonce).context(tr("Session monitoring is not available for this engine"))?;
    let rs = query(session, &sql, MAX_ROWS).await.context(tr("Could not read the server's sessions"))?;
    let mut list = SessionList { rows: parse(driver, &rs, &nonce)?, notice: None };
    if matches!(driver, Driver::MySql | Driver::MariaDb) {
        list.notice = my_locks(session, &rs, &mut list.rows).await;
    }
    if rs.truncated {
        list.notice = Some(trf("Only the first {n} sessions are shown", &[("n", &MAX_ROWS)]));
    }
    Ok(list)
}

/// MySQL / MariaDB: open transactions and lock waits, from whichever views the server has. Their
/// absence (or a missing privilege) only leaves them out; the reason is returned.
async fn my_locks(session: &mut Session, rs: &ResultSet, rows: &mut [SessionRow]) -> Option<String> {
    let (id, state_ms) = (column(rs, "s_id"), column(rs, "s_state_ms"));
    let cell = |r: &Vec<Option<String>>, c: Option<usize>| c.and_then(|c| r.get(c)).cloned().flatten();
    let ms: std::collections::HashMap<String, u64> =
        rs.rows.iter().filter_map(|r| Some((cell(r, id)?, cell(r, state_ms)?.trim().parse::<f64>().ok()? as u64))).collect();
    let pairs = |rs: ResultSet| -> Vec<(String, String)> {
        rs.rows.into_iter().filter_map(|r| Some((r.first()?.clone()?, r.get(1)?.clone()?))).collect()
    };
    let open: HashSet<String> = match query(session, "SELECT CAST(trx_mysql_thread_id AS CHAR) FROM information_schema.INNODB_TRX", MAX_ROWS).await {
        Ok(rs) => rs.rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect(),
        Err(_) => HashSet::new(),
    };
    for row in rows.iter_mut().filter(|r| r.idle && open.contains(&r.id)) {
        row.idle = false;
        row.in_transaction = true;
        row.duration_ms = ms.get(&row.id).copied();
    }
    let waits = match query(session, "SELECT CAST(waiting_pid AS CHAR), CAST(blocking_pid AS CHAR) FROM sys.innodb_lock_waits", MAX_ROWS).await {
        Ok(rs) => pairs(rs),
        Err(_) => match query(
            session,
            "SELECT CAST(r.trx_mysql_thread_id AS CHAR), CAST(b.trx_mysql_thread_id AS CHAR) FROM information_schema.INNODB_LOCK_WAITS w \
             JOIN information_schema.INNODB_TRX b ON b.trx_id = w.blocking_trx_id JOIN information_schema.INNODB_TRX r ON r.trx_id = w.requesting_trx_id",
            MAX_ROWS,
        )
        .await
        {
            Ok(rs) => pairs(rs),
            Err(_) => return Some(tr("Lock waits are not shown: the sys schema and the InnoDB lock views are not available").into()),
        },
    };
    for (waiting, blocking) in waits {
        if let Some(row) = rows.iter_mut().find(|r| r.id == waiting)
            && is_number(&blocking)
            && !row.blocked_by.contains(&blocking)
        {
            row.blocked_by.push(blocking);
        }
    }
    None
}

/// Cancels or terminates; fails when the server says the session or query is already gone.
pub async fn act(session: &mut Session, driver: Driver, action: &Action) -> anyhow::Result<()> {
    let sql = action_sql(driver, action)?;
    let rs = query(session, &sql, 10).await?;
    match driver {
        // No row: the backend (or the one with that start time) is gone; false: it could not be signalled.
        Driver::Postgres => {
            anyhow::ensure!(rs.rows.first().and_then(|r| r.first()).and_then(|v| v.as_deref()) == Some("true"), tr("The session has already ended"))
        }
        Driver::ClickHouse => anyhow::ensure!(!rs.rows.is_empty(), tr("The query has already finished")),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(columns: &[&str], rows: &[&[Option<&str>]]) -> ResultSet {
        ResultSet {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: rows.iter().map(|r| r.iter().map(|v| v.map(String::from)).collect()).collect(),
            ..Default::default()
        }
    }

    const PG: [&str; 14] = ["s_id", "s_started", "s_user", "s_db", "s_client", "s_app", "s_state", "s_ms", "s_wait", "s_blocked", "s_query", "s_idle", "s_own", "s_dc"];

    #[test]
    fn parses_postgres_rows() {
        let rs = rs(
            &PG,
            &[
                &[Some("101"), Some("1790000000000000"), Some("app"), Some("shop"), Some("10.0.0.5:51544"), Some("psql"), Some("active"), Some("2500"), Some("Lock: transactionid"), Some("102,103"), Some("update orders set paid = true"), Some("0"), Some("0"), Some("0")],
                &[Some("102"), Some("1790000000000001"), Some("app"), Some("shop"), Some("local"), Some("DataCraft"), Some("idle in transaction"), Some("61000"), None, Some(""), Some("select 1"), Some("0"), Some("1"), Some("1")],
                &[Some("103"), None, Some("app"), None, None, None, Some("idle"), None, None, Some(""), Some(""), Some("1"), Some("0"), Some("0")],
                &[None, None, None, None, None, None, None, None, None, None, None, None, None, None],
            ],
        );
        let rows = parse(Driver::Postgres, &rs, "").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].blocked_by, ["102", "103"]);
        assert_eq!((rows[0].duration_ms, rows[0].wait.as_str(), rows[0].idle), (Some(2500), "Lock: transactionid", false));
        assert_eq!(rows[0].started.as_deref(), Some("1790000000000000"));
        assert!(rows[1].own && rows[1].datacraft && rows[1].blocked_by.is_empty());
        assert!(rows[2].idle && rows[2].duration_ms.is_none() && rows[2].started.is_none());
    }

    #[test]
    fn parses_mysql_sqlserver_oracle_and_clickhouse_rows() {
        let my = rs(
            &["S_ID", "S_USER", "S_DB", "S_CLIENT", "S_STATE", "S_MS", "S_STATE_MS", "S_WAIT", "S_QUERY", "S_IDLE", "S_OWN"],
            &[&[Some("7"), Some("root"), Some("shop"), Some("172.17.0.1:40112"), Some("Query"), Some("3000"), Some("3000"), Some("updating"), Some("UPDATE t SET a = 1"), Some("0"), Some("1")]],
        );
        let rows = parse(Driver::MySql, &my, "").unwrap();
        assert_eq!((rows[0].id.as_str(), rows[0].state.as_str(), rows[0].duration_ms, rows[0].own), ("7", "Query", Some(3000), true));

        let ms = rs(
            &["s_id", "s_user", "s_db", "s_client", "s_app", "s_state", "s_ms", "s_wait", "s_blocked", "s_query", "s_idle", "s_tx", "s_own", "s_dc"],
            &[
                &[Some("53"), Some("sa"), Some("shop"), Some("10.0.0.9"), Some("DataCraft"), Some("suspended"), Some("1200"), Some("LCK_M_X"), Some("54"), Some("UPDATE t"), Some("0"), Some("0"), Some("0"), Some("1")],
                &[Some("53"), Some("sa"), Some("shop"), None, None, Some("running"), None, None, None, None, Some("0"), Some("0"), Some("0"), Some("1")],
                &[Some("54"), Some("sa"), Some("shop"), None, None, Some("sleeping"), Some("90000"), None, Some("-2"), None, Some("0"), Some("1"), Some("0"), Some("0")],
            ],
        );
        let rows = parse(Driver::MsSql, &ms, "").unwrap();
        assert_eq!(rows.len(), 2, "MARS duplicates keep the first request");
        assert_eq!((rows[0].blocked_by.as_slice(), rows[0].datacraft), (["54".to_string()].as_slice(), true));
        assert!(rows[1].in_transaction && !rows[1].idle && rows[1].blocked_by.is_empty());

        let ora = rs(
            &["S_ID", "S_SERIAL", "S_USER", "S_STATE", "S_BLOCKED", "S_IDLE", "S_OWN", "S_DC"],
            &[&[Some("45"), Some("6789"), Some("SCOTT"), Some("ACTIVE"), Some("12"), Some("0"), Some("0"), Some("0")]],
        );
        let rows = parse(Driver::Oracle, &ora, "").unwrap();
        assert_eq!((rows[0].label(), rows[0].blocked_by.as_slice()), ("45,6789".to_string(), ["12".to_string()].as_slice()));

        let ch = rs(
            &["s_id", "s_user", "s_client", "s_app", "s_state", "s_ms", "s_query"],
            &[
                &[Some("a1b2-c3"), Some("default"), Some("::ffff:10.0.0.4"), Some("ClickHouse client"), Some("running"), Some("1500"), Some("SELECT count() FROM hits")],
                &[Some("d4"), Some("default"), None, None, Some("running"), Some("0"), Some("SELECT /* DataCraft session monitor n0nce */ query_id")],
                &[Some("e5"), Some("default"), None, None, Some("running"), Some("0"), Some("SELECT /* DataCraft session monitor other */ query_id")],
            ],
        );
        let rows = parse(Driver::ClickHouse, &ch, "n0nce").unwrap();
        assert_eq!((rows[0].own, rows[0].datacraft, rows[0].duration_ms), (false, false, Some(1500)));
        assert!(rows[1].own && rows[1].datacraft);
        assert!(!rows[2].own && rows[2].datacraft, "another monitor's query is DataCraft's, not ours");
        assert!(parse(Driver::Postgres, &rs(&["pid"], &[]), "").is_err());
    }

    fn action(kind: ActionKind, id: &str) -> Action {
        Action { kind, id: id.into(), serial: None, started: None }
    }

    #[test]
    fn builds_statements_per_dialect() {
        let mut pg = action(ActionKind::Cancel, "4242");
        pg.started = Some("1790000000000000".into());
        assert_eq!(
            action_sql(Driver::Postgres, &pg).unwrap(),
            "SELECT pg_cancel_backend(pid)::text FROM pg_stat_activity WHERE pid = 4242 AND pid <> pg_backend_pid() AND (extract(epoch FROM backend_start) * 1000000)::bigint = 1790000000000000"
        );
        pg.kind = ActionKind::Terminate;
        assert!(action_sql(Driver::Postgres, &pg).unwrap().starts_with("SELECT pg_terminate_backend(pid)::text"));
        assert_eq!(action_sql(Driver::MySql, &action(ActionKind::Cancel, "7")).unwrap(), "KILL QUERY 7");
        assert_eq!(action_sql(Driver::MariaDb, &action(ActionKind::Terminate, "7")).unwrap(), "KILL 7");
        assert_eq!(action_sql(Driver::MsSql, &action(ActionKind::Terminate, "53")).unwrap(), "KILL 53");
        assert!(action_sql(Driver::MsSql, &action(ActionKind::Cancel, "53")).is_err());
        let ora = Action { serial: Some("6789".into()), ..action(ActionKind::Cancel, "45") };
        assert_eq!(action_sql(Driver::Oracle, &ora).unwrap(), "ALTER SYSTEM CANCEL SQL '45,6789'");
        assert_eq!(action_sql(Driver::Oracle, &Action { kind: ActionKind::Terminate, ..ora }).unwrap(), "ALTER SYSTEM KILL SESSION '45,6789' IMMEDIATE");
        assert!(action_sql(Driver::Oracle, &action(ActionKind::Cancel, "45")).is_err(), "Oracle needs the serial#");
        assert_eq!(action_sql(Driver::ClickHouse, &action(ActionKind::Cancel, "it's\\x")).unwrap(), "KILL QUERY WHERE query_id = 'it\\'s\\\\x' ASYNC");
        assert!(action_sql(Driver::ClickHouse, &action(ActionKind::Terminate, "q")).is_err());
        assert!(action_sql(Driver::ClickHouse, &action(ActionKind::Cancel, "")).is_err());
        assert!(action_sql(Driver::Sqlite, &action(ActionKind::Cancel, "1")).is_err());
    }

    #[test]
    fn rejects_ids_that_are_not_numbers() {
        for id in ["", "1; DROP TABLE t", "12a", "-1", " 1", "1 OR 1=1", "١٢", "123456789012345678901"] {
            for driver in [Driver::Postgres, Driver::MySql, Driver::MsSql] {
                assert!(action_sql(driver, &action(ActionKind::Terminate, id)).is_err(), "{driver:?} accepted {id:?}");
            }
        }
        let started = Action { started: Some("1 OR true".into()), ..action(ActionKind::Cancel, "1") };
        assert!(action_sql(Driver::Postgres, &started).is_err());
        let serial = Action { serial: Some("1' IMMEDIATE --".into()), ..action(ActionKind::Terminate, "45") };
        assert!(action_sql(Driver::Oracle, &serial).is_err());
    }

    fn row(id: &str, blocked_by: &[&str], idle: bool) -> SessionRow {
        SessionRow { id: id.into(), blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(), idle, query: format!("select {id}"), user: "app".into(), ..Default::default() }
    }

    #[test]
    fn highlights_blocked_sessions_and_head_blockers() {
        let rows = [row("1", &[], true), row("2", &["1"], false), row("3", &["2"], false), row("4", &[], false)];
        let blockers = blockers(&rows);
        let marks: Vec<_> = rows.iter().map(|r| highlight(r, &blockers)).collect();
        assert_eq!(marks, [Highlight::Blocker, Highlight::Blocked, Highlight::Blocked, Highlight::None]);
    }

    #[test]
    fn filters_idle_sessions_and_text() {
        let rows = [row("1", &[], true), row("2", &["1"], false), row("5", &[], true)];
        let blockers = blockers(&rows);
        let shown = |filter: &str, hide: bool| rows.iter().filter(|r| visible(r, filter, hide, &blockers)).map(|r| r.id.as_str()).collect::<Vec<_>>();
        assert_eq!(shown("", false), ["1", "2", "5"]);
        assert_eq!(shown("", true), ["1", "2"], "an idle blocker stays visible");
        assert_eq!(shown("select 5", false), ["5"]);
        assert_eq!(shown("app", true), ["1", "2"]);
        assert!(shown("nothing", false).is_empty());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(400), "0.4 s");
        assert_eq!(format_duration(12_049), "12.0 s");
        assert_eq!(format_duration(185_000), "3 min 05 s");
        assert_eq!(format_duration(7_830_000), "2 h 10 min");
    }

    #[test]
    fn engines_without_sessions_have_no_query() {
        for driver in Driver::ALL {
            assert_eq!(list_sql(driver, "n").is_some(), supported(driver), "{driver:?}");
        }
        assert!(cancel_unsupported(Driver::Postgres).is_none() && terminate_unsupported(Driver::Oracle).is_none());
        assert!(cancel_unsupported(Driver::MsSql).is_some() && terminate_unsupported(Driver::ClickHouse).is_some());
    }

    /// Lists until `found` matches a row (5 s at most).
    async fn wait_row(monitor: &mut Session, found: impl Fn(&SessionRow, &[SessionRow]) -> bool) -> (SessionRow, Vec<SessionRow>) {
        for _ in 0..50 {
            let rows = list(monitor, Driver::Postgres).await.unwrap().rows;
            if let Some(row) = rows.iter().find(|r| found(r, &rows)) {
                return (row.clone(), rows);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the expected session never showed up");
    }

    /// Against a real PostgreSQL (`cargo test -- --ignored` with DATACRAFT_PG_HOST, _PORT, _USER,
    /// _PASSWORD and _DB, as in `db::postgres`): a sleeping query is listed and cancelled, then a
    /// session waiting for a row lock shows its blocker, which is terminated.
    #[tokio::test]
    #[ignore]
    async fn postgres_cancel_blocking_and_terminate() {
        let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else {
            eprintln!("DATACRAFT_PG_HOST not set: skipping the PostgreSQL test");
            return;
        };
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let mut cfg = crate::db::ConnectionConfig::new(1, Driver::Postgres);
        cfg.host = host;
        cfg.port = var("DATACRAFT_PG_PORT", "5432").parse().unwrap();
        cfg.user = var("DATACRAFT_PG_USER", "postgres");
        cfg.password = var("DATACRAFT_PG_PASSWORD", "");
        cfg.database = var("DATACRAFT_PG_DB", "postgres");
        let connect = || Session::connect(&cfg);
        let pid = async |s: &mut Session| query(s, "select pg_backend_pid()::text", 1).await.unwrap().rows[0][0].clone().unwrap();
        let mut monitor = connect().await.unwrap();
        let own = pid(&mut monitor).await;

        // A sleeping query on a second connection: listed as active, then cancelled.
        let mut sleeper = connect().await.unwrap();
        let sleeper_pid = pid(&mut sleeper).await;
        // English server messages (a superuser setting), so the cancellation can be recognised.
        let _ = sleeper.execute("set lc_messages to 'C'", 0).await;
        let sleeping = tokio::spawn(async move {
            let result = sleeper.execute("select pg_sleep(30)", 1).await;
            (sleeper, result)
        });
        let (row, rows) = wait_row(&mut monitor, |r, _| r.id == sleeper_pid && r.state == "active" && r.query.contains("pg_sleep")).await;
        assert!(row.datacraft && !row.own && !row.idle && row.started.is_some() && row.duration_ms.is_some());
        let mine = rows.iter().find(|r| r.own).expect("the monitor's own session is listed");
        assert_eq!(mine.id, own);
        assert!(act(&mut monitor, Driver::Postgres, &Action::new(ActionKind::Terminate, mine)).await.is_err(), "never the monitor's own session");
        let stale = Action { started: Some("1".into()), ..Action::new(ActionKind::Cancel, &row) };
        assert_eq!(act(&mut monitor, Driver::Postgres, &stale).await.unwrap_err().to_string(), "The session has already ended");
        act(&mut monitor, Driver::Postgres, &Action::new(ActionKind::Cancel, &row)).await.unwrap();
        let (mut sleeper, result) = tokio::time::timeout(std::time::Duration::from_secs(10), sleeping).await.expect("cancelled in time").unwrap();
        let error = format!("{:#}", result.expect_err("the sleep was cancelled"));
        assert!(error.contains("canceling statement due to user request"), "{error}");
        assert_eq!(pid(&mut sleeper).await, sleeper_pid, "cancelling keeps the session");

        // A lock wait: A holds a row lock in an open transaction, B waits for it.
        let table = format!("dc_session_monitor_{own}");
        sleeper.execute(&format!("drop table if exists {table}; create table {table} (id int primary key, v int); insert into {table} values (1, 0)"), 0).await.unwrap();
        let mut a = connect().await.unwrap();
        let a_pid = pid(&mut a).await;
        a.execute(&format!("begin; update {table} set v = v + 1 where id = 1"), 0).await.unwrap();
        let mut b = connect().await.unwrap();
        let b_pid = pid(&mut b).await;
        let update = format!("update {table} set v = v + 10 where id = 1");
        let waiting = tokio::spawn(async move { b.execute(&update, 0).await });
        let (blocked, rows) = wait_row(&mut monitor, |r, _| r.id == b_pid && !r.blocked_by.is_empty()).await;
        assert_eq!(blocked.blocked_by, std::slice::from_ref(&a_pid));
        assert!(blocked.wait.starts_with("Lock"), "{}", blocked.wait);
        let blocker = rows.iter().find(|r| r.id == a_pid).unwrap();
        assert_eq!(blocker.state, "idle in transaction");
        let marks = blockers(&rows);
        assert_eq!((highlight(blocker, &marks), highlight(&blocked, &marks)), (Highlight::Blocker, Highlight::Blocked));
        assert!(visible(blocker, "", true, &marks), "an idle-in-transaction blocker stays visible");

        // Terminating A rolls its update back and lets B through.
        act(&mut monitor, Driver::Postgres, &Action::new(ActionKind::Terminate, blocker)).await.unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(10), waiting).await.expect("B got the lock").unwrap();
        assert_eq!(done.unwrap()[0].affected, Some(1));
        assert!(a.execute("select 1", 1).await.is_err(), "A was terminated");
        wait_row(&mut monitor, |_, rows| !rows.iter().any(|r| r.id == a_pid)).await;
        let v = query(&mut sleeper, &format!("select v::text from {table}"), 1).await.unwrap();
        assert_eq!(v.rows[0][0].as_deref(), Some("10"), "A's update was rolled back");
        sleeper.execute(&format!("drop table {table}"), 0).await.unwrap();
    }
}
