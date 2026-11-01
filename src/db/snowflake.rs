//! Snowflake driver: the REST protocol of the official JDBC driver (snowflake-jdbc 4.x),
//! reimplemented on top of `reqwest`.
//!
//! - Login: `POST /session/v1/login-request` with the account, user and either the password or
//!   a key-pair JWT (`AUTHENTICATOR = SNOWFLAKE_JWT`, from the `private_key_file` driver property
//!   as DataGrip passes it to the JDBC driver), or an OAuth / programmatic access token.
//!   It returns a session token (sent as `Authorization: Snowflake Token="…"`) and a master
//!   token that renews it (`/session/token-request`) when the server answers 390112.
//! - Queries: `POST /queries/v1/query-request`. Results are asked for as JSON
//!   (`JDBC_QUERY_RESULT_FORMAT`, what the JDBC driver does with Arrow disabled): the first rows
//!   come inline and the rest in chunks downloaded from cloud storage, fetched only up to
//!   `max_rows`. A query still running (333333 / 333334) is polled at its `getResultUrl`.
//! - Cancel: `POST /queries/v1/abort-request` with the running request id.
//!
//! Values arrive in Snowflake's internal text form (days since epoch, `seconds.nanos`…) and are
//! formatted here the way DataGrip shows them.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::{Value, json};

use super::{AuthMode, Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};

/// Identity sent at login: the JDBC driver this protocol was taken from, so the server picks
/// the same result formats and parameters for it.
const CLIENT_APP_ID: &str = "JDBC";
const CLIENT_APP_VERSION: &str = "4.3.4";

/// Response codes (see `SessionUtil` / `StmtUtil` in the JDBC driver).
const QUERY_IN_PROGRESS: &str = "333333";
const QUERY_IN_PROGRESS_ASYNC: &str = "333334";
const SESSION_EXPIRED: &str = "390112";
const SESSION_GONE: &str = "390111";
const MASTER_TOKEN_EXPIRED: &str = "390114";

/// Longest wait for any single HTTP request (login, query, poll, chunk download).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Lifetime of the key-pair JWT; it is only used to log in.
const JWT_LIFETIME_SECS: i64 = 60;

pub struct SfSession {
    inner: Arc<Inner>,
    /// Set when the session can't be renewed or logged in again.
    broken: bool,
}

/// What the session and its cancel handle share.
struct Inner {
    http: reqwest::Client,
    /// `https://<account>.snowflakecomputing.com[:port]`.
    base: String,
    /// To log in again when the master token expires.
    cfg: ConnectionConfig,
    tokens: Mutex<Tokens>,
    /// Request id and text of the running query, for `abort-request`.
    running: Mutex<Option<(String, String)>>,
    /// Set by a cancel that arrives while result chunks are downloading (the query itself is
    /// over, so there is nothing to abort on the server): no further chunk is fetched.
    stop_chunks: std::sync::atomic::AtomicBool,
    sequence: Mutex<u64>,
}

#[derive(Default)]
struct Tokens {
    session: String,
    master: String,
}

/// Cancels the running query of a Snowflake session from outside it.
#[derive(Clone)]
pub struct SfCancel(Arc<Inner>);

impl SfCancel {
    pub async fn cancel(&self) -> anyhow::Result<()> {
        let running = self.0.running.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some((request_id, sql)) = running else {
            self.0.stop_chunks.store(true, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        };
        let body = json!({ "sqlText": sql, "requestId": request_id });
        self.0.post_json("/queries/v1/abort-request", &[], &body, "application/json").await?;
        Ok(())
    }
}

/// Server error in the JDBC driver's (and DataGrip's) style: `[sqlState][code] message`.
fn sf_error(resp: &Value) -> anyhow::Error {
    let message = resp["message"].as_str().unwrap_or("").trim();
    let message = if message.is_empty() { tr("Snowflake returned an error without a message") } else { message };
    let code = resp["code"].as_str().unwrap_or("");
    match resp["data"]["sqlState"].as_str() {
        Some(state) if !code.is_empty() => anyhow!("[{state}][{}] {message}", code.trim_start_matches('0')),
        _ if !code.is_empty() => anyhow!("[{code}] {message}"),
        _ => anyhow!("{message}"),
    }
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `https://host[:port]` from the Host field, which may be the account URL
/// (`xy12345.us-east-1.snowflakecomputing.com`), a full `https://` URL or only the account
/// identifier (`orgname-account`).
pub fn base_url(host: &str, port: u16) -> String {
    let h = host.trim().trim_start_matches("https://").trim_start_matches("http://");
    let h = h.split('/').next().unwrap_or(h);
    let (h, explicit_port) = match h.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse::<u16>().ok()),
        _ => (h, None),
    };
    let h = if h.contains('.') { h.to_string() } else { format!("{h}.snowflakecomputing.com") };
    match explicit_port.unwrap_or(port) {
        0 | 443 => format!("https://{h}"),
        p => format!("https://{h}:{p}"),
    }
}

/// Account name for the login and the JWT: the first label of the host, uppercase.
pub fn account_name(host: &str) -> String {
    let base = base_url(host, 443);
    let h = base.trim_start_matches("https://");
    h.split(['.', ':']).next().unwrap_or(h).to_ascii_uppercase()
}

impl Inner {
    fn auth_header(&self, master: bool) -> String {
        let t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        format!("Snowflake Token=\"{}\"", if master { &t.master } else { &t.session })
    }

