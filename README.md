<p align="center">
  <img src="assets/icon.png" width="112" height="112" alt="DataCraft application icon">
</p>

<h1 align="center">DataCraft</h1>

<p align="center">
  <strong>A native workspace for your databases.</strong><br>
  Explore schemas, write queries and edit data in a focused desktop app built with Rust and egui.<br>
  macOS · Windows · Linux
</p>

<p align="center">
  <a href="LICENSE-MIT"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-22a8b8?style=flat-square"></a>
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-253b53?style=flat-square&logo=rust">
  <a href="https://github.com/javy-fernandez/datacraft/releases"><img alt="Download for macOS, Windows and Linux" src="https://img.shields.io/badge/download-macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-22a8b8?style=flat-square"></a>
</p>

<p align="center">
  <a href="#download">Download</a> ·
  <a href="#highlights">Highlights</a> ·
  <a href="#supported-databases">Databases</a> ·
  <a href="#build-from-source">Build</a> ·
  <a href="#documentation">Documentation</a> ·
  <a href="#contributing">Contribute</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: light)" srcset="docs/screenshots/readme/workspace-light.png">
    <img src="docs/screenshots/readme/workspace-dark.png" alt="The complete DataCraft window with its unified header, database explorer, orders grid and table inspector" width="100%">
  </picture>
  <br>
  <sub>The table workspace, with navigation, editing tools and object information in one view. Light and dark themes are included.</sub>
</p>

Screenshots show the current source build on Windows with synthetic data. Every capture includes the complete application window and its custom header. Published releases may not yet include all changes shown here.

## Highlights

| Explore | Query and edit | Manage |
| --- | --- | --- |
| Browse databases in a compact list or grid. Inspect tables, columns and records without losing your place. | Write SQL with completion, navigate cells with the keyboard and review pending changes before submitting them. | Organize objects with folders and colors, move data between databases and run engine-specific backups. |

DataCraft starts on an empty **Objects** workspace. Saved query and table tabs remain available, and selecting a database opens its objects. Layout choices and **Save on row change** are remembered between sessions.

## Browse with context

The Database Explorer loads databases and schemas as you expand them. Object folders start collapsed, so large connections stay manageable.

Use **List** for catalog statistics or **Grid** for compact names arranged from top to bottom. Select an object to inspect its columns, storage information and engine-provided timestamps. Unavailable metadata is shown explicitly; exact row counts are fetched on request.

<table>
  <tr>
    <td width="50%"><img src="docs/screenshots/readme/objects-list.png" alt="Full DataCraft window with the object list, catalog statistics and table inspector"></td>
    <td width="50%"><img src="docs/screenshots/readme/objects-grid.png" alt="Full DataCraft window with compact object columns, colored folders and the inspector"></td>
  </tr>
  <tr>
    <td align="center"><sub>List: compare catalog information.</sub></td>
    <td align="center"><sub>Grid: scan names and organize related objects.</sub></td>
  </tr>
</table>

- Select multiple objects with Shift, Ctrl/Command or a mouse rectangle. Drag them into local groups and assign colors to tables or folders.
- Use **Find in Schema…** to search accessible SQL tables across a selected database or schema.
- Close tabs individually, to either side, or in bulk from their context menu.

Groups and colors are local preferences; they do not change the database. See [object browsing](docs/object-browser.md) and [navigation, search and organization](docs/navigation-search.md).

## Write queries, keep the results close

SQL consoles provide syntax highlighting, completion, formatting, find and replace, query history, execution plans and cancellation. Run the current statement, a selection or the entire script. SQL Server scripts support <code>GO</code> batches.

Schedule SQL or full database backups at a fixed interval, including after closing the window, with the per-user [task scheduler](docs/sql-scheduler.md).

On PostgreSQL, MySQL and MariaDB, **Explain Plan** (⇧⌘E) shows the plan as a tree with each step's object, estimated rows and cost; **Explain Analyze** in the editor's menu also runs the statement and adds actual rows and time, asking first when the statement writes. The steps that take most of the cost or time are highlighted, and **Show as Text** gives the server's own output. Other engines show their text plans.

<p align="center">
  <img src="docs/screenshots/readme/sql-console.png" alt="Full DataCraft window with a highlighted SQL query and its result grid" width="100%">
  <br>
  <sub>A query and its results, with the active database visible in the workspace.</sub>
</p>

Redis/Valkey and MongoDB consoles use their own commands, including <code>HGETALL user:1</code> and <code>db.orders.find({total: {$gte: 100}})</code>. Their result grids are read-only.

The Kusto console takes KQL queries and control commands, such as <code>StormEvents | summarize count() by State</code> and <code>.show tables</code>; requests are separated by blank lines, and the result grids are read-only too.

## Edit data with clear feedback

