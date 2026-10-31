"""Exercise SQL and recurring backups without a GUI, using a disposable profile."""

import json
import os
from contextlib import closing
from pathlib import Path
import sqlite3
import socket
import subprocess
import sys
import tempfile
import time
import uuid

executable = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="datacraft-scheduler-") as temp:
    root = Path(temp)
    profile = root / "DataCraft"
    profile.mkdir()
    sample = root / "sample.sqlite"
    backups = root / "backups"
    backups.mkdir()
    with closing(sqlite3.connect(sample)) as db, db:
        db.executescript("CREATE TABLE records(value INTEGER); INSERT INTO records VALUES (0);")
    identity = uuid.uuid4().int % 1000000000000000 + 1000000000000000
    (profile / "connections.json").write_text(
        json.dumps([{"id": identity, "name": "test", "driver": "Sqlite", "file_path": str(sample), "auth": "NoAuth"}]),
        encoding="utf-8",
    )
    env = os.environ.copy()
    env["DATACRAFT_TEST_DATA_DIR"] = str(profile)
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    # Initialize the schema without registering startup; an empty scheduler exits cleanly.
    subprocess.run([str(executable), "--scheduler"], env=env, creationflags=flags, timeout=20, check=True)
    schedule = profile / "scheduler.sqlite"
    with closing(sqlite3.connect(schedule)) as db, db:
        sql_id = db.execute(
            "INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run) VALUES (?, ?, ?, ?, ?, 1, ?)",
            ("test", identity, "main", "UPDATE records SET value=value+1", 3, int(time.time()) + 3600),
        ).lastrowid
        backup_id = db.execute(
            "INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run, backup_json) VALUES (?, ?, ?, '', ?, 1, ?, ?)",
            ("backup", identity, "main", 3, int(time.time()) + 3600,
             json.dumps({"folder": str(backups), "utility": "", "compress": False, "archive": False, "directory": ""})),
        ).lastrowid
    worker = subprocess.Popen([str(executable), "--scheduler"], env=env, creationflags=flags)
    def wake():
        endpoint = profile / "scheduler.wake"
        try:
            port = int(endpoint.read_text().strip())
        except FileNotFoundError:
            return
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            sender.sendto(b"1", ("127.0.0.1", port))
    try:
        deadline = time.monotonic() + 20
        while not (profile / "scheduler.wake").exists() or not (profile / "scheduler.wake").read_text().strip():
            assert worker.poll() is None, f"worker exited: {worker.returncode}"
            assert time.monotonic() < deadline, "scheduler did not publish its wakeup endpoint"
            time.sleep(0.1)
        with closing(sqlite3.connect(schedule)) as db, db:
            db.execute("UPDATE jobs SET next_run=?", (int(time.time()),))
        wake()  # The original deadline was one hour away; this must interrupt the wait.
        duplicate = subprocess.run([str(executable), "--scheduler"], env=env, creationflags=flags, timeout=10)
        assert duplicate.returncode == 0, "second scheduler did not exit cleanly"
        deadline = time.monotonic() + 20
        runs = []
        while time.monotonic() < deadline:
            assert worker.poll() is None, f"worker exited: {worker.returncode}"
            with closing(sqlite3.connect(schedule)) as db:
                runs = db.execute("SELECT job_id, status, message FROM runs ORDER BY id").fetchall()
            if all(sum(job == job_id and status == "succeeded" for job, status, _ in runs) >= 2 for job_id in (sql_id, backup_id)):
                break
            time.sleep(0.1)
        with closing(sqlite3.connect(schedule)) as db, db:
            db.execute("UPDATE jobs SET enabled=0")
        wake()
        deadline = time.monotonic() + 10
        while True:
            with closing(sqlite3.connect(schedule)) as db:
                runs = db.execute("SELECT job_id, status, message FROM runs ORDER BY id").fetchall()
            if all(status != "running" for _, status, _ in runs):
                break
            assert time.monotonic() < deadline, runs
            time.sleep(0.1)
        assert all(status == "succeeded" for _, status, _ in runs), runs
        sql_runs = [message for job, _, message in runs if job == sql_id]
        destinations = [Path(message) for job, _, message in runs if job == backup_id]
        assert len(sql_runs) >= 2 and all(message == "" for message in sql_runs), runs
        assert len(destinations) >= 2 and len(set(destinations)) == len(destinations), runs
        assert set(backups.iterdir()) == set(destinations), runs
        for destination in destinations:
            assert destination.parent == backups and destination.suffix == ".sqlite", destination
            with closing(sqlite3.connect(destination)) as db:
                value, = db.execute("SELECT value FROM records").fetchone()
                assert 0 <= value <= len(sql_runs), value
        with closing(sqlite3.connect(sample)) as db:
            assert db.execute("SELECT value FROM records").fetchone() == (len(sql_runs),)
        assert worker.wait(timeout=10) == 0, "idle scheduler did not exit cleanly"
    finally:
        if worker.poll() is None:
            worker.terminate()
            worker.wait(timeout=10)
