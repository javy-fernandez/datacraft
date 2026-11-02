//! Native backups on private sessions; local files are published only after success.
use super::{ConnectionConfig, Driver, Session};
use crate::{edit::literal, i18n::tr};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupMode {
    Full,
    Schema,
    Data,
    Differential,
    Log,
}
impl BackupMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => tr("Full backup"),
            Self::Schema => tr("Structure only"),
            Self::Data => tr("Data only"),
            Self::Differential => tr("Differential backup"),
            Self::Log => tr("Transaction log backup"),
        }
    }
    pub fn for_driver(driver: Driver) -> &'static [Self] {
        match driver {
            Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::Oracle => {
                &[Self::Full, Self::Schema, Self::Data]
            }
            Driver::MsSql => &[Self::Full, Self::Differential, Self::Log],
            Driver::Sqlite | Driver::Snowflake => &[Self::Full],
            _ => &[],
        }
    }
}

#[derive(Clone)]
pub struct BackupRequest {
    pub db: String,
    pub destination: String,
    pub directory: String,
    pub utility: String,
    pub mode: BackupMode,
    pub compress: bool,
    pub archive: bool,
    pub include_objects: bool,
    pub transaction: bool,
    pub copy_only: bool,
    pub checksum: bool,
    pub export_log: bool,
    pub trust_cert: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub conn: u64,
    pub db: String,
    pub destination: String,
    pub mode: BackupMode,
    pub created: String,
    pub bytes: Option<u64>,
}

