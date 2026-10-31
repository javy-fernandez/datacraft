//! Connection persistence in `~/Library/Application Support/DataCraft/connections.json`.
//! Passwords and secret driver properties are stored in the macOS Keychain (Windows Credential
//! Manager), never in the JSON.

use crate::db::ConnectionConfig;
use std::path::PathBuf;

const KEYCHAIN_SERVICE: &str = "DataCraft";

pub(crate) fn data_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("DATACRAFT_TEST_DATA_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("DataCraft")
}

fn connections_file() -> PathBuf {
    data_dir().join("connections.json")
}

fn keychain_entry(id: u64) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("connection-{id}")).ok()
}

/// Secret driver properties of a connection, as a JSON object `{name: value}`.
fn properties_entry(id: u64) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("connection-{id}-properties")).ok()
}

/// The SSH tunnel's password and key passphrase, and the TLS client key's passphrase, as a JSON
/// object.
fn ssh_entry(id: u64) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("connection-{id}-ssh")).ok()
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SshSecrets {
    password: String,
    passphrase: String,
    tls_key_passphrase: String,
}

pub fn load_connections() -> Vec<ConnectionConfig> {
    let file = connections_file();
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Vec::new();
    };
    let mut list: Vec<ConnectionConfig> = match serde_json::from_str(&text) {
        Ok(list) => list,
        Err(e) => {
            // Don't lose the connections: set the unreadable file aside before it gets overwritten.
            let backup = file.with_extension("json.bak");
            let _ = std::fs::copy(&file, &backup);
            eprintln!("unreadable connections.json ({e}); copy at {}", backup.display());
            Vec::new()
        }
    };
    // Secrets an earlier build wrote to the file move to the keychain, and the file is rewritten.
    let mut leaked = false;
    for c in &mut list {
        c.migrate();
        if !c.secret_properties().is_empty() {
            leaked = true;
            let password = keychain_entry(c.id).and_then(|e| e.get_password().ok()).unwrap_or_default();
            store_secrets(&ConnectionConfig { password, ..c.clone() });
        }
        if let Some(pw) = keychain_entry(c.id).and_then(|e| e.get_password().ok()) {
            c.password = pw;
        }
        let secrets: std::collections::HashMap<String, String> = properties_entry(c.id)
            .and_then(|e| e.get_password().ok())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        fill_secret_properties(c, &secrets);
        if (c.ssh.enabled || !c.tls.key_file.trim().is_empty())
            && let Some(ssh) = ssh_entry(c.id).and_then(|e| e.get_password().ok()).and_then(|json| serde_json::from_str::<SshSecrets>(&json).ok())
        {
            (c.ssh.password, c.ssh.passphrase, c.tls.key_passphrase) = (ssh.password, ssh.passphrase, ssh.tls_key_passphrase);
        }
    }
    if leaked && let Err(e) = save_connections(&list) {
        eprintln!("could not rewrite connections.json without its secrets: {e:#}");
    }
    list
}

/// Puts the keychain's values back into their (blank) property rows.
fn fill_secret_properties(c: &mut ConnectionConfig, secrets: &std::collections::HashMap<String, String>) {
    for (k, v) in &mut c.properties {
        if let Some(secret) = secrets.get(k.trim()) {
            *v = secret.clone();
        }
    }
}

/// Keep this process-wide guard alive only in the separate scheduler process.
#[cfg(target_os = "macos")]
pub fn disable_background_keychain_interaction() -> anyhow::Result<security_framework::os::macos::keychain::KeychainUserInteractionLock> {
    use anyhow::Context;
    security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
        .context(crate::i18n::tr("Could not disable Keychain prompts for the background scheduler"))
}

fn read_background_secret(entry: Option<keyring::Entry>) -> anyhow::Result<Option<String>> {
    use anyhow::Context;
    match entry.context("open credential store entry")?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error).context(crate::i18n::tr("Could not read saved credentials. Open DataCraft and authorize access to this data source's credentials before retrying the task")),
    }
}

