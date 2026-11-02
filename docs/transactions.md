# Manual transactions in the SQL console

By default a console runs in **Auto** mode: each statement is committed as soon
as it runs. Switch the console toolbar's **Tx: Auto** menu to **Manual** to run
several statements, check their effect and then decide whether to keep them.

![SQL console in Manual mode with an open transaction](screenshots/transactions/manual-console-dark.png)

## How it works

- The first statement you run in Manual mode opens a transaction (`BEGIN`,
  `BEGIN TRANSACTION` or `START TRANSACTION`; Oracle simply stops committing each
  statement). Every later run joins the same transaction until you click
  **Commit** or **Roll Back** in the toolbar.
- The orange indicator shows that a transaction is open, how many statements ran
  in it and for how long. Commit and Roll Back report their result in **Output**
  like any other statement.
- A Manual console works on a database session of its own. Other consoles, the
  explorer and the data editor keep using the connection's shared session, so
  they don't see your uncommitted changes and their own statements are not run
  inside your transaction. They can still wait for rows or tables your
  transaction has locked, as with any other client.
- `COMMIT`, `ROLLBACK`, `BEGIN` or `START TRANSACTION` typed in the console are
  followed too. On MySQL, MariaDB, Oracle and Snowflake, DDL such as
  `CREATE TABLE` commits the open transaction by itself; the console says so.
- Result grids of a Manual console are read-only: change rows with SQL
  statements, so the changes stay in the transaction.
- Read-only data sources can use Manual mode to read in one transaction. Writes
  are still refused, by the app and, where the engine supports it, by the
  server.

| Mode menu | Failed transaction (PostgreSQL) |
| --- | --- |
| ![The Tx menu with Auto and Manual](screenshots/transactions/tx-mode-menu-dark.png) | ![A failed transaction that must be rolled back](screenshots/transactions/failed-transaction-dark.png) |

## Errors inside a transaction

After an error, PostgreSQL aborts the whole transaction and refuses further
statements until it is rolled back. The indicator turns red and only **Roll
Back** is offered; `ROLLBACK TO SAVEPOINT` also recovers it. SQL Server reports
the same state for a doomed transaction. Other engines keep the transaction open
and only the failed statement has no effect; a MySQL or MariaDB deadlock rolls
back the whole transaction, which the console reports.

## Nothing is lost by accident

Closing the tab, switching the console's connection or database, switching back
to Auto, disconnecting the data source and quitting DataCraft ask first while a
transaction is open:

![Commit, Roll Back or Cancel before closing a tab](screenshots/transactions/close-tab-confirm-dark.png)

**Commit** commits and then continues; if the commit fails nothing is closed and
the error is shown. **Roll Back** rolls back and continues. **Cancel** keeps the
transaction open. If the connection is lost or closed by other means (for
example by editing the data source), the server rolls the transaction back and
the console says so.

## Engines

Manual mode is available for PostgreSQL, SQLite, SQL Server, MySQL, MariaDB,
Oracle and Snowflake. ClickHouse, Redis, MongoDB, InfluxDB and Kusto have no
console transactions, so their Manual item is disabled. On SQLite an open write
transaction locks the database file for other writers until it ends.

Cancelling a running statement works in Manual mode too. On engines without a
native cancel (SQL Server, for example) the statement is abandoned by closing
the console's session, which also rolls back its transaction.

Screenshot regeneration is described in the [transactions screenshot notes](screenshots/transactions/README.md).
