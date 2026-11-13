# Editor execution and tab switching

The statement at the caret is outlined. A selection outlines its execution range. Gutter Run buttons execute their statement through the existing read-only, pending-edit, transaction and dangerous-statement guards. Per-query durations remain next to unchanged SQL.

Open File Structure with Cmd/Ctrl+F12 (also Cmd+Option+7 on macOS or Alt+7 elsewhere). Filter and select statements, then execute selected statements or all statements.

Hold Ctrl and press Tab to preview recent tabs. Use Tab, Shift+Tab or arrows to choose; release Ctrl to switch. Esc cancels.

Editor analysis and outline geometry are cached by text revision, caret and layout.
