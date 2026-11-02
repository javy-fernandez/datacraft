# Compare schemas

**Compare Schemas…** shows what differs between two schemas and writes the script that
makes the second one (the **target**) match the first one (the **source**). Both can be on
the same connection or on different ones, for example production and staging.

It is available on PostgreSQL, MySQL, MariaDB, SQL Server, Oracle and SQLite, from the
Database Explorer menu of a connection, a database or a schema. The place it is opened
from becomes the source; the target starts on another connection of the same engine when
there is one.

![Comparing shop with shop_staging: the changed orders table side by side and the migration script](screenshots/schema-compare/compare-result-dark.png)

## Comparing

1. Pick the source and the target: connection, database and schema (MySQL, MariaDB and
   SQLite have one schema per database, chosen with it).
2. Click **Compare**. Both schemas are read in the background on sessions of their own;
   the dialog shows how many objects have been read, and **Cancel** stops the catalog
   query under way.
3. The objects are grouped by status:

| Status | Meaning | In the script |
| --- | --- | --- |
| Only in source | The target lacks it | Created (checked) |
| Only in target | The source lacks it | Dropped, only if you check it (it deletes its data) |
| Different | Both have it, with changes | Altered (checked) |
| Identical | Nothing to do | Hidden unless **Show identical** is on |

Select an object to see what changed and both definitions side by side; the changed lines
are tinted (green on the source, red on the target). Uncheck an object to leave it out of
the script; the script is regenerated at once.

What is compared:

- **Tables**: columns (added, dropped, type, nullability, default, identity /
  auto-increment), primary key, indexes and foreign keys (with their delete and update
  rules).
- **Views**: the definition text.

Trivial differences are ignored so that equal schemas show **No differences**: each side's
own schema name in qualifiers, sequence names and view text, letter case and blanks
outside quoted text, names where the engine ignores case (SQLite, MySQL, MariaDB, SQL
Server), and equivalent type spellings (`int4` and `integer`, MySQL `int(11)` and `int`).
Column order is not compared.

## The script

The script is never run by the dialog. **Copy** puts it on the clipboard, **Save .sql…**
writes it to a file and **Open in Console** opens it in a new console on the target
database, where you review it and run it yourself. On a read-only target the console still
opens, with a warning that it can't be run there.

Statements are ordered so that they can run top to bottom:

1. Foreign keys that are in the way: those on tables or columns that are dropped, on
   columns whose type changes, or on a primary key that changes. Those that should stay
   are added back at step 6.
2. Views to drop: dropped views, changed views on PostgreSQL and SQLite, and on
   PostgreSQL the views on dropped or retyped columns (and the views on those), which
   are created again at step 7.
3. Dropped tables.
4. New tables, with their primary key and indexes (PostgreSQL `serial` columns stay
   `serial`; other sequences are created first).
5. Changed tables, with the same statements as **Modify Table…**. SQLite can't alter most
   of a table in place: new nullable columns and index changes are made with `ALTER
   TABLE`, anything else rebuilds the table (a new table with the source definition, the
   rows copied, the old one dropped and the new one renamed). The script then starts with
   `pragma foreign_keys = off`.
6. Foreign keys.
7. New and changed views, each after the views it uses.

Changes that can lose data (dropped columns, new types) are listed as comments at the top,
as are the things the script can't do.

Comparing two different engines shows the differences but writes no script: types, keys
and SQL differ too much for one script. MySQL and MariaDB count as the same engine.

## Limitations

- Not compared: CHECK constraints, triggers, sequences on their own, user types (enums,
  domains), comments, collations, partitions, grants, materialized views, procedures and
  functions.
- Expression and partial indexes are compared by name only and are not created by the
  script; generated (computed) columns are compared by type, and created as ordinary
  columns, with a warning.
- A SQLite rebuild keeps the columns, keys, foreign keys, UNIQUE constraints and indexes of
  the source table, but not its CHECK constraints, collations or triggers.
- SQL Server can't add or remove a column's identity in place: the script says so.
- Adding a `NOT NULL` column without a default fails on a table that has rows: the script
  warns about it.
- Snowflake and ClickHouse schemas can't be compared yet; NoSQL connections don't offer it.

The previews in [screenshots/schema-compare](screenshots/schema-compare/README.md) show the
dialog before comparing, and the result in the light theme.
