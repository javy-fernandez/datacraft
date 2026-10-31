//! Full-backup settings persisted with a schedule; every run gets a new destination.

use crate::db::{Driver, backup::BackupRequest};
use crate::i18n::tr;
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledBackup {
    pub folder: String,
    pub utility: String,
    pub compress: bool,
    pub archive: bool,
    pub directory: String,
    #[serde(default = "default_transaction")]
    pub transaction: bool,
    #[serde(default)]
    pub trust_cert: bool,
}

fn default_transaction() -> bool {
    true
}

impl ScheduledBackup {
    pub fn new(driver: Driver) -> Self {
        let request = BackupRequest::new(String::new(), driver);
        Self {
            folder: String::new(),
            utility: request.utility,
            compress: request.compress,
            archive: request.archive,
            directory: request.directory,
            transaction: request.transaction,
            trust_cert: request.trust_cert,
        }
    }

    /// Validate syntax only; remote server paths must not be checked on the GUI host.
    pub fn validate(&self, driver: Driver, database: &str) -> anyhow::Result<()> {
        if database.trim().is_empty() {
            bail!(tr("Choose a database for the backup task"));
        }
        if driver == Driver::Sqlite && database != "main" {
            bail!(tr("SQLite tasks can only use the main database"));
        }
        if BackupRequest::local(driver) && (!Path::new(&self.folder).is_absolute() || self.folder.chars().any(char::is_control)) {
            bail!(tr("Choose an absolute backup folder path"));
        }
        if driver == Driver::MsSql && (!absolute_server_folder(&self.folder) || self.folder.chars().any(char::is_control)) {
            bail!(tr("Choose an absolute folder path on the database server"));
        }
        self.with_name(driver, database, "backup").validate(driver)
    }