    /// POST with the session token; returns the response JSON whatever its `success`.
    async fn post_json(&self, path: &str, query: &[(&str, String)], body: &Value, accept: &str) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .query(&[("requestId", request_id())])
            .query(query)
            .header("Authorization", self.auth_header(false))
            .header("Accept", accept)
            .json(body)
            .send()
            .await
            .map_err(http_error)?;
        read_json(resp).await
    }

    async fn get_json(&self, path: &str) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .header("Authorization", self.auth_header(false))
            .header("Accept", "application/snowflake")
            .send()
            .await
            .map_err(http_error)?;
        read_json(resp).await
    }

    /// Renews the session token with the master token; if that one expired too, logs in again.
    /// If both fail the tokens are cleared, which marks the session broken (it reconnects on the
    /// next run instead of failing every statement).
    async fn renew(&self) -> anyhow::Result<()> {
        let result = self.try_renew().await;
        if result.is_err() {
            *self.tokens.lock().unwrap_or_else(|e| e.into_inner()) = Tokens::default();
        }
        result
    }

    async fn try_renew(&self) -> anyhow::Result<()> {
        let old = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).session.clone();
        let resp = self
            .http
            .post(format!("{}/session/token-request", self.base))
            .query(&[("requestId", request_id())])
            .header("Authorization", self.auth_header(true))
            .header("Accept", "application/json")
            .json(&json!({ "oldSessionToken": old, "requestType": "RENEW" }))
            .send()
            .await
            .map_err(http_error)?;
        let v = read_json(resp).await?;
        if v["success"].as_bool() == Some(true) {
            let mut t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
            t.session = v["data"]["sessionToken"].as_str().unwrap_or_default().to_string();
            if let Some(m) = v["data"]["masterToken"].as_str() {
                t.master = m.to_string();
            }
            return Ok(());
        }
        let fresh = login(&self.http, &self.base, &self.cfg).await?;
        *self.tokens.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        Ok(())
    }

    /// Runs `sql` and returns the `data` of the finished query.
    async fn query(&self, sql: &str) -> anyhow::Result<Value> {
        let sequence = {
            let mut s = self.sequence.lock().unwrap_or_else(|e| e.into_inner());
            *s += 1;
            *s
        };
        let rid = request_id();
        let body = json!({
            "sqlText": sql,
            "sequenceId": sequence,
            "querySubmissionTime": chrono::Utc::now().timestamp_millis(),
            "isInternal": false,
            "asyncExec": false,
            "describeOnly": false,
            "parameters": { "JDBC_QUERY_RESULT_FORMAT": "JSON" },
        });
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some((rid.clone(), sql.to_string()));
        let result = self.query_loop(&rid, &body).await;
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
        result
    }

    async fn query_loop(&self, rid: &str, body: &Value) -> anyhow::Result<Value> {
        let mut renewed = false;
        let mut resp = loop {
            let resp = self
                .http
                .post(format!("{}/queries/v1/query-request", self.base))
                .query(&[("requestId", rid)])
                .header("Authorization", self.auth_header(false))
                .header("Accept", "application/snowflake")
                .json(body)
                .send()
                .await
                .map_err(http_error)?;
            let v = read_json(resp).await?;
            let code = v["code"].as_str().unwrap_or("");
            if !renewed && matches!(code, SESSION_EXPIRED | SESSION_GONE | MASTER_TOKEN_EXPIRED) {
                renewed = true;
                self.renew().await?;
                continue;
            }
            break v;
        };
        // Long queries: the server answers "in progress" and the result is polled.
        loop {
            let code = resp["code"].as_str().unwrap_or("");
            if resp["success"].as_bool() == Some(true) && !matches!(code, QUERY_IN_PROGRESS | QUERY_IN_PROGRESS_ASYNC) {
                return Ok(resp["data"].take());
            }
            if !matches!(code, QUERY_IN_PROGRESS | QUERY_IN_PROGRESS_ASYNC) {
                return Err(sf_error(&resp));
            }
            let Some(url) = resp["data"]["getResultUrl"].as_str().map(str::to_string) else {
                bail!(tr("Snowflake did not say where to get the query result"));
            };
            let mut polled = self.get_json(&url).await?;
            // The session can expire during a long query: renew it and poll again.
            if matches!(polled["code"].as_str().unwrap_or(""), SESSION_EXPIRED | SESSION_GONE | MASTER_TOKEN_EXPIRED) {
                self.renew().await?;
                polled = self.get_json(&url).await?;
            }
            // Keep the poll URL for the next round if a later answer doesn't repeat it.
            if polled["data"]["getResultUrl"].is_null() && matches!(polled["code"].as_str().unwrap_or(""), QUERY_IN_PROGRESS | QUERY_IN_PROGRESS_ASYNC) {
                polled["data"]["getResultUrl"] = json!(url);
            }
            resp = polled;
        }
    }
}

pub(crate) fn http_error(e: reqwest::Error) -> anyhow::Error {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(&format!(": {s}"));
        src = s.source();
    }
    anyhow!(msg)
}

async fn read_json(resp: reqwest::Response) -> anyhow::Result<Value> {
    let status = resp.status();
    let text = resp.text().await.map_err(http_error)?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Ok(v),
        Err(_) if !status.is_success() => {
            let snippet: String = text.chars().take(300).collect();
            Err(anyhow!(trf("HTTP {status} from Snowflake: {body}", &[("status", &status), ("body", &snippet.trim())])))
        }
        Err(e) => Err(anyhow!(trf("Unreadable response from Snowflake: {error}", &[("error", &e)]))),
    }
}

// ---------------------------------------------------------------------------
// Authentication and the JDBC URL
// ---------------------------------------------------------------------------

/// How the login proves who the user is.
#[derive(Debug, PartialEq)]
pub enum AuthMethod {
    Password,
    /// `SNOWFLAKE_JWT` signed with the `private_key_file` property.
    KeyPair { key_file: String, passphrase: String },
    /// `OAUTH` / `PROGRAMMATIC_ACCESS_TOKEN`: the token goes in the login request as is.
    Token { authenticator: &'static str, token: String },
}

#[derive(Debug)]
pub struct Credentials {
    pub user: String,
    pub password: String,
    pub method: AuthMethod,
}

/// Resolves the Authentication choice and the driver properties the way the JDBC driver
/// reads them: `authenticator` picks the method, and a `private_key_file` with the default
/// authenticator means key pair. "No auth" takes `user` and `password` from the properties.
/// Does not touch the key file, so the dialog can validate with it.
pub fn credentials(cfg: &ConnectionConfig) -> anyhow::Result<Credentials> {
    let (user, password) = match cfg.auth {
        AuthMode::NoAuth => (cfg.property("user").unwrap_or("").to_string(), cfg.property("password").unwrap_or("").to_string()),
        _ => (cfg.user.trim().to_string(), cfg.password.clone()),
    };
    if user.is_empty() {
        bail!(tr("User is required"));
    }
    let authenticator = match cfg.auth {
        AuthMode::Authenticator if !cfg.authenticator.trim().is_empty() => cfg.authenticator.trim(),
        _ => cfg.property("authenticator").unwrap_or(""),
    }
    .to_ascii_lowercase();
    let key_file = cfg.property("private_key_file");
    let passphrase = cfg.property("private_key_file_pwd").or(cfg.property("private_key_pwd")).map(str::to_string);
    let token = || cfg.property("token").map(str::to_string).unwrap_or_else(|| password.clone());
    let method = match (authenticator.as_str(), key_file) {
        ("" | "snowflake", None) => AuthMethod::Password,
        ("" | "snowflake" | "snowflake_jwt", Some(key)) => {
            AuthMethod::KeyPair { key_file: key.to_string(), passphrase: passphrase.unwrap_or_else(|| password.clone()) }
        }
        ("snowflake_jwt", None) => bail!(tr("Key pair authentication needs the private_key_file property (Advanced tab)")),
        ("oauth", _) => AuthMethod::Token { authenticator: "OAUTH", token: token() },
        ("programmatic_access_token", _) => AuthMethod::Token { authenticator: "PROGRAMMATIC_ACCESS_TOKEN", token: token() },
        (other, _) => bail!(trf("Unsupported authenticator: {name}", &[("name", &other)])),
    };
    Ok(Credentials { user, password, method })
}

/// Query parameters of the URL that have a field on the General tab, in the order of DataGrip's
/// URL template (`warehouse`, `db`, `schema`, `role`).
const URL_FIELDS: [&str; 4] = ["warehouse", "db", "schema", "role"];

/// `jdbc:snowflake://host:443?warehouse=…&db=…&schema=…&role=…`: DataGrip's default URL
/// template, with only the fields that are set.
pub fn jdbc_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let mut url = if cfg.port == 0 { format!("jdbc:snowflake://{host}") } else { format!("jdbc:snowflake://{host}:{}", cfg.port) };
    let values = [&cfg.warehouse, &cfg.database, &cfg.schema, &cfg.role];
    let params: Vec<String> =
        URL_FIELDS.iter().zip(values).filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| format!("{k}={}", url_encode(v.trim()))).collect();
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    url
}

