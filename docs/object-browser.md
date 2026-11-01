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

Right-click a table in the explorer tree or the Objects list/grid to open the same
table actions. Opening data and copying names support multiple selected objects;
dialogs that change one table require exactly one selection. Groups and colors
remain local preferences.

- **Modify Table**, **New Table**, **Import Data from File**, **Rename** and **Drop**
  use the existing dialogs. **Empty Table** deletes rows; **Truncate** uses the
  engine's truncate operation. SQLite uses DELETE and preserves its sequence.
  Destructive actions show their SQL before confirmation. Save or discard pending
  edits in the affected table's data editor before changing its structure or rows.
- **Duplicate Table** offers structure only or structure and data, asks for a new
  name, and previews the catalog-backed SQL. The destination must not exist.
  Defaults, generated/identity columns, supported keys and indexes are preserved.
  Definitions that cannot be reproduced faithfully are rejected with a reason;
  they are never silently reduced to a column-only copy.
- **Export Table** writes CSV, TSV or JSON from the complete table, independently
  of the editor's page, selection and filters. Binary values are exported in full
  hexadecimal form. **Dump SQL File** offers structure and data, structure only,
  and data only, using the source dialect. These operations run in the background
  and can be cancelled. An existing destination is replaced only after a successful
  export; failure or cancellation leaves it intact.
- **Set Privileges** loads a user or role's explicit table grants, shows a SQL
  preview and checks for changes again before applying. Explicit grants are not
  the account's full effective permissions. Grant options, ownership, column
  permissions and other complex grant states may require the SQL editor. SQLite
  has no server table grants.
- **Print Table** creates an HTML schema report and opens it in the system browser;
  choose Print there to select a printer or PDF. Including table data is an
  explicit option. Names and values are escaped in the report.
- **Reverse Tables to Model** reads selected tables and their declared foreign
  keys, including directly related catalog tables. Drag the tables to arrange the
  model and save JSON or export SVG. No relationships are inferred from names.
- **Create Open Table Shortcut** creates a Windows `.lnk`, macOS `.command` or
  Linux `.sh` launcher. It references an existing saved connection and qualified
  table, without copying credentials. Moving the executable or removing the saved
  connection can invalidate a shortcut.
- **Refresh** reloads the database catalog and selected object's metadata.
  **Object Information** focuses its inspector. **Show DDL** reads the supported
  engine's actual table definition when connected. Unsupported catalog readers
  and disconnected connections retain local DDL generation from cached columns.

Complete data exports support SQLite, PostgreSQL, SQL Server, MySQL and MariaDB.
Faithful duplication supports those engines and Snowflake. Schema-only printing
is available for all SQL engines; Snowflake also supports structure-only SQL dumps.
Unsupported operations are disabled; table-specific
features, server versions and catalog permissions can impose additional limits.
Read-only connections allow reading, modeling and exports, while write actions
remain disabled. Database-changing SQL actions are not offered for NoSQL objects;
cached DDL generation remains available.

SQL Server exports use SNAPSHOT isolation when the database has enabled it, and
READ COMMITTED otherwise. They do not request SERIALIZABLE range locks for the
duration of the dump. No database isolation options are changed by the export.

For self-referencing foreign keys, structure-and-data dumps defer constraint
validation until the rows are restored. SQLite also supports data-only dumps of
these tables. Other engines reject that data-only case with a suggestion to use
structure and data. References to other tables require those tables and their
referenced data to exist at the destination.

PostgreSQL catalog SQL dumps preserve supported core schema and values but
currently omit table/column comments, custom column storage/compression settings,
and unlogged persistence for owned serial sequences. Use a native backup when
those attributes must also be preserved. MySQL definition reads require a direct
TRIGGER or ALL grant so hidden trigger metadata cannot be mistaken for no triggers.

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
