//! Kusto (Azure Data Explorer) driver over its REST API (reqwest).
//!
//! The console takes KQL queries, sent to `/v2/rest/query`, and control commands (anything that
//! starts with a dot: `.show tables`…), sent to `/v1/rest/mgmt`. A script is split into requests
//! by blank lines, as in Kusto Explorer: `;` belongs to KQL (`let n = 10; T | take n`, or
//! `A; B` for two results from one request). The cluster's databases are the databases, and their
//! tables and materialized views the tables and views. The data is read-only: rows are ingested,
//! not edited in place.
//!
//! Authentication: "Azure CLI" takes a token from `az account get-access-token` (the user's
//! `az login`); "User & Password" is an Entra ID application, the User being its client ID and
//! the Password its client secret; "No auth" sends no token (the Kusto emulator). The `tenant`
//! property picks the tenant (required for an application). The login endpoint and the token's
//! audience come from the cluster itself (`/v1/rest/auth/metadata`), so other Azure clouds work.
//!
//! On read-only connections every request carries `request_readonly` (the server refuses control
//! commands that write) and `request_callout_disabled` (plugins such as `sql_request` or
//! `http_request` can't reach other services), besides the console's own check.

use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};

use super::snowflake::http_error;
use super::{AuthMode, Cell, ColumnInfo, ConnectionConfig, Driver, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};

/// The public cloud's Entra ID, for clusters that don't publish theirs.
const LOGIN_ENDPOINT: &str = "https://login.microsoftonline.com";
/// Tokens are renewed this long before they expire.
const TOKEN_MARGIN: Duration = Duration::from_secs(300);

/// Where `az` may be. Apps started from the macOS Finder don't get the shell's PATH, and on
/// Windows the CLI is a batch file.
#[cfg(target_os = "windows")]
const AZ: &[&str] = &["az.cmd"];
#[cfg(target_os = "macos")]
const AZ: &[&str] = &["az", "/opt/homebrew/bin/az", "/usr/local/bin/az"];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const AZ: &[&str] = &["az"];

enum Auth {
    /// No token (the emulator).
    None,
    /// The Azure CLI's signed-in account.
    AzureCli { tenant: Option<String> },
    /// An Entra ID application (client credentials).
    Application { tenant: String, client_id: String, secret: String },
}

pub struct KustoSession {
    http: reqwest::Client,
    /// `https://<cluster>[:port]`.
    base: String,
    /// The database of queries and commands.
    database: String,
    auth: Auth,
    /// The cluster's login endpoint and token audience.
    login: String,
    resource: String,
    /// The bearer token and when to renew it.
    token: Option<(String, Instant)>,
    read_only: bool,
}

/// HTTPS unless "Use TLS" was turned off (the emulator serves plain HTTP).
fn tls(cfg: &ConnectionConfig) -> bool {
    !cfg.property("tls").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"))
}

/// The cluster's address without the scheme's default port. A cluster URI typed in the host
/// field (`https://help.kusto.windows.net`) wins over "Use TLS" and the port field.
pub fn cluster_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let (tls, host) = match host.split_once("://") {
        Some((scheme, rest)) => (scheme.eq_ignore_ascii_case("https"), rest),
        None => (tls(cfg), host),
    };
    let authority = host.split('/').next().unwrap_or_default();
    let (host, port) = super::url::parse_authority(authority, cfg.port).unwrap_or((authority.to_string(), cfg.port));
    let host = if host.is_empty() { "localhost".to_string() } else { host };
    let scheme = if tls { "https" } else { "http" };
    match port {
        0 => format!("{scheme}://{host}"),
        443 if tls => format!("{scheme}://{host}"),
        80 if !tls => format!("{scheme}://{host}"),
        port => format!("{scheme}://{host}:{port}"),
    }
}

/// The error Kusto answered with: its detailed `@message` when there is one.
fn service_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let message = api_error_message(&v["error"]).unwrap_or_else(|| {
        // Plain text (`General_BadRequest: …\r\nError details: …`): its first line.
        let text = body.trim().lines().next().unwrap_or_default().trim();
        if text.is_empty() { status.canonical_reason().unwrap_or_default().to_string() } else { text.chars().take(300).collect() }
    });
    anyhow!(trf("HTTP {status} from Kusto: {message}", &[("status", &status), ("message", &message)]))
}

/// `@message` (the details) or `message` of a OneApi error (`{"code": …, "@message": …}`).
fn api_error_message(error: &Value) -> Option<String> {
    error["@message"].as_str().or_else(|| error["message"].as_str()).map(|m| m.trim().to_string()).filter(|m| !m.is_empty())
}

