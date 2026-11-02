# Manual transaction previews

These images render the actual SQL console on synthetic test state with the
Windows GPU renderer. They were generated from the manual transaction
implementation in this branch. Rendering does not open desktop windows, connect
to databases or execute SQL. The open transaction, its statement count and the
results are illustrative; these previews are not evidence of a manual GUI test.
The transaction behavior itself is covered by the worker tests (SQLite) and by
the ignored live PostgreSQL test described in the pull request.

| Preview | Theme |
| --- | --- |
| [Console in Manual mode with an open transaction](manual-console-dark.png) | Dark |
| [Console in Manual mode with an open transaction](manual-console-light.png) | Light |
| [Transaction mode menu](tx-mode-menu-dark.png) | Dark |
| [Failed PostgreSQL transaction](failed-transaction-dark.png) | Dark |
| [Closing a tab with an open transaction](close-tab-confirm-dark.png) | Dark |
| [Closing a tab while a statement still runs](close-tab-running-dark.png) | Dark |

Regenerate on Windows with a working GPU using the render fixture:

```powershell
cargo test --locked app::transactions::tests::render_manual_transactions -- --ignored --exact
```

Review the generated PNGs in `target/transaction-snapshots`, then copy the
selected images here without changing their contents. The elapsed time in the
transaction indicator depends on how long the render takes, so it can differ by
a second or two between runs.
