# Dangerous statements and connection environments

Two safeguards keep a console from changing more than you meant to: the SQL
console asks before statements that act on whole tables, and data sources can
be marked as Development, Staging or Production, with Production asking before
anything run in a console or saved from a grid changes its data.

![Production data source with its badge on the toolbar, tab and explorer](screenshots/safety/production-console-dark.png)

## Dangerous statement guard

Before the console runs a statement, a selection or a script, and before
**Explain Analyze** (which runs the statement to measure it), it looks for:

| Statement | Why it asks |
| --- | --- |
| `UPDATE` without `WHERE` | changes every row of the table |
| `DELETE` without `WHERE` | deletes every row of the table |
| `TRUNCATE` | removes every row |
| `DROP` (tables, views, databases, schemas, functions…) | removes the object and its data |
| `ALTER TABLE … DROP [COLUMN]` | removes the column and its values |
| `ALTER TABLE … DROP PARTITION` | removes the partition and its rows |
| `MERGE … WHEN [NOT] MATCHED THEN DELETE` without `AND` | deletes every row the join reaches |
| `CREATE OR REPLACE TABLE / DATABASE / SCHEMA`, ClickHouse `REPLACE TABLE` | drops what is there |

A `WHERE` whose condition holds for every row (`WHERE 1`, `WHERE TRUE`,
`WHERE 1 = 1`, `WHERE 'a' = 'a'`) counts as none, which also covers ClickHouse
mutations such as `ALTER TABLE t DELETE WHERE 1`. `EXPLAIN ANALYZE …` and
MariaDB's `ANALYZE …` are checked like the statement they run; a plain
`EXPLAIN` runs nothing.

The prompt lists each of them with its target; **Run** sends exactly the
statements you ran, **Cancel** (or Esc) runs nothing.

![Dangerous statements on a Staging data source](screenshots/safety/dangerous-confirm-dark.png)

The check reads the SQL with the same lexer the console uses to split and
highlight statements, for the data source's engine:

- Strings, quoted identifiers and comments (`--`, `/* */`, MySQL `#`, Snowflake
  `//`, PostgreSQL dollar quotes, Oracle `q'[…]'`) are never mistaken for
  statements, and a `WHERE` inside them does not count.
- Any name can be a target, including tables named like keywords (`log`,
  `settings`, `errors`, `output`, `partition`…).
- Only a `WHERE` of the statement itself counts: one inside a subquery
  (`UPDATE t SET a = (SELECT … WHERE …)`) or a `USING (…)` list does not filter
  the rows that change.
- CTEs: `WITH … UPDATE` / `WITH … DELETE` are checked like the plain
  statements, and PostgreSQL's data-modifying CTEs
  (`WITH gone AS (DELETE FROM t RETURNING *) …`) too.
- PostgreSQL `UPDATE … FROM` and `DELETE … USING` without `WHERE` still change
  every row; `ONLY t` and `WHERE CURRENT OF` are understood.
- MySQL and MariaDB: like `--safe-updates`, a `LIMIT` bounds the statement, so
  `DELETE FROM logs ORDER BY id LIMIT 1000` runs without asking. Multi-table
  `DELETE a, b FROM …` and `UPDATE a JOIN b … SET` name every target.
- SQL Server: `DELETE FROM x FROM y JOIN …` targets `x`; `TOP (n)` bounds the
  statement (`TOP (n) PERCENT` does not). Statements written without `;` are
  told apart (`UPDATE … WHERE …` followed by `DELETE FROM b` on the next line,
  also after `CREATE TABLE`, `CREATE TYPE` or `ALTER TABLE`), as are `IF`,
  `BEGIN TRAN` / `TRY` blocks, `CASE` expressions, table hints and the
  `ON DELETE CASCADE` / `ON UPDATE NO ACTION` of foreign keys in generated
  scripts.