/// The other direction: sets host, port and the URL fields from an edited URL (a parameter
/// that is missing clears its field). `user`/`password` fill those fields, and any other
/// parameter (`authenticator`, `private_key_file`…) becomes a driver property. Returns false
/// (and changes nothing) when it is not a Snowflake URL.
pub fn apply_jdbc_url(cfg: &mut ConnectionConfig, url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("jdbc:snowflake://") else { return false };
    // DataGrip's second template also takes the account URL with its scheme.
    let rest = rest.trim_start_matches("https://").trim_start_matches("http://");
    let (authority, query) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], rest[i..].trim_start_matches('/').trim_start_matches('?')),
        None => (rest, ""),
    };
    let Some((host, port)) = super::url::parse_authority(authority, 443) else { return false };
    (cfg.host, cfg.port) = (host, port);
    let mut fields: [String; 4] = Default::default();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (url_decode(k).to_ascii_lowercase(), url_decode(v));
        let k = if k == "database" { "db".to_string() } else { k };
        match URL_FIELDS.iter().position(|f| *f == k) {
            Some(i) => fields[i] = v,
            None if k == "user" => cfg.user = v,
            None if k == "password" => cfg.password = v,
            None => cfg.set_property(&k, &v),
        }
    }
    [cfg.warehouse, cfg.database, cfg.schema, cfg.role] = fields;
    true
}

/// Percent-encodes what would break a query parameter.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~$".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Logs in and returns the session and master tokens.
async fn login(http: &reqwest::Client, base: &str, cfg: &ConnectionConfig) -> anyhow::Result<Tokens> {
    let account = account_name(&cfg.host);
    let creds = credentials(cfg)?;
    let user = creds.user.as_str();
    let mut data = json!({
        "CLIENT_APP_ID": CLIENT_APP_ID,
        "CLIENT_APP_VERSION": CLIENT_APP_VERSION,
        "ACCOUNT_NAME": account,
        "LOGIN_NAME": user,
        "CLIENT_ENVIRONMENT": {
            "APPLICATION": "DataCraft",
            "OS": std::env::consts::OS,
            "OS_VERSION": std::env::consts::ARCH,
        },
        "SESSION_PARAMETERS": {
            "JDBC_QUERY_RESULT_FORMAT": "JSON",
            "CLIENT_SESSION_KEEP_ALIVE": false,
        },
    });
    match &creds.method {
        AuthMethod::Password => data["PASSWORD"] = json!(creds.password),
        AuthMethod::KeyPair { key_file, passphrase } => {
            let key = load_private_key(key_file, passphrase)?;
            data["AUTHENTICATOR"] = json!("SNOWFLAKE_JWT");
            data["TOKEN"] = json!(jwt(&key, &account, user)?);
        }
        AuthMethod::Token { authenticator, token } => {
            data["AUTHENTICATOR"] = json!(authenticator);
            data["TOKEN"] = json!(token);
        }
    }

    // The General tab wins over the driver properties of the same meaning.
    let mut query = vec![("requestId", request_id())];
    for (name, value, prop) in [
        ("databaseName", &cfg.database, "db"),
        ("schemaName", &cfg.schema, "schema"),
        ("warehouse", &cfg.warehouse, "warehouse"),
        ("roleName", &cfg.role, "role"),
    ] {
        let value = Some(value.trim()).filter(|v| !v.is_empty()).or_else(|| cfg.property(prop));
        if let Some(v) = value {
            query.push((name, v.to_string()));
        }
    }
    let resp = http
        .post(format!("{base}/session/v1/login-request"))
        .query(&query)
        .header("Accept", "application/json")
        .json(&json!({ "data": data }))
        .send()
        .await
        .map_err(http_error)
        .with_context(|| trf("Could not connect to {host}", &[("host", &base)]))?;
    let v = read_json(resp).await?;
    if v["success"].as_bool() != Some(true) {
        return Err(sf_error(&v));
    }
    Ok(Tokens {
        session: v["data"]["token"].as_str().unwrap_or_default().to_string(),
        master: v["data"]["masterToken"].as_str().unwrap_or_default().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Key-pair authentication
// ---------------------------------------------------------------------------

/// Reads a PEM (or DER) RSA private key: PKCS#8, encrypted PKCS#8 (`passphrase`) or PKCS#1.
fn load_private_key(path: &str, passphrase: &str) -> anyhow::Result<RsaKeyPair> {
    let path = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(path),
    };
    let bytes = std::fs::read(&path).with_context(|| trf("Could not read the private key {path}", &[("path", &path.display())]))?;
    let rejected = |e: aws_lc_rs::error::KeyRejected| anyhow!(trf("Invalid RSA private key: {error}", &[("error", &e)]));

    let Ok(text) = std::str::from_utf8(&bytes) else {
        // Binary DER.
        return RsaKeyPair::from_pkcs8(&bytes).or_else(|_| RsaKeyPair::from_der(&bytes)).map_err(rejected);
    };
    let label = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("-----BEGIN ")?.strip_suffix("-----"))
        .ok_or_else(|| anyhow!(tr("The private key file is not in PEM format")))?
        .to_string();
    if text.contains("Proc-Type: 4,ENCRYPTED") {
        bail!(tr("Legacy encrypted PEM keys are not supported; convert it with: openssl pkcs8 -topk8 -v2 aes256 -in key.pem -out key.p8"));
    }
    let body: String = text
        .lines()
        .skip_while(|l| !l.trim().starts_with("-----BEGIN "))
        .skip(1)
        .take_while(|l| !l.trim().starts_with("-----END "))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = STANDARD.decode(body).map_err(|e| anyhow!(trf("Invalid RSA private key: {error}", &[("error", &e)])))?;

    match label.as_str() {
        "ENCRYPTED PRIVATE KEY" => {
            if passphrase.is_empty() {
                bail!(tr("The private key is encrypted: enter its passphrase"));
            }
            let info = pkcs8::EncryptedPrivateKeyInfo::try_from(der.as_slice())
                .map_err(|e| anyhow!(trf("Invalid RSA private key: {error}", &[("error", &e)])))?;
            let plain = info.decrypt(passphrase).map_err(|_| anyhow!(tr("Could not decrypt the private key: wrong passphrase?")))?;
            RsaKeyPair::from_pkcs8(plain.as_bytes()).map_err(rejected)
        }
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der).map_err(rejected),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der).map_err(rejected),
        other => bail!(trf("Unsupported private key type: {kind}", &[("kind", &other)])),
    }
}

