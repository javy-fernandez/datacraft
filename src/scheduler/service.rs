//! Per-user startup registration for the scheduler process.

use anyhow::{Context, bail};
use std::path::Path;
use std::process::Command;

fn command(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().with_context(|| format!("start {program}"))?;
    if !output.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

pub fn enable() -> anyhow::Result<()> {
    let executable = std::env::current_exe().context("find the DataCraft executable")?;
    install(&executable)?;
    wait_for_start()
}

fn isolated_profile_without_native_services() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("DATACRAFT_TEST_DATA_DIR").is_some()
        && !(std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") && std::env::var("DATACRAFT_TEST_NATIVE_SERVICE").as_deref() == Ok("1"))
}

fn install(executable: &Path) -> anyhow::Result<()> {
    if isolated_profile_without_native_services() {
        bail!("Native scheduler startup is disabled for isolated test profiles");
    }
    install_native(executable)
}

pub(super) fn unregister() -> anyhow::Result<()> {
    if isolated_profile_without_native_services() {
        return Ok(());
    }
    unregister_native()
}

fn wait_for_start() -> anyhow::Result<()> {
    for _ in 0..50 {
        if is_running()? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("The background scheduler did not start; check the system startup settings")
}

pub(super) fn is_running() -> anyhow::Result<bool> {
    let path = super::database_path()?.with_extension("lock");
    let lock = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("open scheduler lock"),
    };
    match lock.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error).context("check scheduler lock"),
    }
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove scheduler startup file {}", path.display())),
    }
}

#[cfg(windows)]
fn unregister_native() -> anyhow::Result<()> {
    unregister_windows(r"Software\Microsoft\Windows\CurrentVersion\Run")
}