impl KustoSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let tenant = cfg.property("tenant").map(str::to_string);
        let auth = match cfg.auth {
            AuthMode::NoAuth => Auth::None,
            AuthMode::UserPassword => {
                let client_id = cfg.user.trim().to_string();
                if client_id.is_empty() {
                    bail!(tr("The application's client ID is required"));
                }
                if cfg.password.trim().is_empty() {
                    bail!(tr("The application's client secret is required"));
                }
                let Some(tenant) = tenant else { bail!(tr("The tenant is required to sign in as an application")) };
                Auth::Application { tenant, client_id, secret: cfg.password.trim().to_string() }
            }
            AuthMode::AzureCli | AuthMode::Authenticator => Auth::AzureCli { tenant },
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No overall timeout: Kusto stops a query at its own server timeout (4 minutes by
            // default), and the app can stop it sooner.
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(format!("DataCraft/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(http_error)?;
        let base = cluster_url(cfg);
        // A token over plain HTTP could be read on the way; only the emulator (no auth) uses it.
        if !matches!(auth, Auth::None) && !base.starts_with("https://") {
            bail!(tr("Signing in to Kusto needs HTTPS: turn on \"Use TLS\""));
        }
        let (login, resource) = match auth {
            Auth::None => (String::new(), String::new()),
            _ => cloud(&http, &base).await,
        };
        let mut session = Self {
            http,
            base,
            database: cfg.database.trim().to_string(),
            auth,
            login,
            resource,
            token: None,
            read_only: cfg.read_only,
        };
        // Checks the address and the credentials.
        let databases = session.list_databases().await?;
        if session.database.is_empty()
            && let Some(first) = databases.first()
        {
            session.database = first.clone();
        }
        Ok(session)
    }

    /// A bearer token for the cluster, renewed shortly before it expires.
    async fn bearer(&mut self) -> anyhow::Result<Option<String>> {
        if let Some((token, renew)) = &self.token
            && Instant::now() < *renew
        {
            return Ok(Some(token.clone()));
        }
        let (token, lifetime) = match &self.auth {
            Auth::None => return Ok(None),
            Auth::AzureCli { tenant } => az_token(&self.resource, tenant.as_deref()).await?,
            Auth::Application { tenant, client_id, secret } => {
                app_token(&self.http, &self.login, tenant, client_id, secret, &self.resource).await?
            }
        };
        let renew = Instant::now() + lifetime.saturating_sub(TOKEN_MARGIN).max(Duration::from_secs(30));
        self.token = Some((token.clone(), renew));
        Ok(Some(token))
    }

    /// One request to `/v2/rest/query` or `/v1/rest/mgmt`, with the client request `options`.
    async fn post(&mut self, path: &str, db: &str, csl: &str, mut options: Value) -> anyhow::Result<Value> {
        let bearer = self.bearer().await?;
        if self.read_only {
            options["request_readonly"] = json!(true);
            options["request_callout_disabled"] = json!(true);
        }
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header(ACCEPT, "application/json")
            .header("x-ms-app", "DataCraft")
            .header("x-ms-client-version", concat!("DataCraft:", env!("CARGO_PKG_VERSION")))
            .header("x-ms-client-request-id", format!("DataCraft;{}", uuid::Uuid::new_v4()))
            .json(&json!({ "db": db, "csl": csl, "properties": json!({ "Options": options }).to_string() }));
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(http_error)?;
        let status = resp.status();
        let body = resp.text().await.map_err(http_error)?;
        if !status.is_success() {
            return Err(service_error(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| anyhow!(trf("Unreadable response from Kusto: {error}", &[("error", &e)])))
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        Ok(self.database.clone())
    }

    /// The databases the account can see.
    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        let body = self.post("/v1/rest/mgmt", "", ".show databases | project DatabaseName", json!({})).await?;
        let mut names: Vec<String> = parse_v1(&body, usize::MAX)
            .into_iter()
            .next()
            .map(|rs| rs.rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
            .unwrap_or_default();
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        if !db.is_empty() {
            self.database = db.to_string();
        }
        Ok(())
    }

    /// "Azure Data Explorer 1.0.9788.37182" (the emulator and Fabric don't name their offering).
    pub async fn server_version(&mut self) -> Option<String> {
        let body = self.post("/v1/rest/mgmt", "", ".show version", json!({})).await.ok()?;
        let rs = parse_v1(&body, 1).into_iter().next()?;
        let col = |name: &str| rs.columns.iter().position(|c| c == name).and_then(|i| rs.rows.first()?.get(i)?.clone());
        let offering = col("ServiceOffering")
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["Type"].as_str().map(str::to_string))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Kusto".into());
        Some(format!("{offering} {}", col("BuildVersion")?))
    }

    /// The current database's tables and materialized views with their columns, from one
    /// `.show database … schema as json`.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        if self.database.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.database.clone();
        let command = format!(".show database {} schema as json", Driver::Kusto.quote_ident(&db));
        let body = self.post("/v1/rest/mgmt", &db, &command, json!({})).await?;
        let json = parse_v1(&body, 1).into_iter().next().and_then(|rs| rs.rows.into_iter().next()?.into_iter().next()?);
        let schema: Value = json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        Ok(vec![SchemaInfo { name: db, tables: schema_tables(&schema) }])
    }

    /// Runs ONE request: a KQL query (possibly several statements and results) or a control
    /// command.
    pub async fn execute(&mut self, script: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let start = Instant::now();
        let statement = script.trim().to_string();
        if statement.is_empty() {
            return Ok(Vec::new());
        }
        let max_rows = if max_rows == 0 { usize::MAX } else { max_rows };
        let db = self.database.clone();
        let mut results = if is_command(&statement) {
            // From the dot: Kusto refuses comments before a command. Control commands ignore
            // `truncationmaxrecords`, so the rows are cut here.
            let command = skip_comments(&statement).to_string();
            parse_v1(&self.post("/v1/rest/mgmt", &db, &command, json!({})).await?, max_rows)
        } else {
            let options = if max_rows < usize::MAX {
                // The rows up to the limit come, then an error that `parse_v2` takes as the cut.
                json!({ "truncationmaxrecords": max_rows, "deferpartialqueryfailures": true })
            } else {
                json!({})
            };
            parse_v2(&self.post("/v2/rest/query", &db, &statement, options).await?, max_rows)?
        };
        if results.is_empty() {
            results.push(ResultSet::default());
        }
        for rs in &mut results {
            rs.statement.clone_from(&statement);
            rs.elapsed = start.elapsed();
        }
        Ok(results)
    }
}

/// Entra ID login hosts of the Azure clouds: the client secret is only sent to one of these.
const LOGIN_HOSTS: &[&str] = &["login.microsoftonline.com", "login.microsoftonline.us", "login.chinacloudapi.cn", "login.partner.microsoftonline.cn"];
/// Domains of Kusto's token audiences: a token for anything else (Azure Resource Manager…) is
/// never requested for the cluster.
const KUSTO_DOMAINS: &[&str] =
    &["kusto.windows.net", "kusto.usgovcloudapi.net", "kusto.chinacloudapi.cn", "kusto.fabric.microsoft.com", "kustomfa.windows.net"];

/// The host of an `https://` URL, lowercased.
fn https_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    (url.scheme() == "https").then(|| url.host_str().map(str::to_ascii_lowercase)).flatten()
}

/// The cluster's Entra ID login endpoint and token audience, or the public cloud's. The metadata
/// comes from the cluster unauthenticated, so it is only taken when it names a Microsoft login
/// host and a Kusto audience (or the cluster itself): a hostile address can't redirect the client
/// secret, nor get a token meant for another Azure service.
async fn cloud(http: &reqwest::Client, base: &str) -> (String, String) {
    let metadata: Option<Value> = async {
        let resp = http.get(format!("{base}/v1/rest/auth/metadata")).timeout(Duration::from_secs(10)).send().await.ok()?;
        resp.json().await.ok()
    }
    .await;
    trusted_cloud(metadata.as_ref(), base)
}

/// `cloud`'s choice from the cluster's metadata.
fn trusted_cloud(metadata: Option<&Value>, base: &str) -> (String, String) {
    let get = |key: &str| {
        metadata.and_then(|m| m["AzureAD"][key].as_str()).map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty())
    };
    let login = get("LoginEndpoint").filter(|l| https_host(l).is_some_and(|h| LOGIN_HOSTS.contains(&h.as_str())));
    let cluster = https_host(base);
    let resource = get("KustoServiceResourceId").filter(|r| {
        https_host(r).is_some_and(|h| Some(&h) == cluster.as_ref() || KUSTO_DOMAINS.iter().any(|d| h.ends_with(&format!(".{d}"))))
    });
    (login.unwrap_or_else(|| LOGIN_ENDPOINT.into()), resource.unwrap_or_else(|| base.to_string()))
}

