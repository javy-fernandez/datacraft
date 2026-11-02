# Dangerous statement guard and environment previews

These images render the actual DataCraft window on synthetic test state with the
Windows GPU renderer. They were generated from the implementation in this
branch. Rendering does not open desktop windows, connect to databases or execute
SQL: the prompts are opened by running the console on connections marked as
connected, and the guard stops before anything reaches a server. The data
sources, tables and statements are illustrative; these previews are not evidence
of a manual GUI test.

| Preview | Theme |
| --- | --- |
| [Production data source: badge on the toolbar, tab and explorer](production-console-dark.png) | Dark |
| [Production data source: badge on the toolbar, tab and explorer](production-console-light.png) | Light |
| [Statements that change Production data](production-confirm-dark.png) | Dark |
| [Statements that change Production data](production-confirm-light.png) | Light |
| [Dangerous statements on a Staging data source](dangerous-confirm-dark.png) | Dark |
| [Dangerous statements on a Staging data source](dangerous-confirm-light.png) | Light |
| [A dangerous statement on a Production data source](production-dangerous-confirm-dark.png) | Dark |
| [Saving grid changes to a Production data source](production-grid-save-dark.png) | Dark |
| [Environment, badge colour and the guard in the data source's Options tab](data-source-options-dark.png) | Dark |

Regenerate on Windows with a working GPU using the render fixture:

```powershell
cargo test --locked app::safety::tests::render_safety_screenshots -- --ignored --exact
```

Review the generated PNGs in `target/safety-snapshots`, then copy the selected
images here without changing their contents.
