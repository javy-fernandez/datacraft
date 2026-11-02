//! Native restores: `pg_restore` / `psql`, `mysql` / `mariadb` and SQL Server `RESTORE DATABASE`.
use super::{
    ConnectionConfig, Driver, ResultSet, Session,
    backup::{Credentials, DumpChild, find_utility, hide, pg_value, quoted_option},
};
use crate::{edit::literal, i18n::tr};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreFormat {
    Custom,
    Directory,
    Tar,
    Sql,
    SqlGz,
    MsSql,
}
impl RestoreFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Custom => tr("PostgreSQL custom archive"),
            Self::Directory => tr("PostgreSQL directory archive"),
            Self::Tar => tr("PostgreSQL tar archive"),
            Self::Sql => tr("Plain SQL script"),
            Self::SqlGz => tr("Compressed SQL script (gzip)"),
            Self::MsSql => tr("SQL Server backup"),
        }
    }
    pub fn archive(self) -> bool {
        matches!(self, Self::Custom | Self::Directory | Self::Tar)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreContent {
    All,
    Schema,
    Data,
}
impl RestoreContent {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => tr("Schema and data"),
            Self::Schema => tr("Structure only"),
            Self::Data => tr("Data only"),
        }
    }
}

/// Engines with a native restore; Oracle Data Pump imports, SQLite and Snowflake are not offered.
pub fn supported(driver: Driver) -> bool {
    matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::MsSql)
}

/// Local engines read the backup on this computer; SQL Server reads it on the server.
pub fn local(driver: Driver) -> bool {
    supported(driver) && driver != Driver::MsSql
}

