# Database workspace

Database Explorer starts with databases, schemas and object folders collapsed.
Expanding a database shows its tables, views and backup history; filtering still
reveals matching objects. A collapsed database does not display child folders.

Table editors have **Cell Editor**, **Filter & Sort** and **Information** toggles.
Their visibility is restored with each saved table tab. The information pane shares
its layout with Objects, with a large colored object icon and small icon tabs.
Objects separates metadata from its columns; table editors offer table information,
selected-column details and a record inspector that follows the selected row and
shows pending values. Metadata actions live in the inspector's menu. Missing
metadata is shown as unavailable. Exact row counts are explicit actions; opening
a table does not automatically scan it. Objects' search field, refresh and list/grid
icons sit in the bottom status bar beside the result statistics; their layout
preference is still saved. Ctrl+F focuses the search field. The object inspector
starts alongside the editor tabs, above the object-list heading.

The cell editor shows multiline text and UTF-8 hex bytes. Apply or moving to another
cell stages the value through the same pending-edit system as the grid, including
bulk selections, undo, SQL preview, read-only protection and Save on row change.
Reset discards only the current panel draft. Hex is a viewer, not a binary editor.

Colored add buttons distinguish adding conditions, groups and sort columns.
The filter builder supports enabled conditions, nested AND/OR groups, ranges,
newline-separated value lists, NULL/empty values and literal text matching.
Each row's AND/OR button, immediately before its remove button, joins it to the
next enabled condition at the same level. Click it to switch operators; the last
row has no trailing connector. Disabled conditions and empty groups are skipped.
AND binds more tightly than OR: `X AND Y AND Z OR P AND Q` means
`(X AND Y AND Z) OR (P AND Q)`. Use the visible group parentheses for a different
order, such as `(X OR Y) AND Z`. Removing a row also removes its connector.
[Grouped filter example](screenshots/readme/filter-groups.png).
Pattern characters are escaped. Sort rules can be reordered independently. Text
mode accepts custom WHERE and ORDER BY expressions. Apply reloads the first page;
changing a draft alone does not query the database. Pending edits require the
existing discard confirmation before applying filters or changing pages.

The bottom toolbar groups row operations, refresh/cancel, page navigation and
page size. The status line shows the executed query, selected record/page and
last execution time. Last page explicitly counts filtered rows on an isolated
session, without changing the table's unfiltered metadata count. Queries use
primary-key order when available and no explicit sort was supplied. Offset paging
without a unique order may repeat or skip rows, especially during concurrent
writes. Oracle page navigation beyond the first page requires Oracle 12c or later.

The structured builder and SQL paging apply to SQL engines. Redis and MongoDB keep
their existing engine-specific query fields. Backups are described in
[Database backups](database-backups.md).

Startup opens the empty Objects workspace instead of creating or selecting a SQL
console. Existing query and data tabs are restored and remain available; selecting
a database in the explorer opens its objects.