/// Loads only the requested data source for a background job without reading other
/// connections' credentials or rewriting their configuration.
pub fn load_background_connection(id: u64) -> anyhow::Result<ConnectionConfig> {
    use anyhow::Context;
    let text = std::fs::read_to_string(connections_file()).context("read saved data sources")?;
    let list: Vec<ConnectionConfig> = serde_json::from_str(&text).context("parse saved data sources")?;
    let mut cfg = list.into_iter().find(|cfg| cfg.id == id).context("The data source no longer exists")?;
    cfg.migrate();
    if !cfg.secret_properties().is_empty() {
        anyhow::bail!("Open DataCraft to migrate this data source's credentials before scheduling it");
    }
    if cfg.driver != crate::db::Driver::Sqlite && cfg.auth != crate::db::AuthMode::NoAuth
        && let Some(password) = read_background_secret(keychain_entry(id))?
    {
        cfg.password = password;
    }
    if cfg.properties.iter().any(|(name, _)| crate::db::is_secret_property(name))
        && let Some(json) = read_background_secret(properties_entry(id))?
    {
        let secrets = serde_json::from_str(&json).context("parse saved secret properties")?;
        fill_secret_properties(&mut cfg, &secrets);
    }
    if (cfg.ssh.enabled || !cfg.tls.key_file.trim().is_empty()) && let Some(json) = read_background_secret(ssh_entry(id))? {
        let secrets: SshSecrets = serde_json::from_str(&json).context("parse saved SSH credentials")?;
        (cfg.ssh.password, cfg.ssh.passphrase, cfg.tls.key_passphrase) = (secrets.password, secrets.passphrase, secrets.tls_key_passphrase);
    }
    Ok(cfg)
}

