# SQL execution and editor navigation

An unfilled outline previews the current statement or text selection, preserving the editor background and syntax colors. Its colors match DataGrip's Islands editor scheme: `#3D7A49` green in the dark theme and `#BAA8FF` lavender in the light theme. Click a statement's gutter triangle to execute that statement directly.

**Run statement** and Command/Ctrl+Enter immediately execute the outlined scope: the statement at the caret or the selected text. **Run all statements** executes the whole script. The execution scope and Run use the same ranges, with no additional statement-selection dialog.

After a query finishes, its duration (for example, `12 ms` or `1.25 s`) appears beside its first line. This is elapsed execution and data-retrieval time, excluding connection setup. SQL Server batches show one duration for the batch; SQLite scripts show the individual statement durations returned by the driver. Labels follow unchanged SQL when lines move, disappear when edits change its SQL or statement boundaries or when its connection/database changes, and refresh on rerun. They are session-only annotations and are never inserted into SQL.

**File Structure** supports filtering, navigation, and explicitly executing several statements. Ctrl/Command-click selects several; Shift-click selects a range. Separate outlines mark separate choices, and selected statements run in source order. The structure popup closes if its SQL or connection changes. All execution goes through the existing pending-edit, read-only, transaction, and dangerous-statement checks. Statement boundaries use the connection's dialect, including PostgreSQL dollar quotes, SQL Server batches, MySQL delimiters, Oracle blocks, and SQLite triggers.

Open **Navigate** in the application menu, SQL toolbar, or editor context menu for the navigation actions. **Help → Keyboard shortcuts** also lists them. These are the defaults; Command becomes Ctrl on Windows and Linux.

| Action | Shortcut or control |
| --- | --- |
| Recent tabs and tool windows | Hold physical Ctrl and press Tab; Shift reverses direction. Arrows change the choice; releasing Ctrl activates it. Esc cancels. |
| Previous/next navigation jump, including other tabs | Command+Alt+Left / Right |
| Last edited location | Command+Shift+Backspace |
| Beginning/end of editor | Command+Page Up / Page Down |
| Next/previous word; extend selection | Alt+Right / Left on macOS; Ctrl+Right / Left elsewhere; add Shift to select with the same endpoints |
| Paragraph movement, with or without selection | Navigate menu or Find navigation action |
| Find navigation action | Command+Shift+A |
| Next occurrence of the word at the caret | Command+Alt+G on macOS; Ctrl+F3 elsewhere; whole-word search avoids matching parts of identifiers |
| Current statement context | Command+Alt+I on macOS; Alt+Q elsewhere |
| Matching bracket | Command+Shift+M |
| Go to line or line:column | Command+L; Ctrl+G also works on Windows/Linux when Find is closed |
| Search statement structure | Command+F12; Command+Alt+7 on macOS or Alt+7 elsewhere; Enter navigates, and the context menu can execute the selected statements |
| Toggle bookmark | Command+Alt+Shift+B on macOS; F11 elsewhere |
| Assign a letter or digit to a bookmark | Command+Alt+Shift+M on macOS; Ctrl+F11 elsewhere |
| Manage bookmarks across open editors | Command+Alt+Shift+K on macOS; Shift+F11 elsewhere; edit descriptions, sort, delete, or type a mnemonic to jump |
| Jump to a numbered bookmark | Physical Ctrl+Shift+0…9 |
| Next/previous highlighted issue | F2 / Shift+F2 |
| Select a referenced object in Database Explorer | Command+Alt+B on macOS; Alt+Shift+B elsewhere |
| Open a reference's DDL or local declaration | Command+B or Command-click |

The switcher scrolls only when the selected row changes, so an idle switcher stops repainting. Esc or its close button cancels and returns focus to the current editor. Losing application focus also cancels, including when the operating system misses Control release. A quick Ctrl+Tab press and release switches immediately even when both arrive in one frame.

Bookmarks are saved with each console. Editor navigation settings and recorded shortcuts apply globally to existing and new tabs. Older per-console settings migrate from the previously active tab. Unknown saved commands or keys are skipped without losing tabs or other UI state. Bookmark anchors follow text inserted or deleted before them. Assigning a mnemonic replaces its previous assignment in another open console.

**Navigate → Editor navigation settings** controls line numbers, word movement endpoints, stopping at line breaks, and issue priority. It also records additional shortcuts for navigation actions, including paragraph selection. The standard shortcuts remain available; Reset removes an additional assignment. Recorded shortcuts require Ctrl or Command, and known system and editing shortcuts remain reserved. Character-producing Option and AltGr input is preserved.

Reference navigation uses the console's connection and database metadata. It handles tables, views, columns, schemas, routines, table aliases, and local CTE declarations. Quoted names retain their case. Ambiguous catalog matches present a choice. Explorer navigation expands the object's folders and scrolls to the selected node; Generated DDL reuses a matching active SQL console; otherwise it opens a new editable console. Connected engines use the existing server definition readers where supported; an offline table definition can be an approximation from cached columns. Local declarations can be navigated without a connection.

Issue navigation currently covers unmatched brackets, unfinished quoted text or block comments, and trailing whitespace. Right-click a gutter issue marker to choose all issues or the highest priority. Full SQL grammar and semantic validation continue to come from the database when a statement runs. Reference resolution is conservative and does not implement a complete SQL semantic model for nested or correlated queries.

To try execution scopes in a connected PostgreSQL or SQLite console, enter `SELECT 1;` and `SELECT 2;` on separate lines. Move the caret between them and check that the outline follows the current statement. Run with Command/Ctrl+Enter, or click either gutter triangle, and check that only that statement executes and receives a timing label. Open File Structure, select both statements, and execute the selection to run both in source order.

To try the switcher, open two SQL consoles with different text and caret positions. Hold physical Ctrl, press Tab, then release Ctrl to activate the previewed tab. Repeat with Esc to cancel and continue typing in the original editor. Navigate lists the other actions; try a bookmark and a Go to Line jump, then use Back to return to the previous jump.

The automated behavior checks run with `cargo test --locked navigation::tests` and `cargo test --locked ui::console::execution::tests`. They cover editor input, scopes and gutter execution, File Structure, tab switching, timing invalidation, shortcuts, navigation history, DDL reuse, and execution guards.
