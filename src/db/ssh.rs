//! SSH tunnels: a data source can reach its database through a bastion host.
//!
//! `Session::connect` asks for the data source's tunnel (`shared`): a listener on `127.0.0.1`
//! whose connections go, through the bastion (`direct-tcpip`), to the database's host and port as
//! the bastion sees them. The driver connects to the listener as it would to the database, and
//! the sessions of a data source share it while any of them is open. When the SSH connection
//! drops, the next connection that comes in opens it again.
//!
//! Server keys are trusted when `~/.ssh/known_hosts` or DataCraft's own `ssh_known_hosts` lists
//! them. An unknown key is refused and kept (`unknown_key`), so the data source dialog can show
//! its fingerprint and record it once the user trusts it (`trust`). A key different from the one
//! recorded is always refused, naming both fingerprints.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail};
use russh::client;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};

use super::{ConnectionConfig, Driver};
use crate::i18n::{tr, trf};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A keepalive every 30 s; three unanswered ones close the connection.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// How the tunnel signs in to the bastion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum SshAuth {
    #[default]
    Password,
    /// A private key file (OpenSSH or PEM), with its passphrase if it has one.
    KeyPair,
    /// The running SSH agent (`SSH_AUTH_SOCK`; on Windows, OpenSSH's agent or Pageant).
    Agent,
}

/// The SSH tab of a data source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SshSettings {
    pub enabled: bool,
    pub host: String,
    /// 0 = 22.
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub key_file: String,
    /// In the keychain, like the database password.
    #[serde(skip)]
    pub password: String,
    #[serde(skip)]
    pub passphrase: String,
}

impl SshSettings {
    pub fn port(&self) -> u16 {
        if self.port == 0 { 22 } else { self.port }
    }

    fn host(&self) -> &str {
        self.host.trim()
    }

    /// "user@bastion" (with the port when it isn't 22), for messages.
    pub fn describe(&self) -> String {
        let at = if self.user.trim().is_empty() {
            String::new()
        } else {
            format!("{}@", self.user.trim())
        };
        match self.port() {
            22 => format!("{at}{}", self.host()),
            port => format!("{at}{}:{port}", self.host()),
        }
    }

    /// The secrets, for the keychain.
    pub fn has_secrets(&self) -> bool {
        !self.password.is_empty() || !self.passphrase.is_empty()
    }
}

/// true when `cfg` connects through its SSH tunnel.
pub fn tunnels(cfg: &ConnectionConfig) -> bool {
    cfg.ssh.enabled && cfg.driver.supports_ssh()
}

/// A listener on `127.0.0.1:port` forwarding to the database through the bastion. Dropping it
/// stops accepting; connections already forwarded go on until they close.
pub struct Tunnel {
    pub port: u16,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

type Handle = client::Handle<Client>;

/// Open tunnels, by settings and destination, while a session holds them.
static TUNNELS: LazyLock<Mutex<HashMap<u64, Weak<Tunnel>>>> = LazyLock::new(Default::default);

/// The database's address as the bastion reaches it.
fn destination(cfg: &ConnectionConfig) -> anyhow::Result<(String, u16)> {
    if cfg.driver == Driver::MongoDb
        && super::mongo::connection_string(cfg).starts_with("mongodb+srv://")
    {
        bail!(tr(
            "A mongodb+srv address can't go through an SSH tunnel: use the host and port of a server"
        ));
    }
    let port = if cfg.port == 0 {
        cfg.driver.default_port()
    } else {
        cfg.port
    };
    if cfg.driver == Driver::Prometheus {
        return Ok(host_port(&cfg.host, port));
    }
    Ok((bare_host(&cfg.host).to_string(), port))
}

/// The host and port of a Prometheus API's host field, which takes a whole URL
/// (`http://vmselect:8481/select/0/prometheus`): a port in it wins over `port`, the port
/// field's, as in `prometheus::base_url`.
pub fn host_port(host: &str, port: u16) -> (String, u16) {
    let authority = bare_host(host);
    // A bare IPv6 address has colons and no port.
    let bare_ipv6 = !authority.starts_with('[') && authority.matches(':').count() > 1;
    super::url::parse_authority(authority, port)
        .filter(|_| !bare_ipv6)
        .unwrap_or_else(|| (authority.to_string(), port))
}

/// The host name of the host field, without a scheme or path (`https://influx.example/` →
/// `influx.example`); `localhost` when empty.
pub fn bare_host(host: &str) -> &str {
    let host = host.trim();
    let host = host.split_once("://").map_or(host, |(_, rest)| rest);
    match host.split('/').next().unwrap_or_default() {
        "" => "localhost",
        h => h,
    }
}

/// The host field pointed at the tunnel's listener, `127.0.0.1`, keep a scheme and a path
/// typed in it (InfluxDB's `https://...`, a Prometheus API's `/select/0/prometheus`).
pub fn local_host(host: &str) -> String {
    let host = host.trim();
    let (scheme, rest) = match host.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), host),
    };
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    format!("{scheme}127.0.0.1{path}")
}