- Routine bodies only run when called, so they are not looked into:
  `CREATE FUNCTION` / `PROCEDURE` / `TRIGGER` in every engine, packages and
  types on Oracle, events on MySQL, rules on PostgreSQL, PostgreSQL
  `BEGIN ATOMIC` bodies, SQLite triggers and SQL Server batches that create or
  alter a routine or a view. Anonymous blocks run now, so their statements are
  checked (`BEGIN … END` in PL/SQL, MariaDB `BEGIN NOT ATOMIC`, T-SQL blocks).
- Other engines, in their own language: Redis `FLUSHALL` / `FLUSHDB`; MongoDB
  `deleteMany({})` and `updateMany({}, …)` with an empty filter, `drop()` and
  `runCommand({drop: …})` / `({dropDatabase: 1})`; Kusto `.drop …`,
  `.clear table … data` and `.purge table …`; InfluxQL `DELETE FROM m` and
  `DROP SERIES FROM m` without `WHERE`, `DROP MEASUREMENT` and other `DROP`s.

Turn it off per data source with **Confirm dangerous statements** in the
**Options** tab of the data source dialog; it is on for new data sources and for
those saved by earlier versions.

## Connection environment

The **Options** tab also sets the data source's **Environment**: None,
Development, Staging or Production. Each has a colour (green, amber and red),
which the colour button next to it can change; **Reset** goes back to the
environment's own.

![Environment, badge colour and the guard in the Options tab](screenshots/safety/data-source-options-dark.png)

A data source with an environment shows it:

- as a badge next to the console's data source selector, with a line in its
  colour under the console and data editor toolbars,
- as a line under every editor tab that uses it,
- as a coloured edge and label on its row in the Database Explorer.

## Production

On a Production data source, every console run that changes data or structure
asks first, once per run, whatever the statements are, in Auto and Manual
transaction mode alike. Reads (`SELECT`, `WITH`, `SHOW`, `EXPLAIN`,
`EXPLAIN ANALYZE SELECT`, Oracle's `EXPLAIN PLAN FOR SELECT`…) run at once: the same rule decides what a read-only
data source may run, reading the statement with the engine's lexer, so words in
strings, quoted names and comments (`WHERE action = 'DELETE'`, a data editor
filter naming `update`) don't count. Redis, MongoDB, InfluxDB and Kusto use
their own read command lists.

![Statements that change Production data](screenshots/safety/production-confirm-dark.png)

When a Production run also has dangerous statements, a single prompt names
both, and Explain Analyze of a write asks with that prompt instead of its own.
In a Manual transaction console the prompt comes before the statements join the
transaction; **Commit** and **Roll Back** don't ask again. Saving changes made
in a result grid or the data editor (Submit, ⇧ / ⌘S or Save on row change)
asks too, before anything is sent; Cancel keeps the pending changes in the grid.

![Saving grid changes to a Production data source](screenshots/safety/production-grid-save-dark.png)

### Not covered on Production

These actions write through their own dialogs, which already show what they
will do and wait for their own OK or Apply, and they don't ask again on a
Production data source: Run SQL Script…, scheduled tasks, Import, Copy Table
into a Production target, Modify Table (it can drop columns), Create table and
Create database, New / Edit View, saving routines in a routine tab, users and
roles, table grants, and the explorer's Drop / Truncate / Rename confirmation.

## Limitations

- The check reads SQL text. Statements built at run time (`EXECUTE IMMEDIATE`,
  `EXEC('…')`, PostgreSQL `DO $$ … $$` blocks) and procedures you call are not
  looked into; on Production they still ask, because they are not reads.
- Only constant conditions count as matching every row; `WHERE id = id` or
  `WHERE x IS NOT NULL OR x IS NULL` count as filters.
- In SQL Server batches without `;`, a statement that follows a `MERGE` or a
  `GRANT` / `REVOKE` / `DENY` is checked as part of it.
- On Production, session commands such as `SET`, `USE` or `BEGIN`, and
  `ANALYZE table` (which writes statistics), count as changes and ask too.
