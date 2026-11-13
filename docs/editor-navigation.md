# SQL execution timing

Run statement and Command/Ctrl+Enter execute the statement at the caret or the selected text. Run all statements executes the whole script. Execution continues through the pending-edit, read-only, transaction, and dangerous-statement guards.

After a query finishes, its duration (for example, `12 ms` or `1.25 s`) appears beside its first line. This includes execution and data retrieval, excluding connection setup. SQL Server batches show one duration for the batch; SQLite scripts show individual statement durations returned by the driver.

Labels follow unchanged SQL when lines move. They disappear when an edit changes the SQL or its statement boundaries, when its connection/database changes, or when the query is rerun. Timings last for the current session and are never inserted into the SQL.
