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
metadata remains available for unsupported catalog readers and offline connections.
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

Definitions that cannot be copied faithfully, such as tables with triggers or
specialized storage features, return an error. MySQL/MariaDB require direct trigger
catalog access. PostgreSQL catalog scripts preserve supported core schema and
values but omit table/column comments, custom column storage/compression settings,
and unlogged persistence for owned serial sequences.

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