impl BackupRequest {
    pub fn new(db: String, driver: Driver) -> Self {
        Self {
            db,
            destination: String::new(),
            directory: if driver == Driver::Snowflake {
                "PUBLIC".into()
            } else {
                String::new()
            },
            utility: if matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb) {
                utility_path(driver)
            } else {
                String::new()
            },
            mode: BackupMode::Full,
            compress: false,
            archive: driver == Driver::Postgres,
            include_objects: true,
            transaction: true,
            copy_only: true,
            checksum: true,
            export_log: true,
            trust_cert: false,
        }
    }

    pub fn local(driver: Driver) -> bool {
        matches!(
            driver,
            Driver::Sqlite | Driver::Postgres | Driver::MySql | Driver::MariaDb
        )
    }

    pub fn validate(&self, driver: Driver) -> anyhow::Result<()> {
        if !BackupMode::for_driver(driver).contains(&self.mode) {
            bail!(tr("Unsupported backup mode"));
        }
        if self.db.is_empty()
            || self.destination.trim().is_empty()
            || [&self.db, &self.destination, &self.directory]
                .iter()
                .any(|s| s.contains('\0'))
        {
            bail!(tr("Invalid backup parameter"));
        }
        if matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb) {
            if self.utility.trim().is_empty() {
                bail!(tr("Invalid backup parameter"));
            }
            if cfg!(windows)
                && Path::new(self.utility.trim_end_matches([' ', '.']))
                    .extension()
                    .is_some_and(|e| !e.eq_ignore_ascii_case("exe"))
            {
                bail!(tr("Choose a native backup executable"));
            }
        }
        if Self::local(driver) && !Path::new(&self.destination).is_absolute() {
            bail!(tr("Choose an absolute backup file path"));
        }
        if driver == Driver::Oracle
            && (self.directory.trim().is_empty()
                || self.destination.contains(['/', '\\'])
                || !self.destination.to_ascii_lowercase().ends_with(".dmp"))
        {
            bail!(tr("Use an Oracle directory object and a .dmp filename"));
        }
        if driver == Driver::Snowflake && self.directory.trim().is_empty() {
            bail!(tr("Invalid backup parameter"));
        }
        if driver == Driver::MsSql && self.mode == BackupMode::Differential && self.copy_only {
            bail!(tr("Differential backups cannot use Copy only"));
        }
        Ok(())
    }

    pub fn sql(&self, driver: Driver, job: &str) -> anyhow::Result<Vec<String>> {
        self.validate(driver)?;
        let value = |v: &str| literal(driver, &Some(v.into()));
        let db = driver.quote_ident(&self.db);
        Ok(match driver {
            Driver::MsSql => {
                let kind = if self.mode == BackupMode::Log {
                    "LOG"
                } else {
                    "DATABASE"
                };
                let mut options = vec!["NOFORMAT", "NOINIT"];
                if self.mode == BackupMode::Differential {
                    options.push("DIFFERENTIAL");
                }
                if self.copy_only {
                    options.push("COPY_ONLY");
                }
                if self.checksum {
                    options.push("CHECKSUM");
                }
                if self.compress {
                    options.push("COMPRESSION");
                }
                vec![format!(
                    "BACKUP {kind} {db} TO DISK = {} WITH {}",
                    value(&self.destination),
                    options.join(", ")
                )]
            }
            Driver::Oracle => {
                let content = match self.mode {
                    BackupMode::Schema => "METADATA_ONLY",
                    BackupMode::Data => "DATA_ONLY",
                    _ => "ALL",
                };
                let log = if self.export_log {
                    format!(
                        "DBMS_DATAPUMP.ADD_FILE(h, {}, {}, filetype => DBMS_DATAPUMP.KU$_FILE_TYPE_LOG_FILE);",
                        value(&format!("{}-{job}.log", self.destination)),
                        value(&self.directory)
                    )
                } else {
                    String::new()
                };
                vec![format!(
                    "DECLARE h NUMBER; state VARCHAR2(30) := 'UNDEFINED'; status ku$_Status; errors NUMBER := 0; BEGIN h := DBMS_DATAPUMP.OPEN(operation => 'EXPORT', job_mode => 'FULL', job_name => {}); DBMS_DATAPUMP.ADD_FILE(h, {}, {}, reusefile => 0); {log} DBMS_DATAPUMP.SET_PARAMETER(h, 'CONTENT', '{content}'); DBMS_DATAPUMP.START_JOB(h); WHILE state NOT IN ('COMPLETED', 'STOPPED') LOOP DBMS_DATAPUMP.GET_STATUS(h, DBMS_DATAPUMP.KU$_STATUS_JOB_ERROR + DBMS_DATAPUMP.KU$_STATUS_JOB_STATUS, 1, state, status); IF BITAND(status.mask, DBMS_DATAPUMP.KU$_STATUS_JOB_ERROR) <> 0 AND status.error IS NOT NULL THEN errors := errors + status.error.COUNT; END IF; END LOOP; DBMS_DATAPUMP.DETACH(h); IF state <> 'COMPLETED' OR errors > 0 THEN RAISE_APPLICATION_ERROR(-20001, 'Data Pump export incomplete; inspect the export log'); END IF; END;",
                    value(job),
                    value(&self.destination),
                    value(&self.directory)
                )]
            }
            Driver::Snowflake => {
                let set = format!(
                    "{db}.{}.{}",
                    driver.quote_ident(&self.directory),
                    driver.quote_ident(&self.destination)
                );
                vec![
                    format!("CREATE BACKUP SET {set} FOR DATABASE {db}"),
                    format!("ALTER BACKUP SET {set} ADD BACKUP"),
                ]
            }
            _ => Vec::new(),
        })
    }
}

pub fn utility_path(driver: Driver) -> String {
    find_utility(&[match driver {
        Driver::Postgres => "pg_dump",
        Driver::MariaDb => "mariadb-dump",
        _ => "mysqldump",
    }])
}

/// The first of `names` found on `PATH` or in common installation directories, else `names[0]`.
pub(super) fn find_utility(names: &[&str]) -> String {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if cfg!(windows)
        && let Some(root) = std::env::var_os("ProgramFiles")
    {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten().take(256) {
                let path = entry.path();
                let label = entry.file_name().to_string_lossy().to_lowercase();
                if label.starts_with("mariadb") {
                    dirs.push(path.join("bin"));
                }
                if (label == "postgresql" || label == "mysql")
                    && let Ok(versions) = fs::read_dir(path)
                {
                    for version in versions.flatten().take(32) {
                        dirs.push(version.path().join("bin"));
                    }
                }
            }
        }
    } else {
        dirs.extend([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
    }
    names
        .iter()
        .flat_map(|name| {
            let executable = if cfg!(windows) {
                format!("{name}.exe")
            } else {
                (*name).into()
            };
            dirs.iter().map(move |d| d.join(&executable))
        })
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| names[0].into())
}

