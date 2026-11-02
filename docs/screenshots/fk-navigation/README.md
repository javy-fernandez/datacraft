# Foreign-key navigation screenshots

These captures render the real `eframe::App::ui` implementation with an in-memory,
synthetic MariaDB catalog (`shop.orders.customer_id → shop.customers.id`, plus four
tables referencing `orders`). No database connection is opened and no query runs;
the counts in the dialog are illustrative state, not query results. The worker,
SQLite and PostgreSQL behaviour is covered by the tests listed in the pull request.

| File | Shows |
| --- | --- |
| [grid-links.png](grid-links.png) | The faint underline on foreign-key values and the follow button of the hovered one; `<null>` keys are not links |
| [context-menu.png](context-menu.png) | **Go to referenced row** (parent table on the right) and **Referenced by…** |
| [referenced-row.png](referenced-row.png) | The parent row opened through the Filter & Sort builder, with the Back button |
| [referenced-by.png](referenced-by.png) | Referencing tables with bounded counts (`1000+`), **Open rows** and the engine's scope note |

Regenerate on Windows with a working GPU:

```sh
cargo test --locked render_fk_navigation_screenshots -- --ignored
```

The images are written to `target/fk-navigation-snapshots/`; review them and copy
them to this directory reduced to a 128-colour palette (about 50 KB each), e.g.
with Pillow: `Image.open(src).convert("RGB").quantize(128, dither=Image.Dither.NONE).save(dst, optimize=True)`.
`DATACRAFT_FK_CELL=x,y` moves the hovered and right-clicked cell if the layout changes. These are Windows UI captures at 1440 × 820 in the
dark theme, not evidence of native macOS runtime testing.