/// The data source's tunnel, opened on first use and shared by its sessions.
pub async fn shared(cfg: &ConnectionConfig) -> anyhow::Result<Arc<Tunnel>> {
    let to = destination(cfg)?;
    let mut hasher = DefaultHasher::new();
    (&cfg.ssh, &to).hash(&mut hasher);
    let key = hasher.finish();
    if let Some(t) = TUNNELS.lock().unwrap().get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    let tunnel = Arc::new(open(&cfg.ssh, to).await?);
    let mut tunnels = TUNNELS.lock().unwrap();
    tunnels.retain(|_, t| t.strong_count() > 0);
    // Another session may have opened one meanwhile: keep that one.
    if let Some(t) = tunnels.get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    tunnels.insert(key, Arc::downgrade(&tunnel));
    Ok(tunnel)
}

/// A tunnel of its own (Test Connection checks every hop again).
pub async fn private(cfg: &ConnectionConfig) -> anyhow::Result<Tunnel> {
    open(&cfg.ssh, destination(cfg)?).await
}

async fn open(settings: &SshSettings, to: (String, u16)) -> anyhow::Result<Tunnel> {
    // Connecting first reports a wrong host, key or password now, not at the first query.
    let ssh = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        connect(settings).await?,
    ))));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let settings = settings.clone();
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (ssh, settings, to) = (ssh.clone(), settings.clone(), to.clone());
            tokio::spawn(async move {
                if let Err(e) = forward(stream, port, &ssh, &settings, &to).await {
                    eprintln!("SSH tunnel to {}:{}: {e:#}", to.0, to.1);
                }
            });
        }
    });
    Ok(Tunnel { port, accept })
}

/// Copies one local connection to the database through a `direct-tcpip` channel, opening the
/// SSH connection again if it dropped.
async fn forward(
    mut stream: TcpStream,
    local_port: u16,
    ssh: &tokio::sync::Mutex<Option<Arc<Handle>>>,
    settings: &SshSettings,
    to: &(String, u16),
) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let mut channel = None;
    for attempt in 0..2 {
        let handle = {
            let mut current = ssh.lock().await;
            match current.as_ref() {
                Some(h) if attempt == 0 && !h.is_closed() => h.clone(),
                _ => {
                    let h = Arc::new(connect(settings).await?);
                    *current = Some(h.clone());
                    h
                }
            }
        };
        match handle
            .channel_open_direct_tcpip(
                to.0.as_str(),
                u32::from(to.1),
                "127.0.0.1",
                u32::from(local_port),
            )
            .await
        {
            Ok(c) => {
                channel = Some(c);
                break;
            }
            Err(e) if attempt == 1 || !handle.is_closed() => {
                bail!(trf(
                    "The SSH server could not reach {host}:{port}: {error}",
                    &[("host", &to.0), ("port", &to.1), ("error", &e)]
                ))
            }
            Err(_) => {}
        }
    }
    let mut remote = channel
        .ok_or_else(|| anyhow!(tr("The SSH connection was closed")))?
        .into_stream();
    tokio::io::copy_bidirectional(&mut stream, &mut remote).await?;
    Ok(())
}