/// `az account get-access-token` for the cluster's audience: (token, lifetime).
async fn az_token(resource: &str, tenant: Option<&str>) -> anyhow::Result<(String, Duration)> {
    let mut args: Vec<String> = ["account", "get-access-token", "--resource", resource, "--output", "json"].map(String::from).to_vec();
    if let Some(tenant) = tenant {
        args.extend(["--tenant".to_string(), tenant.to_string()]);
    }
    let output = tokio::task::spawn_blocking(move || run_az(&args)).await.map_err(|e| anyhow!(e.to_string()))??;
    if !output.status.success() {
        // `ERROR: Please run 'az login' to setup account.`
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(|l| l.trim().trim_start_matches("ERROR:").trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        bail!(trf("Azure CLI: {message}", &[("message", &message)]));
    }
    parse_az_token(&output.stdout, SystemTime::now())
}

fn run_az(args: &[String]) -> anyhow::Result<std::process::Output> {
    for program in AZ {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args).stdin(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            // CREATE_NO_WINDOW: no console window flashing over the app.
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            result => return result.map_err(|e| anyhow!(e)),
        }
    }
    bail!(tr("The Azure CLI (az) was not found: install it and run az login, or choose another authentication"))
}

/// The token and its lifetime from `az account get-access-token`'s JSON (`expires_on` is a POSIX
/// time, a number or a string depending on the CLI's version).
fn parse_az_token(stdout: &[u8], now: SystemTime) -> anyhow::Result<(String, Duration)> {
    let unreadable = |e: &dyn std::fmt::Display| anyhow!(trf("Unreadable response from the Azure CLI: {error}", &[("error", &e)]));
    let v: Value = serde_json::from_slice(stdout).map_err(|e| unreadable(&e))?;
    let token = v["accessToken"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| unreadable(&"accessToken"))?;
    let expires = v["expires_on"].as_i64().or_else(|| v["expires_on"].as_str().and_then(|s| s.trim().parse().ok()));
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let lifetime = expires.map_or(Duration::from_secs(600), |e| Duration::from_secs((e - now).max(0) as u64));
    Ok((token.to_string(), lifetime))
}

/// The `application/x-www-form-urlencoded` body of a client credentials token request.
fn token_request_body(client_id: &str, secret: &str, resource: &str) -> String {
    let scope = format!("{resource}/.default");
    let params = [("grant_type", "client_credentials"), ("client_id", client_id), ("client_secret", secret), ("scope", &scope)];
    reqwest::Url::parse_with_params("http://form.invalid/", &params).ok().and_then(|u| u.query().map(str::to_string)).unwrap_or_default()
}

/// A token for an Entra ID application (client credentials): (token, lifetime).
async fn app_token(
    http: &reqwest::Client,
    login: &str,
    tenant: &str,
    client_id: &str,
    secret: &str,
    resource: &str,
) -> anyhow::Result<(String, Duration)> {
    let resp = http
        .post(format!("{login}/{}/oauth2/v2.0/token", tenant.trim()))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(token_request_body(client_id, secret, resource))
        .send()
        .await
        .map_err(http_error)?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        // `AADSTS7000215: Invalid client secret provided. …` (then trace and correlation IDs).
        let message = v["error_description"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .and_then(|m| m.lines().next())
            .map(|m| m.trim().to_string())
            .unwrap_or_else(|| status.to_string());
        bail!(trf("Sign-in failed: {message}", &[("message", &message)]));
    }
    let token = v["access_token"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!(tr("Sign-in failed: no access token")))?;
    let lifetime = v["expires_in"].as_u64().or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok())).unwrap_or(600);
    Ok((token.to_string(), Duration::from_secs(lifetime)))
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn cell(v: &Value) -> Cell {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        // `dynamic`: compact JSON.
        other => Some(other.to_string()),
    }
}

fn column_names(columns: &Value) -> Vec<String> {
    columns.as_array().into_iter().flatten().map(|c| c["ColumnName"].as_str().unwrap_or_default().to_string()).collect()
}

/// A v2 query response (an array of frames): one grid per `PrimaryResult` table. Errors come in
/// the data (a row that is an error object) or at the end (`DataSetCompletion`); the one for
/// `truncationmaxrecords` only marks the grids that reached `max_rows` as cut.
fn parse_v2(body: &Value, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
    let frames = body.as_array().ok_or_else(|| anyhow!(trf("Unreadable response from Kusto: {error}", &[("error", &"frames")])))?;
    let mut results = Vec::new();
    let mut error: Option<Value> = None;
    for frame in frames {
        match frame["FrameType"].as_str() {
            Some("DataTable") if frame["TableKind"] == "PrimaryResult" => {
                let mut rows: Vec<Vec<Cell>> = Vec::new();
                for row in frame["Rows"].as_array().into_iter().flatten() {
                    match row {
                        Value::Array(cells) => rows.push(cells.iter().map(cell).collect()),
                        other => {
                            error.get_or_insert_with(|| other.clone());
                        }
                    }
                }
                let truncated = rows.len() > max_rows;
                rows.truncate(max_rows);
                results.push(ResultSet { columns: column_names(&frame["Columns"]), rows, truncated, ..Default::default() });
            }
            Some("DataSetCompletion") if frame["HasErrors"] == true || frame["Cancelled"] == true => {
                error.get_or_insert_with(|| frame.clone());
            }
            _ => {}
        }
    }
    if let Some(e) = error {
        let first = &e["OneApiErrors"][0]["error"];
        let message = api_error_message(first).unwrap_or_else(|| tr("The query was cancelled").to_string());
        let cut = message.contains("E_QUERY_RESULT_SET_TOO_LARGE");
        let mut marked = false;
        for rs in results.iter_mut().filter(|rs| cut && rs.rows.len() >= max_rows) {
            rs.truncated = true;
            marked = true;
        }
        if !marked {
            bail!(trf("Kusto: {message}", &[("message", &message)]));
        }
    }
    Ok(results)
}

