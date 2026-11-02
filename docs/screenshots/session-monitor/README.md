# Session Monitor previews

These images render the actual Session Monitor window, its confirmation and the
connection context menu on synthetic test state with the Windows GPU renderer.
Rendering does not connect to a database or run any statement: the PostgreSQL
sessions, users, addresses and queries are illustrative. The monitor's queries,
cancel and terminate were checked separately against a live PostgreSQL 16 server
(see the ignored tests in `src/db/sessions.rs` and `src/worker.rs`).

| Preview | Theme |
| --- | --- |
| [Sessions, a blocking chain and the selected query](session-monitor-dark.png) | Dark |
| [Terminate Session confirmation](session-monitor-terminate-dark.png) | Dark |
| [Sessions, a blocking chain and the selected query](session-monitor-light.png) | Light |
| [Connection context menu](session-monitor-menu-light.png) | Light |

Regenerate on Windows with a working GPU:

```powershell
cargo test --locked app::session_monitor::tests::render_session_monitor -- --ignored --exact
```

Review the generated PNGs in `target/session-monitor-snapshots`, then copy the
selected images here without changing their contents.