/// `SHA256:<base64>` of the public key (DER SubjectPublicKeyInfo), as registered with
/// `ALTER USER … SET RSA_PUBLIC_KEY`.
fn public_key_fingerprint(key: &RsaKeyPair) -> anyhow::Result<String> {
    let der = key.public_key().as_der().map_err(|_| anyhow!(tr("Could not encode the public key")))?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der.as_ref());
    Ok(format!("SHA256:{}", STANDARD.encode(digest.as_ref())))
}

/// RS256 JWT for `SNOWFLAKE_JWT` (see `SessionUtilKeyPair`): `iss` = ACCOUNT.USER.fingerprint,
/// `sub` = ACCOUNT.USER.
fn jwt(key: &RsaKeyPair, account: &str, user: &str) -> anyhow::Result<String> {
    let qualified = format!("{}.{}", account.to_ascii_uppercase(), user.to_ascii_uppercase());
    let now = chrono::Utc::now().timestamp();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = json!({
        "iss": format!("{qualified}.{}", public_key_fingerprint(key)?),
        "sub": qualified,
        "iat": now,
        "exp": now + JWT_LIFETIME_SECS,
    });
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{payload}");
    let mut signature = vec![0; key.public_modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &aws_lc_rs::rand::SystemRandom::new(), signing_input.as_bytes(), &mut signature)
        .map_err(|_| anyhow!(tr("Could not sign the login token with the private key")))?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

impl SfSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        if cfg.host.trim().is_empty() {
            bail!(tr("Host is required"));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Every request, chunk downloads included, as the JDBC driver's socket timeout. A
            // long query doesn't hit it: the server answers "in progress" within ~45 s and the
            // result is polled.
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("DataCraft/{} ({CLIENT_APP_ID} {CLIENT_APP_VERSION})", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(http_error)?;
        let base = base_url(&cfg.host, cfg.port);
        let tokens = login(&http, &base, cfg).await?;
        let inner = Inner {
            http,
            base,
            cfg: cfg.clone(),
            tokens: Mutex::new(tokens),
            running: Mutex::new(None),
            stop_chunks: std::sync::atomic::AtomicBool::new(false),
            sequence: Mutex::new(0),
        };
        Ok(Self { inner: Arc::new(inner), broken: false })
    }

    pub fn cancel_handle(&self) -> SfCancel {
        SfCancel(Arc::clone(&self.inner))
    }

    pub fn is_closed(&self) -> bool {
        self.broken
    }

    pub async fn execute(&mut self, sql: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let start = Instant::now();
        self.inner.stop_chunks.store(false, std::sync::atomic::Ordering::SeqCst);
        let data = match self.inner.query(sql).await {
            Ok(d) => d,
            Err(e) => {
                // A failed re-login leaves the session without tokens.
                if self.inner.tokens.lock().unwrap_or_else(|e| e.into_inner()).session.is_empty() {
                    self.broken = true;
                }
                return Err(e);
            }
        };
        let mut rs = self.result_set(sql.trim(), &data, max_rows).await?;
        rs.elapsed = start.elapsed();
        Ok(vec![rs])
    }

    async fn result_set(&self, statement: &str, data: &Value, max_rows: usize) -> anyhow::Result<ResultSet> {
        let empty = Vec::new();
        let rowtype = data["rowtype"].as_array().unwrap_or(&empty);
        let types: Vec<ColType> = rowtype.iter().map(ColType::from_json).collect();
        let statement_type = data["statementTypeId"].as_u64().unwrap_or(0);

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        push_rows(&mut rows, data["rowset"].as_array().unwrap_or(&empty), &types, max_rows);
        if let Some(chunks) = data["chunks"].as_array() {
            for chunk in chunks {
                if rows.len() >= max_rows {
                    break;
                }
                if self.inner.stop_chunks.load(std::sync::atomic::Ordering::SeqCst) {
                    bail!(tr("Query cancelled"));
                }
                let Some(url) = chunk["url"].as_str() else { continue };
                let chunk_rows = self.download_chunk(url, data).await?;
                push_rows(&mut rows, &chunk_rows, &types, max_rows);
            }
        }
        let total = data["total"].as_u64().unwrap_or(rows.len() as u64);

        // DML (INSERT … MULTI_INSERT): one row with the affected counts per kind.
        if (0x3100..0x3600).contains(&statement_type) {
            let affected = data["rowset"]
                .get(0)
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(|v| v.as_str()?.parse::<u64>().ok()).sum())
                .unwrap_or(0);
            return Ok(ResultSet { statement: statement.to_string(), affected: Some(affected), ..Default::default() });
        }
        // DDL, transactions, USE and ALTER SESSION: a status message, not data.
        let is_command = matches!(statement_type, 0x4000..0x4400 | 0x5000..0x6000 | 0x6000..0x7000);
        if is_command {
            return Ok(ResultSet { statement: statement.to_string(), ..Default::default() });
        }
        Ok(ResultSet {
            statement: statement.to_string(),
            columns: rowtype.iter().map(|c| c["name"].as_str().unwrap_or("").to_string()).collect(),
            truncated: total > rows.len() as u64 && rows.len() >= max_rows,
            rows,
            ..Default::default()
        })
    }

    /// One result chunk: JSON rows separated by commas, without the enclosing brackets.
    async fn download_chunk(&self, url: &str, data: &Value) -> anyhow::Result<Vec<Value>> {
        let mut req = self.inner.http.get(url);
        if let Some(headers) = data["chunkHeaders"].as_object() {
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str().unwrap_or(""));
            }
        } else if let Some(qrmk) = data["qrmk"].as_str() {
            req = req
                .header("x-amz-server-side-encryption-customer-algorithm", "AES256")
                .header("x-amz-server-side-encryption-customer-key", qrmk);
        }
        let resp = req.send().await.map_err(http_error)?;
        if !resp.status().is_success() {
            bail!(trf("Could not download a result chunk: HTTP {status}", &[("status", &resp.status())]));
        }
        let mut bytes = resp.bytes().await.map_err(http_error)?.to_vec();
        if bytes.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::MultiGzDecoder::new(bytes.as_slice()), &mut out)?;
            bytes = out;
        }
        let mut text = Vec::with_capacity(bytes.len() + 2);
        text.push(b'[');
        text.extend_from_slice(&bytes);
        text.push(b']');
        let rows: Vec<Value> = serde_json::from_slice(&text).context(tr("Unreadable result chunk"))?;
        Ok(rows)
    }

    /// First column of each row as text.
    async fn scalar_strings(&mut self, sql: &str) -> anyhow::Result<Vec<String>> {
        let rs = self.execute(sql, usize::MAX).await?;
        Ok(rs.into_iter().flat_map(|r| r.rows).filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Rows of `sql` as maps from (lowercase) column name to value, for SHOW commands whose
    /// column order isn't guaranteed.
    async fn named_rows(&mut self, sql: &str) -> anyhow::Result<Vec<std::collections::HashMap<String, String>>> {
        let rs = self.execute(sql, usize::MAX).await?.pop().unwrap_or_default();
        let cols: Vec<String> = rs.columns.iter().map(|c| c.to_lowercase()).collect();
        Ok(rs
            .rows
            .into_iter()
            .map(|row| cols.iter().cloned().zip(row.into_iter().map(Option::unwrap_or_default)).collect())
            .collect())
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        let db = self.scalar_strings("SELECT CURRENT_DATABASE()").await?.into_iter().next();
        match db {
            Some(db) => Ok(db),
            // No default database: the first one visible.
            None => Ok(self.list_databases().await?.into_iter().next().unwrap_or_default()),
        }
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        let mut names: Vec<String> = self.named_rows("SHOW DATABASES").await?.into_iter().filter_map(|mut r| r.remove("name")).collect();
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        self.execute(&format!("USE DATABASE {}", super::Driver::Snowflake.quote_ident(db)), 0).await.map(|_| ())
    }

    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        self.execute("BEGIN", 0).await?;
        let mut total = 0;
        for stmt in statements {
            let n = match self.execute(stmt, 0).await {
                Ok(rs) => rs.iter().filter_map(|r| r.affected).sum::<u64>(),
                Err(e) => {
                    let _ = self.execute("ROLLBACK", 0).await;
                    return Err(e);
                }
            };
            if n != 1 {
                let _ = self.execute("ROLLBACK", 0).await;
                return Err(super::not_single_row(n, stmt));
            }
            total += n;
        }
        if let Err(e) = self.execute("COMMIT", 0).await {
            let _ = self.execute("ROLLBACK", 0).await;
            return Err(e);
        }
        Ok(total)
    }

    /// Schemas, tables/views and columns of the current database.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let mut schemas: Vec<SchemaInfo> = self
            .scalar_strings(
                "SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA \
                 WHERE SCHEMA_NAME <> 'INFORMATION_SCHEMA' ORDER BY SCHEMA_NAME",
            )
            .await?
            .into_iter()
            .map(|name| SchemaInfo { name, tables: Vec::new() })
            .collect();

        // Primary keys only come from SHOW; without them the explorer still works.
        let mut pks = std::collections::HashSet::new();
        if let Ok(rows) = self.named_rows("SHOW PRIMARY KEYS IN DATABASE").await {
            for r in rows {
                let get = |k: &str| r.get(k).cloned().unwrap_or_default();
                pks.insert((get("schema_name"), get("table_name"), get("column_name")));
            }
        }

        let rs = self
            .execute(
                "SELECT t.TABLE_SCHEMA, t.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.DATA_TYPE, \
                        c.CHARACTER_MAXIMUM_LENGTH, c.NUMERIC_PRECISION, c.NUMERIC_SCALE, \
                        c.DATETIME_PRECISION, c.IS_NULLABLE \
                 FROM INFORMATION_SCHEMA.TABLES t \
                 LEFT JOIN INFORMATION_SCHEMA.COLUMNS c \
                        ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME \
                 WHERE t.TABLE_SCHEMA <> 'INFORMATION_SCHEMA' \
                 ORDER BY t.TABLE_SCHEMA, t.TABLE_NAME, c.ORDINAL_POSITION",
                usize::MAX,
            )
            .await?
            .pop()
            .unwrap_or_default();

        for row in rs.rows {
            let text = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            let int = |i: usize| row.get(i).cloned().flatten().and_then(|v| v.parse::<i64>().ok());
            let (schema, table) = (text(0), text(1));
            let si = match schemas.iter().position(|s| s.name == schema) {
                Some(i) => i,
                None => {
                    schemas.push(SchemaInfo { name: schema.clone(), tables: Vec::new() });
                    schemas.len() - 1
                }
            };
            let s = &mut schemas[si];
            if s.tables.last().is_none_or(|t| t.name != table) {
                s.tables.push(TableInfo {
                    name: table.clone(),
                    kind: if text(2).contains("VIEW") { ObjectKind::View } else { ObjectKind::Table },
                    columns: Vec::new(),
                });
            }
            let Some(t) = s.tables.last_mut() else { continue };
            let Some(Some(column)) = row.get(3).cloned() else { continue };
            t.columns.push(ColumnInfo {
                primary_key: pks.contains(&(schema.clone(), table.clone(), column.clone())),
                name: column,
                data_type: type_label(&text(4), int(5), int(6), int(7), int(8)),
                nullable: text(9) == "YES",
            });
        }
        Ok(schemas)
    }
}