#[cfg(windows)]
fn unregister_windows(subkey: &str) -> anyhow::Result<()> {
    let subkey = subkey.replace('\'', "''");
    // DeleteValue tolerates a missing value but can hide OS errors; verify the removal.
    let script = format!(
        "$ErrorActionPreference = 'Stop'; $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('{subkey}', $true); if ($null -ne $key) {{ try {{ $key.DeleteValue('DataCraftScheduler', $false); if ($key.GetValueNames() -contains 'DataCraftScheduler') {{ throw 'Could not remove scheduler startup registry value' }} }} finally {{ $key.Dispose() }} }}"
    );
    command("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &script])
}

#[cfg(target_os = "linux")]
fn unregister_native() -> anyhow::Result<()> {
    let dir = dirs::config_dir().context("find user configuration directory")?;
    remove_if_present(&dir.join("autostart/datacraft-scheduler.desktop"))?;
    let unit = dir.join("systemd/user/datacraft-scheduler.service");
    if unit.try_exists().context("check scheduler systemd unit")? {
        // Disabling startup must not stop a statement already sent to a database.
        command("systemctl", &["--user", "disable", "datacraft-scheduler.service"])?;
        remove_if_present(&unit)?;
        command("systemctl", &["--user", "daemon-reload"])?;
    }
    remove_if_present(&dir.join("systemd/user/default.target.wants/datacraft-scheduler.service"))
}

#[cfg(windows)]
fn install_native(executable: &Path) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let executable = executable.to_str().context("Executable path is not valid Unicode")?;
    register_windows(executable, r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run")?;
    Command::new(executable).arg("--scheduler").creation_flags(0x08000000).spawn().context("start background scheduler")?;
    Ok(())
}

#[cfg(windows)]
fn register_windows(executable: &str, key: &str) -> anyhow::Result<()> {
    let value = format!("\"{executable}\" --scheduler");
    command("reg", &["add", key, "/v", "DataCraftScheduler", "/t", "REG_SZ", "/d", &value, "/f"])
}

#[cfg(target_os = "linux")]
fn install_native(executable: &Path) -> anyhow::Result<()> {
    if command("systemctl", &["--user", "show-environment"]).is_err() {
        return install_xdg(executable);
    }
    let dir = dirs::config_dir().context("find user configuration directory")?.join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    let unit = dir.join("datacraft-scheduler.service");
    let path = executable.to_str().context("Executable path is not valid Unicode")?;
    std::fs::write(unit, systemd_unit(path)?)?;
    command("systemctl", &["--user", "daemon-reload"])?;
    command("systemctl", &["--user", "enable", "datacraft-scheduler.service"])?;
    // The previous process may still be exiting after releasing the scheduler lock.
    if !is_running()? {
        command("systemctl", &["--user", "restart", "datacraft-scheduler.service"])?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_xdg(executable: &Path) -> anyhow::Result<()> {
    if isolated_profile_without_native_services() {
        bail!("Native scheduler startup is disabled for isolated test profiles");
    }
    let path = executable.to_str().context("Executable path is not valid Unicode")?;
    let dir = dirs::config_dir().context("find user configuration directory")?.join("autostart");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("datacraft-scheduler.desktop"), xdg_entry(path)?)?;
    let mut child = Command::new(executable).arg("--scheduler").spawn().context("start background scheduler")?;
    std::thread::Builder::new().name("sql-scheduler-reaper".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_native(executable: &Path) -> anyhow::Result<()> {
    let dir = dirs::home_dir().context("find home directory")?.join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let path = executable.to_str().context("Executable path is not valid Unicode")?;
    let file = dir.join("com.datacraft.scheduler.plist");
    std::fs::write(&file, launchd_plist(path)?)?;
    let domain = launchd_domain()?;
    let label = format!("{domain}/com.datacraft.scheduler");
    let loaded = Command::new("launchctl").args(["print", &label]).output().context("check scheduler LaunchAgent")?;
    if loaded.status.success() {
        // Refresh the launch path only when no daemon owns active SQL or backup work.
        if is_running()? {
            return command("launchctl", &["enable", &label]);
        }
        command("launchctl", &["bootout", &label])?;
    }
    command("launchctl", &["enable", &label])?;
    command("launchctl", &["bootstrap", &domain, file.to_str().context("LaunchAgent path is not valid Unicode")?])
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> anyhow::Result<String> {
    let uid = Command::new("id").arg("-u").output().context("get user ID")?;
    if !uid.status.success() {
        bail!("Could not determine user ID");
    }
    Ok(format!("gui/{}", String::from_utf8(uid.stdout)?.trim()))
}

#[cfg(target_os = "macos")]
fn unregister_native() -> anyhow::Result<()> {
    let file = dirs::home_dir().context("find home directory")?.join("Library/LaunchAgents/com.datacraft.scheduler.plist");
    let label = format!("{}/com.datacraft.scheduler", launchd_domain()?);
    // bootout would kill a daemon unregistering itself before it can report errors and exit.
    command("launchctl", &["disable", &label])?;
    remove_if_present(&file)
}

#[cfg(any(test, target_os = "linux"))]
fn systemd_unit(path: &str) -> anyhow::Result<String> {
    if path.chars().any(char::is_control) {
        bail!("Executable path contains control characters");
    }
    let path = path.replace('%', "%%").replace('$', "$$").replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "[Unit]\nDescription=DataCraft SQL scheduler\n\n[Service]\nType=exec\nExecStart=\"{path}\" --scheduler\nRestart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[cfg(any(test, target_os = "linux"))]
fn xdg_entry(path: &str) -> anyhow::Result<String> {
    if path.chars().any(char::is_control) || path.contains(['=', '%']) {
        bail!("Executable path cannot be represented in XDG autostart");
    }
    let mut escaped = String::new();
    for ch in path.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\\\\\"),
            '"' => escaped.push_str("\\\\\""),
            '$' => escaped.push_str("\\\\$"),
            '`' => escaped.push_str("\\\\`"),
            ch => escaped.push(ch),
        }
    }
    Ok(format!("[Desktop Entry]\nType=Application\nName=DataCraft SQL scheduler\nExec=\"{escaped}\" --scheduler\nTerminal=false\n"))
}

#[cfg(any(test, target_os = "macos"))]
fn launchd_plist(path: &str) -> anyhow::Result<String> {
    if path.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        bail!("Executable path contains invalid XML characters");
    }
    let escaped = path.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>com.datacraft.scheduler</string><key>ProgramArguments</key><array><string>{escaped}</string><string>--scheduler</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_files_preserve_special_path_characters() {
        let unit = systemd_unit("/home/a b/$name%1/\\\"datacraft").unwrap();
        assert!(unit.contains("ExecStart=\"/home/a b/$$name%%1/\\\\\\\"datacraft\" --scheduler"));
        assert!(systemd_unit("/bad\npath").is_err());
        let entry = xdg_entry("/home/a b/$name/\\\"data").unwrap();
        assert!(entry.contains("Exec=\"/home/a b/\\\\$name/\\\\\\\\\\\\\"data\" --scheduler"));
        assert!(xdg_entry("/bad%name").is_err());
        let plist = launchd_plist("/Applications/A & B <app>/DataCraft").unwrap();
        assert!(plist.contains("<string>/Applications/A &amp; B &lt;app&gt;/DataCraft</string>"));
        assert!(plist.contains("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>"));
        assert!(launchd_plist("/bad\0path").is_err());
    }

    #[test]
    fn removing_startup_files_is_idempotent_and_reports_filesystem_errors() {
        let root = std::env::temp_dir().join(format!("datacraft-service-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("startup");
        remove_if_present(&path).unwrap();
        std::fs::write(&path, "test registration").unwrap();
        remove_if_present(&path).unwrap();
        assert!(!path.exists());
        remove_if_present(&path).unwrap();
        assert!(remove_if_present(&root).is_err());
        std::fs::remove_dir(root).unwrap();
    }

    /// Runs only in an isolated GitHub-hosted runner with its user service manager configured.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "requires a disposable CI user session and its native service manager"]
    fn native_service_runs_sql_without_the_gui() {
        use crate::db::{AuthMode, ConnectionConfig, Driver};
        use crate::scheduler::{Job, Store, now};

        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        assert_eq!(std::env::var("DATACRAFT_TEST_NATIVE_SERVICE").as_deref(), Ok("1"));
        let profile = std::path::PathBuf::from(std::env::var_os("DATACRAFT_TEST_DATA_DIR").unwrap());
        let runner_temp = std::path::PathBuf::from(std::env::var_os("RUNNER_TEMP").unwrap());
        std::fs::create_dir_all(&profile).unwrap();
        assert!(profile.canonicalize().unwrap().starts_with(runner_temp.canonicalize().unwrap()));

        #[cfg(target_os = "linux")]
        let xdg = std::env::var_os("DATACRAFT_TEST_XDG").is_some();
        struct Cleanup {
            #[cfg(target_os = "linux")]
            xdg: bool,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                #[cfg(target_os = "linux")]
                {
                    if let Some(dir) = dirs::config_dir() {
                        if self.xdg {
                            let _ = command("pkill", &["-x", "datacraft"]);
                            let _ = std::fs::remove_file(dir.join("autostart/datacraft-scheduler.desktop"));
                        } else {
                            let _ = command("systemctl", &["--user", "disable", "--now", "datacraft-scheduler.service"]);
                            let _ = std::fs::remove_file(dir.join("systemd/user/datacraft-scheduler.service"));
                            let _ = command("systemctl", &["--user", "daemon-reload"]);
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    if let Ok(uid) = Command::new("id").arg("-u").output() {
                        let label = format!("gui/{}/com.datacraft.scheduler", String::from_utf8_lossy(&uid.stdout).trim());
                        let _ = command("launchctl", &["bootout", &label]);
                    }
                    if let Some(dir) = dirs::home_dir() {
                        let _ = std::fs::remove_file(dir.join("Library/LaunchAgents/com.datacraft.scheduler.plist"));
                    }
                }
            }
        }
        let _cleanup = Cleanup {
            #[cfg(target_os = "linux")]
            xdg,
        };
        let database = profile.join("sample.sqlite");
        let sample = rusqlite::Connection::open(&database).unwrap();
        sample.execute_batch("CREATE TABLE probe(value INTEGER); INSERT INTO probe VALUES(0);").unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.auth = AuthMode::NoAuth;
        cfg.file_path = database.to_string_lossy().into_owned();
        std::fs::write(profile.join("connections.json"), serde_json::to_string(&vec![cfg]).unwrap()).unwrap();
        let mut store = Store::open(&profile.join("scheduler.sqlite")).unwrap();
        let job = Job {
            id: 0,
            name: "native service test".into(),
            connection: 1,
            database: "main".into(),
            sql: "UPDATE probe SET value = value + 1".into(),
            backup: None,
            interval_seconds: 30,
            enabled: true,
            next_run: 0,
        };
        let id = store.save(&job, now() - 30).unwrap();
        let folder = profile.join("backups");
        std::fs::create_dir(&folder).unwrap();
        let mut backup = crate::scheduler::backup::ScheduledBackup::new(Driver::Sqlite);
        backup.folder = folder.to_string_lossy().into_owned();
        let backup_id =
            store.save(&Job { id: 0, name: "native backup test".into(), sql: String::new(), backup: Some(backup), ..job.clone() }, now() - 30).unwrap();
        // Keep the service alive while the completed SQL and backup jobs are paused.
        store.save(&Job { name: "future task".into(), interval_seconds: 3600, ..job.clone() }, now()).unwrap();
        let executable = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/datacraft");
        #[cfg(target_os = "linux")]
        let startup_file =
            dirs::config_dir().unwrap().join(if xdg { "autostart/datacraft-scheduler.desktop" } else { "systemd/user/datacraft-scheduler.service" });
        #[cfg(target_os = "macos")]
        let startup_file = dirs::home_dir().unwrap().join("Library/LaunchAgents/com.datacraft.scheduler.plist");
        let start = |_legacy: bool| {
            #[cfg(target_os = "linux")]
            if xdg {
                install_xdg(&executable).unwrap();
                assert!(Command::new("desktop-file-validate").arg(&startup_file).status().unwrap().success());
            } else {
                install(&executable).unwrap();
            }
            #[cfg(target_os = "macos")]
            if _legacy {
                // Exercise upgrading a registration whose cached KeepAlive policy predates graceful exit.
                std::fs::create_dir_all(startup_file.parent().unwrap()).unwrap();
                let plist = launchd_plist(executable.to_str().unwrap())
                    .unwrap()
                    .replace("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>", "<key>KeepAlive</key><true/>");
                std::fs::write(&startup_file, plist).unwrap();
                let domain = launchd_domain().unwrap();
                command("launchctl", &["enable", &format!("{domain}/com.datacraft.scheduler")]).unwrap();
                command("launchctl", &["bootstrap", &domain, startup_file.to_str().unwrap()]).unwrap();
            } else {
                install(&executable).unwrap();
            }
            wait_for_start().unwrap();
            assert!(startup_file.is_file());
        };
        let wait_for_stop = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while is_running().unwrap() {
                assert!(std::time::Instant::now() < deadline, "service did not exit after all tasks were paused");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert!(!startup_file.exists(), "service left its startup registration behind");
            #[cfg(target_os = "linux")]
            {
                let dir = dirs::config_dir().unwrap();
                assert!(!dir.join("autostart/datacraft-scheduler.desktop").exists());
                assert!(!dir.join("systemd/user/datacraft-scheduler.service").exists());
                assert!(std::fs::symlink_metadata(dir.join("systemd/user/default.target.wants/datacraft-scheduler.service")).is_err());
            }
            #[cfg(target_os = "macos")]
            {
                let disabled = Command::new("launchctl").args(["print-disabled", &launchd_domain().unwrap()]).output().unwrap();
                assert!(disabled.status.success());
                let output = String::from_utf8_lossy(&disabled.stdout);
                assert!(
                    output.lines().any(|line| {
                        line.split_once("=>").is_some_and(|(label, state)| {
                            label.trim().trim_matches('"') == "com.datacraft.scheduler" && matches!(state.trim().trim_end_matches(','), "true" | "disabled")
                        })
                    }),
                    "unexpected LaunchAgent disabled state: {output}"
                );
            }
            unregister().unwrap();
        };
        start(true);
        let wait_for_run = |store: &Store, job_id, kind| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let run = store.runs(job_id).unwrap().into_iter().next();
                if let Some(run) = run.filter(|run| run.status != "running") {
                    assert_eq!(run.status, "succeeded", "job_id={job_id}, task={kind}, message={}", run.message);
                    store.pause(job_id).unwrap();
                    crate::scheduler::wake().unwrap();
                    break run;
                }
                assert!(std::time::Instant::now() < deadline, "service did not finish the task");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        };
        wait_for_run(&store, id, "SQL");
        let backup_run = wait_for_run(&store, backup_id, "backup");
        {
            let snapshot = rusqlite::Connection::open(&backup_run.message).unwrap();
            assert_eq!(std::path::Path::new(&backup_run.message).parent(), Some(folder.as_path()));
            let value = snapshot.query_row("SELECT value FROM probe", [], |row| row.get::<_, i64>(0)).unwrap();
            assert!((0..=1).contains(&value));
        }
        sample.execute_batch("BEGIN IMMEDIATE").unwrap();
        let followup_id = store
            .save(
                &Job {
                    id: 0,
                    name: "native SQL after backup test".into(),
                    sql: "PRAGMA busy_timeout=30000; UPDATE probe SET value = value + 1".into(),
                    ..job.clone()
                },
                now() - 30,
            )
            .unwrap();
        crate::scheduler::wake().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !store.runs(followup_id).unwrap().iter().any(|run| run.status == "running") {
            assert!(std::time::Instant::now() < deadline, "service did not start the blocked SQL task");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        store.pause_all().unwrap();
        crate::scheduler::wake().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while startup_file.exists() {
            assert!(std::time::Instant::now() < deadline, "pausing all tasks did not remove startup registration");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(is_running().unwrap(), "pausing tasks stopped an active SQL statement");
        assert_eq!(store.runs(followup_id).unwrap()[0].status, "running");
        sample.execute_batch("ROLLBACK").unwrap();
        wait_for_run(&store, followup_id, "SQL after backup");
        wait_for_stop();
        assert_eq!(sample.query_row("SELECT value FROM probe", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
        std::thread::sleep(std::time::Duration::from_secs(12));
        assert!(!is_running().unwrap(), "disabled service restarted after a successful exit");
        let resumed_id = store.save(&Job { name: "SQL after re-enable".into(), ..job }, now() - 30).unwrap();
        start(false);
        wait_for_run(&store, resumed_id, "SQL after re-enable");
        wait_for_stop();
        assert_eq!(sample.query_row("SELECT value FROM probe", [], |row| row.get::<_, i64>(0)).unwrap(), 3);
    }

    #[cfg(windows)]
    #[test]
    fn windows_autostart_quotes_the_path_and_unregisters_only_its_own_value() {
        struct TestKey(String);
        impl Drop for TestKey {
            fn drop(&mut self) {
                let _ = command("reg", &["delete", &self.0, "/f"]);
            }
        }
        let subkey = format!(r"Software\DataCraftSchedulerTests\apostrophe'{}", uuid::Uuid::new_v4());
        let key = TestKey(format!(r"HKCU\{subkey}"));
        unregister_windows(&subkey).unwrap();
        register_windows(r"C:\Program Files\DataCraft\DataCraft.exe", &key.0).unwrap();
        let result = Command::new("reg").args(["query", &key.0, "/v", "DataCraftScheduler"]).output().unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains(r#""C:\Program Files\DataCraft\DataCraft.exe" --scheduler"#));
        command("reg", &["add", &key.0, "/v", "OtherProgram", "/d", "preserved", "/f"]).unwrap();
        unregister_windows(&subkey).unwrap();
        unregister_windows(&subkey).unwrap();
        let removed = Command::new("reg").args(["query", &key.0, "/v", "DataCraftScheduler"]).output().unwrap();
        assert!(!removed.status.success());
        command("reg", &["query", &key.0, "/v", "OtherProgram"]).unwrap();
        register_windows(r"C:\Program Files\DataCraft\DataCraft.exe", &key.0).unwrap();
        command("reg", &["query", &key.0, "/v", "DataCraftScheduler"]).unwrap();
    }
}
