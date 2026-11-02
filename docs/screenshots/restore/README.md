# Restore screenshots

These captures render the complete DataCraft window with the real `eframe::App::ui`
implementation and the restore dialog on synthetic state: a MariaDB, a PostgreSQL and
a SQL Server connection with in-memory catalogs, and backup previews filled in as
`pg_restore -l` and `RESTORE HEADERONLY` / `FILELISTONLY` would report them. No database
connection is opened and no restore runs. File names, sizes and progress are illustrative.

| Preview | Theme |
| --- | --- |
| [Database context menu with **Restore database…**](restore-menu.png) | Dark |
| [Inspected PostgreSQL custom archive, contents and options](restore-postgres.png) | Dark |
| [Confirmation before restoring into an existing database](restore-confirm.png) | Dark |
| [Restore into a new database in progress](restore-progress.png) | Light |
| [SQL Server backup set, file moves and T-SQL preview](restore-sqlserver.png) | Light |

Regenerate on Windows with a working GPU:

```sh
cargo test --locked render_restore_screenshots -- --ignored
```

Review the PNG files in `target/restore-snapshots/`, then copy them here unchanged.
Keep the full 1600 × 1000 frame. These are Windows UI captures; the restore itself was
tested separately against PostgreSQL (see [database backups](../../database-backups.md)).
