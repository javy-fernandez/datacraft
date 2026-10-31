# Scheduler previews

These images render the actual scheduler dialogs on synthetic test state with the
Windows GPU renderer. They were generated from the scheduler implementation in this branch.
Rendering does not open desktop windows, connect to databases, register a service
or execute SQL or backups. The task statuses, history and data are illustrative;
these previews are not evidence of a completed manual GUI or background-service
test. The independent runtime checks are described in the pull request.

| Preview | Theme |
| --- | --- |
| [SQL task configuration](sql-task-dark.png) | Dark |
| [Execution history](history-light.png) | Light |
| [Backup task configuration](backup-task-dark.png) | Dark |
| [Backup task configuration](backup-task-light.png) | Light |

Regenerate on Windows with a working GPU using the existing render fixtures:

```powershell
# Use a neutral, writable temporary path in the backup settings preview.
New-Item -ItemType Directory -Path C:\Temp\DataCraftScreenshots -Force | Out-Null
$env:TEMP = 'C:\Temp\DataCraftScreenshots'
$env:TMP = $env:TEMP

cargo test --locked scheduler::dialog::tests::render_scheduler_manager -- --ignored --exact
cargo test --locked scheduler::dialog::tests::render_backup_tasks -- --ignored --exact
```

Review the generated PNGs in `target/scheduler-ui-snapshots`,
then copy the selected images here without changing
their contents. Keep the complete rendered viewport, including the fixed footer.