struct LocalOutput {
    temporary: PathBuf,
    destination: PathBuf,
}
impl LocalOutput {
    fn new(destination: &str) -> anyhow::Result<Self> {
        let destination = PathBuf::from(destination);
        if destination.try_exists()? {
            bail!(tr("Backup destination already exists"));
        }
        let parent = destination
            .parent()
            .context(tr("Choose an absolute backup file path"))?;
        let temporary = parent.join(format!(".datacraft-{}.part", uuid::Uuid::new_v4()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        Ok(Self {
            temporary,
            destination,
        })
    }
    fn publish(&self) -> anyhow::Result<u64> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.temporary)?
            .sync_all()?;
        // hard_link publishes without replacing an existing file; FAT/network filesystems use a guarded copy.
        if fs::hard_link(&self.temporary, &self.destination).is_err() {
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.destination)
                .context(tr("Backup destination already exists"))?;
            let copy = (|| -> std::io::Result<()> {
                std::io::copy(&mut File::open(&self.temporary)?, &mut target)?;
                target.sync_all()
            })();
            if let Err(error) = copy {
                drop(target);
                let _ = fs::remove_file(&self.destination);
                return Err(error.into());
            }
        }
        Ok(fs::metadata(&self.destination)?.len())
    }
}
impl Drop for LocalOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

pub(super) struct Credentials {
    directory: PathBuf,
}
impl Credentials {
    pub(super) fn new() -> anyhow::Result<Self> {
        let directory =
            std::env::temp_dir().join(format!("datacraft-backup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        let credentials = Self { directory };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&credentials.directory, fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        {
            let system = PathBuf::from(
                std::env::var_os("SystemRoot")
                    .context(tr("Could not secure backup credentials"))?,
            )
            .join("System32");
            let mut who = Command::new(system.join("whoami.exe"));
            hide(&mut who);
            let output = who.args(["/user", "/fo", "csv", "/nh"]).output()?;
            let text = String::from_utf8_lossy(&output.stdout);
            let sid = text
                .split('"')
                .find(|part| {
                    part.starts_with("S-1-")
                        && part
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == '-' || c == 'S')
                })
                .context(tr("Could not secure backup credentials"))?;
            let mut acl = Command::new(system.join("icacls.exe"));
            hide(&mut acl);
            let status = acl
                .arg(&credentials.directory)
                .args(["/inheritance:r", "/grant:r", &format!("*{sid}:(OI)(CI)F")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if !status.success() {
                bail!(tr("Could not secure backup credentials"));
            }
        }
        Ok(credentials)
    }
    pub(super) fn write(&self, text: &str) -> anyhow::Result<PathBuf> {
        let path = self.directory.join("credentials");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(path)
    }
}
impl Drop for Credentials {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join("credentials"));
        let _ = fs::remove_dir(&self.directory);
    }
}