impl Drop for SfSession {
    /// Logs the session out (best effort) so it doesn't linger on the server.
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let inner = Arc::clone(&self.inner);
        rt.spawn(async move {
            let _ = inner
                .http
                .post(format!("{}/session", inner.base))
                .query(&[("delete", "true"), ("requestId", &request_id())])
                .header("Authorization", inner.auth_header(false))
                .header("Accept", "application/json")
                .send()
                .await;
        });
    }
}

/// Type as DataGrip shows it: `NUMBER(38,0)`, `VARCHAR(16777216)`, `TIMESTAMP_NTZ(9)`.
pub(super) fn type_label(data_type: &str, max_len: Option<i64>, precision: Option<i64>, scale: Option<i64>, dt_precision: Option<i64>) -> String {
    match data_type {
        "NUMBER" => match (precision, scale) {
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            _ => "NUMBER".into(),
        },
        "TEXT" => match max_len {
            Some(n) => format!("VARCHAR({n})"),
            None => "VARCHAR".into(),
        },
        "BINARY" => match max_len {
            Some(n) => format!("BINARY({n})"),
            None => "BINARY".into(),
        },
        "TIME" | "TIMESTAMP_NTZ" | "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" => match dt_precision {
            Some(p) => format!("{data_type}({p})"),
            None => data_type.to_string(),
        },
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// What formatting needs from a `rowtype` entry.
struct ColType {
    kind: String,
    scale: u32,
}

impl ColType {
    fn from_json(v: &Value) -> Self {
        ColType {
            kind: v["type"].as_str().unwrap_or("text").to_ascii_lowercase(),
            scale: v["scale"].as_u64().unwrap_or(0).min(9) as u32,
        }
    }
}

fn push_rows(out: &mut Vec<Vec<Cell>>, rows: &[Value], types: &[ColType], max_rows: usize) {
    for row in rows {
        if out.len() >= max_rows {
            return;
        }
        let Some(values) = row.as_array() else { continue };
        out.push(
            values
                .iter()
                .enumerate()
                .map(|(i, v)| match v {
                    Value::Null => None,
                    Value::String(s) => Some(match types.get(i) {
                        Some(t) => format_value(t, s),
                        None => s.clone(),
                    }),
                    other => Some(other.to_string()),
                })
                .collect(),
        );
    }
}

/// Snowflake's wire text → display text. Anything unexpected is shown as received.
fn format_value(t: &ColType, raw: &str) -> String {
    let formatted = match t.kind.as_str() {
        "boolean" => match raw {
            "1" => Some("true".to_string()),
            "0" => Some("false".to_string()),
            _ => None,
        },
        "date" => raw
            .parse::<i64>()
            .ok()
            .and_then(|days| NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(chrono::Duration::days(days)))
            .map(|d| d.format("%Y-%m-%d").to_string()),
        "time" => epoch_nanos(raw).and_then(|n| {
            let secs = u32::try_from(n.div_euclid(1_000_000_000)).ok()?;
            let nanos = u32::try_from(n.rem_euclid(1_000_000_000)).ok()?;
            let time = NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)?;
            // Whole seconds as DataGrip shows times: no `.000`.
            let frac = if nanos == 0 { String::new() } else { fraction(nanos, t.scale) };
            Some(format!("{}{frac}", time.format("%H:%M:%S")))
        }),
        "timestamp_ntz" => epoch_nanos(raw).and_then(|n| {
            let dt = DateTime::from_timestamp_nanos(i64::try_from(n).ok()?).naive_utc();
            Some(format!("{}{}", dt.format("%Y-%m-%d %H:%M:%S"), fraction(dt.and_utc().timestamp_subsec_nanos(), t.scale)))
        }),
        // Shown in the local time zone, like DataGrip (which uses the JVM's).
        "timestamp_ltz" => epoch_nanos(raw).and_then(|n| {
            let dt = Local.timestamp_nanos(i64::try_from(n).ok()?);
            Some(format!("{}{} {}", dt.format("%Y-%m-%d %H:%M:%S"), fraction(dt.timestamp_subsec_nanos(), t.scale), dt.format("%:z")))
        }),
        // `seconds.nanos offset`, where offset = minutes east of UTC + 1440.
        "timestamp_tz" => raw.split_once(' ').and_then(|(ts, off)| {
            let minutes = off.trim().parse::<i32>().ok()?.checked_sub(1440)?;
            let tz = FixedOffset::east_opt(minutes.checked_mul(60)?)?;
            let dt = tz.timestamp_nanos(i64::try_from(epoch_nanos(ts)?).ok()?);
            Some(format!("{}{} {}", dt.format("%Y-%m-%d %H:%M:%S"), fraction(dt.timestamp_subsec_nanos(), t.scale), dt.format("%:z")))
        }),
        _ => None,
    };
    formatted.unwrap_or_else(|| raw.to_string())
}

/// `"-12.5"` (seconds, up to 9 decimals) → nanoseconds.
fn epoch_nanos(raw: &str) -> Option<i128> {
    let (neg, digits) = match raw.trim().strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.trim()),
    };
    let (secs, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: i128 = secs.parse().ok()?;
    let nanos: i128 = if frac.is_empty() { 0 } else { format!("{frac:0<9}").parse().ok()? };
    let total = secs.checked_mul(1_000_000_000)?.checked_add(nanos)?;
    Some(if neg { -total } else { total })
}

