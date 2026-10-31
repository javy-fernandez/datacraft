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
| PostgreSQL | Full, structure only, data only; plain SQL or custom archive; optional compression | Local file; matching `pg_dump` installation. Restore custom archives with `pg_restore`. |
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

References: [pg_dump](https://www.postgresql.org/docs/current/app-pgdump.html),
[mariadb-dump](https://mariadb.com/docs/server/clients-and-utilities/backup-restore-and-import-clients/mariadb-dump),
[SQLite online backup](https://www.sqlite.org/backup.html),
[SQL Server BACKUP](https://learn.microsoft.com/en-us/sql/t-sql/statements/backup-transact-sql),
[Oracle Data Pump](https://docs.oracle.com/en/database/oracle/oracle-database/21/sutil/oracle-data-pump-export-utility.html),
[Snowflake backups](https://docs.snowflake.com/en/user-guide/backups).