async fn connect(s: &SshSettings) -> anyhow::Result<Handle> {
    let (host, port) = (s.host(), s.port());
    if host.is_empty() {
        bail!(tr("The SSH host is required"));
    }
    if s.user.trim().is_empty() {
        bail!(tr("The SSH user is required"));
    }
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let refused = Arc::new(Mutex::new(None));
    let handler = Client {
        host: host.to_string(),
        port,
        refused: refused.clone(),
    };
    let mut handle = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config, (host, port), handler),
    )
    .await
    {
        Err(_) => bail!(trf(
            "Timed out connecting to the SSH server {host}:{port}",
            &[("host", &host), ("port", &port)]
        )),
        Ok(Err(e)) => {
            if let Some(reason) = refused.lock().unwrap().take() {
                return Err(reason);
            }
            bail!(trf(
                "Could not connect to the SSH server {host}:{port}: {error}",
                &[("host", &host), ("port", &port), ("error", &e)]
            ))
        }
        Ok(Ok(h)) => h,
    };
    let signed_in = tokio::time::timeout(CONNECT_TIMEOUT, authenticate(&mut handle, s))
        .await
        .map_err(|_| {
            anyhow!(trf(
                "Timed out signing in to the SSH server {host}",
                &[("host", &host)]
            ))
        })??;
    if !signed_in {
        bail!(trf(
            "The SSH server {host} refused to sign in {user}",
            &[("host", &host), ("user", &s.user.trim())]
        ));
    }
    Ok(handle)
}

/// true once the bastion accepted the user.
async fn authenticate(handle: &mut Handle, s: &SshSettings) -> anyhow::Result<bool> {
    let user = s.user.trim();
    match s.auth {
        SshAuth::Password => Ok(handle
            .authenticate_password(user, s.password.as_str())
            .await?
            .success()),
        SshAuth::KeyPair => {
            let file = key_path(&s.key_file);
            if s.key_file.trim().is_empty() {
                bail!(tr("Choose the SSH private key file"));
            }
            let passphrase = (!s.passphrase.is_empty()).then_some(s.passphrase.as_str());
            let key = russh::keys::load_secret_key(&file, passphrase).map_err(|e| match e {
                russh::keys::Error::KeyIsEncrypted => anyhow!(tr(
                    "The SSH private key is protected by a passphrase: enter it"
                )),
                russh::keys::Error::IO(io) => {
                    anyhow!(trf(
                        "Could not read the SSH private key {file}: {error}",
                        &[("file", &file.display()), ("error", &io)]
                    ))
                }
                other if passphrase.is_some() => anyhow!(trf(
                    "Could not open the SSH private key: {error} (is the passphrase right?)",
                    &[("error", &other)]
                )),
                other => anyhow!(trf(
                    "Could not open the SSH private key: {error}",
                    &[("error", &other)]
                )),
            })?;
            let hash = handle.best_supported_rsa_hash().await?.flatten();
            Ok(handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
                .success())
        }
        SshAuth::Agent => agent_sign_in(handle, user).await,
    }
}

/// `~/…` is the home folder, as in OpenSSH.
fn key_path(file: &str) -> PathBuf {
    let file = file.trim();
    match (
        file.strip_prefix("~/").or_else(|| file.strip_prefix("~\\")),
        dirs::home_dir(),
    ) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(file),
    }
}

#[cfg(unix)]
async fn agent_sign_in(handle: &mut Handle, user: &str) -> anyhow::Result<bool> {
    let agent = AgentClient::connect_env()
        .await
        .map_err(|_| anyhow!(tr("No SSH agent is running (SSH_AUTH_SOCK is not set)")))?;
    sign_in_with_agent(handle, user, agent).await
}