/// `.123` with the column's scale, trailing zeros trimmed down to milliseconds as DataGrip does.
fn fraction(nanos: u32, scale: u32) -> String {
    if scale == 0 {
        return String::new();
    }
    let digits = format!("{nanos:09}");
    let mut s = &digits[..scale as usize];
    while s.len() > 3 && s.ends_with('0') {
        s = &s[..s.len() - 1];
    }
    format!(".{s}")
}

#[cfg(test)]
mod tests {
    //! Unit tests for the protocol helpers. The live test runs with `cargo test -- --ignored` and
    //! DATACRAFT_SF_HOST (required; skipped without it), DATACRAFT_SF_USER, and either
    //! DATACRAFT_SF_PASSWORD or DATACRAFT_SF_KEY (private key file; DATACRAFT_SF_PASSWORD is then
    //! its passphrase), plus optional DATACRAFT_SF_WAREHOUSE, DATACRAFT_SF_ROLE, DATACRAFT_SF_DB.

    use super::*;
    use crate::db::Driver;

    fn sf() -> ConnectionConfig {
        let mut cfg = ConnectionConfig::new(1, Driver::Snowflake);
        cfg.host = "ACME-XY12345.snowflakecomputing.com".into();
        cfg.user = "JDOE".into();
        cfg
    }