Navigate with arrow keys and Tab; the grid scrolls to keep the active cell visible. Extend selections with Shift, select a column by its name, paste clipboard matrices or fill selected cells with one value.

Pending edits have a distinct highlight. Updates, inserted rows and deletions can be reviewed as generated SQL and submitted in one transaction. **Save on row change** is optional and disabled by default; for a bulk edit, it submits when you leave the edited row range.

<p align="center">
  <img src="docs/screenshots/readme/cell-editor.png" alt="Full DataCraft window with a multiline cell editor below the grid and the selected record in the right inspector" width="100%">
  <br>
  <sub>Edit longer values below the grid and inspect the selected record alongside it.</sub>
</p>

The cell editor supports multiline text and a UTF-8 hex view. The inspector switches between table information, the selected column and the current record. Read-only connections disable editing and DDL.

To read a whole value, Alt+Click (⌥-click on macOS) any cell, double-click a cell that can't be edited, or choose View Value from its context menu: a popup below the cell shows it, with JSON indented, and Esc or a click outside closes it.

[Table editing and shortcuts →](docs/table-editor.md)

## Build filters before applying them

Choose **AND/OR** on each condition row to mix operators, and use groups with visible parentheses to control precedence. Add comparison or text conditions and ordered sort columns. Switch to **Text** mode for custom <code>WHERE</code> and <code>ORDER BY</code> expressions. Editing the draft does not run a query; **Apply** reloads the data.

<p align="center">
  <img src="docs/screenshots/readme/filter-sort.png" alt="Full DataCraft window with the structured filter and sort builder above the table" width="100%">
  <br>
  <sub>Mixed AND/OR conditions and a sort rule, ready to apply.</sub>
</p>

Ctrl+F, or Command+F on macOS, searches the currently loaded cells. **Find in Schema…** searches the database instead. See [search behavior and scope](docs/navigation-search.md).

## Move, export and back up data

- **Import and export:** import CSV/TSV into tables; export results as CSV, TSV, JSON, SQL inserts or Markdown.
- **Copy tables:** transfer selected data between connections, databases or schemas, translate column types between engines and optionally include related rows through foreign keys.
- **Mask values:** apply consistent per-column masking during table copies or exports.
- **Manage objects:** create and modify tables and views, review generated DDL, and manage MySQL/MariaDB users or PostgreSQL roles and privileges.
- **Procedures and functions:** browse, create and edit them with a parameter table, the engine's options and a live DDL preview, then execute them with typed arguments and see output parameters. See [procedures and functions](docs/routines.md).
- **Back up databases:** open **Back up database…** from the Database Explorer, review engine-specific options and choose a destination. Jobs run in the background with cancellation and local history.
- **Run SQL scripts:** **Run SQL Script…** on a connection or database runs one or more `.sql` files without opening them. They are read in pieces, so a dump of several GB doesn't fill memory, and each engine's separators are respected (`GO` on SQL Server, `DELIMITER` on MySQL, `/` on Oracle). Progress and Cancel are shown; a failing statement asks Retry, Skip, Skip All or Stop, and the summary lists each failure with its file and line.

<p align="center">
  <img src="docs/screenshots/readme/database-backup.png" alt="Full DataCraft window with the MariaDB backup dialog and its destination, utility and compression settings" width="100%">
  <br>
  <sub>Backup settings in context. Available modes and destination rules depend on the engine.</sub>
</p>

PostgreSQL, MySQL and MariaDB backups require their native dump utilities. SQLite uses its online backup API. SQL Server, Oracle and Snowflake use server-side backup or export facilities with engine-specific privileges and destinations. See [backup modes, prerequisites and limitations](docs/database-backups.md).

## Supported databases