pub fn utility_path(driver: Driver, format: RestoreFormat) -> String {
    match driver {
        Driver::Postgres if format.archive() => find_utility(&["pg_restore"]),
        Driver::Postgres => find_utility(&["psql"]),
        Driver::MariaDb => find_utility(&["mariadb", "mysql"]),
        _ => find_utility(&["mysql"]),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRecord {
    pub conn: u64,
    pub db: String,
    pub source: String,
    pub created: String,
}

#[derive(Clone, Debug)]
pub struct RestoreRequest {
    /// Local file or folder; SQL Server: a `.bak` path on the server.
    pub source: String,
    /// Known after inspecting the source.
    pub format: Option<RestoreFormat>,
    /// Target database.
    pub db: String,
    /// Create the target database first (PostgreSQL, MySQL, MariaDB).
    pub create: bool,
    /// PostgreSQL database used to run `CREATE DATABASE`.
    pub maintenance: String,
    pub utility: String,
    pub clean: bool,
    pub no_owner: bool,
    pub content: RestoreContent,
    pub jobs: u32,
    /// SQL Server backup set (`FILE =`).
    pub position: u32,
    /// SQL Server (logical file, new physical path on the server).
    pub moves: Vec<(String, String)>,
    pub replace: bool,
    pub single_user: bool,
    pub trust_cert: bool,
}

impl RestoreRequest {
    pub fn new(db: String) -> Self {
        Self {
            source: String::new(),
            format: None,
            maintenance: db.clone(),
            db,
            create: false,
            utility: String::new(),
            clean: false,
            no_owner: false,
            content: RestoreContent::All,
            jobs: 1,
            position: 0,
            moves: Vec::new(),
            replace: false,
            single_user: false,
            trust_cert: false,
        }
    }

    pub fn validate(&self, driver: Driver) -> anyhow::Result<()> {
        if !supported(driver) {
            bail!(tr("Restore is not available for this engine"));
        }
        let fields = [&self.source, &self.db, &self.utility, &self.maintenance];
        if self.source.trim().is_empty() || self.db.trim().is_empty() || fields.iter().any(|s| s.contains('\0')) {
            bail!(tr("Invalid restore parameter"));
        }
        let Some(format) = self.format else {
            bail!(tr("Inspect the backup before restoring"));
        };
        if (driver == Driver::MsSql) != (format == RestoreFormat::MsSql) || (driver != Driver::Postgres && format.archive()) {
            bail!(tr("Choose a .sql or .sql.gz dump for this engine"));
        }
        if driver == Driver::MsSql {
            if self.position == 0 || self.moves.iter().any(|(l, p)| l.is_empty() || p.trim().is_empty() || p.contains('\0')) {
                bail!(tr("Invalid restore parameter"));
            }
            return Ok(());
        }
        if !Path::new(&self.source).is_absolute() {
            bail!(tr("Choose an absolute backup file path"));
        }
        if self.utility.trim().is_empty() {
            bail!(tr("Invalid restore parameter"));
        }
        if cfg!(windows)
            && Path::new(self.utility.trim_end_matches([' ', '.']))
                .extension()
                .is_some_and(|e| !e.eq_ignore_ascii_case("exe"))
        {
            bail!(tr("Choose a native restore executable"));
        }
        if driver == Driver::Postgres && [&self.db, &self.maintenance].iter().any(|s| s.contains(['\n', '\r'])) {
            bail!(tr("Invalid restore parameter"));
        }
        if self.clean && self.content == RestoreContent::Data {
            bail!(tr("Clean cannot be combined with Data only"));
        }
        if !(1..=64).contains(&self.jobs) || (self.jobs > 1 && !matches!(format, RestoreFormat::Custom | RestoreFormat::Directory)) {
            bail!(tr("Parallel jobs need a custom or directory archive"));
        }
        if self.create && driver == Driver::Postgres && self.maintenance.trim().is_empty() {
            bail!(tr("Invalid restore parameter"));
        }
        Ok(())
    }

    pub fn create_sql(&self, driver: Driver) -> String {
        let db = driver.quote_ident(&self.db);
        if driver == Driver::Postgres {
            // template0: an archive restores into an empty database, without template1's additions.
            format!("CREATE DATABASE {db} TEMPLATE template0")
        } else {
            format!("CREATE DATABASE {db}")
        }
    }

    /// SQL Server batch: optional single-user switch, then `RESTORE DATABASE`.
    pub fn mssql_sql(&self) -> anyhow::Result<String> {
        self.validate(Driver::MsSql)?;
        let value = |v: &str| literal(Driver::MsSql, &Some(v.into()));
        let db = Driver::MsSql.quote_ident(&self.db);
        let mut options = vec![format!("FILE = {}", self.position)];
        options.extend(self.moves.iter().map(|(logical, physical)| format!("MOVE {} TO {}", value(logical), value(physical))));
        if self.replace {
            options.push("REPLACE".into());
        }
        options.extend(["RECOVERY".into(), "STATS = 5".into()]);
        let restore = format!("RESTORE DATABASE {db} FROM DISK = {} WITH {}", value(&self.source), options.join(", "));
        Ok(if self.single_user {
            format!("ALTER DATABASE {db} SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\n{restore}")
        } else {
            restore
        })
    }
}

pub fn mssql_headers_sql(source: &str) -> String {
    format!("RESTORE HEADERONLY FROM DISK = {}", literal(Driver::MsSql, &Some(source.into())))
}
pub fn mssql_files_sql(source: &str, position: u32) -> String {
    format!("RESTORE FILELISTONLY FROM DISK = {} WITH FILE = {position}", literal(Driver::MsSql, &Some(source.into())))
}

/// Shared between the dialog and the job: cancellation and progress in per mille.
pub struct RestoreProgress {
    pub cancel: AtomicBool,
    permille: AtomicU32,
}
impl Default for RestoreProgress {
    fn default() -> Self {
        Self { cancel: AtomicBool::new(false), permille: AtomicU32::new(u32::MAX) }
    }
}
impl RestoreProgress {
    /// None while the utility reports no measurable progress.
    pub fn fraction(&self) -> Option<f32> {
        let value = self.permille.load(Ordering::Relaxed);
        (value != u32::MAX).then(|| value.min(1000) as f32 / 1000.0)
    }
    pub fn set(&self, done: u64, total: u64) {
        if let Some(permille) = done.saturating_mul(1000).checked_div(total) {
            self.permille.store(permille.min(1000) as u32, Ordering::Relaxed);
        }
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSet {
    pub position: u32,
    /// 1 database, 2 log, 5 differential… (`RESTORE HEADERONLY`'s BackupType).
    pub kind: u32,
    pub database: String,
    pub server: String,
    pub started: String,
}
impl BackupSet {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            1 => tr("Full backup"),
            2 => tr("Transaction log backup"),
            5 => tr("Differential backup"),
            _ => tr("Other backup"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupFile {
    pub logical: String,
    pub physical: String,
    /// D data, L log, F full-text catalog, S FILESTREAM.
    pub kind: String,
}

#[derive(Clone, Debug, Default)]
pub struct RestorePreview {
    pub format: Option<RestoreFormat>,
    pub size: Option<u64>,
    /// Archive header (`pg_restore -l`) or backup-set properties.
    pub details: Vec<(String, String)>,
    /// Archive TOC entries, or the first lines of a SQL script.
    pub entries: Vec<String>,
    pub sets: Vec<BackupSet>,
    pub files: Vec<BackupFile>,
}

pub fn detect(driver: Driver, source: &str) -> anyhow::Result<RestoreFormat> {
    if driver == Driver::MsSql {
        return Ok(RestoreFormat::MsSql);
    }
    let path = Path::new(source);
    let format = if path.is_dir() {
        if !path.join("toc.dat").is_file() {
            bail!(tr("The folder is not a pg_dump directory archive"));
        }
        RestoreFormat::Directory
    } else {
        let mut head = Vec::with_capacity(512);
        File::open(path)?.take(512).read_to_end(&mut head)?;
        sniff(&head)
    };
    if driver != Driver::Postgres && format.archive() {
        bail!(tr("Choose a .sql or .sql.gz dump for this engine"));
    }
    Ok(format)
}

fn sniff(head: &[u8]) -> RestoreFormat {
    if head.starts_with(b"PGDMP") {
        RestoreFormat::Custom
    } else if head.starts_with(&[0x1f, 0x8b]) {
        RestoreFormat::SqlGz
    } else if head.get(257..262) == Some(b"ustar") {
        RestoreFormat::Tar
    } else {
        RestoreFormat::Sql
    }
}

/// Archive header properties and TOC entries.
type Toc = (Vec<(String, String)>, Vec<String>);

/// `pg_restore -l` output: header properties and TOC entries without their ids.
pub fn parse_toc(text: &str) -> Toc {
    let mut details = Vec::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(comment) = line.strip_prefix(';') {
            if let Some((key, value)) = comment.trim().split_once(": ")
                && !key.is_empty()
                && !value.trim().is_empty()
            {
                details.push((key.trim().into(), value.trim().into()));
            }
        } else if let Some((id, rest)) = line.split_once("; ")
            && id.trim().parse::<u64>().is_ok()
        {
            // "<tableoid> <oid> TYPE schema name owner"
            let entry = rest.splitn(3, ' ').nth(2).unwrap_or(rest).trim();
            entries.push(entry.into());
        }
    }
    (details, entries)
}

fn column(result: &ResultSet, name: &str) -> Option<usize> {
    result.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
}
fn cell(result: &ResultSet, row: &[Option<String>], name: &str) -> String {
    column(result, name).and_then(|i| row.get(i).cloned().flatten()).unwrap_or_default()
}

pub fn parse_headers(result: &ResultSet) -> Vec<BackupSet> {
    result
        .rows
        .iter()
        .filter_map(|row| {
            Some(BackupSet {
                position: cell(result, row, "Position").trim().parse().ok()?,
                kind: cell(result, row, "BackupType").trim().parse().unwrap_or(0),
                database: cell(result, row, "DatabaseName"),
                server: cell(result, row, "ServerName"),
                started: cell(result, row, "BackupStartDate"),
            })
        })
        .collect()
}

pub fn parse_files(result: &ResultSet) -> Vec<BackupFile> {
    result
        .rows
        .iter()
        .map(|row| BackupFile {
            logical: cell(result, row, "LogicalName"),
            physical: cell(result, row, "PhysicalName"),
            kind: cell(result, row, "Type"),
        })
        .filter(|f| !f.logical.is_empty())
        .collect()
}

/// New server paths: the original folder, with the source database name in the file name
/// replaced by the target (or the target prefixed), so a copy never reuses the source's files.
pub fn default_moves(files: &[BackupFile], source_db: &str, target: &str) -> Vec<(String, String)> {
    let safe: String = target.chars().map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).collect();
    files
        .iter()
        .map(|f| {
            let split = f.physical.rfind(['\\', '/']).map_or(0, |i| i + 1);
            let (folder, name) = f.physical.split_at(split);
            let name = if target == source_db {
                name.to_string()
            } else if !source_db.is_empty() && name.contains(source_db) {
                name.replacen(source_db, &safe, 1)
            } else {
                format!("{safe}_{name}")
            };
            (f.logical.clone(), format!("{folder}{name}"))
        })
        .collect()
}

fn pg_dsn(cfg: &ConnectionConfig, db: &str) -> String {
    format!(
        "host={} port={} user={} dbname={} connect_timeout=15",
        pg_value(&cfg.host),
        cfg.port,
        pg_value(&cfg.user),
        pg_value(db)
    )
}

fn restore_command(cfg: &ConnectionConfig, request: &RestoreRequest, credentials: &Credentials) -> anyhow::Result<Command> {
    if [&cfg.host, &cfg.user, &cfg.password].iter().any(|s| s.contains('\0')) {
        bail!(tr("Invalid restore parameter"));
    }
    let format = request.format.context(tr("Inspect the backup before restoring"))?;
    let mut command = Command::new(&request.utility);
    hide(&mut command);
    match cfg.driver {
        Driver::Postgres => {
            if [&cfg.host, &cfg.user, &cfg.password].iter().any(|s| s.contains(['\n', '\r'])) {
                bail!(tr("Invalid restore parameter"));
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
            // English client messages: progress and diagnostics are recognised by their text.
            command.env_remove("PGPASSWORD").env("PGPASSFILE", pass).env("LC_MESSAGES", "C").arg("--dbname").arg(pg_dsn(cfg, &request.db)).arg("--no-password");
            if format.archive() {
                command.args(["--exit-on-error", "--verbose"]);
                if request.clean {
                    command.args(["--clean", "--if-exists"]);
                }
                if request.no_owner {
                    command.arg("--no-owner");
                }
                match request.content {
                    RestoreContent::Schema => {
                        command.arg("--schema-only");
                    }
                    RestoreContent::Data => {
                        command.arg("--data-only");
                    }
                    RestoreContent::All => {}
                }
                if request.jobs > 1 {
                    command.arg(format!("--jobs={}", request.jobs));
                }
                command.arg(&request.source);
            } else {
                command.args(["--no-psqlrc", "--quiet", "--set=ON_ERROR_STOP=1"]);
            }
        }
        Driver::MySql | Driver::MariaDb => {
            let pass = credentials.write(&format!(
                "[client]\nhost={}\nport={}\nuser={}\npassword={}\n",
                quoted_option(&cfg.host),
                cfg.port,
                quoted_option(&cfg.user),
                quoted_option(&cfg.password)
            ))?;
            command
                .env_remove("MYSQL_PWD")
                .arg(format!("--defaults-file={}", pass.display()))
                .args(["--protocol=TCP", "--default-character-set=utf8mb4"]);
            command.arg(match (cfg.driver, request.trust_cert) {
                (Driver::MySql, false) => "--ssl-mode=VERIFY_IDENTITY",
                (Driver::MySql, true) => "--ssl-mode=PREFERRED",
                (_, false) => "--ssl-verify-server-cert",
                (_, true) => "--skip-ssl-verify-server-cert",
            });
            command.arg(format!("--database={}", request.db));
        }
        _ => bail!(tr("Restore is not available for this engine")),
    }
    Ok(command)
}

/// mysqldump's `CREATE DATABASE` and `USE` lines would send the restore back to the source database.
fn switches_database(line: &[u8]) -> bool {
    let line = line.trim_ascii();
    let head = line[..line.len().min(16)].to_ascii_uppercase();
    head.starts_with(b"CREATE DATABASE ") || (head.starts_with(b"USE `") && line.ends_with(b"`;"))
}

/// pg_restore's verbose lines that mark one finished step.
fn progress_line(line: &str) -> bool {
    line.strip_prefix("pg_restore: ").is_some_and(|rest| ["creating ", "processing data ", "executing ", "finished item "].iter().any(|p| rest.starts_with(p)))
}
/// Diagnostics worth showing: everything but pg_restore's verbose chatter.
fn diagnostic_line(line: &str) -> bool {
    !line.starts_with("pg_restore: ") || ["error", "warning", "TOC entry", "Command was"].iter().any(|k| line.contains(k))
}

struct Counted<R> {
    inner: R,
    read: u64,
    total: u64,
    progress: Arc<RestoreProgress>,
}
impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        self.progress.set(self.read, self.total);
        Ok(n)
    }
}

fn feed(request: &RestoreRequest, driver: Driver, mut stdin: impl Write, progress: Arc<RestoreProgress>) -> anyhow::Result<()> {
    let file = File::open(&request.source)?;
    let total = file.metadata()?.len();
    let counted = Counted { inner: file, read: 0, total, progress: progress.clone() };
    let reader: Box<dyn Read> = if request.format == Some(RestoreFormat::SqlGz) {
        Box::new(flate2::read::MultiGzDecoder::new(counted))
    } else {
        Box::new(counted)
    };
    let mut reader = BufReader::with_capacity(1 << 16, reader);
    let filter = driver != Driver::Postgres;
    let mut line = Vec::new();
    loop {
        if progress.cancelled() {
            bail!(tr("Restore was cancelled"));
        }
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if !(filter && switches_database(&line)) {
            stdin.write_all(&line)?;
        }
    }
}

fn list_archive(utility: &str, source: &str) -> anyhow::Result<Toc> {
    let mut command = Command::new(utility);
    hide(&mut command);
    let output = command
        .env("LC_MESSAGES", "C")
        .arg("--list")
        .arg(source)
        .stdin(Stdio::null())
        .output()
        .context(tr("Restore utility was not found or could not be started"))?;
    if !output.status.success() {
        bail!("{}: {}", tr("Could not read the backup"), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(parse_toc(&String::from_utf8_lossy(&output.stdout)))
}

fn named(utility: &str, name: &str) -> bool {
    Path::new(utility).file_stem().is_some_and(|s| s.eq_ignore_ascii_case(name))
}

fn run_native(cfg: &ConnectionConfig, request: &RestoreRequest, progress: &Arc<RestoreProgress>) -> anyhow::Result<()> {
    let format = request.format.context(tr("Inspect the backup before restoring"))?;
    let total = if format.archive() { list_archive(&request.utility, &request.source)?.1.len() as u64 } else { 0 };
    let credentials = Credentials::new()?;
    let mut command = restore_command(cfg, request, &credentials)?;
    command.stdout(Stdio::null()).stderr(Stdio::piped()).stdin(if format.archive() { Stdio::null() } else { Stdio::piped() });
    let mut child = DumpChild(command.spawn().context(tr("Restore utility was not found or could not be started"))?);
    let stderr = child.0.stderr.take().context(tr("Database restore failed"))?;
    let counter = progress.clone();
    let reader = std::thread::Builder::new().name("restore-stderr".into()).spawn(move || {
        let mut saved = String::new();
        let mut done = 0;
        for line in BufReader::new(stderr).split(b'\n') {
            let line = String::from_utf8_lossy(&line?).trim_end().to_string();
            if total > 0 && progress_line(&line) {
                done += 1;
                counter.set(done, total + 1);
            } else if diagnostic_line(&line) && saved.len() < 65536 {
                saved.push_str(&line);
                saved.push('\n');
            }
        }
        std::io::Result::Ok(saved)
    })?;
    let feeder = match child.0.stdin.take() {
        Some(stdin) => {
            let (request, driver, progress) = (request.clone(), cfg.driver, progress.clone());
            Some(std::thread::Builder::new().name("restore-input".into()).spawn(move || feed(&request, driver, stdin, progress))?)
        }
        None => None,
    };
    let status = loop {
        if progress.cancelled() {
            bail!(tr("Restore was cancelled"));
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stderr = reader.join().map_err(|_| anyhow::anyhow!(tr("Database restore failed")))??;
    let fed = match feeder {
        Some(feeder) => feeder.join().map_err(|_| anyhow::anyhow!(tr("Database restore failed")))?,
        None => Ok(()),
    };
    if !status.success() {
        bail!("{}: {}", tr("Database restore failed"), stderr.trim());
    }
    // A utility that exits cleanly after truncated input (a corrupt gzip stream) is still a failure.
    fed.context(tr("Database restore failed"))?;
    progress.set(1, 1);
    Ok(())
}

async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Session> {
    tokio::time::timeout(Duration::from_secs(30), Session::connect(cfg)).await.context(tr("Restore operation timed out"))?
}

async fn first_cell(session: &mut Session, sql: &str) -> Option<String> {
    session.execute(sql, 1).await.ok()?.into_iter().next()?.rows.into_iter().next()?.into_iter().next()?
}

async fn run_mssql(mut cfg: ConnectionConfig, request: RestoreRequest, progress: Arc<RestoreProgress>) -> anyhow::Result<()> {
    cfg.database = "master".into();
    let batch = request.mssql_sql()?;
    let mut session = connect(&cfg).await?;
    let spid: u32 = first_cell(&mut session, "SELECT @@SPID").await.and_then(|s| s.trim().parse().ok()).context(tr("Database restore failed"))?;
    // tiberius drops informational messages, so STATS progress is read from the same counter.
    let mut monitor = connect(&cfg).await.ok();
    let outcome = {
        let restore = session.execute(&batch, 0);
        tokio::pin!(restore);
        loop {
            tokio::select! {
                result = &mut restore => break result.map(|_| ()),
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if progress.cancelled() {
                        if let Some(monitor) = monitor.as_mut() {
                            let _ = tokio::time::timeout(Duration::from_secs(10), monitor.execute(&format!("KILL {spid}"), 0)).await;
                        }
                        break Err(anyhow::anyhow!(tr("Restore was cancelled")));
                    }
                    if let Some(monitor) = monitor.as_mut() {
                        let sql = format!("SELECT CAST(percent_complete * 10 AS int) FROM sys.dm_exec_requests WHERE session_id = {spid}");
                        if let Ok(Some(value)) = tokio::time::timeout(Duration::from_secs(5), first_cell(monitor, &sql)).await
                            && let Ok(permille) = value.trim().parse::<u64>()
                        {
                            progress.set(permille, 1000);
                        }
                    }
                }
            }
        }
    };
    if request.single_user {
        // Restored databases keep the backup's access mode; a failed restore must not stay single-user.
        let reset = format!("IF DB_ID({0}) IS NOT NULL AND DATABASEPROPERTYEX({0}, 'UserAccess') = 'SINGLE_USER' AND DATABASEPROPERTYEX({0}, 'Status') = 'ONLINE' ALTER DATABASE {1} SET MULTI_USER", literal(Driver::MsSql, &Some(request.db.clone())), Driver::MsSql.quote_ident(&request.db));
        let cleanup = async {
            let mut session = match monitor {
                Some(monitor) => monitor,
                None => connect(&cfg).await?,
            };
            session.execute(&reset, 0).await
        };
        let _ = tokio::time::timeout(Duration::from_secs(15), cleanup).await;
    }
    outcome
}

async fn create_database(cfg: &ConnectionConfig, request: &RestoreRequest) -> anyhow::Result<()> {
    let mut cfg = cfg.clone();
    if cfg.driver == Driver::Postgres {
        cfg.database.clone_from(&request.maintenance);
    }
    let mut session = connect(&cfg).await?;
    session.execute(&request.create_sql(cfg.driver), 0).await?;
    Ok(())
}

/// Reads what the backup contains; local archives are listed without a database connection.
pub async fn inspect(mut cfg: ConnectionConfig, request: RestoreRequest) -> anyhow::Result<RestorePreview> {
    if !supported(cfg.driver) {
        bail!(tr("Restore is not available for this engine"));
    }
    if request.source.trim().is_empty() || request.source.contains('\0') {
        bail!(tr("Invalid restore parameter"));
    }
    if cfg.driver == Driver::MsSql {
        cfg.database = "master".into();
        let mut session = connect(&cfg).await?;
        let headers = session.execute(&mssql_headers_sql(&request.source), 0).await?;
        let sets = headers.first().map(parse_headers).unwrap_or_default();
        let set = sets
            .iter()
            .find(|s| s.position == request.position)
            .or_else(|| sets.iter().rev().find(|s| s.kind == 1))
            .context(tr("The file contains no full database backup"))?
            .clone();
        let files = session.execute(&mssql_files_sql(&request.source, set.position), 0).await?;
        let details = vec![
            (tr("Database").to_string(), set.database.clone()),
            (tr("Server").to_string(), set.server.clone()),
            (tr("Created").to_string(), set.started.clone()),
        ];
        return Ok(RestorePreview {
            format: Some(RestoreFormat::MsSql),
            details,
            files: files.first().map(parse_files).unwrap_or_default(),
            sets,
            ..Default::default()
        });
    }
    if !Path::new(&request.source).is_absolute() {
        bail!(tr("Choose an absolute backup file path"));
    }
    let driver = cfg.driver;
    tokio::task::spawn_blocking(move || {
        let format = detect(driver, &request.source)?;
        let size = std::fs::metadata(&request.source).ok().filter(|m| m.is_file()).map(|m| m.len());
        let (details, entries) = if format.archive() {
            let utility = if named(&request.utility, "pg_restore") { request.utility.clone() } else { utility_path(driver, format) };
            list_archive(&utility, &request.source)?
        } else {
            let file = File::open(&request.source)?;
            let reader: Box<dyn Read> = if format == RestoreFormat::SqlGz { Box::new(flate2::read::MultiGzDecoder::new(file)) } else { Box::new(file) };
            let lines = BufReader::new(reader.take(64 * 1024)).split(b'\n').map_while(Result::ok);
            let lines = lines.map(|l| String::from_utf8_lossy(&l).trim_end().chars().take(200).collect::<String>()).filter(|l| !l.is_empty()).take(20);
            (Vec::new(), lines.collect())
        };
        Ok(RestorePreview { format: Some(format), size, details, entries, ..Default::default() })
    })
    .await?
}

pub async fn run(cfg: ConnectionConfig, request: RestoreRequest, progress: Arc<RestoreProgress>) -> anyhow::Result<()> {
    request.validate(cfg.driver)?;
    if progress.cancelled() {
        bail!(tr("Restore was cancelled"));
    }
    if cfg.read_only {
        bail!(tr("This connection is read-only"));
    }
    if cfg.driver == Driver::MsSql {
        return run_mssql(cfg, request, progress).await;
    }
    if request.create {
        create_database(&cfg, &request).await?;
    }
    tokio::task::spawn_blocking(move || run_native(&cfg, &request, &progress)).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg_request(file: &str, format: RestoreFormat) -> RestoreRequest {
        let mut request = RestoreRequest::new("shop".into());
        request.source = std::env::temp_dir().join(file).to_string_lossy().into_owned();
        request.format = Some(format);
        request.utility = "tool".into();
        request
    }

    #[test]
    fn formats_are_recognised_from_their_signatures() {
        assert_eq!(sniff(b"PGDMP\x01\x0f\x00"), RestoreFormat::Custom);
        assert_eq!(sniff(&[0x1f, 0x8b, 8, 0]), RestoreFormat::SqlGz);
        let mut tar = vec![0_u8; 512];
        tar[..7].copy_from_slice(b"toc.dat");
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tar), RestoreFormat::Tar);
        assert_eq!(sniff(b"-- MariaDB dump 10.19\nCREATE TABLE t (id int);"), RestoreFormat::Sql);
        assert_eq!(sniff(b""), RestoreFormat::Sql);
        assert_eq!(detect(Driver::MsSql, "relative.bak").unwrap(), RestoreFormat::MsSql);
    }

    #[test]
    fn pg_restore_listing_keeps_header_details_and_entries() {
        let text = ";\n; Archive created at 2026-11-02 10:00:00 CET\n;     dbname: shop\n;     TOC Entries: 9\n;     Format: CUSTOM\n;     Dumped from database version: 16.4\n;\n;\n; Selected TOC Entries:\n;\n215; 1259 16386 TABLE public items postgres\n3340; 0 16386 TABLE DATA public items postgres\n3195; 2606 16392 CONSTRAINT public items items_pkey postgres\n";
        let (details, entries) = parse_toc(text);
        assert!(details.contains(&("dbname".into(), "shop".into())));
        assert!(details.contains(&("Format".into(), "CUSTOM".into())));
        assert!(details.contains(&("Dumped from database version".into(), "16.4".into())));
        assert_eq!(entries, ["TABLE public items postgres", "TABLE DATA public items postgres", "CONSTRAINT public items items_pkey postgres"]);
    }

    #[test]
    fn pg_restore_and_psql_commands_carry_options_but_never_the_password() {
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.password = "secret'\\value".into();
        let mut request = pg_request("shop.dump", RestoreFormat::Custom);
        request.clean = true;
        request.no_owner = true;
        request.content = RestoreContent::Schema;
        request.jobs = 4;
        request.validate(Driver::Postgres).unwrap_or_else(|e| panic!("{e}"));
        let credentials = Credentials::new().unwrap();
        let args = |command: &Command| command.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let archive = args(&restore_command(&cfg, &request, &credentials).unwrap());
        for flag in ["--exit-on-error", "--clean", "--if-exists", "--no-owner", "--schema-only", "--jobs=4", request.source.as_str()] {
            assert!(archive.contains(&flag.to_string()), "{flag}: {archive:?}");
        }
        assert!(!format!("{archive:?}").contains("secret"));
        let credentials = Credentials::new().unwrap();
        let plain = args(&restore_command(&cfg, &pg_request("shop.sql", RestoreFormat::Sql), &credentials).unwrap());
        assert!(plain.contains(&"--set=ON_ERROR_STOP=1".to_string()) && plain.contains(&"--no-psqlrc".to_string()));
        assert!(!plain.iter().any(|a| a.contains("shop.sql") || a.contains("secret")));
    }

    #[test]
    fn mysql_restores_target_the_chosen_database_with_a_defaults_file() {
        for driver in [Driver::MySql, Driver::MariaDb] {
            let mut cfg = ConnectionConfig::new(1, driver);
            cfg.password = "secret".into();
            let mut request = pg_request("shop.sql.gz", RestoreFormat::SqlGz);
            request.db = "shop copy".into();
            request.validate(driver).unwrap();
            let credentials = Credentials::new().unwrap();
            let command = restore_command(&cfg, &request, &credentials).unwrap();
            let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
            assert!(args[0].starts_with("--defaults-file="), "{args:?}");
            assert!(args.contains(&"--database=shop copy".to_string()));
            assert!(!format!("{args:?}").contains("secret"));
            request.format = Some(RestoreFormat::Custom);
            assert!(request.validate(driver).is_err());
        }
    }

    #[test]
    fn invalid_combinations_are_rejected_before_starting() {
        let mut request = pg_request("shop.dump", RestoreFormat::Custom);
        request.clean = true;
        request.content = RestoreContent::Data;
        assert!(request.validate(Driver::Postgres).is_err());
        request.clean = false;
        request.format = Some(RestoreFormat::Tar);
        request.jobs = 2;
        assert!(request.validate(Driver::Postgres).is_err());
        request.jobs = 1;
        assert!(request.validate(Driver::Postgres).is_ok());
        request.format = None;
        assert!(request.validate(Driver::Postgres).is_err());
        request.format = Some(RestoreFormat::Sql);
        request.source = "relative.sql".into();
        assert!(request.validate(Driver::Postgres).is_err());
        assert!(request.validate(Driver::Oracle).is_err());
        assert!(request.validate(Driver::Sqlite).is_err());
    }

    #[test]
    fn mysqldump_database_switches_are_skipped() {
        assert!(switches_database(b"CREATE DATABASE /*!32312 IF NOT EXISTS*/ `shop` /*!40100 DEFAULT CHARACTER SET utf8mb4 */;\n"));
        assert!(switches_database(b"USE `shop`;\r\n"));
        assert!(switches_database(b"create database shop;\n"));
        assert!(!switches_database(b"INSERT INTO `t` VALUES ('USE `x`;');\n"));
        assert!(!switches_database(b"CREATE TABLE `users` (\n"));
    }

    #[test]
    fn pg_restore_verbose_lines_drive_progress_and_errors_stay_visible() {
        assert!(progress_line("pg_restore: creating TABLE \"public.items\""));
        assert!(progress_line("pg_restore: processing data for table \"public.items\""));
        assert!(progress_line("pg_restore: finished item 3340 TABLE DATA items"));
        assert!(!progress_line("pg_restore: error: could not execute query"));
        assert!(!diagnostic_line("pg_restore: connecting to database for restore"));
        assert!(diagnostic_line("pg_restore: error: could not execute query: ERROR:  relation \"items\" already exists"));
        assert!(diagnostic_line("Command was: CREATE TABLE public.items ();"));
        assert!(diagnostic_line("psql:<stdin>:12: ERROR:  syntax error at or near \"x\""));
    }

    #[test]
    fn sql_server_restore_moves_files_and_quotes_every_value() {
        let headers = ResultSet {
            columns: ["BackupName", "BackupType", "Position", "ServerName", "DatabaseName", "BackupStartDate"].map(String::from).to_vec(),
            rows: vec![
                ["full", "1", "1", "SQL01", "shop", "2026-10-01 10:00:00"].map(|s| Some(s.to_string())).to_vec(),
                ["log", "2", "2", "SQL01", "shop", "2026-10-01 11:00:00"].map(|s| Some(s.to_string())).to_vec(),
            ],
            ..Default::default()
        };
        let sets = parse_headers(&headers);
        assert_eq!(sets.len(), 2);
        assert_eq!((sets[0].position, sets[0].kind, sets[0].database.as_str()), (1, 1, "shop"));
        assert_eq!(sets[1].kind_label(), "Transaction log backup");
        let list = ResultSet {
            columns: ["LogicalName", "PhysicalName", "Type", "FileGroupName"].map(String::from).to_vec(),
            rows: vec![
                ["shop", "C:\\Data\\shop.mdf", "D", "PRIMARY"].map(|s| Some(s.to_string())).to_vec(),
                ["shop_log", "C:\\Data\\shop_log.ldf", "L", ""].map(|s| Some(s.to_string())).to_vec(),
                ["archive", "/var/opt/mssql/data/arch.ndf", "D", "ARCHIVE"].map(|s| Some(s.to_string())).to_vec(),
            ],
            ..Default::default()
        };
        let files = parse_files(&list);
        let moves = default_moves(&files, "shop", "shop'copy");
        assert_eq!(moves[0], ("shop".into(), "C:\\Data\\shop_copy.mdf".into()));
        assert_eq!(moves[1], ("shop_log".into(), "C:\\Data\\shop_copy_log.ldf".into()));
        assert_eq!(moves[2].1, "/var/opt/mssql/data/shop_copy_arch.ndf");
        assert_eq!(default_moves(&files, "shop", "shop")[0].1, "C:\\Data\\shop.mdf");
        let mut request = RestoreRequest::new("shop'copy]".into());
        request.source = "D:\\Backups\\shop's.bak".into();
        request.format = Some(RestoreFormat::MsSql);
        request.position = 1;
        request.moves = moves;
        request.replace = true;
        request.single_user = true;
        let sql = request.mssql_sql().unwrap();
        assert!(sql.starts_with("ALTER DATABASE [shop'copy]]] SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\nRESTORE DATABASE [shop'copy]]] FROM DISK = N'D:\\Backups\\shop''s.bak' WITH FILE = 1, MOVE N'shop' TO N'C:\\Data\\shop_copy.mdf'"), "{sql}");
        assert!(sql.ends_with("REPLACE, RECOVERY, STATS = 5"), "{sql}");
        assert_eq!(mssql_files_sql("x'.bak", 3), "RESTORE FILELISTONLY FROM DISK = N'x''.bak' WITH FILE = 3");
        request.position = 0;
        assert!(request.mssql_sql().is_err());
        assert!(RestoreRequest::new("x".into()).create_sql(Driver::Postgres).ends_with("TEMPLATE template0"));
    }

    #[tokio::test]
    async fn plain_scripts_are_previewed_without_a_connection_and_cancellation_runs_nothing() {
        let root = std::env::temp_dir().join(format!("dc-restore-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("shop.sql.gz");
        let mut gzip = flate2::write::GzEncoder::new(File::create(&source).unwrap(), flate2::Compression::default());
        gzip.write_all(b"-- MySQL dump 10.13\n\nCREATE TABLE t (id int);\n").unwrap();
        gzip.finish().unwrap();
        let cfg = ConnectionConfig::new(1, Driver::MySql);
        let mut request = RestoreRequest::new("shop".into());
        request.source = source.to_string_lossy().into_owned();
        let preview = inspect(cfg.clone(), request.clone()).await.unwrap();
        assert_eq!(preview.format, Some(RestoreFormat::SqlGz));
        assert_eq!(preview.entries, ["-- MySQL dump 10.13", "CREATE TABLE t (id int);"]);
        request.format = preview.format;
        request.utility = root.join("missing.exe").to_string_lossy().into_owned();
        let progress = Arc::new(RestoreProgress::default());
        progress.cancel.store(true, Ordering::Relaxed);
        assert!(run(cfg.clone(), request.clone(), progress).await.unwrap_err().to_string().contains("cancelled"));
        assert!(run(cfg, request, Arc::new(RestoreProgress::default())).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Backup → restore round trip on a real PostgreSQL with `pg_dump`, `pg_restore` and `psql`
    /// on PATH (DATACRAFT_PG_HOST, DATACRAFT_PG_PORT, DATACRAFT_PG_USER, DATACRAFT_PG_PASSWORD, DATACRAFT_PG_DB).
    #[tokio::test]
    #[ignore = "needs a PostgreSQL server and its client utilities"]
    async fn postgres_backup_restores_into_a_new_database() {
        let Ok(host) = std::env::var("DATACRAFT_PG_HOST") else {
            eprintln!("DATACRAFT_PG_HOST not set: skipping the PostgreSQL restore test");
            return;
        };
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.host = host;
        cfg.port = env("DATACRAFT_PG_PORT", "5432").parse().unwrap();
        cfg.user = env("DATACRAFT_PG_USER", "postgres");
        cfg.password = env("DATACRAFT_PG_PASSWORD", "");
        cfg.database = env("DATACRAFT_PG_DB", "postgres");
        let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let (source_db, root) = (format!("dc_restore_src_{suffix}"), std::env::temp_dir().join(format!("dc-restore-{suffix}")));
        std::fs::create_dir(&root).unwrap();
        let mut admin = Session::connect(&cfg).await.unwrap();
        admin.execute(&format!("CREATE DATABASE {source_db}"), 0).await.unwrap();
        let mut source_cfg = cfg.clone();
        source_cfg.database.clone_from(&source_db);
        let mut source = Session::connect(&source_cfg).await.unwrap();
        source.execute("CREATE TABLE items (id serial PRIMARY KEY, name text NOT NULL)", 0).await.unwrap();
        source.execute("INSERT INTO items (name) VALUES ('alpha'), ('beta'), ('gamma')", 0).await.unwrap();
        drop(source);
        let mut targets = Vec::new();
        for archive in [true, false] {
            let mut backup = crate::db::backup::BackupRequest::new(source_db.clone(), Driver::Postgres);
            backup.archive = archive;
            backup.compress = !archive;
            backup.destination = root.join(if archive { "shop.dump" } else { "shop.sql.gz" }).to_string_lossy().into_owned();
            crate::db::backup::run(cfg.clone(), backup.clone(), Arc::new(AtomicBool::new(false))).await.unwrap();
            let target = format!("dc_restore_{}_{suffix}", if archive { "archive" } else { "plain" });
            let mut request = RestoreRequest::new(target.clone());
            request.source = backup.destination.clone();
            request.maintenance.clone_from(&cfg.database);
            request.create = true;
            request.no_owner = true;
            let preview = inspect(cfg.clone(), request.clone()).await.unwrap();
            assert_eq!(preview.format, Some(if archive { RestoreFormat::Custom } else { RestoreFormat::SqlGz }));
            if archive {
                assert!(preview.entries.iter().any(|e| e.starts_with("TABLE DATA public items")), "{:?}", preview.entries);
                request.jobs = 2;
            }
            request.format = preview.format;
            request.utility = utility_path(Driver::Postgres, preview.format.unwrap());
            let progress = Arc::new(RestoreProgress::default());
            run(cfg.clone(), request, progress.clone()).await.unwrap_or_else(|e| panic!("{e:#}"));
            assert_eq!(progress.fraction(), Some(1.0));
            let mut target_cfg = cfg.clone();
            target_cfg.database.clone_from(&target);
            let mut restored = Session::connect(&target_cfg).await.unwrap();
            let rows = restored.execute("SELECT string_agg(name, ',' ORDER BY id) || ':' || nextval('items_id_seq') FROM items", 10).await.unwrap();
            assert_eq!(rows[0].rows[0][0].as_deref(), Some("alpha,beta,gamma:4"));
            drop(restored);
            if archive {
                // Into the now existing database: without Clean the objects collide, with Clean they are replaced.
                request = RestoreRequest::new(target.clone());
                request.source = backup.destination.clone();
                request.format = preview.format;
                request.utility = utility_path(Driver::Postgres, RestoreFormat::Custom);
                let error = run(cfg.clone(), request.clone(), Arc::new(RestoreProgress::default())).await.unwrap_err();
                // The server's own text follows its lc_messages; pg_restore's part is English.
                assert!(format!("{error:#}").contains("pg_restore: error:") && format!("{error:#}").contains("items"), "{error:#}");
                request.clean = true;
                run(cfg.clone(), request, Arc::new(RestoreProgress::default())).await.unwrap_or_else(|e| panic!("{e:#}"));
            }
            targets.push(target);
        }
        // A directory archive written by pg_dump -Fd, restored with parallel jobs.
        let folder = root.join("shop.dir");
        let status = Command::new(crate::db::backup::utility_path(Driver::Postgres))
            .env("PGPASSWORD", &cfg.password)
            .args(["-Fd", "-h", &cfg.host, "-p", &cfg.port.to_string(), "-U", &cfg.user, "-f"])
            .arg(&folder)
            .arg(&source_db)
            .status()
            .unwrap();
        assert!(status.success());
        let target = format!("dc_restore_dir_{suffix}");
        let mut request = RestoreRequest::new(target.clone());
        request.source = folder.to_string_lossy().into_owned();
        request.maintenance.clone_from(&cfg.database);
        request.create = true;
        request.jobs = 3;
        request.format = inspect(cfg.clone(), request.clone()).await.unwrap().format;
        assert_eq!(request.format, Some(RestoreFormat::Directory));
        request.utility = utility_path(Driver::Postgres, RestoreFormat::Directory);
        run(cfg.clone(), request, Arc::new(RestoreProgress::default())).await.unwrap_or_else(|e| panic!("{e:#}"));
        targets.push(target);
        // Cancelling a running restore stops pg_restore promptly.
        let mut source = Session::connect(&source_cfg).await.unwrap();
        source.execute("INSERT INTO items (name) SELECT repeat(md5(i::text), 8) FROM generate_series(1, 1500000) i", 0).await.unwrap();
        drop(source);
        let mut backup = crate::db::backup::BackupRequest::new(source_db.clone(), Driver::Postgres);
        backup.destination = root.join("large.dump").to_string_lossy().into_owned();
        crate::db::backup::run(cfg.clone(), backup.clone(), Arc::new(AtomicBool::new(false))).await.unwrap();
        let target = format!("dc_restore_cancel_{suffix}");
        let mut request = RestoreRequest::new(target.clone());
        request.source = backup.destination;
        request.maintenance.clone_from(&cfg.database);
        request.create = true;
        request.format = Some(RestoreFormat::Custom);
        request.utility = utility_path(Driver::Postgres, RestoreFormat::Custom);
        let progress = Arc::new(RestoreProgress::default());
        let cancel = progress.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            cancel.cancel.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let error = run(cfg.clone(), request, progress).await.unwrap_err();
        assert!(error.to_string().contains("cancelled") && started.elapsed() < Duration::from_secs(10), "{error:#} after {:?}", started.elapsed());
        targets.push(target);
        for db in targets.iter().chain([&source_db]) {
            admin.execute(&format!("DROP DATABASE {db} WITH (FORCE)"), 0).await.unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
