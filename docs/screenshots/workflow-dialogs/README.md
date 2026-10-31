# Workflow dialog previews

These are offscreen renders of the real egui forms on synthetic data. They do
not connect to databases or execute backups or routines. The examples show the
shared bounded frame, scrolling content and fixed footer actions.

- [Manual backup](manual-backup-dark.png)
- [Schema search](schema-search-light.png)
- [Routine definition](routine-definition-dark.png)
- [Routine execution](routine-execution-light.png)

Run the exact ignored render fixtures `ui::backup_dialog::tests::render_backup_settings`,
`ui::schema_search::tests::render_schema_search`, `ui::routine_dialog::tests::render_routine_dialogs`
and `ui::routine_execute::tests::render_execute_dialog` with
`cargo test --locked <name> -- --ignored --exact`, then inspect the generated PNGs
under the respective `target/*-snapshots` directories. No live manual GUI flow
was completed for these changes.
