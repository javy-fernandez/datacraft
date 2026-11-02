# Database backups

For recurring full backups, use **File → Task scheduler… → Backup**. Choose the
database, destination and interval without writing SQL. See [scheduled backups](sql-scheduler.md#recurring-backups-without-sql)
for destination differences and native utility requirements.

Right-click a connected SQL database in Database Explorer and choose **Back up
database…**. Review the destination and engine-specific options, then select
**Start backup**. Jobs run in the background and can be cancelled from the dialog.
Successful jobs appear in that database's collapsed **Backups** folder. History is
local metadata; deleting or moving a backup outside DataCraft does not update it.

| Engine | Backup | Destination and requirements |
| --- | --- | --- |
| PostgreSQL | Full, structure only, data only; plain SQL or custom archive; optional compression | Local file; matching `pg_dump` installation. Restore with **Restore database…** (below). |
| MySQL / MariaDB | Full, structure only, data only; optional routines, events, triggers and gzip | Local file; `mysqldump` / `mariadb-dump`. Transaction mode is for InnoDB; disable it to lock non-transactional tables. |
| SQLite | Consistent online backup, including committed WAL data | Local SQLite file; no external utility. Implicit rowids are preserved. |
| SQL Server | Full, differential, transaction log; optional checksums and compression | Path on the database server, writable by its service account. Existing backup media are appended to. Copy-only is enabled by default except for differential backups. |
| Oracle | Data Pump full export, metadata only, data only; optional export log | Server `DIRECTORY` object and a new `.dmp` filename; full-export and directory privileges. This is a logical export, not an RMAN physical backup. |
| Snowflake | Native database backup in a newly created backup set | Database, schema and new backup-set name; native backup-set support and privileges. No automatic expiration is configured. |

PostgreSQL, MySQL and MariaDB need their native client installed. DataCraft looks
on `PATH` and common installation directories; the dialog also accepts an explicit
executable. Native clients must support the server version and selected flags.
MySQL and MariaDB inherit **Trust server certificate** from the connection, with
an override in the backup dialog. Unchecked means certificate verification; MySQL
requires TLS in that mode. PostgreSQL uses libpq's TLS configuration.

Database passwords are passed through temporary credential files with restricted
permissions, not command-line arguments. Local output is staged and published
only after success. Existing local files are never overwritten; incomplete local
output and temporary credentials are removed after failure or cancellation.

Server backups need write privileges and are unavailable on read-only data
sources. Read-only connections can still create local logical dumps or SQLite
snapshots. No backup changes the source database's recovery or logging settings.
Logical dumps omit binary/transaction log files; a SQL Server full backup includes
the recovery log needed to restore it. Oracle's export log option controls a job
report, not the database redo log.

Cancel stops local utilities or the SQLite backup. Server cancellation is best
effort: verify the server job and destination after interruption before retrying.
Oracle additionally attempts to stop its own named Data Pump job. A failed
Snowflake job may leave its newly created backup set. Keep the application open
until completion or cancellation finishes. Jobs have a 30-minute execution limit.

The Windows tests restore a temporary SQLite WAL database, verify preserved
rowids, reject existing destinations, and exercise failure cleanup and credential
file removal. Other engine implementations have command/SQL and dialog tests;
backup/restore against external servers and macOS still require integration tests.

## Restoring a database

Right-click a connected database and choose **Restore database…** (or use the
connection's menu to restore into a new database, or **Restore from this backup…**
on an entry in the **Backups** folder). Choose the backup and select **Inspect**:
DataCraft reads its format and contents before anything is restored. Then pick the
target database and the engine options and select **Start restore**. Restoring into a
database that already exists asks for confirmation first. The job runs in the
background with progress and **Cancel**; finished restores are listed under
**Previous restores** and the explorer is refreshed.

| Engine | Backup formats | Options |
| --- | --- | --- |
| PostgreSQL | Custom, directory and tar archives through `pg_restore`; plain `.sql` and `.sql.gz` through `psql` | Existing or new target database (created from `template0`), clean with `--if-exists`, `--no-owner`, schema only / data only, parallel jobs for custom and directory archives. The archive's table of contents is listed with `pg_restore --list`. |
| MySQL / MariaDB | Plain `.sql` and `.sql.gz` dumps through `mysql` / `mariadb` | Existing or new target database, server certificate verification. `CREATE DATABASE` and `` USE `…` `` lines in the dump are skipped so every statement lands in the chosen target. |
| SQL Server | Full database backups (`.bak`) on the server | `RESTORE HEADERONLY` lists the backup sets and `RESTORE FILELISTONLY` the data and log files; every file gets an editable `MOVE` path (a copy under a new name gets new file names). Target name, `WITH REPLACE`, and closing other connections (`SINGLE_USER WITH ROLLBACK IMMEDIATE`, reset to multi-user afterwards). Runs `WITH RECOVERY, STATS = 5`. |

Oracle Data Pump imports, SQLite and Snowflake restores are not offered. Read-only
data sources cannot restore. As with backups, passwords reach the native clients
through temporary credential files (`PGPASSFILE`, `--defaults-file`), never the
command line, and the utility path can be changed in **Options**.

Every client stops at the first error (`--exit-on-error`, `ON_ERROR_STOP`, the
`mysql` default) and reports the utility's diagnostics. Plain SQL scripts are not
transactional: statements that ran before an error or cancellation stay applied.
They run like any script given to `psql` or `mysql`, including client commands, so
restore only files you trust. A database created for a restore that then fails is
kept for inspection. SQL Server reads the backup path on the server, with its service
account. The SQL Server driver does not surface `STATS` messages, so progress comes
from the same counter in `sys.dm_exec_requests` (it needs `VIEW SERVER STATE`;
otherwise the dialog shows indeterminate progress). Cancel stops local utilities;
on SQL Server it kills the restoring session, which can leave the target in the
`RESTORING` state until it is restored again or dropped.

Restoring has command, statement and parsing tests plus an opt-in live test
(`cargo test --locked postgres_backup_restores_into_a_new_database -- --ignored`
with `DATACRAFT_PG_*` set and `pg_dump`, `pg_restore`, `psql` on `PATH`). It backs up
a PostgreSQL database with DataCraft, restores the custom archive and the gzip plain
dump into new databases, restores again into an existing one with and without clean,
restores a `pg_dump -Fd` directory archive with parallel jobs and cancels a running
restore. MySQL, MariaDB and SQL Server restores are covered by unit tests only.
[Screenshots](screenshots/restore/README.md).

References: [pg_dump](https://www.postgresql.org/docs/current/app-pgdump.html),
[pg_restore](https://www.postgresql.org/docs/current/app-pgrestore.html),
[mysql client](https://dev.mysql.com/doc/refman/8.4/en/mysql.html),
[SQL Server RESTORE](https://learn.microsoft.com/en-us/sql/t-sql/statements/restore-statements-transact-sql),
[mariadb-dump](https://mariadb.com/docs/server/clients-and-utilities/backup-restore-and-import-clients/mariadb-dump),
[SQLite online backup](https://www.sqlite.org/backup.html),
[SQL Server BACKUP](https://learn.microsoft.com/en-us/sql/t-sql/statements/backup-transact-sql),
[Oracle Data Pump](https://docs.oracle.com/en/database/oracle/oracle-database/21/sutil/oracle-data-pump-export-utility.html),
[Snowflake backups](https://docs.snowflake.com/en/user-guide/backups).