#[cfg(windows)]
async fn agent_sign_in(handle: &mut Handle, user: &str) -> anyhow::Result<bool> {
    // OpenSSH's agent service first, then Pageant.
    if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        return sign_in_with_agent(handle, user, agent).await;
    }
    let agent = AgentClient::connect_pageant().await.map_err(|_| {
        anyhow!(tr(
            "No SSH agent is running (OpenSSH Authentication Agent or Pageant)"
        ))
    })?;
    sign_in_with_agent(handle, user, agent).await
}

/// Tries the agent's keys in turn.
async fn sign_in_with_agent<R: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    handle: &mut Handle,
    user: &str,
    mut agent: AgentClient<R>,
) -> anyhow::Result<bool> {
    let identities = agent.request_identities().await?;
    if identities.is_empty() {
        bail!(tr("The SSH agent has no keys: add one with ssh-add"));
    }
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    for identity in identities {
        let key = identity.public_key().into_owned();
        if handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .is_ok_and(|r| r.success())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Server keys
// ---------------------------------------------------------------------------

struct Client {
    host: String,
    port: u16,
    /// Why the server's key was refused, for `connect`'s error.
    refused: Arc<Mutex<Option<anyhow::Error>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        match check_host_key(&self.host, self.port, &key, &known_hosts_files()) {
            Ok(()) => Ok(true),
            Err(reason) => {
                *self.refused.lock().unwrap() = Some(reason);
                Ok(false)
            }
        }
    }
}

/// Server keys seen but not trusted yet, by host and port.
static UNKNOWN: LazyLock<Mutex<HashMap<(String, u16), PublicKey>>> =
    LazyLock::new(Default::default);

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// OpenSSH's known hosts, then DataCraft's own (where `trust` records keys).
fn known_hosts_files() -> Vec<PathBuf> {
    #[cfg(test)]
    if let Some(file) = tests::KNOWN_HOSTS.lock().unwrap().clone() {
        return vec![file];
    }
    let mut files: Vec<PathBuf> = dirs::home_dir()
        .map(|h| h.join(".ssh").join("known_hosts"))
        .into_iter()
        .collect();
    files.push(datacraft_known_hosts());
    files
}

fn datacraft_known_hosts() -> PathBuf {
    #[cfg(test)]
    if let Some(file) = tests::KNOWN_HOSTS.lock().unwrap().clone() {
        return file;
    }
    crate::config::data_dir().join("ssh_known_hosts")
}

/// Ok when a file lists the key for this host. A different key of the same type is refused; an
/// unknown one is kept for `unknown_key`.
fn check_host_key(host: &str, port: u16, key: &PublicKey, files: &[PathBuf]) -> anyhow::Result<()> {
    for file in files {
        // A file that can't be read (missing, or with a key type it doesn't know) is skipped.
        let Ok(known) = russh::keys::known_hosts::known_host_keys_path(host, port, file) else {
            continue;
        };
        if known.iter().any(|(_, k)| k.key_data() == key.key_data()) {
            return Ok(());
        }
        if let Some((line, old)) = known.iter().find(|(_, k)| k.algorithm() == key.algorithm()) {
            bail!(trf(
                "The SSH server {host} has a different key than before: {old} was trusted, now it is {new}. If the server was reinstalled, remove line {line} of {file}",
                &[
                    ("host", &host),
                    ("old", &fingerprint(old)),
                    ("new", &fingerprint(key)),
                    ("line", &line),
                    ("file", &file.display())
                ]
            ));
        }
    }
    UNKNOWN
        .lock()
        .unwrap()
        .insert((host.to_string(), port), key.clone());
    bail!(trf(
        "The SSH server {host} is not trusted yet. Its key is {fingerprint}: check it, then choose Trust in the data source's SSH tab",
        &[("host", &host), ("fingerprint", &fingerprint(key))]
    ))
}

/// The fingerprint of the key the bastion showed and that isn't trusted yet.
pub fn unknown_key(s: &SshSettings) -> Option<String> {
    UNKNOWN
        .lock()
        .unwrap()
        .get(&(s.host().to_string(), s.port()))
        .map(fingerprint)
}

/// Trusts the key the bastion showed: it goes to DataCraft's `ssh_known_hosts`.
pub fn trust(s: &SshSettings) -> anyhow::Result<()> {
    let key = UNKNOWN
        .lock()
        .unwrap()
        .remove(&(s.host().to_string(), s.port()))
        .ok_or_else(|| anyhow!(tr("Test the connection first")))?;
    russh::keys::known_hosts::learn_known_hosts_path(
        s.host(),
        s.port(),
        &key,
        datacraft_known_hosts(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::Channel;
    use russh::keys::PrivateKey;
    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session as ServerSession};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The known hosts file of the end-to-end test, instead of the user's.
    pub(super) static KNOWN_HOSTS: Mutex<Option<PathBuf>> = Mutex::new(None);

    fn key(line: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(line.split(' ').nth(1).unwrap()).unwrap()
    }

    const ED_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFYt3+wLax7smKipHBpACNp0QkTx5I3tOdNsz1XbCUuf";
    const ED_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILw6T0013JNbmIcLlRrqUZ29R+h373hBqwizbG8QIHqI";

    #[test]
    fn local_host_keeps_scheme_and_path() {
        use super::local_host;
        assert_eq!(local_host("db.example.com"), "127.0.0.1");
        assert_eq!(
            local_host(" https://influx.example/ "),
            "https://127.0.0.1/"
        );
        assert_eq!(
            local_host("http://vmselect:8481/select/0/prometheus"),
            "http://127.0.0.1/select/0/prometheus"
        );
        assert_eq!(
            local_host("vmselect/select/0/prometheus"),
            "127.0.0.1/select/0/prometheus"
        );
    }

    #[test]
    fn known_unknown_and_changed_keys() {
        let dir = std::env::temp_dir().join(format!("datacraft-ssh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let openssh = dir.join("known_hosts");
        let ours = dir.join("ssh_known_hosts");
        std::fs::write(
            &openssh,
            format!("bastion.example {ED_A}\n[other.example]:2222 {ED_A}\n"),
        )
        .unwrap();
        let files = [openssh.clone(), dir.join("missing"), ours.clone()];

        assert!(check_host_key("bastion.example", 22, &key(ED_A), &files).is_ok());
        assert!(check_host_key("other.example", 2222, &key(ED_A), &files).is_ok());

        // Same type, another key: refused with both fingerprints and where the old one is.
        let err = check_host_key("bastion.example", 22, &key(ED_B), &files)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&fingerprint(&key(ED_A))) && err.contains(&fingerprint(&key(ED_B))),
            "{err}"
        );
        assert!(err.contains("line 1"), "{err}");

        // Unknown (also on another port): refused and kept until it is trusted.
        let s = SshSettings {
            host: "new.example".into(),
            port: 2200,
            ..Default::default()
        };
        let err = check_host_key("new.example", 2200, &key(ED_B), &files)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&fingerprint(&key(ED_B))), "{err}");
        assert_eq!(unknown_key(&s), Some(fingerprint(&key(ED_B))));
        let pending = UNKNOWN
            .lock()
            .unwrap()
            .remove(&("new.example".to_string(), 2200))
            .unwrap();
        russh::keys::known_hosts::learn_known_hosts_path("new.example", 2200, &pending, &ours)
            .unwrap();
        assert!(check_host_key("new.example", 2200, &key(ED_B), &files).is_ok());
        assert_eq!(unknown_key(&s), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprints_are_openssh_sha256() {
        let fp = fingerprint(&key(ED_A));
        assert!(fp.starts_with("SHA256:") && !fp.ends_with('='), "{fp}");
    }

    #[test]
    fn key_paths_and_descriptions() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                key_path("~/.ssh/id_ed25519"),
                home.join(".ssh").join("id_ed25519")
            );
        }
        assert_eq!(key_path(" /keys/id "), PathBuf::from("/keys/id"));
        let mut s = SshSettings {
            host: " bastion ".into(),
            user: "deploy".into(),
            ..Default::default()
        };
        assert_eq!(s.describe(), "deploy@bastion");
        s.port = 2222;
        assert_eq!(s.describe(), "deploy@bastion:2222");
    }

    #[test]
    fn engines_and_destinations() {
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.ssh.enabled = true;
        assert!(tunnels(&cfg));
        cfg.host = "  ".into();
        cfg.port = 0;
        assert_eq!(destination(&cfg).unwrap(), ("localhost".to_string(), 5432));
        for d in [Driver::Sqlite, Driver::Snowflake, Driver::Kusto] {
            cfg.driver = d;
            assert!(!tunnels(&cfg), "{d:?}");
        }
        cfg.driver = Driver::InfluxDb;
        cfg.host = "https://influx.example/".into();
        cfg.port = 8086;
        assert_eq!(
            destination(&cfg).unwrap(),
            ("influx.example".to_string(), 8086)
        );
        let mut mongo = ConnectionConfig::new(2, Driver::MongoDb);
        mongo.host = "cluster0.abcde.mongodb.net".into();
        assert!(destination(&mongo).is_err());
    }

    /// A Prometheus API's host field takes a URL with a port: the tunnel goes to that port, and
    /// the name kept for TLS has none.
    #[tokio::test]
    async fn prometheus_ports_in_the_host_field() {
        let mut cfg = ConnectionConfig::new(1, Driver::Prometheus);
        cfg.host = "http://vmselect:8481/select/0/prometheus".into();
        assert_eq!(destination(&cfg).unwrap(), ("vmselect".to_string(), 8481));
        let tunnel = Tunnel {
            port: 40123,
            accept: tokio::spawn(async {}),
        };
        let through = super::super::through(&cfg, Some(&tunnel));
        assert_eq!(through.tunnel_host.as_deref(), Some("vmselect"));
        assert_eq!(
            super::super::prometheus::base_url(&through),
            "http://127.0.0.1:40123/select/0/prometheus"
        );

        // Without one, the port field's (9090 when it is empty).
        for (host, port, to) in [
            ("vmselect", 8481, ("vmselect", 8481)),
            (
                "https://prom.example.com/prefix",
                0,
                ("prom.example.com", 9090),
            ),
            ("fd00::12", 9090, ("fd00::12", 9090)),
            ("[fd00::12]:8428", 9090, ("[fd00::12]", 8428)),
            // Not a port: left as typed, for the SSH server to refuse.
            ("vmselect:99999", 9090, ("vmselect:99999", 9090)),
        ] {
            (cfg.host, cfg.port) = (host.into(), port);
            assert_eq!(
                destination(&cfg).unwrap(),
                (to.0.to_string(), to.1),
                "{host}"
            );
        }

        // The other engines keep the host's name, as before.
        cfg.driver = Driver::InfluxDb;
        (cfg.host, cfg.port) = ("https://influx.example/".into(), 8086);
        let through = super::super::through(&cfg, Some(&tunnel));
        assert_eq!(through.tunnel_host.as_deref(), Some("influx.example"));
    }

    fn ed25519(seed: u8) -> PrivateKey {
        PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
    }

    /// A bastion in the test: accepts "deploy" with password "secret" or the client key, and
    /// forwards `direct-tcpip` channels to whatever it is asked for.
    #[derive(Clone)]
    struct Bastion {
        client_key: PublicKey,
    }

    impl server::Handler for Bastion {
        type Error = russh::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if user == "deploy" && password == "secret" {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: server::VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            Ok(
                if user == "deploy" && key.public_key.key_data() == self.client_key.key_data() {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: Channel<Msg>,
            host: &str,
            port: u32,
            _: &str,
            _: u32,
            reply: ChannelOpenHandle,
            _: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            let Ok(mut tcp) = TcpStream::connect((host, port as u16)).await else {
                return Ok(());
            };
            reply.accept().await;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
            });
            Ok(())
        }
    }

    /// The bastion's port, serving until the runtime ends.
    async fn bastion(host_key: PrivateKey, client_key: PublicKey) -> u16 {
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::ZERO,
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let handler = Bastion {
                    client_key: client_key.clone(),
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        port
    }

    /// The "database": echoes what it receives.
    async fn echo() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = socket.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        port
    }

    async fn round_trip(port: u16, text: &[u8]) -> Vec<u8> {
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tcp.write_all(text).await.unwrap();
        let mut buf = vec![0; text.len()];
        tcp.read_exact(&mut buf).await.unwrap();
        buf
    }

    /// Everything in this process on 127.0.0.1: no SSH server of the system is used.
    #[tokio::test]
    async fn tunnel_through_a_bastion() {
        let dir = std::env::temp_dir().join(format!("datacraft-ssh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        *KNOWN_HOSTS.lock().unwrap() = Some(dir.join("known_hosts"));
        let (host_key, client_key) = (ed25519(7), ed25519(9));
        let host_fingerprint = fingerprint(host_key.public_key());
        let ssh_port = bastion(host_key, client_key.public_key().clone()).await;
        let db_port = echo().await;

        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.host = "127.0.0.1".into();
        cfg.port = db_port;
        cfg.ssh = SshSettings {
            enabled: true,
            host: "127.0.0.1".into(),
            port: ssh_port,
            user: "deploy".into(),
            password: "secret".into(),
            ..Default::default()
        };

        // First the bastion's key is unknown: refused, and offered with its fingerprint.
        let err = private(&cfg).await.err().expect("unknown key").to_string();
        assert!(err.contains(&host_fingerprint), "{err}");
        assert_eq!(unknown_key(&cfg.ssh), Some(host_fingerprint.clone()));
        trust(&cfg.ssh).unwrap();
        assert_eq!(unknown_key(&cfg.ssh), None);

        // Trusted: the tunnel carries bytes both ways, for several connections.
        let tunnel = private(&cfg).await.unwrap();
        assert_eq!(round_trip(tunnel.port, b"SELECT 1").await, b"SELECT 1");
        assert_eq!(round_trip(tunnel.port, b"again").await, b"again");
        drop(tunnel);

        // Shared while a session holds it; a new one after the last is dropped.
        let a = shared(&cfg).await.unwrap();
        let b = shared(&cfg).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let port = a.port;
        drop((a, b));
        let c = shared(&cfg).await.unwrap();
        assert_ne!(c.port, port);
        assert_eq!(round_trip(c.port, b"x").await, b"x");
        drop(c);

        // A key file.
        let key_file = dir.join("id_ed25519");
        std::fs::write(
            &key_file,
            client_key
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let mut by_key = cfg.clone();
        by_key.ssh.auth = SshAuth::KeyPair;
        by_key.ssh.key_file = key_file.to_string_lossy().into_owned();
        by_key.ssh.password.clear();
        let tunnel = private(&by_key).await.unwrap();
        assert_eq!(round_trip(tunnel.port, b"key").await, b"key");
        by_key.ssh.key_file = dir.join("missing").to_string_lossy().into_owned();
        let err = private(&by_key).await.err().unwrap().to_string();
        assert!(err.contains("missing"), "{err}");

        // A wrong password, and a database the bastion can't reach.
        let mut wrong = cfg.clone();
        wrong.ssh.password = "nope".into();
        let err = private(&wrong).await.err().unwrap().to_string();
        assert!(err.contains("refused") && err.contains("deploy"), "{err}");
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut unreachable = cfg.clone();
        unreachable.port = closed;
        let tunnel = private(&unreachable).await.unwrap();
        let mut tcp = TcpStream::connect(("127.0.0.1", tunnel.port))
            .await
            .unwrap();
        let mut buf = [0; 1];
        assert_eq!(
            tcp.read(&mut buf).await.unwrap_or(0),
            0,
            "closed when the bastion can't reach it"
        );

        *KNOWN_HOSTS.lock().unwrap() = None;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