pub(super) fn hide(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

pub(super) fn quoted_option(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}
pub(super) fn pg_value(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn dump_command(
    cfg: &ConnectionConfig,
    request: &BackupRequest,
    credentials: &Credentials,
) -> anyhow::Result<Command> {
    if [&cfg.host, &cfg.user, &cfg.password]
        .iter()
        .any(|s| s.contains('\0'))
    {
        bail!(tr("Invalid backup parameter"));
    }
    let mut command = Command::new(&request.utility);
    hide(&mut command);
    match cfg.driver {
        Driver::Postgres => {
            if [&cfg.host, &cfg.user, &cfg.password, &request.db]
                .iter()
                .any(|s| s.contains(['\n', '\r', '\0']))
            {
                bail!(tr("Invalid backup parameter"));
            }
            let escape = |s: &str| s.replace('\\', "\\\\").replace(':', "\\:");
            let pass = credentials.write(&format!(
                "{}:{}:{}:{}:{}\n",
                escape(&cfg.host),
                cfg.port,
                escape(&request.db),
                escape(&cfg.user),
                escape(&cfg.password)
            ))?;
            let dsn = format!(
                "host={} port={} user={} dbname={} connect_timeout=15",
                pg_value(&cfg.host),
                cfg.port,
                pg_value(&cfg.user),
                pg_value(&request.db)
            );
            command
                .env_remove("PGPASSWORD")
                .env("PGPASSFILE", pass)
                .arg("--dbname")
                .arg(dsn)
                .arg("--no-password")
                .arg(if request.archive {
                    "--format=custom"
                } else {
                    "--format=plain"
                });
            command.arg(if request.compress {
                "--compress=6"
            } else {
                "--compress=0"
            });
            match request.mode {
                BackupMode::Schema => {
                    command.arg("--schema-only");
                }
                BackupMode::Data => {
                    command.arg("--data-only");
                }
                _ => {}
            }
        }
        Driver::MySql | Driver::MariaDb => {
            let contents = format!(
                "[client]\nhost={}\nport={}\nuser={}\npassword={}\n",
                quoted_option(&cfg.host),
                cfg.port,
                quoted_option(&cfg.user),
                quoted_option(&cfg.password)
            );
            let pass = credentials.write(&contents)?;
            command
                .env_remove("MYSQL_PWD")
                .arg(format!("--defaults-file={}", pass.display()))
                .args([
                    "--protocol=TCP",
                    "--quick",
                    "--hex-blob",
                    "--default-character-set=utf8mb4",
                ]);
            command.arg(if request.transaction {
                "--single-transaction"
            } else {
                "--lock-tables"
            });
            command.arg(match (cfg.driver, request.trust_cert) {
                (Driver::MySql, false) => "--ssl-mode=VERIFY_IDENTITY",
                (Driver::MySql, true) => "--ssl-mode=PREFERRED",
                (_, false) => "--ssl-verify-server-cert",
                (_, true) => "--skip-ssl-verify-server-cert",
            });
            if request.include_objects && request.mode != BackupMode::Data {
                command.args(["--routines", "--events", "--triggers"]);
            } else {
                command.args(["--skip-routines", "--skip-events", "--skip-triggers"]);
            }
            match request.mode {
                BackupMode::Schema => {
                    command.arg("--no-data");
                }
                BackupMode::Data => {
                    command.args(["--no-create-info", "--no-create-db"]);
                }
                _ => {}
            }
            command.args(["--databases", "--"]).arg(&request.db);
        }
        _ => bail!(tr("Unsupported backup mode")),
    }
    Ok(command)
}

pub(super) struct DumpChild(pub(super) std::process::Child);
impl Drop for DumpChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_dump(
    cfg: &ConnectionConfig,
    request: &BackupRequest,
    cancel: &AtomicBool,
) -> anyhow::Result<u64> {
    let output = LocalOutput::new(&request.destination)?;
    let credentials = Credentials::new()?;
    let mut command = dump_command(cfg, request, &credentials)?;
    command
        .stdout(Stdio::from(
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&output.temporary)?,
        ))
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = DumpChild(
        command
            .spawn()
            .context(tr("Backup utility was not found or could not be started"))?,
    );
    let mut stderr = child
        .0
        .stderr
        .take()
        .context(tr("Database backup failed"))?;
    let reader = std::thread::Builder::new()
        .name("backup-stderr".into())
        .spawn(move || {
            let mut saved = Vec::new();
            let mut bytes = [0_u8; 8192];
            loop {
                let n = stderr.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                let keep = n.min(65536_usize.saturating_sub(saved.len()));
                saved.extend_from_slice(&bytes[..keep]);
            }
            std::io::Result::Ok(saved)
        })?;
    let start = std::time::Instant::now();
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            bail!(tr("Backup was cancelled"));
        }
        if start.elapsed() > Duration::from_secs(1800) {
            bail!(tr("Backup operation timed out"));
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stderr = reader
        .join()
        .map_err(|_| anyhow::anyhow!(tr("Database backup failed")))??;
    if !status.success() {
        bail!(
            "{}: {}",
            tr("Database backup failed"),
            String::from_utf8_lossy(&stderr)
        );
    }
    if request.compress && matches!(cfg.driver, Driver::MySql | Driver::MariaDb) {
        let compressed = LocalOutput {
            temporary: output.temporary.with_extension("gz.part"),
            destination: output.destination.clone(),
        };
        let mut gzip = flate2::write::GzEncoder::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&compressed.temporary)?,
            flate2::Compression::default(),
        );
        let mut input = File::open(&output.temporary)?;
        let mut bytes = [0_u8; 65536];
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!(tr("Backup was cancelled"));
            }
            let n = input.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            gzip.write_all(&bytes[..n])?;
        }
        gzip.finish()?.sync_all()?;
        return compressed.publish();
    }
    output.publish()
}