/// A v1 response (`{"Tables": […]}`, control commands). When the last table is a table of
/// contents (`Ordinal, Kind, Name, Id…`), only the tables it lists as `QueryResult` are results.
fn parse_v1(body: &Value, max_rows: usize) -> Vec<ResultSet> {
    let tables: &[Value] = body["Tables"].as_array().map_or(&[], Vec::as_slice);
    let toc = tables.last().filter(|t| column_names(&t["Columns"]).iter().take(4).eq(["Ordinal", "Kind", "Name", "Id"].iter()));
    let wanted: Option<Vec<u64>> = toc.map(|t| {
        t["Rows"].as_array().into_iter().flatten().filter(|r| r[1] == "QueryResult").filter_map(|r| r[0].as_u64()).collect()
    });
    tables
        .iter()
        .enumerate()
        .filter(|(i, _)| wanted.as_ref().is_none_or(|w| w.contains(&(*i as u64))))
        .map(|(_, t)| {
            let mut rows: Vec<Vec<Cell>> =
                t["Rows"].as_array().into_iter().flatten().map(|r| r.as_array().into_iter().flatten().map(cell).collect()).collect();
            let truncated = rows.len() > max_rows;
            rows.truncate(max_rows);
            ResultSet { columns: column_names(&t["Columns"]), rows, truncated, ..Default::default() }
        })
        .collect()
}

/// Tables and materialized views of `.show database … schema as json`, by name.
fn schema_tables(schema: &Value) -> Vec<TableInfo> {
    let Some(db) = schema["Databases"].as_object().and_then(|d| d.values().next()) else { return Vec::new() };
    let mut tables: Vec<TableInfo> = [("Tables", ObjectKind::Table), ("MaterializedViews", ObjectKind::View)]
        .into_iter()
        .flat_map(|(key, kind)| {
            db[key].as_object().into_iter().flat_map(|o| o.values()).map(move |t| TableInfo {
                name: t["Name"].as_str().unwrap_or_default().to_string(),
                kind,
                columns: t["OrderedColumns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| ColumnInfo {
                        name: c["Name"].as_str().unwrap_or_default().to_string(),
                        data_type: c["CslType"].as_str().unwrap_or_default().to_string(),
                        // Every Kusto column takes nulls, and there are no keys.
                        nullable: true,
                        primary_key: false,
                    })
                    .collect(),
            })
        })
        .collect();
    tables.sort_by_key(|t| t.name.to_lowercase());
    tables
}

// ---------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------

/// The text after leading blanks and `//` comment lines.
pub(crate) fn skip_comments(s: &str) -> &str {
    let mut s = s.trim_start();
    while s.starts_with("//") {
        s = s.split_once('\n').map_or("", |(_, rest)| rest).trim_start();
    }
    s
}

/// true for a control command (`.show tables`), false for a query.
pub fn is_command(statement: &str) -> bool {
    skip_comments(statement).starts_with('.')
}

/// true if the request only reads: any query (they can't write, and on read-only connections
/// the server refuses callouts), or a `.show` command. One request holds one command.
pub fn is_read_command(statement: &str) -> bool {
    match skip_comments(statement).strip_prefix('.') {
        Some(rest) => {
            let word = rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_').next().unwrap_or_default();
            word.eq_ignore_ascii_case("show")
        }
        None => true,
    }
}

/// The requests of a script: blocks of lines separated by blank lines, as Kusto Explorer runs
/// them (a blank line inside a ```` ``` ```` multi-line string doesn't end one). Blocks with
/// only `//` comments are skipped. Byte ranges, trimmed.
pub fn split_script(script: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut block: Option<(usize, usize, bool)> = None; // (start, end, has code)
    let mut in_fence = false;
    let mut pos = 0;
    let mut flush = |block: &mut Option<(usize, usize, bool)>| {
        if let Some((start, end, true)) = block.take() {
            out.push((start, end));
        }
    };
    for line in script.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let trimmed = line.trim();
        if trimmed.is_empty() && !in_fence {
            flush(&mut block);
            continue;
        }
        let first = start + (line.len() - line.trim_start().len());
        let last = start + line.trim_end().len();
        let code = in_fence || !trimmed.starts_with("//");
        block = Some(match block {
            Some((s, _, c)) => (s, last.max(s), c || code),
            None => (first, last, code),
        });
        if line.matches("```").count() % 2 == 1 {
            in_fence = !in_fence;
        }
    }
    flush(&mut block);
    out
}

