# Browse database objects

Click a database, schema, **tables** folder or **views** folder in Database
Explorer to open the **Objects** tab. Existing SQL consoles and table editors
stay open in their tabs. Empty table/view categories remain available.

- **List** uses compact rows with Name, Rows, Data length, Engine, Created date,
  Modified date, Collation and Comment columns. Scroll horizontally if needed.
- **Grid** uses compact icon/name rows arranged in several columns, without
  metadata or large tiles. Names fill the available height from top to bottom,
  then continue in the next column. Extra columns scroll horizontally; resizing
  the window adjusts the number of rows per column.
- Search filters the current scope by object or schema name. Ctrl+F focuses it
  (Command+F on macOS).
- Click an object to show its information on the right. Double-click it, press
  Enter, or choose **Open data** to open its data editor.
- Up/Down and Home/End move through the object list; selection scrolls into view.
- Resize the information pane by dragging its left border. Use its DDL button
  to open the existing generated definition in a console.

The information pane contains the database/schema, type, columns and primary
keys, and engine-provided row statistics, sizes, creation/modification timestamps,
storage engine and comment when available. MySQL/MariaDB also expose collation,
row format, average/max row storage, check time, auto increment, free space and
creation options. Refresh information to fetch it again.
List statistics are fetched in one catalog query for the current scope, rather
than one query per object. Catalog queries run on the background worker. Switching selection invalidates
older requests so a late response cannot replace the current object's details.

## Table actions

Right-click a table in the explorer tree or the Objects list/grid to use the same
table actions: open data, modify/create tables and views, import, copy to another
connection, rename, empty/truncate, drop, refresh and object information. Opening
data and copying names support multiple selections; single-object dialogs require
exactly one supported object. Connection state and read-only mode gate writes.

**Show DDL** reads catalog definitions for supported live SQL tables. Cached column
metadata remains available for unsupported/offline readers and catalog failures;
fallback scripts and the status message identify that cached columns were used.
**Empty Table** uses DELETE; **Truncate** uses the engine's operation. SQLite uses
DELETE and preserves its sequence. Destructive actions preview their SQL. Save or
discard pending edits in the affected table's data editor before changing it.

**Duplicate Table** offers structure-only and structure-and-data copies into the
same schema on SQLite, PostgreSQL, MySQL/MariaDB, SQL Server and Snowflake. Choose a
new name and preview the SQL before applying it. Existing destination objects are
never overwritten. The background operation can be cancelled; transactional DDL
rolls back, while nontransactional engines remove only a successfully created
incomplete destination. Data copies retain complete values, including binary data,
and apply foreign keys after copying where the dialect permits it.
If CREATE has an unconfirmed outcome, the target may exist: the error names it for
manual inspection, and it is never removed without confirmed ownership.

Definitions that cannot be copied faithfully, such as tables with triggers or
specialized storage features, return an error. MySQL/MariaDB require direct trigger
catalog access. PostgreSQL catalog scripts preserve supported core schema and
values but omit table/column comments, custom column storage/compression settings,
and unlogged persistence for owned serial sequences.

**Export Table** writes CSV, TSV or JSON from the complete table on a separate
background connection, independently of the data editor's page and filters. It
supports read-only sources, row progress and cancellation. **Replace file?** is
explicit: an existing destination is replaced only after the export succeeds.
Errors and cancellation preserve the previous file.

**Dump SQL File** offers structure and data, structure only, or data only. Data
exports support SQLite, PostgreSQL, SQL Server, MySQL and MariaDB; structure-only
dumps also support Snowflake. Dumps retain full binary/long values and omit
generated columns from INSERTs. Data-only dumps require an existing target schema
and any external parent tables. Non-SQLite self-referencing foreign keys require
Structure and Data so constraints can be added after the rows. Unsupported
definition features return an error instead of producing incomplete DDL.
SQL Server uses snapshot isolation when enabled, otherwise READ COMMITTED, where
concurrent changes can be visible during the export.

**Print Table** creates escaped HTML with schema details. Including all table data
is an explicit choice and follows the data-export engine support above. Submit
**Create report** to write the file and open it in the system browser; use the
browser's Print command to select a printer or PDF. Schema-only reports are
available for every SQL engine.

### Explicit table privileges

**Set Privileges…** opens a user/role selector for one connected, writable server
table. It displays explicit grants on that table and previews the exact GRANT and
REVOKE statements before applying them. These grants are not the user's full
effective permissions: ownership, role membership, PUBLIC and broader privileges
can independently provide access. Save or discard pending table edits first.

PostgreSQL, MySQL/MariaDB, SQL Server, Oracle, Snowflake and ClickHouse use their
own live catalogs. SQLite and NoSQL engines do not offer table grants. Catalog
visibility is required; inaccessible or truncated metadata is an error. MariaDB
offers user accounts, while Snowflake offers account roles and requires the table
owner role outside managed access schemas.

The editor refuses grants with options, foreign grantors, column permissions,
DENY/partial-revoke states or other scopes it cannot safely preserve. Use the SQL
editor for those cases. Before applying, it reloads grants and provenance and
rejects a changed snapshot or preview. Work uses a cancellable private session.
Some engines commit each statement independently; after cancellation or an error,
use **Reload privileges** to inspect the actual state before making more changes.

ClickHouse ignores wildcard grants outside the selected database/table scope and
refuses matching prefix grants. Exact grants work on pre-24.10 catalogs without
`is_wildcard`; prefix-capable versions that omit this metadata require the SQL
editor because exact and prefix provenance cannot be distinguished safely.

## Row counts and timestamps

Browsing reads catalog statistics and **never automatically scans a table**.
Catalog row counts are conservatively shown with **≈**. **Count rows exactly**
explicitly runs COUNT(*) against the selected object, including views; the worker
limits the metadata/count query to 15 seconds after acquiring its session.

| Engine | Catalog information used |
| --- | --- |
| PostgreSQL | pg_class estimates, table/index sizes, object comments |
| MySQL / MariaDB | information_schema.TABLES rows, sizes, engine, dates, comments |
| SQL Server | sys.partitions estimates, sys.objects dates, MS_Description |
| Oracle | ALL_TABLES statistics, ALL_OBJECTS dates, ALL_TAB_COMMENTS |
| Snowflake | INFORMATION_SCHEMA.TABLES rows, bytes, dates, comments |
| SQLite | sqlite_schema existence; exact count is available on demand |

Unavailable values appear as **—** (Not available), including creation/modification
timestamps that PostgreSQL and SQLite do not record in these catalogs. Modified
timestamps can describe definition changes, statistics or engine maintenance;
they do not universally indicate the latest row edit. Permissions and statistics
freshness also affect what the server returns. The panel displays errors without
replacing missing values with zero or guessed dates.

Catalog references: [PostgreSQL pg_class](https://www.postgresql.org/docs/current/catalog-pg-class.html),
[MySQL TABLES](https://dev.mysql.com/doc/refman/8.4/en/information-schema-tables-table.html),
[SQL Server sys.partitions](https://learn.microsoft.com/en-us/sql/relational-databases/system-catalog-views/sys-partitions-transact-sql),
[Oracle ALL_OBJECTS](https://docs.oracle.com/en/database/oracle/oracle-database/21/refrn/ALL_OBJECTS.html),
[Snowflake TABLES](https://docs.snowflake.com/en/sql-reference/info-schema/tables),
[SQLite schema](https://www.sqlite.org/schematab.html).