fn run_sqlite(
    cfg: &ConnectionConfig,
    request: &BackupRequest,
    cancel: &AtomicBool,
) -> anyhow::Result<u64> {
    let output = LocalOutput::new(&request.destination)?;
    let source = rusqlite::Connection::open_with_flags(
        &cfg.file_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut target = rusqlite::Connection::open(&output.temporary)?;
    {
        let backup = rusqlite::backup::Backup::new(&source, &mut target)?;
        let start = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!(tr("Backup was cancelled"));
            }
            if start.elapsed() > Duration::from_secs(1800) {
                bail!(tr("Backup operation timed out"));
            }
            match backup.step(256)? {
                rusqlite::backup::StepResult::Done => break,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    drop(target);
    if cancel.load(Ordering::Relaxed) {
        bail!(tr("Backup was cancelled"));
    }
    output.publish()
}

pub async fn run(
    mut cfg: ConnectionConfig,
    request: BackupRequest,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<Option<u64>> {
    request.validate(cfg.driver)?;
    if cancel.load(Ordering::Relaxed) {
        bail!(tr("Backup was cancelled"));
    }
    if cfg.read_only && !BackupRequest::local(cfg.driver) {
        bail!(tr("This connection is read-only"));
    }
    if matches!(
        cfg.driver,
        Driver::Postgres | Driver::MySql | Driver::MariaDb
    ) {
        return tokio::task::spawn_blocking(move || run_dump(&cfg, &request, &cancel))
            .await?
            .map(Some);
    }
    if cfg.driver == Driver::Sqlite {
        return tokio::task::spawn_blocking(move || run_sqlite(&cfg, &request, &cancel))
            .await?
            .map(Some);
    }
    if cfg.driver != Driver::Oracle {
        cfg.database.clone_from(&request.db);
    }
    let mut session = tokio::time::timeout(Duration::from_secs(30), Session::connect(&cfg))
        .await
        .context(tr("Backup operation timed out"))??;
    session.use_database(&request.db).await?;
    let job = format!(
        "DC_BKP_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..20]
    );
    let statements = request.sql(cfg.driver, &job)?;
    let handle = session.cancel_handle();
    let query = async {
        for statement in statements {
            session.execute(&statement, 1).await?;
        }
        anyhow::Ok(())
    };
    let cancelled = async {
        while !cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let outcome = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(1800), query) => match result { Ok(result) => result, Err(_) => Err(anyhow::anyhow!(tr("Backup operation timed out"))) },
        _ = cancelled => Err(anyhow::anyhow!(tr("Backup was cancelled"))),
    };
    if let Err(error) = outcome {
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(3), handle.cancel()).await;
        }
        if cfg.driver == Driver::Oracle {
            let stop = async {
                let mut cleanup = Session::connect(&cfg).await?;
                cleanup.execute(&format!("DECLARE h NUMBER; BEGIN h := DBMS_DATAPUMP.ATTACH({}); DBMS_DATAPUMP.STOP_JOB(h, immediate => 1, keep_master => 0); END;", literal(Driver::Oracle, &Some(job))), 1).await
            };
            let _ = tokio::time::timeout(Duration::from_secs(15), stop).await;
        }
        return Err(error);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_commands_quote_names_and_do_not_replace_or_purge_existing_backups() {
        let mut request = BackupRequest::new("odd]db'".into(), Driver::MsSql);
        request.destination = "D:\\backup's.bak".into();
        let sql = request.sql(Driver::MsSql, "job").unwrap().join("\n");
        assert!(sql.contains("[odd]]db']"));
        assert!(sql.contains("backup''s.bak"));
        assert!(sql.contains("COPY_ONLY"));
        assert!(sql.contains("NOINIT"));
        request.mode = BackupMode::Differential;
        assert!(request.sql(Driver::MsSql, "job").is_err());
        request.mode = BackupMode::Full;
        request.directory = "BACKUP_DIR".into();
        request.destination = "backup.dmp".into();
        assert!(request.sql(Driver::Oracle, "job").unwrap()[0].contains("reusefile => 0"));
        assert!(request.sql(Driver::Snowflake, "job").unwrap()[0].starts_with("CREATE BACKUP SET"));
    }
    #[tokio::test]
    async fn sqlite_backup_restores_committed_rows_and_never_overwrites_an_existing_file() {
        let root = std::env::temp_dir().join(format!("dc-backup-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let source = root.join("source.db");
        let target = root.join("copy.db");
        let db = rusqlite::Connection::open(&source).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE sample (value TEXT); INSERT INTO sample(rowid, value) VALUES (42, 'backup value');").unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = source.to_string_lossy().into_owned();
        let mut request = BackupRequest::new("main".into(), Driver::Sqlite);
        request.destination = target.to_string_lossy().into_owned();
        assert!(
            run(
                cfg.clone(),
                request.clone(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .unwrap()
            .unwrap()
                > 0
        );
        let restored = rusqlite::Connection::open(&target).unwrap();
        assert_eq!(
            restored
                .query_row("SELECT value FROM sample", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "backup value"
        );
        assert!(
            run(cfg, request, Arc::new(AtomicBool::new(false)))
                .await
                .is_err()
        );
        drop(restored);
        drop(db);
        // macOS's system SQLite keeps the -wal/-shm files after closing.
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn client_passwords_stay_out_of_arguments_and_credentials_are_removed() {
        for driver in [Driver::Postgres, Driver::MySql, Driver::MariaDb] {
            let mut cfg = ConnectionConfig::new(7, driver);
            cfg.password = "secret'\\value".into();
            let mut request = BackupRequest::new("database' name".into(), driver);
            request.utility = "tool".into();
            let directory;
            {
                let credentials = Credentials::new().unwrap();
                directory = credentials.directory.clone();
                let command = dump_command(&cfg, &request, &credentials).unwrap();
                assert!(
                    !format!("{:?}", command.get_args().collect::<Vec<_>>()).contains("secret")
                );
                assert!(directory.join("credentials").is_file());
            }
            assert!(!directory.exists());
        }
    }

    #[tokio::test]
    async fn cancellation_and_missing_utilities_do_not_publish_or_leave_partial_files() {
        let root = std::env::temp_dir().join(format!("dc-backup-failure-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let cfg = ConnectionConfig::new(1, Driver::Postgres);
        let mut request = BackupRequest::new("shop".into(), Driver::Postgres);
        request.utility = root.join("missing.exe").to_string_lossy().into_owned();
        request.destination = root.join("backup.dump").to_string_lossy().into_owned();
        assert!(
            run(
                cfg.clone(),
                request.clone(),
                Arc::new(AtomicBool::new(true))
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
        );
        assert!(
            run(cfg, request, Arc::new(AtomicBool::new(false)))
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir(root).unwrap();
    }
}