pub fn save_connections(list: &[ConnectionConfig]) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    // Atomic write: quitting halfway doesn't leave a truncated file.
    let file = connections_file();
    let tmp = file.with_extension("json.tmp");
    let list: Vec<ConnectionConfig> = list.iter().map(ConnectionConfig::without_secrets).collect();
    std::fs::write(&tmp, serde_json::to_string_pretty(&list)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Saves a connection's password and secret driver properties to the keychain (an empty one
/// removes its entry).
pub fn store_secrets(cfg: &ConnectionConfig) {
    store(keychain_entry(cfg.id), &cfg.password);
    let secrets: std::collections::HashMap<String, String> =
        cfg.secret_properties().into_iter().map(|(k, v)| (k.trim().to_string(), v)).collect();
    let json = if secrets.is_empty() { String::new() } else { serde_json::to_string(&secrets).unwrap_or_default() };
    store(properties_entry(cfg.id), &json);
    let ssh = SshSecrets {
        password: cfg.ssh.password.clone(),
        passphrase: cfg.ssh.passphrase.clone(),
        tls_key_passphrase: cfg.tls.key_passphrase.clone(),
    };
    let any = cfg.ssh.has_secrets() || !cfg.tls.key_passphrase.is_empty();
    let json = if any { serde_json::to_string(&ssh).unwrap_or_default() } else { String::new() };
    store(ssh_entry(cfg.id), &json);
}

fn store(entry: Option<keyring::Entry>, value: &str) {
    if let Some(entry) = entry {
        if value.is_empty() {
            let _ = entry.delete_credential();
        } else {
            let _ = entry.set_password(value);
        }
    }
}

/// Key of the deterministic copy masks, created on first use and kept in the keychain (anyone
/// with it could tell which real value a masked one came from, by trying candidates).
pub fn masking_key() -> String {
    if std::env::var("DATACRAFT_DEMO").is_ok_and(|v| !v.is_empty()) {
        return "datacraft-demo".into();
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, "masking-key").ok();
    if let Some(key) = entry.as_ref().and_then(|e| e.get_password().ok()).filter(|k| !k.is_empty()) {
        return key;
    }
    let key = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    store(entry, &key);
    key
}

pub fn delete_secrets(id: u64) {
    for entry in [keychain_entry(id), properties_entry(id), ssh_entry(id)].into_iter().flatten() {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;

    #[test]
    fn background_credential_reads_preserve_missing_values_and_access_errors() {
        let missing = keyring::Entry::new_with_credential(Box::<keyring::mock::MockCredential>::default());
        assert!(read_background_secret(Some(missing)).unwrap().is_none());

        let saved = keyring::Entry::new_with_credential(Box::<keyring::mock::MockCredential>::default());
        saved.set_password("test-password").unwrap();
        assert_eq!(read_background_secret(Some(saved)).unwrap().as_deref(), Some("test-password"));

        let denied = keyring::mock::MockCredential::default();
        denied.set_error(keyring::Error::PlatformFailure(Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "test access denied",
        ))));
        let entry = keyring::Entry::new_with_credential(Box::new(denied));
        let error = read_background_secret(Some(entry)).unwrap_err();
        assert!(matches!(error.downcast_ref::<keyring::Error>(), Some(keyring::Error::PlatformFailure(_))));
        assert!(error.to_string().contains("DataCraft"));
        assert!(format!("{error:#}").contains("test access denied"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn background_keychain_interaction_is_process_local() {
        use security_framework::os::macos::keychain::SecKeychain;
        use std::io::{BufRead, Write};
        use std::process::Stdio;
        const PROBE: &str = "DATACRAFT_TEST_KEYCHAIN_INTERACTION";
        if std::env::var_os(PROBE).is_some() {
            assert!(SecKeychain::user_interaction_allowed().unwrap());
            let guard = disable_background_keychain_interaction().unwrap();
            assert!(!SecKeychain::user_interaction_allowed().unwrap());
            assert!(!std::thread::spawn(SecKeychain::user_interaction_allowed).join().unwrap().unwrap());
            println!("keychain-interaction-disabled");
            std::io::stdout().flush().unwrap();
            std::io::stdin().read_line(&mut String::new()).unwrap();
            drop(guard);
            assert!(SecKeychain::user_interaction_allowed().unwrap());
            return;
        }
        // Inspect interaction metadata only; never read or modify the user's credentials.
        let before = SecKeychain::user_interaction_allowed().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "config::tests::background_keychain_interaction_is_process_local", "--nocapture"])
            .env(PROBE, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let ready = (&mut output).lines().any(|line| line.unwrap().ends_with("keychain-interaction-disabled"));
        let during = SecKeychain::user_interaction_allowed().unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        assert!(ready);
        assert_eq!(during, before);
        assert_eq!(SecKeychain::user_interaction_allowed().unwrap(), before);
    }

    #[test]
    fn secret_properties_never_reach_the_file() {
        let mut cfg = ConnectionConfig::new(1, Driver::Snowflake);
        cfg.password = "PASSWORD-VALUE".into();
        cfg.set_property("private_key_file", "/k.p8");
        cfg.set_property("private_key_file_pwd", "PASSPHRASE-VALUE");
        cfg.set_property("token", "TOKEN-VALUE");
        let json = serde_json::to_string(&cfg.without_secrets()).unwrap();
        for secret in ["PASSPHRASE-VALUE", "TOKEN-VALUE", "PASSWORD-VALUE"] {
            assert!(!json.contains(secret), "{secret} written to the file: {json}");
        }
        assert!(json.contains("/k.p8"));

        // Loading puts the keychain's values back in their rows.
        let mut loaded: ConnectionConfig = serde_json::from_str(&json).unwrap();
        let secrets = cfg.secret_properties().into_iter().collect();
        fill_secret_properties(&mut loaded, &secrets);
        assert_eq!(loaded.properties, cfg.properties);
    }

    #[test]
    fn ssh_secrets_never_reach_the_file() {
        let mut cfg = ConnectionConfig::new(2, Driver::Postgres);
        cfg.ssh = crate::db::ssh::SshSettings {
            enabled: true,
            host: "bastion.example".into(),
            user: "deploy".into(),
            key_file: "~/.ssh/id_ed25519".into(),
            password: "SSH-PASSWORD".into(),
            passphrase: "SSH-PASSPHRASE".into(),
            ..Default::default()
        };
        cfg.tls.key_file = "~/certs/client.key".into();
        cfg.tls.key_passphrase = "TLS-PASSPHRASE".into();
        let json = serde_json::to_string(&cfg.without_secrets()).unwrap();
        assert!(!json.contains("SSH-PASSWORD") && !json.contains("SSH-PASSPHRASE") && !json.contains("TLS-PASSPHRASE"), "{json}");
        let loaded: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!((loaded.ssh.host.as_str(), loaded.ssh.key_file.as_str()), ("bastion.example", "~/.ssh/id_ed25519"));
        // Files from before the SSH tab load without one.
        let old: ConnectionConfig = serde_json::from_str(r#"{"id": 3, "driver": "Postgres", "host": "db"}"#).unwrap();
        assert!(!old.ssh.enabled);
    }
}