| Database | Connection and workspace |
| --- | --- |
| PostgreSQL | SQL console, data editor, schemas, views, roles and logical backups |
| MySQL / MariaDB | SQL console, data editor, views, users and logical backups |
| SQL Server | SQL console with <code>GO</code> batches, data editor, object management and native backups |
| Oracle | SQL console, data editor, object management and Data Pump exports; requires Oracle Instant Client |
| Snowflake | Native REST connection without Java; password, key-pair, OAuth or programmatic access token authentication |
| SQLite | Local database files, SQL console, data editor and online backups |
| ClickHouse | Native TCP protocol (port 9000, or 9440 with TLS checked against the system's certificates), SQL console and data editor for inserts only (updates and deletes are asynchronous mutations, run them from the console). Copies into ClickHouse run without a transaction |
| Redis / Valkey | Native commands, keys grouped by prefix, type/TTL information and read-only result grids |
| MongoDB | <code>mongosh</code>-style commands, collection browsing and read-only result grids |
| InfluxDB 2 | HTTP API (port 8086, HTTPS with "Use TLS") with an API token and an organization; Flux and InfluxQL consoles, buckets and measurements with their tags and fields, and read-only result grids filtered by a Flux predicate and a time range (the last hour by default). InfluxQL needs InfluxDB 2.4+ or a DBRP mapping |
| Kusto / Azure Data Explorer | REST API over HTTPS, signed in with the Azure CLI's account (`az login`), an Entra ID application (client ID and secret), or no authentication for the Kusto emulator; KQL console with control commands (requests separated by blank lines, as in Kusto Explorer), databases with their tables and materialized views, and read-only result grids filtered and sorted with KQL. Read-only connections are also enforced by the server |

### SSH tunnels

Every engine reached over TCP (all but SQLite, Snowflake and Kusto) can connect through a bastion host. Tick **Use SSH tunnel** in the data source's **SSH** tab and give the bastion's host, port and user, then sign in with a password, a private key (OpenSSH or PEM, with its passphrase) or the running SSH agent. The host and port in **General** stay the database's address as the bastion reaches it, often `localhost`.

- The first time a bastion is reached, Test Connection shows its key fingerprint and asks before trusting it. Keys already in `~/.ssh/known_hosts` are trusted, others go to DataCraft's own `ssh_known_hosts`, and a bastion whose key changed is refused, naming both fingerprints.
- The sessions of a data source share one SSH connection, with keepalives every 30 seconds. If it drops, the next query opens it again.
- With TLS, the certificate is still checked against the database's own host name for PostgreSQL, MySQL, MariaDB, SQL Server, ClickHouse and InfluxDB. MongoDB checks the certificate chain but not the name, which can't match the tunnel's local address. A replica set or `mongodb+srv` address can't be used through a tunnel: point it at one server.
- SSH passwords and passphrases are kept in the system keychain, like database passwords.

### SSL

PostgreSQL, MySQL and MariaDB data sources choose how the connection is encrypted in the **SSL** part of the **SSH/SSL** tab:

| Mode | Encrypted | Certificate |
| --- | --- | --- |
| Prefer (default) | When the server offers it | Not checked |
| Disable | Never | — |
| Require | Always | Not checked |
| Verify CA | Always | Signed by the CA file, or by a CA the system trusts |
| Verify full | Always | Signed as above and issued for the host |

- **Client certificates:** a client certificate and key (PEM; an encrypted PKCS#8 key takes its passphrase, kept in the keychain) sign users in when the server asks for one.
- **Renewed certificates:** the files are read on every connect, so a renewed certificate needs no edit.
- **Test Connection** names the TLS version and cipher. A refused certificate says which CA didn't sign it, or which host it wasn't issued for.
- **Through an SSH tunnel,** the certificate is still checked against the database's own host name.

### Bring your connections

- **Paste a URL:** the URL field also takes `postgresql://`, `mysql://`, `mariadb://`, `sqlserver://`, `mongodb://`, `mongodb+srv://`, `redis://`, `rediss://` and `clickhouse://` URLs, and a SQLite file's path. The engine, user, password and SSL settings are filled in; the password goes to the system keychain.
- **Copy as URL:** a connection's context menu copies its URL for other tools, without the password.
- **Import from DataGrip:** **+ → Import from DataGrip…** lists the data sources of DataGrip's projects (or any `dataSources.xml`) with their engine, folder and address. Engines DataCraft doesn't support are shown but can't be ticked, existing ones are left unticked and a name in use is imported as a copy. Passwords aren't imported: DataCraft asks for each one on the first connect. DataGrip's files are only read.

Support varies by engine, server version and account privileges. [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) must be installed separately for Oracle connections.

## Designed for the desktop

- **macOS, Windows and Linux:** native macOS menus and window controls, a unified application header on Windows, and a native Wayland/X11 window on Linux.
- **Light and dark themes:** adjustable interface zoom and editor font size.
- **Eight interface languages:** English, Spanish, French, Brazilian Portuguese, German, Polish, Simplified Chinese and Japanese. Follow the system language or choose one in Settings.
- **OS credential storage:** saved passwords use macOS Keychain, Windows Credential Manager or the Linux Secret Service (GNOME Keyring, KWallet), rather than the connections file.

Built with Rust, egui/wgpu and Tokio. Database work runs through background workers to keep the interface responsive.

## Download

Get packaged builds from the official [GitHub Releases](https://github.com/javy-fernandez/datacraft/releases).

| Platform | Package | Install |
| --- | --- | --- |
| macOS 11+ · Apple Silicon and Intel | <code>DataCraft-&lt;version&gt;-universal.dmg</code> | Open the disk image and move DataCraft to Applications. |
| Windows 10/11 · x64 | <code>DataCraft-&lt;version&gt;-windows-x64.zip</code> | Extract the archive and run <code>DataCraft.exe</code>. |
| Windows on ARM · ARM64 | <code>DataCraft-&lt;version&gt;-windows-arm64.zip</code> | Extract the archive and run <code>DataCraft.exe</code>. |
| Linux · x64 and ARM64 (glibc 2.35+) | <code>DataCraft-&lt;version&gt;-linux-x64.tar.gz</code> / <code>-linux-arm64.tar.gz</code> | Extract the archive and run <code>DataCraft/datacraft</code>. Needs a Vulkan driver, SQLite, D-Bus and a Secret Service provider (GNOME Keyring or KWallet). |

The macOS app is not notarized and Windows executables are unsigned, so the operating system may show a first-launch warning. Check the release notes for the build you download.

**Updates:** a few seconds after launch DataCraft checks the latest release on GitHub and, when a newer one is out, shows a notice with a link to its page (**Skip This Version** hides it until the next one). **Check for Updates…** checks right away, and the check can be turned off in Settings. Nothing is downloaded or installed automatically.

## Build from source

Use a Rust toolchain with support for the 2024 edition and the native build tools for your platform.

~~~sh
git clone https://github.com/javy-fernandez/datacraft.git
cd datacraft
cargo run --locked
~~~

For an optimized Windows executable:

~~~sh
cargo build --locked --release
~~~

The output is <code>target/release/datacraft.exe</code>. Native MSVC builds require the Visual Studio C++ build tools and Windows SDK, including the resource compiler used to embed the application icon.

On Linux, the same commands build a native Wayland/X11 app rendered with Vulkan. The build links the system SQLite and D-Bus libraries, so install their development packages first:

~~~sh
sudo dnf install gcc pkgconf-pkg-config sqlite-devel dbus-devel      # Fedora
sudo apt install build-essential pkg-config libsqlite3-dev libdbus-1-dev  # Debian / Ubuntu
~~~

At runtime it needs a Vulkan driver and a Secret Service provider, such as GNOME Keyring or KWallet, to store saved passwords.

On macOS:

~~~sh
scripts/bundle.sh --install
scripts/bundle.sh --universal --dmg
~~~

The universal build requires both Apple Silicon and Intel Rust targets. The bundle script uses <code>DATACRAFT_SIGN_IDENTITY</code>, or a certificate named <code>DataCraft</code>, when available; otherwise it uses ad hoc signing. A stable signing identity helps preserve Keychain access between builds.

<details>
<summary>Cross-compile Windows builds from macOS</summary>

For x64, install the GNU Windows target and MinGW-w64:

~~~sh
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
cargo build --locked --release --target x86_64-pc-windows-gnu
~~~

For ARM64, use cargo-xwin with LLVM and LLD:

~~~sh
rustup target add aarch64-pc-windows-msvc
cargo install cargo-xwin
brew install llvm lld
cargo xwin build --locked --release --target aarch64-pc-windows-msvc
~~~

cargo-xwin downloads the Windows SDK and requires acceptance of Microsoft's license.

</details>

## Documentation

| Guide | Contents |
| --- | --- |
| [Database workspace](docs/database-workspace.md) | Inspectors, cell editor, structured filters and pagination |
| [Object browser](docs/object-browser.md) | List/grid layouts, object metadata and row-count semantics |
| [Table editor](docs/table-editor.md) | Keyboard navigation, selections, bulk edits and optional automatic saving |
| [Navigation and search](docs/navigation-search.md) | Table search, schema search, local groups/colors and tab management |
| [Database backups](docs/database-backups.md) | Engine support, native utilities, destinations and cancellation |
| [Procedures and functions](docs/routines.md) | Browsing, the routine dialog, editing per engine and executing with parameters |
| [Application icon](docs/application-icon.md) | Shared artwork and native Windows/macOS icon assets |
| [Window header](docs/window-chrome.md) | Platform-specific window controls and validation notes |
| [Low-power rendering](docs/low-power-rendering.md) | Software adapters, Remote Desktop and frame statistics |
| [Screenshots](docs/screenshots/readme/README.md) | Reproduce the full-window documentation captures |

## Contributing

Bug reports, focused pull requests and translation improvements are welcome. Include your operating system, database engine/version and steps to reproduce the issue. Use synthetic data in screenshots and examples.

Run the relevant checks before submitting a change:

~~~sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
~~~

Live integration tests require separately configured database services. Run only the relevant ignored tests against a disposable environment; do not run the entire ignored suite against personal or production databases.

Translations live in <code>locales/&lt;code&gt;.json</code>, keyed by their English labels. The MySQL/MariaDB and PostgreSQL user editors were inspired by [DBCraft](https://github.com/DarkSoe/DBCraft) by DarkSoe.

## License

DataCraft is available under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Bundled fonts retain their own SIL Open Font License 1.1; see the [font notices](assets/fonts/README.md).