    /// A local server that answers every request with Snowflake's "session expired" (390112),
    /// so renewing and logging in again both fail.
    async fn expired_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let body = r#"{"success":false,"code":"390112","message":"Session expired"}"#;
                    let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// A session whose renewal and re-login both fail is marked broken, so it reconnects on the
    /// next run instead of failing every statement (found in review of the Snowflake PR).
    #[tokio::test]
    async fn failed_relogin_breaks_the_session() {
        let mut cfg = sf();
        cfg.password = "pw".into();
        let inner = Inner {
            http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            base: expired_server().await,
            cfg,
            tokens: Mutex::new(Tokens { session: "s".into(), master: "m".into() }),
            running: Mutex::new(None),
            stop_chunks: std::sync::atomic::AtomicBool::new(false),
            sequence: Mutex::new(0),
        };
        let mut session = SfSession { inner: Arc::new(inner), broken: false };
        assert!(session.execute("select 1", 10).await.is_err());
        assert!(session.inner.tokens.lock().unwrap().session.is_empty());
        assert!(session.is_closed(), "a session that can't log in again must report itself closed");
    }

    /// Out-of-range values from the server fall back to the raw text instead of overflowing
    /// (found in review of the Snowflake PR).
    #[test]
    fn huge_time_values_do_not_overflow() {
        assert_eq!(epoch_nanos("170141183460469231731687303715884105727"), None);
        assert_eq!(epoch_nanos("-12.5"), Some(-12_500_000_000));
        for (kind, raw) in [
            ("timestamp_tz", "1.0 99999999999"),
            ("timestamp_tz", "1.0 2147483647"),
            ("time", "99999999999999999999"),
            ("timestamp_ntz", "170141183460469231731687303715884105727"),
        ] {
            let t = ColType::from_json(&json!({ "type": kind, "scale": 3 }));
            assert_eq!(format_value(&t, raw), raw, "{kind}");
        }
    }

    #[test]
    fn url_round_trip() {
        let mut cfg = sf();
        assert_eq!(jdbc_url(&cfg), "jdbc:snowflake://ACME-XY12345.snowflakecomputing.com:443");
        cfg.database = "ANALYTICS".into();
        cfg.role = "DATA ENG".into();
        let url = jdbc_url(&cfg);
        assert_eq!(url, "jdbc:snowflake://ACME-XY12345.snowflakecomputing.com:443?db=ANALYTICS&role=DATA%20ENG");

        let mut back = sf();
        back.warehouse = "STALE".into();
        assert!(apply_jdbc_url(&mut back, &url));
        assert_eq!((back.database.as_str(), back.role.as_str(), back.warehouse.as_str()), ("ANALYTICS", "DATA ENG", ""));

        assert!(apply_jdbc_url(&mut back, "jdbc:snowflake://acme.snowflakecomputing.com:8443/?schema=PUBLIC&authenticator=snowflake_jwt&tracing=ALL"));
        assert_eq!((back.host.as_str(), back.port, back.schema.as_str()), ("acme.snowflakecomputing.com", 8443, "PUBLIC"));
        assert_eq!((back.property("authenticator"), back.property("tracing")), (Some("snowflake_jwt"), Some("ALL")));
        assert!(apply_jdbc_url(&mut back, "jdbc:snowflake://https://acme.snowflakecomputing.com"));
        assert_eq!((back.host.as_str(), back.port), ("acme.snowflakecomputing.com", 443));
        assert!(!apply_jdbc_url(&mut back, "jdbc:postgresql://localhost:5432/db"));
    }

    #[test]
    fn credentials_follow_authentication_and_properties() {
        let mut cfg = sf();
        cfg.password = "pw".into();
        assert_eq!(credentials(&cfg).unwrap().method, AuthMethod::Password);

        // Authenticator with an empty field and a private_key_file property: key pair, as in DataGrip.
        cfg.auth = AuthMode::Authenticator;
        cfg.set_property("private_key_file", "/Users/me/.ssh/key.p8");
        assert_eq!(
            credentials(&cfg).unwrap().method,
            AuthMethod::KeyPair { key_file: "/Users/me/.ssh/key.p8".into(), passphrase: "pw".into() }
        );
        cfg.set_property("private_key_file_pwd", "secret");
        assert!(matches!(credentials(&cfg).unwrap().method, AuthMethod::KeyPair { passphrase, .. } if passphrase == "secret"));

        cfg.properties.clear();
        cfg.authenticator = "SNOWFLAKE_JWT".into();
        assert!(credentials(&cfg).is_err());
        cfg.authenticator = "externalbrowser".into();
        assert!(credentials(&cfg).is_err());
        cfg.authenticator = "oauth".into();
        assert_eq!(credentials(&cfg).unwrap().method, AuthMethod::Token { authenticator: "OAUTH", token: "pw".into() });

        // No auth: the user field is ignored, the properties supply it.
        cfg.auth = AuthMode::NoAuth;
        assert!(credentials(&cfg).is_err());
        cfg.set_property("user", "SVC");
        assert_eq!(credentials(&cfg).unwrap().user, "SVC");
    }