/// The data editor's query: the table, narrowed by the filter row's left side (a KQL
/// predicate: `State == "TEXAS" and DamageProperty > 0`) and sorted by its right side
/// (`StartTime desc`).
pub fn table_query(table: &str, filter: &str, order: &str, max_rows: usize) -> String {
    let mut q = Driver::Kusto.quote_ident(table);
    if !filter.is_empty() {
        q.push_str(&format!("\n| where {filter}"));
    }
    if !order.is_empty() {
        q.push_str(&format!("\n| order by {order}"));
    }
    q.push_str(&format!("\n| take {max_rows}"));
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(script: &str) -> Vec<&str> {
        split_script(script).into_iter().map(|(s, e)| &script[s..e]).collect()
    }

    #[test]
    fn scripts_split_on_blank_lines_not_semicolons() {
        let script = "let n = 2;\nT | take n\n\n  // a comment\n  \n.show tables\n// only comments\n\n// alone\n\nA | count; B | count\n";
        assert_eq!(blocks(script), ["let n = 2;\nT | take n", ".show tables\n// only comments", "A | count; B | count"]);
        // A multi-line string keeps its blank lines.
        let fenced = "print s = ```\n\nkeep\n```\n\nprint 2";
        assert_eq!(blocks(fenced), ["print s = ```\n\nkeep\n```", "print 2"]);
        assert_eq!(blocks("  \n\n"), Vec::<&str>::new());
        // Byte ranges in the original text, for the statement under the cursor.
        let (s, e) = split_script("x\n\n  print 'ü'  \n")[1];
        assert_eq!((s, e), (5, 15));
    }

    #[test]
    fn commands_and_read_only() {
        assert!(is_command("// list\n.show tables"));
        assert!(!is_command("T | where Name startswith '.'"));
        for read in ["T | take 10", "let x = 1; print x", "// c\n.show tables", ".SHOW databases", ".show table T extents | count"] {
            assert!(is_read_command(read), "{read}");
        }
        for write in [".create table T (a: string)", ".set-or-append T <| print 1", ".drop table T", "// c\n.ingest inline into table T <| 1", ".execute database script <| .show tables", ".show-x"] {
            assert!(!is_read_command(write), "{write}");
        }
    }

    #[test]
    fn query_frames_become_grids() {
        let body: Value = serde_json::from_str(
            r#"[{"FrameType":"DataSetHeader","IsProgressive":false,"Version":"v2.0"},
                {"FrameType":"DataTable","TableId":0,"TableKind":"QueryProperties","TableName":"@ExtendedProperties","Columns":[{"ColumnName":"Key","ColumnType":"string"}],"Rows":[["Visualization"]]},
                {"FrameType":"DataTable","TableId":1,"TableKind":"PrimaryResult","TableName":"PrimaryResult","Columns":[{"ColumnName":"l","ColumnType":"long"},{"ColumnName":"r","ColumnType":"real"},{"ColumnName":"s","ColumnType":"string"},{"ColumnName":"d","ColumnType":"datetime"},{"ColumnName":"b","ColumnType":"bool"},{"ColumnName":"dyn","ColumnType":"dynamic"},{"ColumnName":"dec","ColumnType":"decimal"},{"ColumnName":"n","ColumnType":"long"},{"ColumnName":"nan","ColumnType":"real"}],
                 "Rows":[[9007199254740993,1.5,"a\"b","2024-01-02T03:04:05.678Z",true,{"x":[1,2]},"1.25",null,"NaN"]]},
                {"FrameType":"DataTable","TableId":2,"TableKind":"PrimaryResult","TableName":"PrimaryResult","Columns":[{"ColumnName":"Count","ColumnType":"long"}],"Rows":[[59066]]},
                {"FrameType":"DataTable","TableId":3,"TableKind":"QueryCompletionInformation","Columns":[],"Rows":[]},
                {"FrameType":"DataSetCompletion","HasErrors":false,"Cancelled":false}]"#,
        )
        .unwrap();
        let sets = parse_v2(&body, 100).unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].columns, ["l", "r", "s", "d", "b", "dyn", "dec", "n", "nan"]);
        let row: Vec<Option<&str>> = sets[0].rows[0].iter().map(Option::as_deref).collect();
        assert_eq!(
            row,
            [Some("9007199254740993"), Some("1.5"), Some("a\"b"), Some("2024-01-02T03:04:05.678Z"), Some("true"), Some(r#"{"x":[1,2]}"#), Some("1.25"), None, Some("NaN")]
        );
        assert_eq!((sets[1].rows[0][0].as_deref(), sets[1].truncated), (Some("59066"), false));
    }

    fn completion(message: &str) -> String {
        format!(
            r#"{{"FrameType":"DataSetCompletion","HasErrors":true,"Cancelled":false,"OneApiErrors":[{{"error":{{"code":"LimitsExceeded","message":"Request is invalid and cannot be executed.","@message":"{message}"}}}}]}}"#
        )
    }

    #[test]
    fn the_row_limit_error_only_cuts() {
        let grid = |rows: usize| {
            let rows: Vec<String> = (0..rows).map(|i| format!("[{i}]")).collect();
            format!(r#"{{"FrameType":"DataTable","TableKind":"PrimaryResult","Columns":[{{"ColumnName":"x"}}],"Rows":[{}]}}"#, rows.join(","))
        };
        let too_large = "The results of this query exceed the set limit of 3 records (E_QUERY_RESULT_SET_TOO_LARGE, 0x80DA0003).";
        let body: Value = serde_json::from_str(&format!("[{},{},{}]", grid(3), grid(1), completion(too_large))).unwrap();
        let sets = parse_v2(&body, 3).unwrap();
        assert_eq!(sets.iter().map(|rs| (rs.rows.len(), rs.truncated)).collect::<Vec<_>>(), [(3, true), (1, false)]);
        // Other errors stay errors, also with rows.
        let body: Value = serde_json::from_str(&format!("[{},{}]", grid(3), completion("Low memory condition (E_LOW_MEMORY_CONDITION)."))).unwrap();
        assert!(parse_v2(&body, 3).unwrap_err().to_string().contains("E_LOW_MEMORY_CONDITION"));
        // An error in the data.
        let body: Value = serde_json::from_str(
            r#"[{"FrameType":"DataTable","TableKind":"PrimaryResult","Columns":[{"ColumnName":"x"}],"Rows":[[1],{"OneApiErrors":[{"error":{"code":"x","@message":"Partial query failure: boom"}}]}]}]"#,
        )
        .unwrap();
        assert!(parse_v2(&body, 3).unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn command_tables_follow_the_table_of_contents() {
        let body: Value = serde_json::from_str(
            r#"{"Tables":[
                {"TableName":"Table_0","Columns":[{"ColumnName":"DatabaseName","DataType":"String"}],"Rows":[["Samples"],["Trender"],["X"]]},
                {"TableName":"Table_1","Columns":[{"ColumnName":"Value","DataType":"String"}],"Rows":[["{}"]]},
                {"TableName":"Table_2","Columns":[{"ColumnName":"Ordinal"},{"ColumnName":"Kind"},{"ColumnName":"Name"},{"ColumnName":"Id"},{"ColumnName":"PrettyName"}],
                 "Rows":[[0,"QueryResult","PrimaryResult","",""],[1,"QueryProperties","@ExtendedProperties","",""]]}]}"#,
        )
        .unwrap();
        let sets = parse_v1(&body, 2);
        assert_eq!(sets.len(), 1);
        assert_eq!((sets[0].columns.as_slice(), sets[0].rows.len(), sets[0].truncated), (["DatabaseName".to_string()].as_slice(), 2, true));
        // Without a table of contents, every table.
        let body: Value = serde_json::from_str(r#"{"Tables":[{"TableName":"Table_0","Columns":[{"ColumnName":"BuildVersion"}],"Rows":[["1.0"]]}]}"#).unwrap();
        assert_eq!(parse_v1(&body, 10)[0].rows, [[Some("1.0".to_string())]]);
    }

    #[test]
    fn schema_lists_tables_and_materialized_views() {
        let schema: Value = serde_json::from_str(
            r#"{"Databases":{"Samples":{"Name":"Samples",
                "Tables":{"StormEvents":{"Name":"StormEvents","OrderedColumns":[{"Name":"StartTime","Type":"System.DateTime","CslType":"datetime"},{"Name":"State","CslType":"string"}]},
                          "a_table":{"Name":"a_table","OrderedColumns":[]}},
                "MaterializedViews":{"DailyCovid19":{"Name":"DailyCovid19","OrderedColumns":[{"Name":"Count","CslType":"long"}]}},
                "Functions":{"MyFunction1":{"Name":"MyFunction1"}}}}}"#,
        )
        .unwrap();
        let tables = schema_tables(&schema);
        let names: Vec<(&str, ObjectKind)> = tables.iter().map(|t| (t.name.as_str(), t.kind)).collect();
        assert_eq!(names, [("a_table", ObjectKind::Table), ("DailyCovid19", ObjectKind::View), ("StormEvents", ObjectKind::Table)]);
        let storm: Vec<(&str, &str)> = tables[2].columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
        assert_eq!(storm, [("StartTime", "datetime"), ("State", "string")]);
    }

    #[test]
    fn errors_carry_the_details() {
        let body = r#"{"error":{"code":"General_BadRequest","message":"Request is invalid and cannot be executed.","@message":"Syntax error: SYN0002: Query operator expected. [line:position=1:15]"}}"#;
        let err = service_error(reqwest::StatusCode::BAD_REQUEST, body).to_string();
        assert!(err.contains("400 Bad Request") && err.contains("SYN0002"), "{err}");
        let err = service_error(reqwest::StatusCode::BAD_REQUEST, "General_BadRequest: Request is invalid.\r\nError details: x").to_string();
        assert!(err.ends_with("General_BadRequest: Request is invalid."), "{err}");
        let err = service_error(reqwest::StatusCode::UNAUTHORIZED, "").to_string();
        assert!(err.contains("401 Unauthorized"), "{err}");
    }

    #[test]
    fn editor_query_and_names() {
        assert_eq!(table_query("My Table", "", "", 50), "['My Table']\n| take 50");
        assert_eq!(table_query("T", "State == \"TX\"", "StartTime desc", 10), "['T']\n| where State == \"TX\"\n| order by StartTime desc\n| take 10");
        assert_eq!(Driver::Kusto.quote_ident(r"a'b\c"), r"['a\'b\\c']");
    }

    #[test]
    fn cluster_addresses() {
        let mut cfg = ConnectionConfig::new(1, Driver::Kusto);
        cfg.host = "help.kusto.windows.net".into();
        assert_eq!(cluster_url(&cfg), "https://help.kusto.windows.net");
        cfg.host = "HTTPS://help.kusto.windows.net/Samples".into();
        assert_eq!(cluster_url(&cfg), "https://help.kusto.windows.net");
        cfg.host = "http://localhost:8080".into();
        assert_eq!(cluster_url(&cfg), "http://localhost:8080");
        cfg.host = "localhost".into();
        cfg.port = 8080;
        cfg.set_property("tls", "false");
        assert_eq!(cluster_url(&cfg), "http://localhost:8080");
    }

    #[test]
    fn only_microsoft_logins_and_kusto_audiences_are_trusted() {
        let meta = |login: &str, resource: &str| json!({ "AzureAD": { "LoginEndpoint": login, "KustoServiceResourceId": resource } });
        let base = "https://help.kusto.windows.net";
        let public = meta("https://login.microsoftonline.com/", "https://kusto.kusto.windows.net");
        assert_eq!(trusted_cloud(Some(&public), base), (LOGIN_ENDPOINT.into(), "https://kusto.kusto.windows.net".into()));
        let gov = meta("https://login.microsoftonline.us", "https://kusto.kusto.usgovcloudapi.net");
        assert_eq!(trusted_cloud(Some(&gov), "https://c.kusto.usgovcloudapi.net").0, "https://login.microsoftonline.us");
        // A hostile cluster: the secret stays with Microsoft's login, the token is for the cluster.
        let evil = meta("https://login.evil.example", "https://management.azure.com");
        assert_eq!(trusted_cloud(Some(&evil), "https://evil.example"), (LOGIN_ENDPOINT.into(), "https://evil.example".into()));
        let lookalike = meta("http://login.microsoftonline.com", "https://kusto.windows.net.evil.example");
        assert_eq!(trusted_cloud(Some(&lookalike), base), (LOGIN_ENDPOINT.into(), base.into()));
        // The cluster's own address as the audience.
        assert_eq!(trusted_cloud(Some(&meta("", "https://my.cluster.example/")), "https://my.cluster.example").1, "https://my.cluster.example");
        assert_eq!(trusted_cloud(None, base), (LOGIN_ENDPOINT.into(), base.into()));
    }

    #[test]
    fn azure_cli_tokens() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let (token, lifetime) = parse_az_token(br#"{"accessToken":"t","expires_on":1003600,"tokenType":"Bearer"}"#, now).unwrap();
        assert_eq!((token.as_str(), lifetime), ("t", Duration::from_secs(3600)));
        // Older CLIs: a string.
        let (_, lifetime) = parse_az_token(br#"{"accessToken":"t","expires_on":"1000060"}"#, now).unwrap();
        assert_eq!(lifetime, Duration::from_secs(60));
        assert!(parse_az_token(b"not json", now).is_err());
        assert!(parse_az_token(br#"{"accessToken":""}"#, now).is_err());
    }

    #[test]
    fn client_credentials_form() {
        let body = token_request_body("id", "s&c=r t+", "https://kusto.kusto.windows.net");
        assert_eq!(body, "grant_type=client_credentials&client_id=id&client_secret=s%26c%3Dr+t%2B&scope=https%3A%2F%2Fkusto.kusto.windows.net%2F.default");
    }
}


/// Against the Kusto emulator (DATACRAFT_KUSTO_URL, e.g. `http://127.0.0.1:8080`; no
/// authentication): each test works in a volatile database of its own, dropped at the end. The
/// `real_cluster` tests read a real cluster with the Azure CLI's account (DATACRAFT_KUSTO_AZ_URL,
/// e.g. `https://help.kusto.windows.net/Samples`, a database with a `StormEvents` table).
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::db::Session;

    fn emulator_cfg() -> Option<ConnectionConfig> {
        let url = std::env::var("DATACRAFT_KUSTO_URL").ok()?;
        let mut cfg = ConnectionConfig::new(1, Driver::Kusto);
        assert!(crate::db::url::apply_jdbc_url(&mut cfg, &url), "{url}");
        cfg.auth = AuthMode::NoAuth;
        Some(cfg)
    }

    fn cluster_cfg() -> Option<ConnectionConfig> {
        let url = std::env::var("DATACRAFT_KUSTO_AZ_URL").ok()?;
        let mut cfg = ConnectionConfig::new(1, Driver::Kusto);
        assert!(crate::db::url::apply_jdbc_url(&mut cfg, &url), "{url}");
        assert_eq!(cfg.auth, AuthMode::AzureCli);
        Some(cfg)
    }

    /// A database with a `Storm Events` table, a `ByState` materialized view and a `Texas()`
    /// function.
    struct Fixture {
        cfg: ConnectionConfig,
    }

    impl Fixture {
        async fn new() -> Option<Self> {
            let mut cfg = emulator_cfg()?;
            cfg.database = format!("dc_test_{}", uuid::Uuid::new_v4().simple());
            Self::admin().await?.execute(&format!(".create database {} volatile", cfg.database), 10).await.unwrap();
            let mut s = KustoSession::connect(&cfg).await.unwrap();
            for command in [
                ".create table ['Storm Events'] (StartTime: datetime, State: string, Damage: long, Ratio: real, Tags: dynamic, Ok: bool)",
                ".ingest inline into table ['Storm Events'] <|\n\
                 2024-01-01T00:00:00Z,TEXAS,100,0.5,\"{\"\"a\"\":[1,2]}\",true\n\
                 2024-01-02T00:00:00Z,\"NEW YORK, \"\"NY\"\"\",0,,[],false\n\
                 2024-01-03T00:00:00Z,TEXAS,250,1.25,,true",
                ".create materialized-view ByState on table ['Storm Events'] { ['Storm Events'] | summarize Total = sum(Damage) by State }",
                ".create function Texas() { ['Storm Events'] | where State == 'TEXAS' }",
            ] {
                s.execute(command, 10).await.unwrap_or_else(|e| panic!("{command}: {e}"));
            }
            Some(Self { cfg })
        }

        async fn admin() -> Option<KustoSession> {
            let mut cfg = emulator_cfg()?;
            cfg.database = "NetDefaultDB".into();
            Some(KustoSession::connect(&cfg).await.unwrap())
        }

        async fn session(&self) -> KustoSession {
            KustoSession::connect(&self.cfg).await.unwrap()
        }

        async fn drop_database(self) {
            Self::admin().await.unwrap().execute(&format!(".drop database {} ifexists", self.cfg.database), 10).await.unwrap();
        }
    }

    fn column<'a>(rs: &'a ResultSet, name: &str) -> Vec<Option<&'a str>> {
        let i = rs.columns.iter().position(|c| c == name).unwrap_or_else(|| panic!("no {name} in {:?}", rs.columns));
        rs.rows.iter().map(|r| r[i].as_deref()).collect()
    }

    #[tokio::test]
    #[ignore]
    async fn explorer() {
        let Some(f) = Fixture::new().await else { return };
        let mut s = f.session().await;
        assert!(s.list_databases().await.unwrap().contains(&f.cfg.database));
        assert!(s.server_version().await.unwrap().starts_with("Kusto 1."));
        let schema = s.introspect().await.unwrap();
        assert_eq!(schema[0].name, f.cfg.database);
        let objects: Vec<(&str, ObjectKind)> = schema[0].tables.iter().map(|t| (t.name.as_str(), t.kind)).collect();
        assert_eq!(objects, [("ByState", ObjectKind::View), ("Storm Events", ObjectKind::Table)]);
        let columns: Vec<String> = schema[0].tables[1].columns.iter().map(|c| format!("{} {}", c.name, c.data_type)).collect();
        assert_eq!(columns, ["StartTime datetime", "State string", "Damage long", "Ratio real", "Tags dynamic", "Ok bool"]);
        f.drop_database().await;
    }

    #[tokio::test]
    #[ignore]
    async fn queries_and_commands() {
        let Some(f) = Fixture::new().await else { return };
        let mut s = f.session().await;
        // One request, two results; `;` is KQL's.
        let script = "let s = 'TEXAS';\n['Storm Events'] | where State == s | count;\n['Storm Events'] | where State != s | project State, Tags, Ratio";
        let sets = s.execute(script, 100).await.unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(column(&sets[0], "Count"), [Some("2")]);
        assert_eq!(column(&sets[1], "State"), [Some("NEW YORK, \"NY\"")]);
        assert_eq!((column(&sets[1], "Tags"), column(&sets[1], "Ratio")), (vec![Some("[]")], vec![None]));
        assert!(sets.iter().all(|rs| rs.statement == script));
        let rs = s.execute("['Storm Events'] | where Damage == 100 | project StartTime, Tags, Ok, Ratio", 100).await.unwrap().remove(0);
        assert_eq!(rs.rows[0].iter().map(Option::as_deref).collect::<Vec<_>>(), [Some("2024-01-01T00:00:00Z"), Some(r#"{"a":[1,2]}"#), Some("true"), Some("0.5")]);
        // Control commands and functions.
        let rs = s.execute("// the tables\n.show tables | project TableName", 100).await.unwrap().remove(0);
        assert_eq!(column(&rs, "TableName"), [Some("Storm Events")]);
        assert_eq!(column(&s.execute("Texas() | count", 100).await.unwrap()[0], "Count"), [Some("2")]);
        // A script is split by blank lines, as the console runs it.
        let script = "print a = 1\n\n// next\nprint b = 2; print c = 3";
        let requests: Vec<String> = crate::sql::split_batches(script, Driver::Kusto).into_iter().map(|st| st.text).collect();
        assert_eq!(requests, ["print a = 1", "// next\nprint b = 2; print c = 3"]);
        assert_eq!(s.execute(&requests[1], 10).await.unwrap().len(), 2);
        // Errors carry Kusto's details.
        let err = s.execute("['Storm Events'] | wher x", 10).await.unwrap_err().to_string();
        assert!(err.contains("400 Bad Request") && err.contains("SYN0002"), "{err}");
        let err = s.execute("NoSuchTable | take 1", 10).await.unwrap_err().to_string();
        assert!(err.contains("NoSuchTable"), "{err}");
        let err = s.execute(".show tablez", 10).await.unwrap_err().to_string();
        assert!(err.contains("Syntax error"), "{err}");
        f.drop_database().await;
    }

    #[tokio::test]
    #[ignore]
    async fn row_limit() {
        let Some(f) = Fixture::new().await else { return };
        let mut s = f.session().await;
        let rs = s.execute("range x from 1 to 100000 step 1", 100).await.unwrap().remove(0);
        assert_eq!((rs.rows.len(), rs.truncated, rs.rows[99][0].as_deref()), (100, true, Some("100")));
        let rs = s.execute("range x from 1 to 100 step 1", 100).await.unwrap().remove(0);
        assert_eq!((rs.rows.len(), rs.truncated), (100, false));
        // Per result: only the second one is cut.
        let sets = s.execute("range x from 1 to 5 step 1; range y from 1 to 50 step 1", 10).await.unwrap();
        assert_eq!(sets.iter().map(|rs| (rs.rows.len(), rs.truncated)).collect::<Vec<_>>(), [(5, false), (10, true)]);
        // Control commands are cut here.
        let rs = s.execute(".show databases", 1).await.unwrap().remove(0);
        assert_eq!((rs.rows.len(), rs.truncated), (1, true));
        f.drop_database().await;
    }

    #[tokio::test]
    #[ignore]
    async fn data_editor_and_dispatch() {
        let Some(f) = Fixture::new().await else { return };
        let mut s = Session::connect(&f.cfg).await.unwrap();
        let q = crate::app::table_query(Driver::Kusto, &f.cfg.database, "Storm Events", "State == \"TEXAS\"", "Damage desc", 100);
        let rs = s.execute(&q, 100).await.unwrap().remove(0);
        assert_eq!(column(&rs, "Damage"), [Some("250"), Some("100")]);
        let q = crate::app::table_query(Driver::Kusto, &f.cfg.database, "ByState", "", "State asc", 100);
        assert_eq!(column(&s.execute(&q, 100).await.unwrap()[0], "Total"), [Some("0"), Some("350")]);
        assert!(s.apply_changes(&["x".into()]).await.is_err());
        assert!(s.run_transaction(&["x".into()]).await.is_err());
        assert_eq!(s.current_database().await.unwrap(), f.cfg.database);
        f.drop_database().await;
    }

    #[tokio::test]
    #[ignore]
    async fn read_only_connections() {
        let Some(f) = Fixture::new().await else { return };
        let mut cfg = f.cfg.clone();
        cfg.read_only = true;
        let mut s = KustoSession::connect(&cfg).await.unwrap();
        // The server refuses writes, whatever the app lets through.
        for write in [".create table Blocked (a: string)", ".set-or-append ['Storm Events'] <| print StartTime = now()"] {
            let err = s.execute(write, 10).await.unwrap_err().to_string();
            assert!(err.contains("readonly"), "{write}: {err}");
        }
        let err = s.execute("evaluate http_request('https://127.0.0.1/')", 10).await.unwrap_err().to_string();
        assert!(err.contains("request_callout_disabled"), "{err}");
        assert_eq!(column(&s.execute(".show tables", 10).await.unwrap()[0], "TableName"), [Some("Storm Events")]);
        assert_eq!(column(&s.execute("Texas() | count", 10).await.unwrap()[0], "Count"), [Some("2")]);
        f.drop_database().await;
    }

    #[tokio::test]
    #[ignore]
    async fn real_cluster() {
        let Some(cfg) = cluster_cfg() else { return };
        let mut s = KustoSession::connect(&cfg).await.unwrap();
        assert!(s.list_databases().await.unwrap().contains(&cfg.database));
        let version = s.server_version().await.unwrap();
        assert!(version.starts_with("Azure Data Explorer "), "{version}");
        let schema = s.introspect().await.unwrap();
        let storm = schema[0].tables.iter().find(|t| t.name == "StormEvents").expect("StormEvents");
        assert_eq!((storm.kind, storm.columns[0].name.as_str(), storm.columns[0].data_type.as_str()), (ObjectKind::Table, "StartTime", "datetime"));
        let rs = s.execute("StormEvents | project State", 10).await.unwrap().remove(0);
        assert_eq!((rs.rows.len(), rs.truncated), (10, true));
        // Read-only: the server takes the flags.
        let mut cfg = cfg.clone();
        cfg.read_only = true;
        let mut s = KustoSession::connect(&cfg).await.unwrap();
        assert_eq!(column(&s.execute("StormEvents | count", 10).await.unwrap()[0], "Count").len(), 1);
    }

    #[tokio::test]
    #[ignore]
    async fn real_cluster_sign_in_errors() {
        let Some(mut cfg) = cluster_cfg() else { return };
        cfg.auth = AuthMode::UserPassword;
        cfg.user = "00000000-0000-0000-0000-000000000001".into();
        cfg.password = "not-a-secret".into();
        let err = KustoSession::connect(&cfg).await.err().unwrap().to_string();
        assert!(err.contains("tenant"), "{err}");
        cfg.set_property("tenant", "00000000-0000-0000-0000-000000000002");
        let err = KustoSession::connect(&cfg).await.err().unwrap().to_string();
        assert!(err.starts_with("Sign-in failed: AADSTS"), "{err}");
        cfg.auth = AuthMode::AzureCli;
        let err = KustoSession::connect(&cfg).await.err().unwrap().to_string();
        assert!(err.starts_with("Azure CLI: "), "{err}");
    }
}
