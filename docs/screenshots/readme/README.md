# README screenshots

These captures render the complete DataCraft interface, including the application
header, activity bars, explorer, editor tabs and bottom status bar. They use the
real `eframe::App::ui` implementation with an in-memory, synthetic MariaDB catalog
and result sets. No database connection or backup job is started.

Regenerate on Windows with a working GPU:

```sh
cargo test --locked render_readme_screenshots -- --ignored
```

Review the PNG files in `target/readme-snapshots/`, then copy the selected files to
this directory. Keep the full 1600 × 1000 frame; do not crop away the custom header.
The light and dark workspace images are selected by the README's `<picture>`.
`filter-sort.png` shows mixed row connectors; `filter-groups.png` shows explicit
parentheses changing their precedence.
These are Windows UI captures, not evidence of native macOS runtime testing.
