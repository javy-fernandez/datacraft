# Compare Schemas previews

These images render the actual Compare Schemas dialog on synthetic test state with the
Windows GPU renderer. Rendering does not open desktop windows, connect to databases or
execute SQL. The `shop` (production) and `shop_staging` schemas, their differences and the
script are illustrative; these previews are not evidence of a completed manual GUI test.
The script itself is checked by the SQLite and PostgreSQL round-trip tests in
`src/db/schema_compare/live_tests.rs`.

| Preview | Theme |
| --- | --- |
| [Result with a changed table and the script](compare-result-dark.png) | Dark |
| [Source and target before comparing](compare-pickers-dark.png) | Dark |
| [Result with a changed table and the script](compare-result-light.png) | Light |

Regenerate on Windows with a working GPU using the render fixture:

```powershell
cargo test --locked ui::schema_compare::tests::render_schema_compare -- --ignored --exact
```

Review the generated PNGs in `target/schema-compare-snapshots`, then copy the selected
images here without changing their contents. Keep the complete rendered viewport,
including the fixed footer.
