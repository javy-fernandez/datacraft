# Scheduled SQL and backup tasks

Open **File → Task scheduler…** in the DataCraft application menu. Add a task,
choose **SQL** or **Backup**, and set an interval in seconds, minutes, hours or
days. SQL tasks show the statement before saving. Leave the SQL task's database
field blank to use the data
source's default database. A task can be paused without stopping an execution
that is already in progress. Editing or deleting an active task is refused until
that execution has finished; pausing prevents its next execution.

The task list stays on the left, while **Configuration** provides the SQL editor
and schedule settings and **History** shows recent executions and errors. Save
and Cancel stay visible below both views. Switching tasks or closing with unsaved
changes asks whether to keep editing or discard the draft. A saved task remains
saved if background startup fails; the error is shown so startup can be retried
without creating a duplicate task.

## Recurring backups without SQL

Choose **Backup**, a saved data source, and the database. The database picker
can load the source's available databases using the refresh button; SQLite uses
`main` and Oracle uses the service saved in its connection. Choose a destination
and confirm the settings. Only full backups are scheduled. Every execution
generates a new filename or backup-set name from the database, UTC time and a
unique identifier. Existing copies are never replaced or automatically deleted.
The successful destination appears in **History**.
Oracle tasks are tied to that saved service; changing the connection's service
requires updating the task rather than silently exporting a different service.

| Engine | Destination | Prerequisites |
| --- | --- | --- |
| SQLite | Local folder | An existing writable folder; uses the consistent online backup API |
| PostgreSQL | Local folder | `pg_dump` installed on this computer; archive or SQL format, optional compression |
| MySQL / MariaDB | Local folder | `mysqldump` / `mariadb-dump` installed on this computer; optional gzip |
| SQL Server | Folder on the database server | Folder writable by the SQL Server service; copy-only full backups with checksums |
| Oracle | Server DIRECTORY object | Data Pump full-export privileges; the file is on the server |
| Snowflake | Backup-set schema | Native backup-set privileges; creates a new set for each run |

The local file picker is shown only for local destinations. A SQL Server path
can be a Windows drive, UNC share or Unix path regardless of which OS runs
DataCraft. **Backup options** contains the utility path and engine-specific
compression/archive controls. Credentials remain in the OS credential store.
For MySQL/MariaDB, the existing certificate-verification and transaction options
are available too. Consistent transaction mode is for InnoDB; disable it to lock
nontransactional tables during their dump.

When upgrading from the earlier SQL-only scheduler, restart an already running
background process once (signing out and back in restarts its registered user
service). An older process cannot run backup tasks; backup records deliberately
contain no executable SQL so they fail safely until the process is upgraded.

## Background operation

An enabled task starts a separate background process and registers it to start
when the user signs in: the current user's Run entry on Windows, a LaunchAgent
on macOS, or a user systemd service on Linux. When the systemd user manager is
unavailable, Linux uses the desktop's XDG autostart mechanism, which requires
an XDG-compatible desktop session. The process does not open a window and continues after DataCraft
closes. It runs only while the computer and user session are available; it does
not wake a sleeping computer or run while the user is signed out. Keep the
DataCraft executable in its registered location. If it moves, open the scheduler
and enable the background scheduler again. System startup registration failures
are shown in the scheduler window. The background process and its startup entry
are removed automatically when no enabled tasks remain. **Turn off** in the
scheduler window pauses all tasks and removes automatic startup. Executions already
in progress may finish; the process exits after they complete or time out. Saving
an enabled task registers and starts the scheduler again. Tasks and history are
retained when the background scheduler is turned off.

The process sleeps until the next deadline or a local wakeup from a task change;
it does not open a write transaction every second while idle. Task changes and
shutdown are coordinated so enabling a task cannot race with automatic removal.

The first execution is due after the configured interval. Missed intervals are
not replayed individually, and one task never runs concurrently with itself.
If an execution takes longer than its interval, its next run is scheduled after
it finishes. A maximum of four tasks run at once. SQL and backup tasks for the
same SQLite path run sequentially; Unix also recognizes aliases of the same
file. Tasks for other files and data sources can still run in parallel. The
history shows the last 100 runs per task and records failures and interrupted runs. If the process or
computer stops during a statement, the database may have completed it even if
DataCraft could not record the outcome; inspect the database before retrying.
Tasks execute the same dialect-aware batches as the SQL console. Further batches
are not submitted after an error; statements inside a batch follow the database
engine's error semantics. Results are discarded after the entire execution is
checked, and tasks are not automatically wrapped in a transaction.

Each SQL batch has a 30-minute timeout. Connecting and selecting a database have
30 seconds each; cancellation is attempted for up to three seconds. Waiting for
another operation on the same SQLite file is also limited to 30 minutes. A failed
operation can leave native driver cleanup in progress; at most four SQL sessions
may remain active or stopping, and their SQLite file locks stay held until cleanup
ends. During shutdown, already failed sessions get up to ten more seconds to clean
up. Cancellation is best-effort: a server-side statement may still finish after a
timeout, so inspect the database before retrying a write.

Background tasks use the saved data source settings and system credential
store. They cannot answer password or SSH host-key prompts: save the required
credentials and trust the SSH host key through the connection dialog first.
An unavailable or locked credential store may prevent a run. On macOS, the separate
background process disables Keychain interaction, so it records an error instead
of displaying an access prompt. Open DataCraft and authorize the saved connection's
credentials before retrying, including after an application update. Read-only data
sources retain their existing restrictions. The task's SQL and execution
history are stored locally; do not put passwords or tokens in SQL text.

[UI previews and reproduction instructions](screenshots/scheduler/README.md)
show the scheduler on synthetic test data.