    pub fn request(&self, driver: Driver, database: &str) -> anyhow::Result<BackupRequest> {
        self.validate(driver, database)?;
        let prefix: String = database.chars().take(40).map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let name = format!("{prefix}_{}_{}", chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ"), uuid::Uuid::new_v4().simple());
        Ok(self.with_name(driver, database, &name))
    }

    fn with_name(&self, driver: Driver, database: &str, name: &str) -> BackupRequest {
        // Preserve the native defaults without discovering utilities on every validation frame.
        let mut request = BackupRequest {
            db: database.into(),
            destination: String::new(),
            directory: self.directory.clone(),
            utility: self.utility.clone(),
            mode: crate::db::backup::BackupMode::Full,
            compress: self.compress,
            archive: self.archive,
            include_objects: true,
            transaction: self.transaction,
            copy_only: true,
            checksum: true,
            export_log: true,
            trust_cert: self.trust_cert,
        };
        let extension = match driver {
            Driver::Sqlite => ".sqlite",
            Driver::Postgres if self.archive => ".dump",
            Driver::Postgres | Driver::MySql | Driver::MariaDb if self.compress => ".sql.gz",
            Driver::Postgres | Driver::MySql | Driver::MariaDb => ".sql",
            Driver::MsSql => ".bak",
            Driver::Oracle => ".dmp",
            _ => "",
        };
        let filename = format!("{name}{extension}");
        request.destination = if BackupRequest::local(driver) {
            Path::new(&self.folder).join(filename).to_string_lossy().into_owned()
        } else if driver == Driver::MsSql {
            let unix = self.folder.starts_with('/');
            let separator = if unix || self.folder.contains('/') { '/' } else { '\\' };
            let folder = if unix { self.folder.trim_end_matches('/') } else { self.folder.trim_end_matches(['/', '\\']) };
            format!("{folder}{separator}{filename}")
        } else {
            filename
        };
        request
    }
}

fn absolute_server_folder(folder: &str) -> bool {
    if folder.starts_with('/') {
        return true;
    }
    let bytes = folder.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\') {
        return true;
    }
    folder.strip_prefix(r"\\").is_some_and(|unc| {
        let mut parts = unc.split('\\');
        parts.next().is_some_and(|server| !server.is_empty() && server != "?" && server != ".") && parts.next().is_some_and(|share| !share.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_dump_consistency_and_certificate_options_reach_the_native_request() {
        let backup = ScheduledBackup::new(Driver::MySql);
        let mut json = serde_json::to_value(&backup).unwrap();
        json["folder"] = serde_json::json!(std::env::temp_dir().to_string_lossy());
        json["transaction"] = serde_json::json!(false);
        json["trust_cert"] = serde_json::json!(true);
        let decoded: ScheduledBackup = serde_json::from_value(json).unwrap();
        let request = decoded.request(Driver::MySql, "shop").unwrap();
        assert!(!request.transaction);
        assert!(request.trust_cert);
        let mut legacy = serde_json::to_value(&backup).unwrap();
        legacy["folder"] = serde_json::json!(std::env::temp_dir().to_string_lossy());
        legacy.as_object_mut().unwrap().remove("transaction");
        legacy.as_object_mut().unwrap().remove("trust_cert");
        let old: ScheduledBackup = serde_json::from_value(legacy).unwrap();
        assert!(old.transaction && !old.trust_cert);
    }

    #[test]
    fn generated_requests_are_unique_full_backups_with_safe_names() {
        let mut backup = ScheduledBackup::new(Driver::Sqlite);
        backup.folder = std::env::temp_dir().to_string_lossy().into_owned();
        let first = backup.request(Driver::Sqlite, "main").unwrap();
        let second = backup.request(Driver::Sqlite, "main").unwrap();
        assert_ne!(first.destination, second.destination);
        assert_eq!(first.mode, crate::db::backup::BackupMode::Full);
        assert_eq!(first.db, "main");
        assert!(first.copy_only && first.checksum && first.transaction && first.include_objects);
        assert!(first.destination.ends_with(".sqlite"));

        let mut postgres_backup = ScheduledBackup::new(Driver::Postgres);
        postgres_backup.folder.clone_from(&backup.folder);
        let postgres = postgres_backup.request(Driver::Postgres, "../../odd'/:db").unwrap();
        assert_eq!(Path::new(&postgres.destination).parent(), Some(Path::new(&backup.folder)));
        let file = Path::new(&postgres.destination).file_name().unwrap().to_string_lossy();
        assert!(file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')));
    }

    #[test]
    fn local_folder_validation_does_not_require_it_to_exist_yet() {
        let mut backup = ScheduledBackup::new(Driver::Sqlite);
        for folder in ["", "relative/backups", "bad\0folder"] {
            backup.folder = folder.into();
            assert!(backup.validate(Driver::Sqlite, "main").is_err(), "{folder:?}");
        }
        backup.folder = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string()).to_string_lossy().into_owned();
        assert!(backup.validate(Driver::Sqlite, "main").is_ok());
        assert!(backup.request(Driver::Sqlite, "main").is_ok());
        assert!(!Path::new(&backup.folder).exists());
        assert!(backup.validate(Driver::Sqlite, "attached").is_err());
        assert!(backup.validate(Driver::Postgres, "").is_err());
        assert!(backup.validate(Driver::ClickHouse, "shop").is_err());
    }

    #[test]
    fn sql_server_paths_follow_the_server_platform_instead_of_the_host() {
        let mut backup = ScheduledBackup::new(Driver::MsSql);
        for (folder, prefix) in [
            (r"C:\backups", r"C:\backups\"),
            ("D:/backups/", "D:/backups/"),
            (r"\\server\share\backups", r"\\server\share\backups\"),
            ("/var/opt/mssql/backups", "/var/opt/mssql/backups/"),
            (r"/backups\", r"/backups\/"),
            ("/", "/"),
        ] {
            backup.folder = folder.into();
            let request = backup.request(Driver::MsSql, "shop").unwrap();
            assert!(request.destination.starts_with(prefix), "{}", request.destination);
            assert!(request.destination.ends_with(".bak"));
        }
        for folder in ["relative", r"C:backups", r"\backups", r"\\server", r"\\server\"] {
            backup.folder = folder.into();
            assert!(backup.validate(Driver::MsSql, "shop").is_err(), "{folder}");
        }
    }

    #[test]
    fn oracle_and_snowflake_use_native_server_destinations() {
        let mut oracle = ScheduledBackup::new(Driver::Oracle);
        assert!(oracle.validate(Driver::Oracle, "XE").is_err());
        oracle.directory = "BACKUP_DIR".into();
        let request = oracle.request(Driver::Oracle, "XE").unwrap();
        assert_eq!(request.directory, "BACKUP_DIR");
        assert!(request.destination.ends_with(".dmp"));
        assert!(!request.destination.contains(['/', '\\']));
        assert!(request.sql(Driver::Oracle, "test").unwrap()[0].contains("reusefile => 0"));

        let snowflake = ScheduledBackup::new(Driver::Snowflake);
        let request = snowflake.request(Driver::Snowflake, "shop").unwrap();
        assert_eq!(request.directory, "PUBLIC");
        assert!(request.sql(Driver::Snowflake, "test").unwrap()[0].starts_with("CREATE BACKUP SET \"shop\".\"PUBLIC\"."));
    }

    #[test]
    fn native_dump_options_determine_the_file_extension() {
        for (driver, archive, compress, extension) in [
            (Driver::Postgres, true, true, ".dump"),
            (Driver::Postgres, false, false, ".sql"),
            (Driver::Postgres, false, true, ".sql.gz"),
            (Driver::MySql, false, false, ".sql"),
            (Driver::MariaDb, false, true, ".sql.gz"),
        ] {
            let mut backup = ScheduledBackup::new(driver);
            backup.folder = std::env::temp_dir().to_string_lossy().into_owned();
            backup.archive = archive;
            backup.compress = compress;
            assert!(backup.request(driver, "shop").unwrap().destination.ends_with(extension));
        }
    }
}