    #[test]
    fn legacy_key_file_migrates_to_a_property() {
        let mut cfg: ConnectionConfig = serde_json::from_str(r#"{"driver":"Snowflake","private_key_file":"~/k.p8"}"#).unwrap();
        cfg.migrate();
        assert_eq!((cfg.auth, cfg.property("private_key_file")), (AuthMode::Authenticator, Some("~/k.p8")));
        assert!(!serde_json::to_string(&cfg).unwrap().contains("\"private_key_file\":"));
    }

    fn t(kind: &str, scale: u32) -> ColType {
        ColType { kind: kind.into(), scale }
    }

    #[test]
    fn hosts_and_accounts() {
        assert_eq!(base_url("ACME-XY12345.snowflakecomputing.com", 443), "https://ACME-XY12345.snowflakecomputing.com");
        assert_eq!(base_url("https://xy12345.us-east-1.snowflakecomputing.com/", 443), "https://xy12345.us-east-1.snowflakecomputing.com");
        assert_eq!(base_url("myorg-acct", 0), "https://myorg-acct.snowflakecomputing.com");
        assert_eq!(base_url("acct.privatelink.snowflakecomputing.com:8443", 443), "https://acct.privatelink.snowflakecomputing.com:8443");
        assert_eq!(account_name("acme-xy12345.snowflakecomputing.com"), "ACME-XY12345");
        assert_eq!(account_name("xy12345.us-east-1.snowflakecomputing.com"), "XY12345");
        assert_eq!(account_name("https://myorg-acct"), "MYORG-ACCT");
    }

    #[test]
    fn values() {
        assert_eq!(format_value(&t("boolean", 0), "1"), "true");
        assert_eq!(format_value(&t("date", 0), "20745"), "2026-10-19");
        assert_eq!(format_value(&t("date", 0), "-1"), "1969-12-31");
        assert_eq!(format_value(&t("time", 9), "45296.123000000"), "12:34:56.123");
        assert_eq!(format_value(&t("time", 0), "45296"), "12:34:56");
        assert_eq!(format_value(&t("time", 9), "45296.000000000"), "12:34:56");
        assert_eq!(format_value(&t("timestamp_ntz", 9), "1792411200.000000000"), "2026-10-19 12:00:00.000");
        assert_eq!(format_value(&t("timestamp_ntz", 6), "-0.500000"), "1969-12-31 23:59:59.500");
        assert_eq!(format_value(&t("timestamp_tz", 3), "1792411200.250 1320"), "2026-10-19 10:00:00.250 -02:00");
        assert_eq!(format_value(&t("fixed", 2), "12.50"), "12.50");
        assert_eq!(format_value(&t("variant", 0), "{\n  \"a\": 1\n}"), "{\n  \"a\": 1\n}");
        assert_eq!(fraction(123_456_789, 9), ".123456789");
        assert_eq!(fraction(120_000_000, 6), ".120");
    }

    #[test]
    fn type_labels() {
        assert_eq!(type_label("NUMBER", None, Some(38), Some(0), None), "NUMBER(38,0)");
        assert_eq!(type_label("TEXT", Some(16777216), None, None, None), "VARCHAR(16777216)");
        assert_eq!(type_label("TIMESTAMP_NTZ", None, None, None, Some(9)), "TIMESTAMP_NTZ(9)");
        assert_eq!(type_label("VARIANT", None, None, None, None), "VARIANT");
    }

    #[test]
    fn jwt_signed_with_the_key() {
        let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let token = jwt(&key, "myorg-acct", "me").unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["sub"], "MYORG-ACCT.ME");
        assert_eq!(claims["iss"], format!("MYORG-ACCT.ME.{}", public_key_fingerprint(&key).unwrap()));
        assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), JWT_LIFETIME_SECS);

        let signed = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, key.public_key().as_ref())
            .verify(signed.as_bytes(), &signature)
            .expect("valid RS256 signature");
    }

    #[test]
    fn pem_keys() {
        let dir = std::env::temp_dir().join(format!("datacraft-sf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let der = key.as_der().unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der.as_ref()).as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\n")
        );
        let path = dir.join("rsa_key.p8");
        std::fs::write(&path, pem).unwrap();
        let loaded = load_private_key(path.to_str().unwrap(), "").unwrap();
        assert_eq!(public_key_fingerprint(&loaded).unwrap(), public_key_fingerprint(&key).unwrap());
        assert!(load_private_key(dir.join("missing.p8").to_str().unwrap(), "").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore]
    async fn live() {
        let Ok(host) = std::env::var("DATACRAFT_SF_HOST") else {
            eprintln!("DATACRAFT_SF_HOST not set: skipping the Snowflake test");
            return;
        };
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let mut cfg = ConnectionConfig::new(1, Driver::Snowflake);
        cfg.host = host;
        cfg.user = env("DATACRAFT_SF_USER");
        cfg.password = env("DATACRAFT_SF_PASSWORD");
        if let Ok(key) = std::env::var("DATACRAFT_SF_KEY") {
            cfg.auth = AuthMode::Authenticator;
            cfg.set_property("private_key_file", &key);
        }
        cfg.warehouse = env("DATACRAFT_SF_WAREHOUSE");
        cfg.role = env("DATACRAFT_SF_ROLE");
        cfg.database = env("DATACRAFT_SF_DB");
        let mut s = SfSession::connect(&cfg).await.expect("login");

        let rs = s
            .execute(
                "SELECT 42 AS i, 12.50::NUMBER(10,2) AS n, 'hello' AS t, TRUE AS b, '2026-10-16'::DATE AS d, \
                 '2026-10-16 12:34:56.789'::TIMESTAMP_NTZ AS ts, PARSE_JSON('{\"a\":1}') AS v, NULL AS z",
                100,
            )
            .await
            .unwrap();
        assert_eq!(rs[0].columns, ["I", "N", "T", "B", "D", "TS", "V", "Z"]);
        let row: Vec<Option<&str>> = rs[0].rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(&row[..6], [Some("42"), Some("12.50"), Some("hello"), Some("true"), Some("2026-10-16"), Some("2026-10-16 12:34:56.789")]);
        assert_eq!(row[7], None);

        let rs = s
            .execute(
                "SELECT '2026-10-16 12:34:56.5 +02:00'::TIMESTAMP_TZ AS tz, '12:34:56'::TIME AS tm, \
                 TO_BINARY('CAFE', 'HEX') AS bin, ARRAY_CONSTRUCT(1, 2) AS arr",
                10,
            )
            .await
            .unwrap();
        let row: Vec<Option<&str>> = rs[0].rows[0].iter().map(|c| c.as_deref()).collect();
        assert_eq!(&row[..3], [Some("2026-10-16 12:34:56.500 +02:00"), Some("12:34:56"), Some("CAFE")]);
        assert!(row[3].unwrap().contains('1'), "{row:?}");

        // Big enough to need chunks; only `max_rows` are fetched.
        let rs = s.execute("SELECT SEQ4() AS n, RANDSTR(200, RANDOM()) AS s FROM TABLE(GENERATOR(ROWCOUNT => 200000))", 5000).await.unwrap();
        assert_eq!(rs[0].rows.len(), 5000);
        assert!(rs[0].truncated);

        let err = s.execute("SELECT * FROM dc_missing_table_xyz", 10).await.unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");

        let dbs = s.list_databases().await.unwrap();
        assert!(!dbs.is_empty());
        let current = s.current_database().await.unwrap();
        if !current.is_empty() {
            s.use_database(&current).await.unwrap();
            let schemas = s.introspect().await.unwrap();
            assert!(!schemas.iter().any(|s| s.name == "INFORMATION_SCHEMA"));
            let tables: usize = schemas.iter().map(|s| s.tables.len()).sum();
            eprintln!("{} databases; {current}: {} schemas, {tables} tables", dbs.len(), schemas.len());
        }
    }
}
