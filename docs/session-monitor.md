# Session Monitor

Right-click a connected PostgreSQL, MySQL, MariaDB, SQL Server, Oracle or ClickHouse
connection in Database Explorer and choose **Session Monitor…**. The window lists the
server's sessions without blocking the rest of the app: consoles, the explorer and other
windows stay usable while it is open, and one monitor can be open per connection.

![Session Monitor with a blocking chain](screenshots/session-monitor/session-monitor-dark.png)

Each row shows the session id, user, database, client address, application, state,
how long the current query (or open transaction) has run, the wait event, the sessions
it is **blocked by**, and the query on one line. Hover the query for its full text, or
select the row to read it, highlighted, in the pane below the table, which has **Copy
Query**. The row's context menu has **Copy Query** and **Copy Session ID**.

Sessions waiting for another are tinted amber; the session at the head of a chain,
which others wait for and which waits for nobody, is tinted red. In the example, 4310
holds a row lock in an open transaction, 4242 waits for it and 4377 waits for 4242.
**Hide idle sessions** keeps blockers visible even when they are idle, and the text
filter matches any column. The monitor's own session is marked *monitor* and in italics;
other DataCraft sessions are marked *DataCraft* where the server reports the
application name (PostgreSQL, SQL Server, Oracle).

**Refresh** reads the list again; **Auto-refresh** repeats it every 2, 5 or 10 seconds
while the window is open. A refresh never starts while the previous one is still running.

## Cancel and terminate

Select a session and choose **Cancel Query…** to stop its running statement (the
session and its transaction stay open) or **Terminate Session…** to disconnect it (its
open transaction is rolled back). Both ask for confirmation and show the statement that
will run. The monitor's own session can't be cancelled or terminated, and a session that
isn't running a query can't be cancelled. On read-only connections the list is
available, but both actions are disabled.

![Terminate Session confirmation](screenshots/session-monitor/session-monitor-terminate-dark.png)

| Engine | Sessions from | Blocking | Cancel | Terminate |
| --- | --- | --- | --- | --- |
| PostgreSQL | `pg_stat_activity` (client backends and autovacuum workers) | `pg_blocking_pids` | `pg_cancel_backend` | `pg_terminate_backend` |
| MySQL / MariaDB | `information_schema.PROCESSLIST` | `sys.innodb_lock_waits`, or the InnoDB lock views, when available | `KILL QUERY` | `KILL` |
| SQL Server | `sys.dm_exec_sessions`, `sys.dm_exec_requests`, `sys.dm_exec_sql_text` | `blocking_session_id` | Not available | `KILL` |
| Oracle | `v$session`, `v$sql` | `blocking_session` | `ALTER SYSTEM CANCEL SQL` (18c and later) | `ALTER SYSTEM KILL SESSION … IMMEDIATE` |
| ClickHouse | `system.processes` (running queries, not sessions) | — | `KILL QUERY` | Not available |

SQLite, Snowflake and the NoSQL engines have no Session Monitor.

On PostgreSQL an action only reaches the backend that was listed: if the process ended
and its id was reused, the action fails with "The session has already ended". Session
ids always come from the listed rows and are checked to be numbers (a ClickHouse query
id is escaped) before they are written into a statement.

## Permissions and errors

The monitor uses a database session of its own, opened on first use and kept until the
connection is closed, so it doesn't wait behind a running console query and never cancels
or takes down a console's session. Each request is limited to 15 seconds.

What the list contains depends on the account. PostgreSQL shows other users' query text
only to superusers and members of `pg_read_all_stats`; cancelling or terminating another
role's session needs `pg_signal_backend` or superuser. MySQL lists other accounts'
threads with `PROCESS` and kills them with `CONNECTION_ADMIN` (or `SUPER`). SQL Server
needs `VIEW SERVER STATE` (`VIEW SERVER PERFORMANCE STATE` on 2022) to see other sessions
and `ALTER ANY CONNECTION` to kill them. Oracle needs access to `v$session` and `v$sql`
and `ALTER SYSTEM`. Errors, such as a missing permission, appear in the window above the
table; the last list stays visible. On MySQL and MariaDB a note explains when lock waits
can't be read.

The PostgreSQL queries, cancel, terminate and the lock-wait display are covered by tests
against a live PostgreSQL 16 server. The other engines' queries and statements have unit
tests but were not run against live servers.

References: [pg_stat_activity](https://www.postgresql.org/docs/current/monitoring-stats.html#MONITORING-PG-STAT-ACTIVITY-VIEW),
[PostgreSQL signaling functions](https://www.postgresql.org/docs/current/functions-admin.html#FUNCTIONS-ADMIN-SIGNAL),
[MySQL KILL](https://dev.mysql.com/doc/refman/8.4/en/kill.html),
[SQL Server KILL](https://learn.microsoft.com/en-us/sql/t-sql/language-elements/kill-transact-sql),
[Oracle ALTER SYSTEM](https://docs.oracle.com/en/database/oracle/oracle-database/19/sqlrf/ALTER-SYSTEM.html),
[ClickHouse KILL](https://clickhouse.com/docs/sql-reference/statements/kill).
