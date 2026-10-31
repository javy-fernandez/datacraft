"""Read-only diagnostics for disposable native-service CI fixtures after a failure."""

import os
from pathlib import Path
import sqlite3
from contextlib import closing

print("Diagnostic SQLite version:", sqlite3.sqlite_version)
for name in ("datacraft-service", "datacraft-xdg"):
    profile = Path(os.environ["RUNNER_TEMP"]) / name
    print("Fixture:", profile)
    if not profile.exists():
        print("Not created")
        continue
    for path in sorted(profile.rglob("*")):
        if path.is_file():
            stat = path.stat()
            print("File:", path.relative_to(profile), "bytes:", stat.st_size, "mode:", oct(stat.st_mode & 0o777))
    store = profile / "scheduler.sqlite"
    if store.exists():
        with closing(sqlite3.connect(store.as_uri() + "?mode=ro", uri=True)) as db:
            print("Scheduler integrity:", db.execute("PRAGMA integrity_check").fetchall())
            print("Runs:", db.execute("SELECT jobs.id, jobs.name, jobs.backup_json IS NOT NULL AS is_backup, runs.status, runs.message FROM jobs LEFT JOIN runs ON runs.job_id=jobs.id ORDER BY runs.id").fetchall())
    sample = profile / "sample.sqlite"
    if sample.exists():
        with closing(sqlite3.connect(sample.as_uri() + "?mode=ro", uri=True)) as db:
            print("Source integrity:", db.execute("PRAGMA integrity_check").fetchall())
            print("Source journal:", db.execute("PRAGMA journal_mode").fetchall())
            print("Source values:", db.execute("SELECT value FROM probe").fetchall())
